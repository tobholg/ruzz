# ruzz ⚡

**Fuzzy search that doesn't make you wait.**

Drop in your CSVs. Get sub-millisecond fuzzy search over millions of records. No Elasticsearch cluster. No 500MB of node_modules. Just one binary and a config file.

```
$ ruzz run
✓ 1,155,509 rows indexed in 2.9s

⚡ ruzz server listening on http://localhost:8888
```

---

## What is this?

ruzz is a fast, embeddable fuzzy search engine built in Rust. It eats CSV files for breakfast and serves typo-tolerant search results before you finish typing.

**The pitch:** You have millions of records in CSV files. You want to search them with typo tolerance. Postgres `pg_trgm` chokes on short queries. Elasticsearch needs a cluster and a weekend. ruzz does it in under a millisecond and you set it up in 2 minutes.

## Features

- **🔍 Fuzzy search** — typos, partial matches, diacritic folding. "amzon" finds "Amazon", "sorlandet" finds "Sørlandet", "cafe" finds "Café" — composed or decomposed Unicode, either way. Your users can't spell, and that's okay.
- **⌨️ Typeahead from the first keystroke** — one- and two-character queries match word prefixes instead of returning nothing, so a search box works from the moment someone starts typing.
- **⚡ Fast** — sub-millisecond to low-millisecond on millions of documents. No pathological cases. Every query is fast, not just the easy ones.
- **📁 CSV & JSONL import** — point at your files (gzipped is fine), define a mapping, done. Multiple files with different schemas? Different column names? Nested JSON addressed by dotted paths? Handled. `ruzz import --check` dry-runs the lot first.
- **📦 Document store** *(optional)* — keep the full record behind each compact search row in a zstd-compressed on-disk store. Search stays lean and fast; `GET /doc/{ref}` returns everything, including nested JSON your CSV can't hold.
- **🔁 Incremental updates** — name a `primary_key` and ship deltas: `ruzz update changed.csv` (or `curl … | ruzz update -`) upserts by key, `ruzz delete <key>` removes, and a running server picks both up in moments without a restart or full rebuild.
- **📈 Activity monitoring** — every import, update and delete is logged; the dashboard's Activity tab shows a year heatmap of records touched, recent operations with failures front and center, and how much superseded store weight a full import would reclaim (also as `GET /activity`).
- **🎛 Memory budget** — tell ruzz how much RAM it can use. `50MB`, `2GB`, `50%`, `unlimited`. Run on a $5 VPS or a beefy server, same binary.
- **🔎 Filters** — exact match on keywords, enums, booleans, numeric range filtering, sort by any keyword/enum/boolean/number field. Fuzzy search + filter by country + sort by revenue desc? One query.
- **🧮 Multi-value fields & OR** — one row can hold `"LEDE,DAGL"` and match either. Pass `role=LEDE,DAGL` to OR across values on any filter. Case-insensitive by default; substring search where you want it.
- **🔢 Real counts** — `count` is the exact number of matches for the current search state, not a capped estimate. For fuzzy queries it counts plausible matches (documents sharing the query's informative trigrams), not every document that grazes a common trigram. That's the number your UI wants to print.
- **📖 Self-documenting API** — `/fields`, `/docs`, `/openapi.json` are generated from your schema, so the docs can't drift. Unknown parameters can be rejected with a spelling suggestion instead of silently ignored.
- **🏅 Ranking that reads right** — results are reranked by true string similarity against the stored value, so the exact name beats a lucky partial match and `_score` is a meaningful 0–1, not an opaque BM25 number. Two-typo queries get an automatic wider second pass.
- **🖥 Web dashboard** — a data-table UI with sortable columns, a detail pane with the full stored document, and configurable default columns/filters (`[dashboard]` in `ruzz.toml`). Light and dark, desktop to phone.
- **📊 Stats & health endpoints** — memory usage, index size, document count, uptime. Ready for monitoring and load balancers.

## Quickstart

```bash
# Install Rust if you haven't
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone & build
git clone https://github.com/tobholg/ruzz && cd ruzz
cargo build --release

# Create your config (see below) and drop CSVs in data/

# Import + serve in one shot
./target/release/ruzz run
```

Open `http://localhost:8888` and start searching.

## Configuration

Create a `ruzz.toml`:

```toml
[server]
port = 8888
index_path = "./data/index"
memory_budget = "2GB"  # or "50%", "100%", "unlimited"
# doc_cache = "128MB"  # decompressed stored-document blocks kept in-process (default 128MB)
# bind = "127.0.0.1"   # default "0.0.0.0"; loopback when a proxy fronts it

# Optional dashboard presentation — cosmetic only, the API is unaffected
# [dashboard]
# name = "US companies"                       # label in the dashboard header
# columns = ["name", "country", "city"]       # default table columns, in order
# filters = ["country", "city", "revenue"]    # filters offered up-front, in order

[schema]
fields = [
    { name = "name", type = "text", search = "fuzzy" },
    { name = "country", type = "enum", values = ["US", "DE", "UK"] },
    { name = "id", type = "keyword" },
    { name = "category", type = "enum", values = "auto" },
    { name = "employees", type = "keyword" },
    { name = "city", type = "keyword" },
    { name = "has_units", type = "boolean" },
    { name = "address", type = "text" },
]

# Each source maps CSV columns to schema fields
[[sources]]
path = "data/companies_us.csv"
defaults = { country = "US" }
mapping = { name = "company_name", id = "ein", category = "naics_code" }

[[sources]]
path = "data/companies_de.csv"
defaults = { country = "DE" }
mapping = { name = "firmenname", id = "handelsregisternummer", category = "wz_code" }

# Reuse mappings for sources with the same CSV structure
[[sources]]
path = "data/companies_uk.csv"
defaults = { country = "UK" }
use_mapping = "anglophone"

[mappings.anglophone]
name = "company_name"
id = "registration_number"
category = "sic_code"
```

A field can hold a list. With `multi = true` the source value is split (on commas by default) and each element is indexed as its own value, so one document can be found by any of them:

```toml
{ name = "roles", type = "keyword", multi = true }
{ name = "nace_codes", type = "keyword", multi = true, separator = ";" }
```

```bash
curl 'localhost:8888/search?roles=chair'          # matches "chair,ceo"
curl 'localhost:8888/search?roles=chair,auditor'  # OR across the list
```

Multi fields come back as JSON arrays in results.

Keyword fields match case-insensitively — `city=Oslo` and `city=OSLO` find the same rows, while the stored value keeps its original casing. Add `case_sensitive = true` to a field when case carries meaning (identifiers, codes).

Any field can carry a `description`. ruzz cannot infer that a number arrived in thousands, or which currency it is in, and a caller has no way to guess — so say it once in the schema and it appears everywhere the field is documented, including on its `_min` / `_max` bounds where the threshold actually gets typed:

```toml
{ name = "revenue", type = "number", description = "Thousands of the reporting currency (usually NOK)." }
```

Text fields can be searched two ways:

```toml
{ name = "name", type = "text", search = "fuzzy" }              # typo-tolerant, searched by q
{ name = "street", type = "text", search = "substring" }        # matches anywhere, own parameter
```

`fuzzy` fields are what `q` searches. A `substring` field is queried through its own parameter and stays out of `q`, so free-text address matching doesn't dilute name relevance:

```bash
curl 'localhost:8888/search?street=forusbeen'      # matches "Forusbeen 50", any casing
curl 'localhost:8888/search?q=equinor&street=forusbeen'
```

Substring matching needs at least 3 characters (it works on trigrams). A query that can't be satisfied returns no rows rather than everything.

Enum and boolean fields are indexed as exact-match filters. Enums keep the casing the source gave them and match case-insensitively; booleans are canonicalized to `TRUE`/`FALSE`:

```toml
{ name = "currency", type = "enum", values = ["NOK", "SEK", "USD"] }
{ name = "company_status", type = "enum", values = "auto", max_values = 128 }
{ name = "is_active", type = "boolean" }
```

`values = "auto"` discovers low-cardinality enum values during import. The initial default cap is `128` distinct values per auto-enum field.

## JSONL sources

Sources can be JSONL/NDJSON — one JSON document per line — instead of CSV. The format follows the file extension (`.jsonl`, `.ndjson`, optionally behind `.gz`), or set `format = "jsonl"` explicitly. The same `mapping` table applies, but values are dotted paths into the document, and a schema field with no mapping entry defaults to its own name:

```toml
[[sources]]
path = "data/enheter.jsonl.gz"
defaults = { country_code = "NO" }
mapping = { name = "navn", org_number = "organisasjonsnummer", city = "forretningsadresse.poststed" }
```

JSON types map with less guessing than CSV strings: `null` is a missing value (not `""` or `0`), numbers and booleans canonicalize directly, and an array feeds a `multi` field its elements — no separator splitting needed. An array in a field that is *not* `multi` contributes only its first element (a scalar field shows one value, and a document never matches on a value it doesn't show); the import notes it and `--check` reports it, so declare the field `multi = true` if you meant all of them. With the document store in row mode, the record *itself* is the stored full document (source `defaults` merged at the top level), so nested structures survive without a sidecar file — for nested documents, a JSONL source is the recommended replacement for the CSV + sidecar combination.

Gzipped input works for CSV too: any source or delta ending `.gz` streams through a decoder during import.

## Checking sources before importing

`ruzz import --check` parses every configured source and reports without writing anything: row counts, rows the import would reject, mapping entries that name no column, sidecar alignment, JSON arrays landing in fields not declared `multi`, and — when `primary_key` is set — empty and duplicate keys. A full import deliberately doesn't pay for per-row dedup at scale, so the check is where duplicate keys get caught; it exits non-zero on any finding, duplicates included, because a full import keeps duplicate rows while an update by key collapses them and the two paths would disagree. A duplicate rate above a few percent almost always means the key doesn't identify a row in that source.

## Segments

A full import ends by merging the index into a single segment — every query pays a little per segment, so one is fastest. Incremental updates each commit a segment of their own, folded together over time by tantivy's merge policy; if an index has grown to many (check `segments` in `/stats`), or an import reported that its final merge failed, `ruzz merge` merges everything into one segment in place. It rewrites the index once, so it needs roughly one index of free disk; a running server keeps serving throughout and picks the merged segment up on its own.

## Incremental updates

A full import rebuilds everything. When only some rows changed, name a primary key and ship a delta instead:

```toml
[schema]
primary_key = "id"    # a keyword field that uniquely identifies a document
fields = [ ... ]
```

```bash
# Upsert: each row replaces any document with the same key, or adds a new one
ruzz update data/changed_rows.csv

# Delete by key
ruzz delete 936512054 987654321
ruzz delete --file data/removed_ids.txt
```

Delta files are CSV or JSONL (detected per file, gzipped fine) in the same shape as a configured source. With one `[[sources]]` entry its mapping and defaults apply automatically; with several, an unambiguous delta is matched to its source by column set — and when that's ambiguous, say which one it follows: `ruzz update delta.csv --like data/companies_us.csv`. When the store runs in sidecar mode, pass the delta's aligned JSONL with `--sidecar`.

Deltas can also arrive on stdin, which makes a cron pipeline a one-liner:

```bash
curl -s https://example.com/todays-changes.csv | ruzz update -
```

Stdin has no extension to sniff, so it follows the matched source's format; `--format csv|jsonl` overrides.

Updates commit into the live index in place — no staging rebuild, no swap. A running server picks the change up by itself within a moment: search results, counts and full-document hydration all reflect the delta with no restart. Keys match the way the field filters (case-insensitively unless the field is `case_sensitive`), and a delta row with an empty key is skipped, not indexed.

Two things to know:

- **The store only grows between full imports.** An upsert appends the new version of the document and abandons the old one's bytes, so a deployment that updates the same rows many times over accumulates dead store space. The next full import rewrites everything compactly — the dashboard's Activity tab (and `GET /activity`) shows the superseded share, so you'll know when it's worth it. Dead index-side segments are merged and collected automatically as updates commit.
- **A changed schema needs a full import.** `ruzz update` refuses to run when the config's schema no longer matches the one the index was built with, rather than writing rows the rest of the index disagrees with.

## Document store (optional)

Your schema keeps the search index lean — but sometimes you want the *whole* record back, not just the indexed fields. Enable the document store and every imported row also lands in a compressed on-disk store, referenced from search results by `_ref`:

```toml
[store]
enabled = true
source = "row"            # "row" or "sidecar"
compression_level = 1     # zstd level (1 = fastest)
block_size = "256KB"      # raw bytes per compressed block
cache = "64MB"            # LRU cache of hot decompressed blocks
# path = "./data/store"   # default: <index_path>-store
```

The store lives next to the index and is named after it — `data/output/index` gets `data/output/index-store` — so you can build `index_v2` alongside a live `index` for a zero-downtime swap without the new import touching the store the live one is serving from. Set `path` explicitly to put it elsewhere (a bigger disk, say). A store left at the pre-0.2 location (a fixed `store` sibling) is still read when it is the only one present, so upgrading the binary under an existing deployment keeps working; the server says so on startup.

Two ways to say what "full" means:

- **`source = "row"`** — the full document is every CSV column (original names, including columns you never mapped into the schema) plus the source's `defaults`, stored as JSON. Zero extra files.
- **`source = "sidecar"`** — each source brings an aligned JSONL file: line *i* is the full document for CSV row *i*, stored byte-for-byte. This is the legacy way to store *nested* documents that a CSV can't represent — for new setups, prefer a [JSONL source](#jsonl-sources), where the record is its own full document and there's no alignment to maintain:

```toml
[[sources]]
path = "data/companies_no.csv"
sidecar = "data/companies_no_full.jsonl"
defaults = { country_code = "NO" }
mapping = { name = "organisasjonsnavn", org_number = "organisasjonsnummer" }
```

The store lives next to the index (`docs.dat` + a tiny block table), is written in one sequential pass during import, and costs nothing on the search hot path. Lookups decompress one block (~100–200μs cold, microseconds cached). Typical compression on record data: 5–10x.

**Ref semantics:** `_ref` is the row's import ordinal. A re-import reshuffles refs, and an incremental update gives the new version of a document a fresh ref — persist your own keys (like `org_number`) externally and resolve them via `/doc?field=value`, not refs.

## API

### `GET /search`

Fuzzy search with optional filters and sorting.

```bash
# Basic fuzzy search
curl 'localhost:8888/search?q=amazn&limit=10'

# With filters
curl 'localhost:8888/search?q=stripe&country=US&city=SAN+FRANCISCO'

# With numeric range
curl 'localhost:8888/search?q=tech&employees_min=100&employees_max=5000'

# With sorting (override relevance ranking)
curl 'localhost:8888/search?q=energy&sort_by=employees&sort_order=desc'

# Page through a numeric/range query
curl 'localhost:8888/search?city=OSLO&revenue_min=100000000&sort_by=revenue&sort_order=desc&limit=100&offset=200'

# Add pagination metadata and capped total counts
curl 'localhost:8888/search?city=OSLO&revenue_min=100000000&sort_by=revenue&sort_order=desc&limit=100&offset=200&include_pagination=true'
```

Every response reports how many documents match — exactly, with no cap:

```json
{
  "took_ms": 4.1,
  "count": 44678,
  "returned": 20,
  "offset": 0,
  "limit": 20,
  "has_more": true,
  "results": []
}
```

`count` is the number of documents matching the current search state (query plus every filter), independent of `limit`/`offset`. It is nearly free on text searches, which already traverse the whole matching set; pass `count=false` to skip it on broad filter-only browses. `returned` is the number of rows in the response. `total` is a deprecated alias of `returned`, kept so existing clients keep working.

Comma-separated values are OR'ed within a parameter, and filters never influence ranking — only `q` does:

```bash
curl 'localhost:8888/search?city=OSLO,BERGEN&limit=20'
```

When `include_pagination=true`, `/search` also includes the legacy `pagination` object:

```json
{
  "took_ms": 4.1,
  "total": 100,
  "pagination": {
    "offset": 200,
    "limit": 100,
    "returned": 100,
    "total": 18432,
    "total_relation": "eq",
    "has_more": true
  },
  "results": []
}
```

The maximum pagination window is `offset + limit <= 100000` — that bounds deep paging, not `count`. Relevance-ranked text searches (`q` without `sort_by`) page within their first 1000 candidates, ranked by one similarity model throughout rather than quietly switching models deeper in; a window past 1000 is refused with a 400. A text search *with* `sort_by` is different: it sorts the query's whole match set, like a browse, up to the 100k window. A document matches when, for every query word, at least *n−3* of the word's *n* distinct trigrams appear in one fuzzy field (never fewer than two, so a short word must appear nearly whole) — one edit destroys at most three trigrams, so words of seven characters and up keep one-typo tolerance. `q=berg&sort_by=revenue` is therefore the companies with "berg" in the name by revenue, not every name containing "ber"; `count` is exact and independent of the page, `desc&limit=1` is the true maximum, and pages stitch. Note the relevance path's `count` is deliberately broader (documents sharing any of the query's informative trigrams), so the two numbers differ for the same `q`.

### `GET /fields`, `GET /docs`, `GET /openapi.json`

Self-documenting, generated from the live schema so they can't drift:

```bash
curl 'localhost:8888/fields'        # every accepted parameter, as JSON
curl 'localhost:8888/docs'          # the whole reference as Markdown
curl 'localhost:8888/openapi.json'  # OpenAPI 3.1
```

`/docs` is sized to be fetched whole by an LLM — one request and an agent knows every valid parameter for your schema.

### Behind a reverse proxy

ruzz listens on every interface by default. When something else terminates TLS in front of it — Caddy, nginx, a load balancer — bind to loopback so the plain-HTTP port cannot be reached from outside the host at all, and the proxy becomes the only way in:

```toml
[server]
bind = "127.0.0.1"
```

The server says which address it bound to on startup. A whole-host Caddyfile for two ruzz instances is six lines:

```
companies.example.com {
	encode zstd gzip
	reverse_proxy 127.0.0.1:8889
}

directors.example.com {
	encode zstd gzip
	reverse_proxy 127.0.0.1:8890
}
```

Use one hostname per instance rather than a path prefix. The dashboard requests `/search` and `/stats` relative to the page root, so serving it under `example.com/directors/` would load the page and then have its own requests miss.

If `auth_token` is set, keep the token out of your proxy's access log — it is a credential, and `?token=…` is written to disk in cleartext otherwise. In Caddy:

```
log {
	format filter {
		request>uri query { replace token REDACTED }
	}
}
```

### Strict parameters

By default an unknown query parameter is reported in `ignored_parameters` on the response. Set `strict_params = true` under `[server]` to reject it instead:

```json
{
  "error": "invalid_parameters",
  "message": "Unknown query parameter(s): registerd_town. See /fields for the full list.",
  "unknown_parameters": ["registerd_town"],
  "did_you_mean": { "registerd_town": "registered_town" }
}
```

Without this, a misspelled filter is silently dropped and the response looks like a valid unfiltered result set.

### `GET /lookup`

Exact match lookup. Lightning fast for keyword, enum, and boolean fields.

```bash
curl 'localhost:8888/lookup?country=US&id=123456789'
```

### Document store endpoints

Available when `[store] enabled = true`. Search results carry a `_ref` per hit.

```bash
# One full document by ref
curl 'localhost:8888/doc/42'

# Batch fetch (order-preserving, null for unknown refs, max 256)
curl 'localhost:8888/docs?refs=42,17,993'

# Resolve exact-match filters to a full document (first match + matched count)
curl 'localhost:8888/doc?country=US&id=123456789'

# Fuzzy search and hydrate hits inline (adds "_full" to each result, limit <= 100)
curl 'localhost:8888/search?q=amazn&full=true&limit=10'

# Works on /lookup too
curl 'localhost:8888/lookup?country=US&id=123456789&full=true'
```

The two-step flow is the intended default: fast fuzzy `/search` (compact rows), let the user pick, then `/doc/{ref}` or `/docs?refs=` for the full records. `full=true` is the one-step convenience for small result pages.

### `GET /stats`

Runtime stats: memory, index size, document count, schema, uptime.

### `GET /activity`

Operation history and server health for monitoring: recent import/update/delete/gc events (newest first, successes and failures), per-day aggregates for the trailing year, resource readings (RSS, system memory, disk free/total on the index volume, process CPU), search load (queries served, plus a ~24h in-memory ring of QPS and p95 latency samples at 15s cadence — reset on restart, deliberately not a time-series database), and — with the store enabled — its on-disk size, compression ratio, cache hit rate and the superseded share a full import would reclaim. Backed by an append-only log at `<index_path>-activity.jsonl` that survives full-import swaps. This is what answers "did last night's delta actually run?"

### `GET /health`

Returns `{"status": "ok"}`. For your load balancer.

### `GET /`

The built-in web dashboard: a Search tab, an **Activity** tab (freshness banner, a year heatmap of records touched per day, resource bars and load sparklines, store stats, recent operations), and generated API docs. The disk bar marks the full-import peak — staging builds beside the live index, so keep about one index of headroom — and the memory bar deliberately avoids used/max framing: RSS here is mostly reclaimable mmap page cache, and `memory_budget` is a warm-up, not a cap. Try it.

## Memory Budget

ruzz controls how much of the index it pre-warms into the OS page cache at startup:

```toml
memory_budget = "100%"     # Warm everything (fastest, default)
memory_budget = "unlimited" # Same as 100%
memory_budget = "2GB"       # Warm the hottest 2GB
memory_budget = "50%"       # Warm half the index
memory_budget = "50MB"      # Minimal warm-up, queries still work
```

When budget < index size, ruzz warms the structures every query touches first — term dictionaries, fast fields and fieldnorms, then the stored documents (the rerank stage reads 50–1000 of them per fuzzy query, so a cold store is the most expensive thing to leave out), then posting lists — and lets the OS page the rest in via mmap on demand. Separately, `doc_cache` (default 128MB) keeps the hottest stored-document blocks *decompressed* in the process, saving the LZ4 decode on repeated candidates. Queries that hit cold pages cost a disk read (~100μs on SSD) instead of a memory lookup (~100ns). Still fast. Just not _absurdly_ fast.

To be clear about what this is: a warm-up, not a cap. Residency is always the OS's call — to actually bound the process's memory, use your platform's mechanism (cgroups, container limits).

## Performance

> The tables below were measured on v0.1, before the query-engine rework
> (block-WAND pruning, rare-trigram driving, similarity reranking). The
> current engine measures 2–8x faster on the fuzzy paths at 5M–20M docs and
> its latency is flat in query length; run `cargo bench` or your own
> dataset for current numbers.

A text search under a *broad* exact filter (one matching 10% or more of the index — `country_code=NO` on a single-country dataset is the extreme) keeps block-WAND pruning by checking the filter per candidate against fast fields instead of intersecting; selective filters keep the intersection, which is cheaper for them. Sorted browses reuse their exact count across pages, and release builds use LTO. `cargo bench --bench search` reports each case; `RUZZ_BENCH_FILTER_STRATEGY=intersect|postfilter` forces either filter path for comparison.

Tested on 1.15M records (single dataset):

| Metric | Value |
|---|---|
| Import speed | **2.9 seconds** (1.15M rows, 16 fields) |
| Index size | 545 MB |
| Memory (full) | ~400 MB |
| Memory (50MB budget) | ~110 MB |
| Fuzzy search (p50) | **0.3 - 2ms** |
| Fuzzy search (p99) | **< 12ms** |
| Exact lookup | **< 0.1ms** |

Tested at scale on 54.6M records across 13 datasets:

| Metric | Value |
|---|---|
| Import speed | **376 seconds** |
| Index size | 29 GB |
| Filtered fuzzy search | **5 - 40ms** |
| Unfiltered fuzzy search | **45 - 280ms** |
| Sort by numeric field | **10 - 33ms** |

For comparison, Postgres `pg_trgm` on the 1.15M dataset: 2ms - 3000ms depending on query. The variance is the problem ruzz solves.

### Benchmarks

The query paths above are guarded by a criterion suite over a deterministic
synthetic corpus (200k docs by default, built once and cached under
`target/`):

```bash
cargo bench
```

To compare a change against the current state, save a baseline first:

```bash
cargo bench --bench search -- --save-baseline before
# ...make the change...
cargo bench --bench search -- --baseline before
```

`RUZZ_BENCH_DOCS=2000000 cargo bench` benches at a different corpus size;
delete `target/tmp/ruzz-bench-*` to force a fixture rebuild.

## Why not just use...

**Postgres pg_trgm?** — Works until you hit a short or common query and wait 3 seconds. ruzz has no pathological cases — every query is bounded.

**Elasticsearch?** — Powerful, but you're running a JVM cluster with YAML config for what might be a single-binary problem.

**MeiliSearch / Typesense?** — Both solid. But RAM-only (no memory budget), no CSV import, and MeiliSearch doesn't expose memory controls.

**SQLite FTS5?** — No fuzzy matching. Exact tokens only.

## Concurrency

Query work runs off the async runtime on a blocking pool, gated by a semaphore sized to the core count: a flood of requests queues instead of oversubscribing the CPUs, and the runtime always has threads free to accept connections and answer `/health` — a saturated server stays responsive rather than reading as dead to a load balancer.

Bulk endpoints (`/match`, `/resolve`) share that same pool, so heavy batches compete with interactive searches for cores like any CPU-bound work — but they can no longer stall the runtime itself. If you drive maximum-size `/match` batches hard alongside interactive traffic, smaller batches still spread the load more fairly.

## Roadmap

- [x] Document store — full records behind compact search rows
- [x] Bulk endpoints off the async workers (`spawn_blocking` + a concurrency bound)
- [x] Atomic re-imports — staged build + swap; a failed import leaves the previous index serving (a running server picks the new index up on restart)
- [x] Incremental delta imports — `primary_key` + `ruzz update`/`ruzz delete`, picked up by a running server without restart
- [ ] Cursor-based pagination (`search_after`) beyond the 100k window
- [ ] Prometheus `/metrics` endpoint — the long-term answer for resource history; the Activity tab's in-memory ring intentionally stops at ~24h
- [x] JSONL import — native nested-document sources with dotted-path mapping; row-mode store keeps the whole record
- [x] Activity log + dashboard Activity tab (`GET /activity`) — import/update/delete history, year heatmap, store dead-weight
- [ ] Direct Postgres/MySQL import
- [ ] Disk-optimized tree index for reduced memory footprint

## Built with

- [Tantivy](https://github.com/quickwit-oss/tantivy) — search engine library (the engine behind [Quickwit](https://quickwit.io))
- [Axum](https://github.com/tokio-rs/axum) — async web framework
- [Rust](https://www.rust-lang.org/)

## License

Apache 2.0.
