//! `html`: parse a page and query it with CSS selectors, backed by `scraper`.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ego_tree::NodeId;
use mlua::{Lua, LuaString, UserData, UserDataMethods};
use scraper::{ElementRef, Html, Selector};

/// A parsed page. `Html` is `Send` but not `Sync`, and mlua userdata must be both, hence the
/// mutex; one plugin state never contends for it.
type Doc = Arc<Mutex<Html>>;

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
    doc.lock().unwrap_or_else(PoisonError::into_inner)
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
    let module = lua.create_table()?;
    let parse = lua.create_function(|_, text: LuaString| {
        let html = Html::parse_document(&text.to_string_lossy());
        Ok(Document(Arc::new(Mutex::new(html))))
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
    async fn html_invalid_selector_errors() {
        let err = run_err(r#"return html.parse("<p></p>"):select("a[")"#).await;
        assert!(err.contains("invalid selector a[: "), "{err}");
        let err = run_err(r#"return html.parse("<p>x</p>"):select_one("p"):select_one("!")"#).await;
        assert!(err.contains("invalid selector !: "), "{err}");
    }
}
