//! `html`: parse a page and query it with CSS selectors, backed by `scraper`.
//!
//! Parsed trees live outside the Lua memory limit and the GC sees only small userdata, so each
//! plugin state gets a budget of live parsed nodes and input bytes, charged at parse and
//! returned when the last userdata of a document is collected.
//!
//! Parsing and selecting run as host jobs (see [`HostJobs`]), off the Lua thread and under the
//! call's deadline: both can cost far more than their input's size suggests. Deep nesting is
//! what makes them slow (each start tag and each selector step can walk every open element), so
//! documents nested over [`MAX_DEPTH`] elements are refused.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use ego_tree::NodeId;
use ego_tree::iter::Edge;
use html5ever::tendril::{StrTendril, TendrilSink};
use mlua::{Lua, LuaString, UserData, UserDataMethods};
use scraper::{ElementRef, Html, HtmlTreeSink, Node, Selector};

use crate::plugins::host::text::over_cap;
use crate::plugins::host::{HostJobs, stopped};

const PARSE_CAP: usize = 16 << 20;
const NODE_BUDGET: usize = 2_000_000;
/// Input bytes of live documents: plain text parses to a few nodes but keeps every byte.
const BYTE_BUDGET: usize = 64 << 20;
/// The deepest element nesting a document may have, counting `<html>` and `<body>`.
pub const MAX_DEPTH: usize = 512;
/// Parsing takes the input this much at a time, checking the depth and the stop flag between
/// pieces, so it stops soon after it goes too deep or its call gives up.
const PARSE_PIECE: usize = 4 << 10;

/// What a document holds, or what one plugin state's live documents hold together.
#[derive(Default, Clone, Copy)]
struct Held {
    nodes: usize,
    bytes: usize,
}

#[derive(Default)]
struct Budget(Mutex<Held>);

impl Budget {
    fn held(&self) -> MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `doc` to what's held, or says which limit it would pass.
    fn try_charge(&self, doc: Held) -> Result<(), String> {
        let mut held = self.held();
        if held.nodes + doc.nodes > NODE_BUDGET {
            return Err(format!("{NODE_BUDGET} nodes"));
        }
        if held.bytes + doc.bytes > BYTE_BUDGET {
            return Err(format!("{} MiB of input", BYTE_BUDGET >> 20));
        }
        held.nodes += doc.nodes;
        held.bytes += doc.bytes;
        Ok(())
    }
}

/// A parsed page. `Html` is `Send` but not `Sync`, and mlua userdata must be both, hence the
/// mutex: a `select` job holds it while it runs.
struct Parsed {
    html: Mutex<Html>,
    charged: Held,
    budget: Arc<Budget>,
    jobs: HostJobs,
}

impl Drop for Parsed {
    fn drop(&mut self) {
        let mut held = self.budget.held();
        held.nodes -= self.charged.nodes;
        held.bytes -= self.charged.bytes;
    }
}

type Doc = Arc<Parsed>;

struct Document(Doc);

/// An element of a document, found again by its node id on each method call.
struct Element {
    doc: Doc,
    node: NodeId,
}

impl UserData for Document {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("select", |lua, this, css: String| {
            let doc = this.0.clone();
            async move { select(&lua, doc, None, css, usize::MAX).await }
        });
        methods.add_async_method("select_one", |lua, this, css: String| {
            let doc = this.0.clone();
            async move { Ok(select(&lua, doc, None, css, 1).await?.pop()) }
        });
    }
}

impl UserData for Element {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method("select", |lua, this, css: String| {
            let (doc, node) = (this.doc.clone(), this.node);
            async move { select(&lua, doc, Some(node), css, usize::MAX).await }
        });
        methods.add_async_method("select_one", |lua, this, css: String| {
            let (doc, node) = (this.doc.clone(), this.node);
            async move { Ok(select(&lua, doc, Some(node), css, 1).await?.pop()) }
        });
        methods.add_method("text", |_, this, ()| {
            let html = lock(&this.doc)?;
            let text: String = element(&html, this.node).text().collect();
            Ok(text.trim().to_string())
        });
        methods.add_method("attr", |_, this, name: String| {
            let html = lock(&this.doc)?;
            Ok(element(&html, this.node)
                .value()
                .attr(&name)
                .map(str::to_string))
        });
        methods.add_method("html", |_, this, ()| {
            let html = lock(&this.doc)?;
            Ok(element(&html, this.node).inner_html())
        });
    }
}

/// The document, unless a `select` job that ran out of time still holds it: the Lua thread
/// must not wait for that.
fn lock(doc: &Doc) -> mlua::Result<MutexGuard<'_, Html>> {
    match doc.html.try_lock() {
        Ok(html) => Ok(html),
        Err(TryLockError::Poisoned(e)) => Ok(e.into_inner()),
        Err(TryLockError::WouldBlock) => Err(mlua::Error::runtime(
            "html: the document is still busy with a query that took too long",
        )),
    }
}

