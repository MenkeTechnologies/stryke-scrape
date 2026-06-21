```
 ███████╗████████╗██████╗ ██╗   ██╗██╗  ██╗███████╗
 ██╔════╝╚══██╔══╝██╔══██╗╚██╗ ██╔╝██║ ██╔╝██╔════╝
 ███████╗   ██║   ██████╔╝ ╚████╔╝ █████╔╝ █████╗
 ╚════██║   ██║   ██╔══██╗  ╚██╔╝  ██╔═██╗ ██╔══╝
 ███████║   ██║   ██║  ██║   ██║   ██║  ██╗███████╗
 ╚══════╝   ╚═╝   ╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═╝╚══════╝
                  [ s c r a p e ]
```

[![CI](https://github.com/MenkeTechnologies/stryke-scrape/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/stryke-scrape/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![stryke](https://img.shields.io/badge/stryke-package-cyan.svg)](https://github.com/MenkeTechnologies/strykelang)

### `[WEB SCRAPING / CRAWLING CLIENT FOR STRYKE // FETCH + CRAWL + SITEMAP + CSS EXTRACT + TABLES + LINKS + STRUCTURED DATA]`

> *"From a URL to records, one pipe away."*

Web scraping / crawling client for stryke. Fetch a page, crawl a site
(robots-respecting, depth/limit/subdomain bounded), discover via sitemap,
then extract with CSS selectors, table-to-records, link harvesting, and
structured data (JSON-LD, OpenGraph, Twitter cards). Opt-in package tier.

### [`strykelang`](https://github.com/MenkeTechnologies/strykelang) &middot; [`MenkeTechnologiesMeta`](https://github.com/MenkeTechnologies/MenkeTechnologiesMeta) · [`stryke-search`](https://github.com/MenkeTechnologies/stryke-search) · [`stryke-selenium`](https://github.com/MenkeTechnologies/stryke-selenium)

---

## Table of Contents

- [\[0x00\] What it does](#0x00-what-it-does)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Quick start](#0x02-quick-start)
- [\[0x03\] Crawl options](#0x03-crawl-options)
- [\[0x04\] API reference](#0x04-api-reference)
- [\[0x05\] FFI layer](#0x05-ffi-layer)
- [\[0x06\] Politeness](#0x06-politeness)
- [\[0x07\] Tests](#0x07-tests)
- [\[0x08\] Dev workflow](#0x08-dev-workflow)
- [\[0x09\] Layout](#0x09-layout)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] What it does

The engine is the [`spider`](https://github.com/spider-rs/spider) crawler —
this package does not reimplement crawling, it vendors spider and exposes it
to stryke, then layers HTML extraction on top. Two halves:

- **Network** (`fetch`, `crawl`, `links`, `sitemap`) drive spider through a
  single embedded tokio runtime owned by the cdylib.
- **Pure** (`extract`, `extract_table`, `extract_links`, `extract_attrs`,
  `extract_text`, `extract_meta`, `extract_images`, `extract_feeds`,
  `structured`, `select`, `absolutize`) run html5ever-backed CSS selection over
  an HTML string with no network — the unit-tested core, usable on any HTML you
  already have.

```stryke
use Scrape
val $page = Scrape::fetch "https://example.com"
val $rec  = Scrape::extract $page->{html}, { title => "h1", price => ".price" }
```

## [0x01] Install

From a release (no rustc needed on the consumer machine):

```sh
s pkg install -g github.com/MenkeTechnologies/stryke-scrape
```

From a local checkout (builds the cdylib via cargo, installs into
`~/.stryke/store/scrape@<version>/`):

```sh
cd ~/projects/stryke-scrape
cargo build --release
s pkg install -g .
```

Or:

```sh
make install
```

## [0x02] Quick start

```stryke
use Scrape

# 1. Fetch one page.
val $page = Scrape::fetch "https://example.com"
p "$page->{status} — #{len $page->{html}} bytes"

# 2. Extract fields. A selector ending in ` @attr` pulls that attribute,
#    otherwise the element text. `all => 1` returns every match.
val $rec = Scrape::extract $page->{html}, {
    title => "h1",
    links => "a @href",
}, all => 1

# 3. Tables → header-keyed records.
val $rows = Scrape::extract_table $page->{html}
for val $r (@$rows) { p $r }

# 4. Structured metadata.
val $meta = Scrape::structured $page->{html}
p $meta->{opengraph}{title}

# 5. Crawl a site — robots respected, bounded, throttled.
val $res = Scrape::crawl "https://example.com",
    limit => 50, depth => 3, delay => 200, concurrency => 4
p "crawled $res->{count} pages"

# 6. Just the link graph (no bodies).
val $map = Scrape::links "https://example.com", limit => 200

# 7. Sitemap-driven crawl.
val $sm = Scrape::sitemap "https://example.com"
```

## [0x03] Crawl options

Every network fn takes the URL first, then `%opts`:

```
limit          → max pages to fetch
depth           → max link depth from the seed
subdomains      → 1 to follow subdomains
respect_robots  → robots.txt compliance (default 1)
delay           → ms between requests (politeness throttle)
concurrency     → max in-flight requests
timeout_ms      → per-request timeout
blacklist       → [ url substrings / patterns to skip ]
proxies         → [ proxy urls ]
user_agent      → override the default stryke-scrape/<ver> UA
include_html    → crawl/sitemap: 0 to omit page bodies (URL map only)
```

## [0x04] API reference

```stryke
# Network (spider engine)
Scrape::fetch    $url, %opts → { url, status, html }
Scrape::crawl    $url, %opts → { count, pages => [ { url, status, html? } ] }
Scrape::links    $url, %opts → { count, links => [ url, ... ] }
Scrape::sitemap  $url, %opts → { count, pages => [ ... ] }

# Pure extraction (no network)
Scrape::extract         $html, \%fields, %opts → \%record   # opts: all
Scrape::extract_table   $html, %opts → [ \%row, ... ]       # opts: selector
Scrape::extract_links   $html, %opts → [ { text, href }, ... ]   # opts: base
Scrape::extract_attrs   $html, $selector, $attr → [ value, ... ]
Scrape::extract_text    $html, %opts → $string              # opts: selector (default body)
Scrape::extract_meta    $html → { title, description, canonical, ... }
Scrape::extract_images  $html, %opts → [ { src, alt }, ... ]     # opts: base
Scrape::extract_feeds   $html, %opts → [ { title, href, type }, ... ]   # opts: base
Scrape::structured      $html → { jsonld => [...], opengraph => {...}, twitter => {...} }
Scrape::select          $html, $selector, %opts → [ html, ... ]   # opts: inner
Scrape::absolutize      $base, $href → $url

# URL / query-string helpers (pure)
Scrape::url_encode      $value → $encoded                    # RFC 3986 percent-encode
Scrape::url_decode      $value, %opts → $decoded             # opts: plus_as_space
Scrape::parse_query     $query → { key => value, ... }
Scrape::build_query     \%params → $query_string
Scrape::url_parse       $url → { scheme, host, port, path, query, fragment, username, password }

Scrape::version → $semver
```

Selector specs in `extract` support a trailing ` @attr`: `"a.nav @href"`
extracts the `href` attribute, a bare `"h1"` extracts text. `extract_links`
resolves relative hrefs when given a `base`.

## [0x05] FFI layer

Each `Scrape::*` wrapper builds a JSON args dict and calls a sibling
`scrape__*` symbol resolved out of `libstryke_scrape.{dylib,so}`. The cdylib
is dlopened in-process on first `use Scrape` (via stryke's
`pkg::commands::try_load_ffi_for` resolver hook). Network calls run on one
embedded multi-thread tokio runtime created lazily and reused for the life of
the process. Wire shape:

* Single record → `{"value": …}` / `{"rows": …}` / `{"links": …}`
* Crawl result → `{"count": N, "pages": [...]}`
* Errors → `{"error": "<msg>"}` — the wrapper `die`s with it

## [0x06] Politeness

`crawl`, `links`, and `sitemap` default `respect_robots` to true and send an
identifying `stryke-scrape/<version>` User-Agent. `delay` and `concurrency`
throttle a crawl. Respecting a site's terms of use, rate limits, and robots
directives is the caller's responsibility — this package provides the
mechanisms and sets the courteous defaults.

## [0x07] Tests

```sh
cargo test                                  # pure extraction engines, no network
s test t/                                    # wrapper surface + extraction
SCRAPE_LIVE_URL=https://example.com s test t/   # opt-in live crawl
```

The pure-engine unit tests (`src/lib.rs`) and the offline surface test
(`t/test_stryke_scrape_surface.stk`) run with no egress, so CI stays green
without network.

## [0x08] Dev workflow

```sh
make             # release build
make debug
make test
make install
make clean
```

## [0x09] Layout

```
stryke-scrape/
  stryke.toml                      # stryke package manifest ([ffi] table)
  Cargo.toml                       # cdylib crate manifest (wraps spider)
  Makefile
  src/lib.rs                       # cdylib — scrape__* extern "C" exports + tokio rt
  lib/
    Scrape.stk                     # `use Scrape` — thin wrapper around the FFI symbols
  t/
    test_scrape.stk                # extraction + opt-in live crawl
    test_stryke_scrape_surface.stk # wrapper completeness + pure-engine pin
  examples/
    extract.stk
    crawl.stk
  docs/
    index.html                     # docs site
    report.html
  .github/workflows/
    ci.yml
    release.yml                    # cross-compile + GH release on tag push
```

## [0xFF] License

MIT.
