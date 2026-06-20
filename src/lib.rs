//! stryke-scrape — web scraping / crawling cdylib loaded in-process by stryke
//! via dlopen.
//!
//! Each `#[no_mangle] extern "C" fn scrape__*` is a JSON-string-in /
//! JSON-string-out wrapper. stryke's FFI bridge resolves these symbols at first
//! `use Scrape`, registers each as a stryke-callable function, passes a
//! JSON-encoded args dict per call, and copies the returned JSON into a stryke
//! string. `stryke_free_cstring` frees that allocation.
//!
//! ## Two halves
//!
//! * **Network** (`fetch`, `crawl`, `links`, `sitemap`) wraps the [`spider`]
//!   crawler. We do not reimplement crawling — spider is the engine. The cdylib
//!   owns a single embedded multi-thread tokio runtime and drives spider's async
//!   methods with `block_on`, so the stryke side is a plain synchronous call.
//! * **Pure** (`extract`, `extract_table`, `extract_links`, `extract_attrs`,
//!   `structured`) runs html5ever-backed CSS selection over an HTML string with
//!   no network. These are unit-tested in CI offline — they are the testable
//!   core of the package.
//!
//! ## Politeness is the default, not an add-on
//!
//! `crawl`/`links`/`sitemap` default `respect_robots` to true and send an
//! identifying `stryke-scrape/<version>` User-Agent. `delay` (per-request) and
//! `concurrency` are exposed so a crawl can be throttled. Callers can disable
//! robots compliance explicitly; the default does the courteous thing.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use anyhow::{anyhow, Result};
use once_cell::sync::OnceCell;
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Map, Value};
use spider::compact_str::CompactString;
use spider::website::Website;
use tokio::runtime::Runtime;
use url::Url;

// ── FFI plumbing ────────────────────────────────────────────────────────────

fn ffi_call<F>(args: *const c_char, handler: F) -> *const c_char
where
    F: FnOnce(Value) -> Result<Value>,
{
    let input = if args.is_null() {
        Value::Null
    } else {
        let cs = unsafe { CStr::from_ptr(args) };
        serde_json::from_slice::<Value>(cs.to_bytes()).unwrap_or(Value::Null)
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| handler(input)));
    let out = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => json!({ "error": e.to_string() }),
        Err(_) => json!({ "error": "stryke-scrape handler panicked" }),
    };
    let s =
        serde_json::to_string(&out).unwrap_or_else(|_| String::from(r#"{"error":"serialize"}"#));
    match CString::new(s) {
        Ok(c) => c.into_raw() as *const c_char,
        Err(_) => std::ptr::null(),
    }
}

/// Free a C string allocated by any export from this cdylib.
///
/// # Safety
/// `p` must be a pointer previously returned by an export, or null.
#[no_mangle]
pub unsafe extern "C" fn stryke_free_cstring(p: *mut c_char) {
    if p.is_null() {
        return;
    }
    drop(CString::from_raw(p));
}

// ── shared runtime ──────────────────────────────────────────────────────────

static RT: OnceCell<Runtime> = OnceCell::new();

/// The cdylib's single embedded tokio runtime. spider spawns tokio tasks
/// internally, so its async methods must run inside one; we drive them from
/// here with `block_on`.
fn rt() -> Result<&'static Runtime> {
    RT.get_or_try_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow!("tokio runtime: {e}"))
    })
}

const DEFAULT_UA: &str = concat!("stryke-scrape/", env!("CARGO_PKG_VERSION"));

// ── spider configuration ────────────────────────────────────────────────────