fn element(html: &Html, node: NodeId) -> ElementRef<'_> {
    html.tree
        .get(node)
        .and_then(ElementRef::wrap)
        .expect("an Element's node is an element of its own document")
}

/// Up to `limit` elements under `scope` (the whole document when `None`) matching `css`, in
/// document order. Runs as a host job.
async fn select(
    lua: &Lua,
    doc: Doc,
    scope: Option<NodeId>,
    css: String,
    limit: usize,
) -> mlua::Result<Vec<Element>> {
    let d = doc.clone();
    let nodes = doc
        .jobs
        .run(lua, "html.select", move |stop| {
            let selector = Selector::parse(&css)
                .map_err(|e| mlua::Error::runtime(format!("invalid selector {css}: {e}")))?;
            let html = d.html.lock().unwrap_or_else(PoisonError::into_inner);
            matching(&html, scope, &selector, limit, stop)
        })
        .await?;
    Ok(nodes
        .into_iter()
        .map(|node| Element {
            doc: doc.clone(),
            node,
        })
        .collect())
}

/// What scraper's `Html::select` and `ElementRef::select` find, checking `stop` at every
/// node.
fn matching(
    html: &Html,
    scope: Option<NodeId>,
    selector: &Selector,
    limit: usize,
    stop: &AtomicBool,
) -> mlua::Result<Vec<NodeId>> {
    let mut found = Vec::new();
    match scope {
        None => {
            for node in html.tree.nodes() {
                if found.len() >= limit {
                    break;
                }
                stopped(stop)?;
                if let Some(e) = ElementRef::wrap(node)
                    && e.parent().is_some()
                    && selector.matches(&e)
                {
                    found.push(e.id());
                }
            }
        }
        Some(scope) => {
            let scope = element(html, scope);
            let mut edges = scope.traverse();
            edges.next(); // `scope` itself
            for edge in edges {
                if found.len() >= limit {
                    break;
                }
                stopped(stop)?;
                if let Edge::Open(node) = edge
                    && let Some(e) = ElementRef::wrap(node)
                    && selector.matches_with_scope(&e, Some(scope))
                {
                    found.push(e.id());
                }
            }
        }
    }
    Ok(found)
}

/// Parses `text` a piece at a time, refusing it once it nests deeper than [`MAX_DEPTH`].
fn parse(text: &str, stop: &AtomicBool) -> mlua::Result<Html> {
    let too_deep = || {
        mlua::Error::runtime(format!(
            "html.parse: the document nests elements more than {MAX_DEPTH} deep"
        ))
    };
    let mut parser =
        html5ever::parse_document(HtmlTreeSink::new(Html::new_document()), Default::default());
    let mut rest = text;
    while !rest.is_empty() {
        stopped(stop)?;
        let mut cut = rest.len().min(PARSE_PIECE);
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (piece, after) = rest.split_at(cut);
        rest = after;
        parser.process(StrTendril::from_slice(piece));
        // The newest node sits where the parser is inserting, as deep as its open elements.
        let html = parser.tokenizer.sink.sink.0.borrow();
        if let Some(newest) = html.tree.nodes().next_back()
            && depth(newest) > MAX_DEPTH
        {
            return Err(too_deep());
        }
    }
    stopped(stop)?;
    let html = parser.finish();
    // Nodes can move as the parser fixes up misnested tags, so check them all.
    if max_depth(&html) > MAX_DEPTH {
        return Err(too_deep());
    }
    Ok(html)
}

/// The elements from `node` up to the root.
fn depth(node: ego_tree::NodeRef<'_, Node>) -> usize {
    std::iter::once(node)
        .chain(node.ancestors())
        .filter(|n| n.value().is_element())
        .count()
}

fn max_depth(html: &Html) -> usize {
    let (mut depth, mut max) = (0, 0);
    for edge in html.tree.root().traverse() {
        match edge {
            Edge::Open(n) if n.value().is_element() => {
                depth += 1;
                max = max.max(depth);
            }
            Edge::Close(n) if n.value().is_element() => depth -= 1,
            _ => {}
        }
    }
    max
}

