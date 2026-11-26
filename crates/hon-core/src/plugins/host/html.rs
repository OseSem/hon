//! `html`: parse a page and query it with CSS selectors, backed by `scraper`.
//!
//! Parsed trees live outside the Lua memory limit and the GC sees only small userdata, so each
//! plugin state gets a budget of live parsed nodes and input bytes, charged at parse and
//! returned when the last userdata of a document is collected.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ego_tree::NodeId;
use mlua::{Lua, LuaString, UserData, UserDataMethods};
use scraper::{ElementRef, Html, Selector};

use crate::plugins::host::text::over_cap;

const PARSE_CAP: usize = 16 << 20;
const NODE_BUDGET: usize = 2_000_000;
/// Input bytes of live documents: plain text parses to a few nodes but keeps every byte.
const BYTE_BUDGET: usize = 64 << 20;

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
/// mutex; one plugin state never contends for it.
struct Parsed {
    html: Mutex<Html>,
    charged: Held,
    budget: Arc<Budget>,
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
        methods.add_method("select", |_, this, css: String| {
            select(&this.0, None, &css, usize::MAX)
        });
        methods.add_method("select_one", |_, this, css: String| {
            Ok(select(&this.0, None, &css, 1)?.pop())
        });
    }
}

impl UserData for Element {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("select", |_, this, css: String| {
            select(&this.doc, Some(this.node), &css, usize::MAX)
        });
        methods.add_method("select_one", |_, this, css: String| {
            Ok(select(&this.doc, Some(this.node), &css, 1)?.pop())
        });
        methods.add_method("text", |_, this, ()| {
            let html = lock(&this.doc);
            let text: String = element(&html, this.node).text().collect();
            Ok(text.trim().to_string())
        });
        methods.add_method("attr", |_, this, name: String| {
            let html = lock(&this.doc);
            Ok(element(&html, this.node)
                .value()
                .attr(&name)
                .map(str::to_string))
        });
        methods.add_method("html", |_, this, ()| {
            Ok(element(&lock(&this.doc), this.node).inner_html())
        });
    }
}

fn lock(doc: &Doc) -> MutexGuard<'_, Html> {
    doc.html.lock().unwrap_or_else(PoisonError::into_inner)
}

fn element(html: &Html, node: NodeId) -> ElementRef<'_> {
    html.tree
        .get(node)
        .and_then(ElementRef::wrap)
        .expect("an Element's node is an element of its own document")
}

/// Up to `limit` elements under `scope` (the whole document when `None`) matching `css`, in
/// document order.
fn select(doc: &Doc, scope: Option<NodeId>, css: &str, limit: usize) -> mlua::Result<Vec<Element>> {
    let selector = Selector::parse(css)
        .map_err(|e| mlua::Error::runtime(format!("invalid selector {css}: {e}")))?;
    let html = lock(doc);
    let nodes: Vec<NodeId> = match scope {
        None => html.select(&selector).take(limit).map(|e| e.id()).collect(),
        Some(node) => element(&html, node)
            .select(&selector)
            .take(limit)
            .map(|e| e.id())
            .collect(),
    };
    Ok(nodes
        .into_iter()
        .map(|node| Element {
            doc: doc.clone(),
            node,
        })
        .collect())
}

/// Registers the `html` global: `html.parse(text)`.
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let budget = Arc::new(Budget::default());
    let module = lua.create_table()?;
    let parse = lua.create_function(move |lua, text: LuaString| {
        if text.as_bytes().len() > PARSE_CAP {
            return Err(over_cap("html.parse: the input", PARSE_CAP));
        }
        // Charged as decoded: each invalid byte becomes a 3-byte U+FFFD the tree keeps.
        let decoded = text.to_string_lossy();
        drop(text);
        let bytes = decoded.len();
        let html = Html::parse_document(&decoded);
        drop(decoded);
        let charged = Held {
            nodes: html.tree.values().count(),
            bytes,
        };
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
        })))
    })?;
    module.raw_set("parse", parse)?;
    lua.globals().set("html", module)
}

#[cfg(test)]
mod tests {
    use crate::plugins::host::testing::{run, run_err};

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
}