/// Build a configured `Website` from a request dict. `url` is required; the
/// rest fall back to courteous defaults (robots respected, identifying UA).
fn build_website(v: &Value) -> Result<Website> {
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("missing url"))?;
    let mut w = Website::new(url);

    let ua = v
        .get("user_agent")
        .and_then(|x| x.as_str())
        .unwrap_or(DEFAULT_UA);
    w.with_user_agent(Some(ua));

    // robots compliance defaults ON.
    let respect = v
        .get("respect_robots")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    w.with_respect_robots_txt(respect);

    if let Some(limit) = v.get("limit").and_then(|x| x.as_u64()) {
        w.with_limit(limit as u32);
    }
    if let Some(depth) = v.get("depth").and_then(|x| x.as_u64()) {
        w.with_depth(depth as usize);
    }
    if let Some(sub) = v.get("subdomains").and_then(|x| x.as_bool()) {
        w.with_subdomains(sub);
    }
    if let Some(delay) = v.get("delay").and_then(|x| x.as_u64()) {
        w.with_delay(delay);
    }
    if let Some(c) = v.get("concurrency").and_then(|x| x.as_u64()) {
        w.with_concurrency_limit(Some(c as usize));
    }
    if let Some(t) = v.get("timeout_ms").and_then(|x| x.as_u64()) {
        w.with_request_timeout(Some(Duration::from_millis(t)));
    }
    if let Some(bl) = v.get("blacklist").and_then(|x| x.as_array()) {
        let list: Vec<CompactString> = bl
            .iter()
            .filter_map(|x| x.as_str().map(CompactString::from))
            .collect();
        if !list.is_empty() {
            w.with_blacklist_url(Some(list));
        }
    }
    if let Some(px) = v.get("proxies").and_then(|x| x.as_array()) {
        let list: Vec<String> = px
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        if !list.is_empty() {
            w.with_proxies(Some(list));
        }
    }
    Ok(w)
}

/// One crawled page rendered to JSON. `html` is omitted when `include_html` is
/// false — useful for link-mapping a large site without hauling every body.
fn page_json(p: &spider::page::Page, include_html: bool) -> Value {
    let mut o = Map::new();
    o.insert("url".into(), json!(p.get_url()));
    o.insert("status".into(), json!(p.status_code.as_u16()));
    if include_html {
        o.insert("html".into(), json!(p.get_html()));
    }
    Value::Object(o)
}

// ── pure extraction engines (no network; unit-tested, run in CI) ─────────────

/// Split a selector spec into `(css, Some(attr))` when it ends in ` @attr`,
/// else `(css, None)`. `"a.link @href"` extracts the href attribute; a bare
/// `"h1"` extracts text content.
fn split_attr(spec: &str) -> (&str, Option<&str>) {
    if let Some(idx) = spec.rfind(" @") {
        let css = spec[..idx].trim_end();
        let attr = spec[idx + 2..].trim();
        if !attr.is_empty() {
            return (css, Some(attr));
        }
    }
    (spec.trim(), None)
}