/// Registers the `html` global: `html.parse(text)`.
pub(crate) fn install(lua: &Lua, jobs: HostJobs) -> mlua::Result<()> {
    let budget = Arc::new(Budget::default());
    let module = lua.create_table()?;
    let parse = lua.create_async_function(move |lua, text: LuaString| {
        let (budget, jobs) = (budget.clone(), jobs.clone());
        async move {
            if text.as_bytes().len() > PARSE_CAP {
                return Err(over_cap("html.parse: the input", PARSE_CAP));
            }
            let input = text.as_bytes().to_vec();
            drop(text);
            let (html, charged) = jobs
                .run(&lua, "html.parse", move |stop| {
                    // Charged as decoded: each invalid byte becomes a 3-byte U+FFFD the tree
                    // keeps.
                    let decoded = String::from_utf8_lossy(&input);
                    let html = parse(&decoded, stop)?;
                    let charged = Held {
                        nodes: html.tree.values().count(),
                        bytes: decoded.len(),
                    };
                    Ok((html, charged))
                })
                .await?;
            // Documents the plugin dropped still count until the GC collects them.
            if budget.try_charge(charged).is_err() {
                lua.gc_collect()?;
                if let Err(limit) = budget.try_charge(charged) {
                    return Err(mlua::Error::runtime(format!(
                        "html.parse: documents held at once are over {limit}"
                    )));
                }
            }
            Ok(Document(Arc::new(Parsed {
                html: Mutex::new(html),
                charged,
                budget: budget.clone(),
                jobs,
            })))
        }
    })?;
    module.raw_set("parse", parse)?;
    lua.globals().set("html", module)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use mlua::Value;

    use crate::plugins::host::testing::{run, run_err, runtime_with};
    use crate::plugins::runtime::{HostConfig, LibSource, Runtime};

    const LIST: &str = r#"local doc = html.parse('<div id=l><a href="/m/1"> One </a><a href="/m/2">Two</a></div>')"#;

    #[tokio::test]
    async fn html_select_text_attr_nested() {
        let (count, first, second_href, nested, missing): (i64, String, String, i64, mlua::Value) =
            run(&format!(
                r##"{LIST}
                   local links = doc:select("#l a")
                   return #links, links[1]:text(), links[2]:attr("href"),
                       #doc:select_one("#l"):select("a"), links[1]:attr("nope")"##
            ))
            .await;
        assert_eq!(count, 2);
        assert_eq!(first, "One");
        assert_eq!(second_href, "/m/2");
        assert_eq!(nested, 2);
        assert_eq!(missing, mlua::Value::Nil);
    }

    #[tokio::test]
    async fn html_iterates_and_reads_inner_html() {
        let (hrefs, inner, none): (String, String, bool) = run(&format!(
            r#"{LIST}
               local hrefs = {{}}
               for _, a in doc:select("a") do table.insert(hrefs, a:attr("href")) end
               local div = doc:select_one("div")
               return table.concat(hrefs, ","), div:select_one("a"):html(),
                   doc:select_one("p") == nil and div:select_one("p") == nil"#
        ))
        .await;
        assert_eq!(hrefs, "/m/1,/m/2");
        assert_eq!(inner, " One ");
        assert!(none);
    }

    #[tokio::test]
    async fn html_text_keeps_inner_whitespace() {
        let text: String =
            run(r#"return html.parse("<p>\n  a <b>b</b>  c\n</p>"):select_one("p"):text()"#).await;
        assert_eq!(text, "a b  c");
    }

    #[tokio::test]
    async fn html_parse_input_is_capped() {
        let err = run_err(r#"return html.parse(string.rep(" ", 17 * 1024 * 1024))"#).await;
        assert!(
            err.contains("html.parse: the input is over 16 MiB"),
            "{err}"
        );
    }

    /// Each document is a bit over 700,000 nodes: three exceed the budget, and two (one held
    /// by a stale register) fit.
    const BIG: &str = r#"local s = string.rep("<br>", 700000)"#;

    #[tokio::test]
    async fn html_held_documents_share_a_node_budget() {
        // An element keeps its document, and its nodes, alive.
        let err = run_err(&format!(
            r#"{BIG} local held = {{html.parse(s), html.parse(s):select_one("br")}}
               return held, html.parse(s)"#
        ))
        .await;
        assert!(
            err.contains("html.parse: documents held at once are over 2000000 nodes"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn html_dropped_documents_return_their_nodes() {
        let n: i64 = run(&format!(
            r#"{BIG} local n = 0
               for i = 1, 4 do
                   if html.parse(s):select_one("br") then n += 1 end
               end
               return n"#
        ))
        .await;
        assert_eq!(n, 4);
    }

    /// Plain text parses to a handful of nodes but keeps every byte.
    const TEXT: &str = r#"local s = string.rep("x", 15 * 1024 * 1024)"#;

    #[tokio::test]
    async fn html_held_documents_share_a_byte_budget() {
        let err = run_err(&format!(
            "{TEXT} local held = {{}} for i = 1, 5 do held[i] = html.parse(s) end"
        ))
        .await;
        assert!(
            err.contains("html.parse: documents held at once are over 64 MiB of input"),
            "{err}"
        );
        let n: i64 = run(&format!(
            r#"{TEXT} local n = 0
               for i = 1, 6 do
                   if html.parse(s):select_one("body") then n += 1 end
               end
               return n"#
        ))
        .await;
        assert_eq!(n, 6);
    }

    /// Each invalid byte decodes to U+FFFD (3 bytes), which is what the document keeps.
    #[tokio::test]
    async fn html_byte_budget_charges_the_decoded_text() {
        let err = run_err(
            r#"local s = string.rep("\255", 6 * 1024 * 1024)
               local held = {} for i = 1, 4 do held[i] = html.parse(s) end"#,
        )
        .await;
        assert!(err.contains("over 64 MiB of input"), "{err}");
    }

    #[tokio::test]
    async fn html_invalid_selector_errors() {
        let err = run_err(r#"return html.parse("<p></p>"):select("a[")"#).await;
        assert!(err.contains("invalid selector a[: "), "{err}");
        let err = run_err(r#"return html.parse("<p>x</p>"):select_one("p"):select_one("!")"#).await;
        assert!(err.contains("invalid selector !: "), "{err}");
    }

    #[tokio::test]
    async fn html_depth_is_capped() {
        // `<html>` and `<body>` count: 510 divs nest 512 deep.
        let n: i64 = run(r#"return #html.parse(string.rep("<div>", 510)):select("div")"#).await;
        assert_eq!(n, 510);
        let err = run_err(r#"return html.parse(string.rep("<div>", 511))"#).await;
        assert!(
            err.contains("html.parse: the document nests elements more than 512 deep"),
            "{err}"
        );
        // Deep, then closed again within one piece of input: the final check finds it.
        let err = run_err(
            r#"return html.parse(string.rep("<div>", 600) .. string.rep("</div>", 600) .. "<p>")"#,
        )
        .await;
        assert!(err.contains("more than 512 deep"), "{err}");
    }

    fn short_deadline() -> HostConfig {
        let mut host = HostConfig::default();
        host.deadline = Duration::from_millis(300);
        host
    }

    async fn run_with_short_deadline(body: &str) -> (String, Duration) {
        let rt = runtime_with(body, short_deadline());
        let start = Instant::now();
        let err = match rt.call::<_, Value>("run", ()).await {
            Ok(v) => panic!("expected an error, got {v:?}"),
            Err(e) => e.to_string(),
        };
        (err, start.elapsed())
    }

    /// The review's repro: thousands of nested divs and a descendant selector took 19 s (20,000)
    /// and 81 s (32,000) on the Lua thread.
    #[tokio::test]
    async fn html_nested_divs_fail_within_the_deadline() {
        for n in [20_000, 32_000] {
            let (err, took) = run_with_short_deadline(&format!(
                r#"local doc = html.parse(string.rep("<div>", {n}))
                   return #doc:select("section div")"#
            ))
            .await;
            assert!(took < Duration::from_millis(1300), "{n}: {took:?}");
            assert!(
                err.contains("time limit") || err.contains("more than 512 deep"),
                "{n}: {err}"
            );
        }
    }

    /// A select that can't finish in time gives up at the deadline, and its job stops at the
    /// next element, so the state's next host job runs at once.
    #[tokio::test]
    async fn html_slow_select_stops_at_the_deadline() {
        // Without scraper's cache, `nth-last-child` counts the following siblings of each of
        // 30,000 elements.
        let code = r#"
            local doc
            return {
                id = "t", name = "T", lang = "en", api = 1,
                search = function() return {} end,
                chapters = function() return {} end,
                pages = function() return {} end,
                load = function() doc = html.parse(string.rep("<p></p>", 30000)) end,
                slow = function() return #doc:select("p:nth-last-child(2)") end,
                quick = function() return html.parse("<p>x</p>"):select_one("p"):text() end,
            }"#;
        let mut host = HostConfig::default();
        host.deadline = Duration::from_secs(1);
        let libs = LibSource::Dir(PathBuf::from("/nonexistent"));
        let rt = Runtime::load(code, "t.lua", libs, host).unwrap();
        rt.call::<_, ()>("load", ()).await.unwrap();

        let start = Instant::now();
        let err = rt
            .call::<_, Value>("slow", ())
            .await
            .unwrap_err()
            .to_string();
        let took = start.elapsed();
        assert!(err.contains("html.select took too long"), "{err}");
        assert!(took < Duration::from_millis(1500), "{took:?}");

        let start = Instant::now();
        assert_eq!(rt.call::<_, String>("quick", ()).await.unwrap(), "x");
        let took = start.elapsed();
        assert!(took < Duration::from_millis(500), "{took:?}");
    }
}
