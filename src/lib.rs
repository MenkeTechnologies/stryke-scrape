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

/// Collapse a selected element's visible text, skipping `script`/`style`/
/// `noscript` subtrees so embedded JS/CSS never leaks into readable output.
fn visible_text(el: &ElementRef) -> String {
    let mut out = String::new();
    for node in el.descendants() {
        if let Some(txt) = node.value().as_text() {
            let skip = node.ancestors().any(|a| {
                a.value()
                    .as_element()
                    .map(|e| matches!(e.name(), "script" | "style" | "noscript"))
                    .unwrap_or(false)
            });
            if !skip {
                out.push_str(txt);
                out.push(' ');
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Readable text of `selector` (default `body`), tags stripped and whitespace
/// collapsed. Returns a single string.
fn extract_text(html: &str, selector: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel =
        Selector::parse(selector.unwrap_or("body")).map_err(|e| anyhow!("bad selector: {e:?}"))?;
    let joined = doc
        .select(&sel)
        .map(|el| visible_text(&el))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(json!(joined
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")))
}

/// Standard `<head>` metadata: `title` plus the common SEO `<meta>`/`<link>`
/// fields (`description`, `keywords`, `robots`, `viewport`, `generator`,
/// `charset`, `canonical`). Only present fields appear in the result.
fn extract_meta(html: &str) -> Result<Value> {
    let doc = Html::parse_document(html);
    let mut o = Map::new();

    let title_sel = Selector::parse("title").unwrap();
    if let Some(el) = doc.select(&title_sel).next() {
        o.insert("title".into(), json!(clean_text(&el)));
    }

    let meta_sel = Selector::parse("meta").unwrap();
    for el in doc.select(&meta_sel) {
        if let Some(charset) = el.value().attr("charset") {
            o.insert("charset".into(), json!(charset));
        }
        if let (Some(name), Some(content)) = (el.value().attr("name"), el.value().attr("content")) {
            match name.to_ascii_lowercase().as_str() {
                "description" => o.insert("description".into(), json!(content)),
                "keywords" => o.insert("keywords".into(), json!(content)),
                "robots" => o.insert("robots".into(), json!(content)),
                "viewport" => o.insert("viewport".into(), json!(content)),
                "generator" => o.insert("generator".into(), json!(content)),
                _ => None,
            };
        }
    }

    let canon_sel = Selector::parse(r#"link[rel="canonical"]"#).unwrap();
    if let Some(href) = doc
        .select(&canon_sel)
        .next()
        .and_then(|el| el.value().attr("href"))
    {
        o.insert("canonical".into(), json!(href));
    }

    Ok(Value::Object(o))
}

/// Resolve an `href` against an optional `base`; relative URLs that cannot be
/// resolved are dropped.
fn resolve(base: &Option<Url>, href: &str) -> Option<String> {
    match base {
        Some(b) => b.join(href).ok().map(|u| u.to_string()),
        None => Some(href.to_string()),
    }
}

/// Build the optional base `Url` shared by the image/feed/link resolvers.
fn parse_base(base: Option<&str>) -> Result<Option<Url>> {
    match base {
        Some(b) => Ok(Some(
            Url::parse(b).map_err(|e| anyhow!("bad base url {b:?}: {e}"))?,
        )),
        None => Ok(None),
    }
}

/// Every `<img src>` as `{ src, alt }`. With `base`, `src` is absolutised.
fn extract_images(html: &str, base: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("img[src]").unwrap();
    let base_url = parse_base(base)?;
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        let Some(src) = el.value().attr("src") else {
            continue;
        };
        let Some(resolved) = resolve(&base_url, src) else {
            continue;
        };
        out.push(json!({ "src": resolved, "alt": el.value().attr("alt").unwrap_or("") }));
    }
    Ok(Value::Array(out))
}

/// RSS/Atom feed `<link rel="alternate">` discovery as `{ title, href, type }`.
fn extract_feeds(html: &str, base: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(r#"link[rel="alternate"]"#).unwrap();
    let base_url = parse_base(base)?;
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        let ty = el.value().attr("type").unwrap_or("");
        if !(ty.contains("rss") || ty.contains("atom") || ty.contains("xml")) {
            continue;
        }
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let Some(resolved) = resolve(&base_url, href) else {
            continue;
        };
        out.push(json!({ "title": el.value().attr("title").unwrap_or(""), "href": resolved, "type": ty }));
    }
    Ok(Value::Array(out))
}

/// Outer (default) or inner HTML of every element matching `selector`.
fn select_html(html: &str, selector: &str, inner: bool) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(selector).map_err(|e| anyhow!("bad selector {selector:?}: {e:?}"))?;
    let out: Vec<Value> = doc
        .select(&sel)
        .map(|el| if inner { el.inner_html() } else { el.html() })
        .map(Value::String)
        .collect();
    Ok(Value::Array(out))
}

/// Cleaned text content of every element matching `selector`, as an array.
/// Where `extract_text` joins all matches into one string and `select` returns
/// HTML, this gives one whitespace-collapsed text string per match — the shape
/// you want when scraping a list (`li`, `.row .title`, …).
fn select_text(html: &str, selector: &str) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse(selector).map_err(|e| anyhow!("bad selector {selector:?}: {e:?}"))?;
    let out: Vec<Value> = doc
        .select(&sel)
        .map(|el| Value::String(visible_text(&el)))
        .collect();
    Ok(Value::Array(out))
}

/// Document outline: every `h1`..`h6` as `{ level, text, id }` in document
/// order. `level` is the integer 1..6; `id` is the heading's `id` attribute or
/// null. Useful for table-of-contents building and SEO heading audits.
fn extract_headings(html: &str) -> Result<Value> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("h1,h2,h3,h4,h5,h6").unwrap();
    let mut out = Vec::new();
    for el in doc.select(&sel) {
        let name = el.value().name();
        // tag name is exactly "h" + one digit for this selector
        let level = name[1..].parse::<u8>().unwrap_or(0);
        out.push(json!({
            "level": level,
            "text": clean_text(&el),
            "id": el.value().attr("id"),
        }));
    }
    Ok(Value::Array(out))
}

/// Every `<form>` as `{ action, method, fields: [ { name, type, value } ] }`.
/// `method` is upper-cased and defaults to GET (HTML default). `fields` covers
/// `input`/`select`/`textarea`; `select` reports type `select`, `textarea`
/// reports type `textarea`, and an `input` without a `type` defaults to `text`.
/// With `base`, the form `action` is resolved to an absolute URL.
fn extract_forms(html: &str, base: Option<&str>) -> Result<Value> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form").unwrap();
    let field_sel = Selector::parse("input,select,textarea").unwrap();
    let base_url = parse_base(base)?;

    let mut forms = Vec::new();
    for form in doc.select(&form_sel) {
        let action_raw = form.value().attr("action").unwrap_or("");
        let action = if action_raw.is_empty() {
            Value::Null
        } else {
            match resolve(&base_url, action_raw) {
                Some(u) => json!(u),
                None => json!(action_raw),
            }
        };
        let method = form
            .value()
            .attr("method")
            .unwrap_or("get")
            .to_ascii_uppercase();

        let mut fields = Vec::new();
        for f in form.select(&field_sel) {
            let tag = f.value().name();
            let ty = match tag {
                "select" => "select".to_string(),
                "textarea" => "textarea".to_string(),
                _ => f.value().attr("type").unwrap_or("text").to_string(),
            };
            let value = match tag {
                "textarea" => Some(clean_text(&f)),
                _ => f.value().attr("value").map(String::from),
            };
            fields.push(json!({
                "name": f.value().attr("name"),
                "type": ty,
                "value": value,
            }));
        }
        forms.push(json!({ "action": action, "method": method, "fields": fields }));
    }
    Ok(Value::Array(forms))
}

/// schema.org microdata: every top-level `[itemscope]` element (one not nested
/// inside another `itemscope`) as `{ type, properties: { name: [values] } }`.
/// A property value is the `content`/`href`/`src`/`datetime` attribute when
/// present (in that order), else the element's text. Complements `structured`
/// (JSON-LD + OpenGraph) with the inline-attribute form.
fn extract_microdata(html: &str) -> Result<Value> {
    let doc = Html::parse_document(html);
    let scope_sel = Selector::parse("[itemscope]").unwrap();
    let prop_sel = Selector::parse("[itemprop]").unwrap();

    let mut items = Vec::new();
    for scope in doc.select(&scope_sel) {
        // top-level only: skip scopes nested in another itemscope
        let nested = scope
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|a| a.value().attr("itemscope").is_some());
        if nested {
            continue;
        }
        let itemtype = scope.value().attr("itemtype");
        let mut props: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for p in scope.select(&prop_sel) {
            let Some(name) = p.value().attr("itemprop") else {
                continue;
            };
            let val = p
                .value()
                .attr("content")
                .or_else(|| p.value().attr("href"))
                .or_else(|| p.value().attr("src"))
                .or_else(|| p.value().attr("datetime"))
                .map(String::from)
                .unwrap_or_else(|| clean_text(&p));
            props.entry(name.to_string()).or_default().push(json!(val));
        }
        let props_obj: Map<String, Value> = props.into_iter().map(|(k, v)| (k, json!(v))).collect();
        items.push(json!({ "type": itemtype, "properties": props_obj }));
    }
    Ok(Value::Array(items))
}

// ── URL / query-string helpers (pure; no network) ────────────────────────────

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode a string for a URL component: RFC 3986 unreserved
/// (`A-Za-z0-9-_.~`) pass through, everything else becomes `%XX`.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode `%XX` escapes. With `plus_as_space` (query-component decoding), `+`
/// also maps to a space. Invalid escapes are left verbatim.
fn url_decode(s: &str, plus_as_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a query string (leading `?` optional) into a `{ key: value }` map.
/// Keys/values are form-decoded (`+` → space, `%XX`). Duplicate keys: last wins.
fn parse_query(qs: &str) -> Value {
    let qs = qs.strip_prefix('?').unwrap_or(qs);
    let mut map = Map::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (url_decode(k, true), url_decode(v, true)),
            None => (url_decode(pair, true), String::new()),
        };
        map.insert(k, Value::String(v));
    }
    Value::Object(map)
}