/// Collapse internal runs of whitespace and trim — element `.text()` often
/// arrives with layout whitespace baked in.
fn clean_text(el: &ElementRef) -> String {
    let raw: String = el.text().collect();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Pull a single value (text or attribute) from an element per the spec.
fn value_of(el: &ElementRef, attr: Option<&str>) -> Option<String> {
    match attr {
        Some(a) => el.value().attr(a).map(String::from),
        None => Some(clean_text(el)),
    }
}

/// Extract a record from `html` per a `fields` map of `name -> selector-spec`.
/// With `all` true each field is an array of every match; otherwise the first
/// match (or null). Selector specs support a trailing ` @attr`.
fn extract_fields(html: &str, fields: &Map<String, Value>, all: bool) -> Result<Value> {
    let doc = Html::parse_document(html);
    let mut out = Map::new();
    for (name, spec_v) in fields {
        let spec = spec_v
            .as_str()
            .ok_or_else(|| anyhow!("field {name:?} selector must be a string"))?;
        let (css, attr) = split_attr(spec);
        let sel = Selector::parse(css).map_err(|e| anyhow!("bad selector {css:?}: {e:?}"))?;
        if all {
            let vals: Vec<Value> = doc
                .select(&sel)
                .filter_map(|el| value_of(&el, attr))
                .map(Value::String)
                .collect();
            out.insert(name.clone(), Value::Array(vals));
        } else {
            let first = doc
                .select(&sel)
                .next()
                .and_then(|el| value_of(&el, attr))
                .map(Value::String)
                .unwrap_or(Value::Null);
            out.insert(name.clone(), first);
        }
    }
    Ok(Value::Object(out))
}

/// Extract the first matching `<table>` (or the table named by `selector`) into
/// an array of header-keyed row dicts. Header is the first row's `th`/`td`;
/// remaining rows become records.
fn extract_table(html: &str, selector: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse(selector.unwrap_or("table"))
        .map_err(|e| anyhow!("bad table selector: {e:?}"))?;
    let tr_sel = Selector::parse("tr").unwrap();
    let cell_sel = Selector::parse("th,td").unwrap();

    let table = doc
        .select(&table_sel)
        .next()
        .ok_or_else(|| anyhow!("no table matched"))?;

    let mut rows = table.select(&tr_sel);
    let header: Vec<String> = match rows.next() {
        Some(hr) => hr.select(&cell_sel).map(|c| clean_text(&c)).collect(),
        None => return Ok(json!([])),
    };

    let mut records = Vec::new();
    for row in rows {
        let cells: Vec<String> = row.select(&cell_sel).map(|c| clean_text(&c)).collect();
        if cells.is_empty() {
            continue;
        }
        let mut rec = Map::new();
        for (i, cell) in cells.into_iter().enumerate() {
            let key = header.get(i).cloned().unwrap_or_else(|| format!("col{i}"));
            rec.insert(key, Value::String(cell));
        }
        records.push(Value::Object(rec));
    }
    Ok(Value::Array(records))
}

/// Extract every `<a href>` as `{ text, href }`. When `base` is given, hrefs are
/// resolved to absolute URLs (relative links that can't resolve are dropped).
fn extract_links(html: &str, base: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("a[href]").unwrap();
    let base_url = match base {
        Some(b) => Some(Url::parse(b).map_err(|e| anyhow!("bad base url {b:?}: {e}"))?),
        None => None,
    };
    let mut links = Vec::new();
    for el in doc.select(&sel) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let resolved = match &base_url {
            Some(b) => match b.join(href) {
                Ok(u) => u.to_string(),
                Err(_) => continue,
            },
            None => href.to_string(),
        };
        links.push(json!({ "text": clean_text(&el), "href": resolved }));
    }
    Ok(Value::Array(links))
}

/// Collect the `attr` value of every element matching `selector`.
fn extract_attrs(html: &str, selector: &str, attr: &str) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(selector).map_err(|e| anyhow!("bad selector {selector:?}: {e:?}"))?;
    let vals: Vec<Value> = doc
        .select(&sel)
        .filter_map(|el| el.value().attr(attr).map(|v| Value::String(v.to_string())))
        .collect();
    Ok(Value::Array(vals))
}

/// Extract structured metadata: JSON-LD blocks, OpenGraph (`og:*`) and Twitter
/// card (`twitter:*`) meta tags. Returns `{ jsonld: [...], opengraph: {...},
/// twitter: {...} }`.
fn extract_structured(html: &str) -> Result<Value> {
    let doc = Html::parse_document(html);

    let ld_sel = Selector::parse(r#"script[type="application/ld+json"]"#).unwrap();
    let mut jsonld = Vec::new();
    for el in doc.select(&ld_sel) {
        let raw: String = el.text().collect();
        if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
            jsonld.push(v);
        }
    }

    let meta_sel = Selector::parse("meta").unwrap();
    // BTreeMap keeps the keys ordered for stable output.
    let mut og: BTreeMap<String, Value> = BTreeMap::new();
    let mut tw: BTreeMap<String, Value> = BTreeMap::new();
    for el in doc.select(&meta_sel) {
        let content = el.value().attr("content").unwrap_or("");
        if let Some(prop) = el.value().attr("property") {
            if let Some(key) = prop.strip_prefix("og:") {
                og.insert(key.to_string(), json!(content));
            }
        }
        if let Some(name) = el.value().attr("name") {
            if let Some(key) = name.strip_prefix("twitter:") {
                tw.insert(key.to_string(), json!(content));
            }
        }
    }

    Ok(json!({
        "jsonld": jsonld,
        "opengraph": Value::Object(og.into_iter().collect()),
        "twitter": Value::Object(tw.into_iter().collect()),
    }))
}

// ── exports: version ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn scrape__version(args: *const c_char) -> *const c_char {
    ffi_call(args, |_| Ok(json!({"version": env!("CARGO_PKG_VERSION")})))
}

// ── exports: network (spider) ────────────────────────────────────────────────

/// Fetch a single URL and return `{ url, status, html }`. Crawling is bounded to
/// the seed page (`limit` 1); links are not followed.
#[no_mangle]
pub extern "C" fn scrape__fetch(args: *const c_char) -> *const c_char {
    ffi_call(args, |mut v| {
        // force single-page semantics regardless of caller-supplied limit
        if let Some(obj) = v.as_object_mut() {
            obj.insert("limit".into(), json!(1));
        }
        let mut w = build_website(&v)?;
        rt()?.block_on(async { w.scrape().await });
        let pages = w.get_pages().ok_or_else(|| anyhow!("no pages fetched"))?;
        let page = pages
            .iter()
            .next()
            .ok_or_else(|| anyhow!("no page fetched"))?;
        Ok(page_json(page, true))
    })
}

/// Crawl a site and return `{ count, pages: [ { url, status, html? } ] }`.
/// Honours `limit`, `depth`, `subdomains`, `respect_robots`, `delay`,
/// `concurrency`, `blacklist`. Set `include_html` false to map URLs only.
#[no_mangle]
pub extern "C" fn scrape__crawl(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let include_html = v
            .get("include_html")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let mut w = build_website(&v)?;
        rt()?.block_on(async { w.scrape().await });
        let pages = w
            .get_pages()
            .ok_or_else(|| anyhow!("crawl produced no pages"))?;
        let out: Vec<Value> = pages.iter().map(|p| page_json(p, include_html)).collect();
        Ok(json!({ "count": out.len(), "pages": out }))
    })
}

/// Crawl link structure only (no page bodies stored) and return the discovered
/// URLs sorted: `{ count, links: [...] }`.
#[no_mangle]
pub extern "C" fn scrape__links(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let mut w = build_website(&v)?;
        rt()?.block_on(async { w.crawl().await });
        let mut links: Vec<String> = w.get_links().iter().map(|l| l.to_string()).collect();
        links.sort();
        Ok(json!({ "count": links.len(), "links": links }))
    })
}

/// Discover and crawl pages via the site's sitemap.xml, returning the same shape
/// as `crawl`.
#[no_mangle]
pub extern "C" fn scrape__sitemap(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let include_html = v
            .get("include_html")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let mut w = build_website(&v)?;
        rt()?.block_on(async { w.crawl_sitemap().await });
        let pages = w
            .get_pages()
            .ok_or_else(|| anyhow!("sitemap produced no pages"))?;
        let out: Vec<Value> = pages.iter().map(|p| page_json(p, include_html)).collect();
        Ok(json!({ "count": out.len(), "pages": out }))
    })
}