/// Build a query string from a `{ key: value }` map (percent-encoded, `&`-joined).
fn build_query(obj: &Map<String, Value>) -> String {
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            format!("{}={}", url_encode(k), url_encode(&val))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Decompose a URL into `{ scheme, host, port, path, query, fragment, username,
/// password }` (port falls back to the scheme's default).
fn url_parse(s: &str) -> Result<Value> {
    let u = Url::parse(s).map_err(|e| anyhow!("bad url {s:?}: {e}"))?;
    Ok(json!({
        "scheme": u.scheme(),
        "host": u.host_str(),
        "port": u.port_or_known_default(),
        "path": u.path(),
        "query": u.query(),
        "fragment": u.fragment(),
        "username": (!u.username().is_empty()).then(|| u.username().to_string()),
        "password": u.password(),
    }))
}

/// True when both URLs share the same tuple origin (scheme + host + port).
/// Opaque/non-tuple origins (e.g. `data:`, `file:`) never count as same-origin,
/// matching the web's same-origin policy. The core scope check for a crawler.
fn same_origin(a: &str, b: &str) -> Result<bool> {
    let ua = Url::parse(a).map_err(|e| anyhow!("bad url {a:?}: {e}"))?;
    let ub = Url::parse(b).map_err(|e| anyhow!("bad url {b:?}: {e}"))?;
    let oa = ua.origin();
    let ob = ub.origin();
    Ok(oa.is_tuple() && ob.is_tuple() && oa == ob)
}

/// Canonicalise a URL for dedup: lowercase scheme + host (done by `url`), drop a
/// default port, remove the fragment, and sort query parameters by key (stable
/// within equal keys). Two URLs that differ only in those respects normalise to
/// the same string — what a crawler's visited-set needs.
fn normalize_url(s: &str) -> Result<String> {
    let mut u = Url::parse(s).map_err(|e| anyhow!("bad url {s:?}: {e}"))?;
    u.set_fragment(None);
    // Drop an explicit port that equals the scheme default.
    if let (Some(port), Some(def)) = (u.port(), default_port(u.scheme())) {
        if port == def {
            let _ = u.set_port(None);
        }
    }
    // Sort query params by key, preserving relative order of equal keys.
    if u.query().is_some() {
        let mut pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if pairs.is_empty() {
            u.set_query(None);
        } else {
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let mut qp = u.query_pairs_mut();
            qp.clear();
            for (k, v) in &pairs {
                qp.append_pair(k, v);
            }
            drop(qp);
        }
    }
    Ok(u.to_string())
}

/// The default port for the common URL schemes, used by `normalize_url`.
fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

/// Merge `params` into the query string of `url`. With `replace` true the new
/// params override existing keys (others kept); with `replace` false the new
/// pairs are appended (duplicate keys allowed). Returns the rebuilt URL.
fn set_query_params(url: &str, params: &Map<String, Value>, replace: bool) -> Result<String> {
    let mut u = Url::parse(url).map_err(|e| anyhow!("bad url {url:?}: {e}"))?;
    let new_str = |v: &Value| -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    };

    if replace {
        // keep existing pairs whose key is not being overridden
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !params.contains_key(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        u.set_query(None);
        let mut qp = u.query_pairs_mut();
        for (k, v) in &kept {
            qp.append_pair(k, v);
        }
        for (k, v) in params {
            qp.append_pair(k, &new_str(v));
        }
        drop(qp);
    } else {
        let mut qp = u.query_pairs_mut();
        for (k, v) in params {
            qp.append_pair(k, &new_str(v));
        }
        drop(qp);
    }
    // An empty query string ("?") is noise; strip it.
    if u.query() == Some("") {
        u.set_query(None);
    }
    Ok(u.to_string())
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

#[no_mangle]
pub extern "C" fn scrape__extract_text(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let selector = v.get("selector").and_then(|x| x.as_str());
        Ok(json!({ "text": extract_text(html, selector)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_meta(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        Ok(json!({ "meta": extract_meta(html)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_images(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let base = v.get("base").and_then(|x| x.as_str());
        Ok(json!({ "images": extract_images(html, base)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__extract_feeds(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let base = v.get("base").and_then(|x| x.as_str());
        Ok(json!({ "feeds": extract_feeds(html, base)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__absolutize(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let base = v
            .get("base")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing base"))?;
        let href = v
            .get("href")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing href"))?;
        let b = Url::parse(base).map_err(|e| anyhow!("bad base url {base:?}: {e}"))?;
        let u = b
            .join(href)
            .map_err(|e| anyhow!("cannot resolve {href:?}: {e}"))?;
        Ok(json!({ "url": u.to_string() }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__select(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let selector = v
            .get("selector")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing selector"))?;
        let inner = v.get("inner").and_then(|x| x.as_bool()).unwrap_or(false);
        Ok(json!({ "html": select_html(html, selector, inner)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__select_text(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let selector = v
            .get("selector")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing selector"))?;
        Ok(json!({ "text": select_text(html, selector)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__headings(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        Ok(json!({ "headings": extract_headings(html)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__forms(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        let base = v.get("base").and_then(|x| x.as_str());
        Ok(json!({ "forms": extract_forms(html, base)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__microdata(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let html = v
            .get("html")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing html"))?;
        Ok(json!({ "items": extract_microdata(html)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__url_encode(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let s = v
            .get("value")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing value"))?;
        Ok(json!({ "value": url_encode(s) }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__url_decode(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let s = v
            .get("value")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing value"))?;
        let plus = v
            .get("plus_as_space")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        Ok(json!({ "value": url_decode(s, plus) }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__parse_query(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let qs = v
            .get("query")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing query"))?;
        Ok(json!({ "params": parse_query(qs) }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__build_query(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let params = v
            .get("params")
            .and_then(|x| x.as_object())
            .ok_or_else(|| anyhow!("missing params object"))?;
        Ok(json!({ "value": build_query(params) }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__url_parse(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing url"))?;
        Ok(json!({ "parts": url_parse(url)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__same_origin(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let a = v
            .get("a")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing a"))?;
        let b = v
            .get("b")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing b"))?;
        Ok(json!({ "same": same_origin(a, b)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__normalize_url(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing url"))?;
        Ok(json!({ "url": normalize_url(url)? }))
    })
}

#[no_mangle]
pub extern "C" fn scrape__set_query_params(args: *const c_char) -> *const c_char {
    ffi_call(args, |v| {
        let url = v
            .get("url")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("missing url"))?;
        let params = v
            .get("params")
            .and_then(|x| x.as_object())
            .ok_or_else(|| anyhow!("missing params object"))?;
        let replace = v.get("replace").and_then(|x| x.as_bool()).unwrap_or(true);
        Ok(json!({ "url": set_query_params(url, params, replace)? }))
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

    #[test]
    fn text_strips_tags_and_skips_script() {
        let html = r#"<body><h1>Hi</h1><p>there  world</p><script>var x=1;</script></body>"#;
        let v = extract_text(html, None).unwrap();
        assert_eq!(v, json!("Hi there world"));
    }

    #[test]
    fn text_scopes_to_selector() {
        let html = r#"<body><nav>skip me</nav><main>keep this</main></body>"#;
        let v = extract_text(html, Some("main")).unwrap();
        assert_eq!(v, json!("keep this"));
    }

    #[test]
    fn meta_pulls_title_description_canonical() {
        let html = r##"<html><head>
            <title>My Page</title>
            <meta name="description" content="A page">
            <meta name="viewport" content="width=device-width">
            <link rel="canonical" href="https://ex.com/p">
        </head><body></body></html>"##;
        let v = extract_meta(html).unwrap();
        assert_eq!(v["title"], json!("My Page"));
        assert_eq!(v["description"], json!("A page"));
        assert_eq!(v["viewport"], json!("width=device-width"));
        assert_eq!(v["canonical"], json!("https://ex.com/p"));
        assert_eq!(v.get("keywords"), None);
    }

    #[test]
    fn images_resolve_against_base() {
        let html = r#"<body><img src="/a.png" alt="A"><img src="b.jpg"></body>"#;
        let v = extract_images(html, Some("https://ex.com/dir/")).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0], json!({"src": "https://ex.com/a.png", "alt": "A"}));
        assert_eq!(
            arr[1],
            json!({"src": "https://ex.com/dir/b.jpg", "alt": ""})
        );
    }

    #[test]
    fn feeds_match_rss_and_atom_only() {
        let html = r##"<head>
            <link rel="alternate" type="application/rss+xml" title="RSS" href="/feed.xml">
            <link rel="alternate" type="application/atom+xml" href="/atom">
            <link rel="alternate" type="text/html" href="/other">
        </head>"##;
        let v = extract_feeds(html, Some("https://ex.com/")).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["href"], json!("https://ex.com/feed.xml"));
        assert_eq!(arr[0]["title"], json!("RSS"));
        assert_eq!(arr[1]["href"], json!("https://ex.com/atom"));
    }

    #[test]
    fn absolutize_resolves_relative() {
        let b = Url::parse("https://site.test/dir/page").unwrap();
        assert_eq!(
            b.join("../x.html").unwrap().to_string(),
            "https://site.test/x.html"
        );
    }

    #[test]
    fn select_returns_inner_and_outer_html() {
        let html = r#"<body><span class="a">hi</span><span class="a">yo</span></body>"#;
        let outer = select_html(html, "span.a", false).unwrap();
        assert_eq!(outer[0], json!(r#"<span class="a">hi</span>"#));
        let inner = select_html(html, "span.a", true).unwrap();
        assert_eq!(inner, json!(["hi", "yo"]));
    }

    #[test]
    fn url_encode_decode_round_trips() {
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(url_encode("safe-_.~"), "safe-_.~");
        assert_eq!(url_decode("a%20b%26c", false), "a b&c");
        assert_eq!(url_decode("a+b", true), "a b");
        assert_eq!(url_decode("a+b", false), "a+b");
        // round trip for arbitrary content
        let s = "héllo world/?#&=+";
        assert_eq!(url_decode(&url_encode(s), false), s);
    }

    #[test]
    fn parse_query_decodes_pairs() {
        let v = parse_query("?a=1&b=hello+world&c=%2F&flag");
        assert_eq!(v["a"], json!("1"));
        assert_eq!(v["b"], json!("hello world"));
        assert_eq!(v["c"], json!("/"));
        assert_eq!(v["flag"], json!(""));
    }

    #[test]
    fn build_query_encodes_and_pairs() {
        // single key avoids depending on serde_json map ordering
        let mut m = Map::new();
        m.insert("q".into(), json!("a b&c"));
        assert_eq!(build_query(&m), "q=a%20b%26c");
        // round-trips through parse_query
        let mut m2 = Map::new();
        m2.insert("k".into(), json!("v/1"));
        assert_eq!(parse_query(&build_query(&m2))["k"], json!("v/1"));
    }

    #[test]
    fn url_parse_decomposes() {
        let v = url_parse("https://user:pw@ex.com:8443/p/q?x=1#frag").unwrap();
        assert_eq!(v["scheme"], json!("https"));
        assert_eq!(v["host"], json!("ex.com"));
        assert_eq!(v["port"], json!(8443));
        assert_eq!(v["path"], json!("/p/q"));
        assert_eq!(v["query"], json!("x=1"));
        assert_eq!(v["fragment"], json!("frag"));
        assert_eq!(v["username"], json!("user"));
        assert_eq!(v["password"], json!("pw"));
    }

    #[test]
    fn select_text_one_string_per_match() {
        let html = r#"<ul><li> a  b </li><li>c</li></ul>"#;
        let v = select_text(html, "li").unwrap();
        assert_eq!(v, json!(["a b", "c"]));
    }

    #[test]
    fn headings_capture_level_text_and_id() {
        let html = r#"<body><h1 id="top">Title</h1><h2>Sub  one</h2><h3>Deep</h3></body>"#;
        let v = extract_headings(html).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], json!({"level": 1, "text": "Title", "id": "top"}));
        assert_eq!(arr[1], json!({"level": 2, "text": "Sub one", "id": null}));
        assert_eq!(arr[2]["level"], json!(3));
    }

    #[test]
    fn forms_capture_action_method_and_fields() {
        let html = r##"<form action="/search" method="post">
            <input name="q" value="hi">
            <input name="page">
            <select name="sort"><option>a</option></select>
            <textarea name="body">note  text</textarea>
        </form>"##;
        let v = extract_forms(html, Some("https://ex.com/")).unwrap();
        let f = &v.as_array().unwrap()[0];
        assert_eq!(f["action"], json!("https://ex.com/search"));
        assert_eq!(f["method"], json!("POST"));
        let fields = f["fields"].as_array().unwrap();
        assert_eq!(
            fields[0],
            json!({"name": "q", "type": "text", "value": "hi"})
        );
        assert_eq!(
            fields[1],
            json!({"name": "page", "type": "text", "value": null})
        );
        assert_eq!(fields[2]["type"], json!("select"));
        assert_eq!(
            fields[3],
            json!({"name": "body", "type": "textarea", "value": "note text"})
        );
    }

    #[test]
    fn forms_default_method_get_and_empty_action() {
        let html = r#"<form><input name="x"></form>"#;
        let v = extract_forms(html, None).unwrap();
        let f = &v.as_array().unwrap()[0];
        assert_eq!(f["method"], json!("GET"));
        assert_eq!(f["action"], Value::Null);
    }

    #[test]
    fn microdata_top_level_items_only() {
        let html = r##"<div itemscope itemtype="https://schema.org/Person">
            <span itemprop="name">Ada</span>
            <a itemprop="url" href="https://ada.example/">site</a>
            <time itemprop="born" datetime="1815-12-10">long ago</time>
            <div itemscope itemtype="https://schema.org/PostalAddress">
                <span itemprop="city">London</span>
            </div>
        </div>"##;
        let v = extract_microdata(html).unwrap();
        let arr = v.as_array().unwrap();
        // only the outer Person scope is top-level
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], json!("https://schema.org/Person"));
        let props = &arr[0]["properties"];
        assert_eq!(props["name"], json!(["Ada"]));
        assert_eq!(props["url"], json!(["https://ada.example/"]));
        assert_eq!(props["born"], json!(["1815-12-10"]));
        // nested PostalAddress city is captured under the parent (descendant scan)
        assert_eq!(props["city"], json!(["London"]));
    }

    #[test]
    fn same_origin_matches_scheme_host_port() {
        assert!(same_origin("https://ex.com/a", "https://ex.com/b?x=1").unwrap());
        assert!(same_origin("https://ex.com:443/a", "https://ex.com/b").unwrap());
        assert!(!same_origin("https://ex.com/a", "http://ex.com/a").unwrap());
        assert!(!same_origin("https://ex.com/a", "https://other.com/a").unwrap());
        assert!(!same_origin("https://ex.com:8443/a", "https://ex.com/a").unwrap());
    }

    #[test]
    fn normalize_url_canonicalises() {
        assert_eq!(
            normalize_url("https://EX.com:443/p?b=2&a=1#frag").unwrap(),
            "https://ex.com/p?a=1&b=2"
        );
        assert_eq!(
            normalize_url("http://ex.com:80/").unwrap(),
            "http://ex.com/"
        );
        // non-default port preserved
        assert_eq!(
            normalize_url("https://ex.com:8443/x").unwrap(),
            "https://ex.com:8443/x"
        );
    }

    #[test]
    fn set_query_params_replace_and_append() {
        let mut p = Map::new();
        p.insert("page".into(), json!("2"));
        // replace: override existing page, keep q
        assert_eq!(
            set_query_params("https://ex.com/s?q=cats&page=1", &p, true).unwrap(),
            "https://ex.com/s?q=cats&page=2"
        );
        // append: both pages present
        assert_eq!(
            set_query_params("https://ex.com/s?page=1", &p, false).unwrap(),
            "https://ex.com/s?page=1&page=2"
        );
        // adds query to a URL that had none
        assert_eq!(
            set_query_params("https://ex.com/s", &p, true).unwrap(),
            "https://ex.com/s?page=2"
        );
    }
}