// ── exports: pure extraction ─────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn scrape__extract(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let fields = v
            .get("fields")
            .and_then(|x| x.as_object())
            .ok_or_else(|| anyhow!("missing fields object"))?;
        let all = v.get("all").and_then(|x| x.as_bool()).unwrap_or(false);
        Ok(json!({ "value": extract_fields(html, fields, all)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_table(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let selector = v.get("selector").and_then(|x| x.as_str());
        Ok(json!({ "rows": extract_table(html, selector)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_links(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let base = v.get("base").and_then(|x| x.as_str());
        Ok(json!({ "links": extract_links(html, base)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_attrs(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let selector = v
            .get("selector")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing selector"))?;
        let attr = v
            .get("attr")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing attr"))?;
        Ok(json!({ "value": extract_attrs(html, selector, attr)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__structured(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        extract_structured(html)
    })
}

// ── unit tests (pure engines; no network — run in CI) ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"
    <html><head>
      <meta property="og:title" content="Widget">
      <meta property="og:type" content="product">
      <meta name="twitter:card" content="summary">
      <script type="application/ld+json">{"@type":"Product","name":"Widget"}</script>
    </head><body>
      <h1>Hello World</h1>
      <a class="nav" href="/about">About</a>
      <a class="nav" href="https://ex.com/x">X</a>
      <table id="t">
        <tr><th>Name</th><th>Qty</th></tr>
        <tr><td>apple</td><td>3</td></tr>
        <tr><td>pear</td><td>5</td></tr>
      </table>
    </body></html>"##;

    fn fields(pairs: &[(&str, &str)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }

    #[test]
    fn split_attr_parses_trailing_attr() {
        assert_eq!(split_attr("a.link @href"), ("a.link", Some("href")));
        assert_eq!(split_attr("h1"), ("h1", None));
        assert_eq!(split_attr("a @"), ("a @", None)); // empty attr → treat as text
    }

    #[test]
    fn extract_text_and_attr_first() {
        let f = fields(&[("title", "h1"), ("first_link", "a.nav @href")]);
        let v = extract_fields(PAGE, &f, false).unwrap();
        assert_eq!(v["title"], json!("Hello World"));
        assert_eq!(v["first_link"], json!("/about"));
    }

    #[test]
    fn extract_all_collects_every_match() {
        let f = fields(&[("links", "a.nav @href")]);
        let v = extract_fields(PAGE, &f, true).unwrap();
        assert_eq!(v["links"], json!(["/about", "https://ex.com/x"]));
    }

    #[test]
    fn extract_missing_field_is_null() {
        let f = fields(&[("nope", ".does-not-exist")]);
        let v = extract_fields(PAGE, &f, false).unwrap();
        assert_eq!(v["nope"], Value::Null);
    }

    #[test]
    fn table_to_header_keyed_records() {
        let v = extract_table(PAGE, Some("#t")).unwrap();
        assert_eq!(
            v,
            json!([
                {"Name": "apple", "Qty": "3"},
                {"Name": "pear",  "Qty": "5"},
            ])
        );
    }

    #[test]
    fn links_resolve_against_base() {
        let v = extract_links(PAGE, Some("https://site.test/dir/")).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["href"], json!("https://site.test/about"));
        assert_eq!(arr[0]["text"], json!("About"));
        assert_eq!(arr[1]["href"], json!("https://ex.com/x"));
    }

    #[test]
    fn links_without_base_stay_raw() {
        let v = extract_links(PAGE, None).unwrap();
        assert_eq!(v.as_array().unwrap()[0]["href"], json!("/about"));
    }

    #[test]
    fn attrs_collects_selected_attribute() {
        let v = extract_attrs(PAGE, "a.nav", "href").unwrap();
        assert_eq!(v, json!(["/about", "https://ex.com/x"]));
    }

    #[test]
    fn structured_pulls_jsonld_and_opengraph() {
        let v = extract_structured(PAGE).unwrap();
        assert_eq!(v["jsonld"][0]["name"], json!("Widget"));
        assert_eq!(v["opengraph"]["title"], json!("Widget"));
        assert_eq!(v["opengraph"]["type"], json!("product"));
        assert_eq!(v["twitter"]["card"], json!("summary"));
    }
}
