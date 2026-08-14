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

- **🔍 Fuzzy search** — typos, partial matches, unicode normalization. "amzon" finds "Amazon". Your users can't spell, and that's okay.
- **⚡ Fast** — sub-millisecond to low-millisecond on millions of documents. No pathological cases. Every query is fast, not just the easy ones.
- **📁 CSV import** — point at your files, define a column mapping, done. Multiple files with different schemas? Different column names? Handled.
- **📦 Document store** *(optional)* — keep the full record behind each compact search row in a zstd-compressed on-disk store. Search stays lean and fast; `GET /doc/{ref}` returns everything, including nested JSON your CSV can't hold.
- **🎛 Memory budget** — tell ruzz how much RAM it can use. `50MB`, `2GB`, `50%`, `unlimited`. Run on a $5 VPS or a beefy server, same binary.
- **🔎 Filters** — exact match on keywords, enums, booleans, numeric range filtering, sort by any field. Fuzzy search + filter by country + sort by revenue desc? One query.
- **🧮 Multi-value fields & OR** — one row can hold `"LEDE,DAGL"` and match either. Pass `role=LEDE,DAGL` to OR across values on any filter. Case-insensitive by default; substring search where you want it.
- **🔢 Real counts** — `count` is the exact number of matches for the current search state, not a capped estimate. That's the number your UI wants to print.
- **📖 Self-documenting API** — `/fields`, `/docs`, `/openapi.json` are generated from your schema, so the docs can't drift. Unknown parameters can be rejected with a spelling suggestion instead of silently ignored.
- **🖥 Web dashboard** — ships with a built-in search UI. Dark mode. Light mode.
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

Enum and boolean fields are indexed as exact-match filters with canonical uppercase values:

```toml
{ name = "currency", type = "enum", values = ["NOK", "SEK", "USD"] }
{ name = "company_status", type = "enum", values = "auto", max_values = 128 }
{ name = "is_active", type = "boolean" }
```

`values = "auto"` discovers low-cardinality enum values during import. The initial default cap is `128` distinct values per auto-enum field.

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
- **`source = "sidecar"`** — each source brings an aligned JSONL file: line *i* is the full document for CSV row *i*, stored byte-for-byte. This is how you store *nested* documents (arrays, objects, history) that a CSV can't represent:

```toml
[[sources]]
path = "data/companies_no.csv"
sidecar = "data/companies_no_full.jsonl"
defaults = { country_code = "NO" }
mapping = { name = "organisasjonsnavn", org_number = "organisasjonsnummer" }
```

The store lives next to the index (`docs.dat` + a tiny block table), is written in one sequential pass during import, and costs nothing on the search hot path. Lookups decompress one block (~100–200μs cold, microseconds cached). Typical compression on record data: 5–10x.

**Ref semantics:** `_ref` is the row's import ordinal. It is stable for the lifetime of an index, but a re-import reshuffles refs — persist your own keys (like `org_number`) externally and resolve them via `/doc?field=value`, not refs.

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

The maximum pagination window is `offset + limit <= 100000` — that bounds deep paging, not `count`.

### `GET /fields`, `GET /docs`, `GET /openapi.json`

Self-documenting, generated from the live schema so they can't drift:

```bash
curl 'localhost:8888/fields'        # every accepted parameter, as JSON
curl 'localhost:8888/docs'          # the whole reference as Markdown
curl 'localhost:8888/openapi.json'  # OpenAPI 3.1
```

`/docs` is sized to be fetched whole by an LLM — one request and an agent knows every valid parameter for your schema.

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

### `GET /health`

Returns `{"status": "ok"}`. For your load balancer.

### `GET /`

The built-in web dashboard. Try it.

## Memory Budget

ruzz lets you control exactly how much RAM to dedicate to the search index:

```toml
memory_budget = "100%"     # Keep everything in memory (fastest, default)
memory_budget = "unlimited" # Same as 100%
memory_budget = "2GB"       # Absolute limit
memory_budget = "50%"       # Half the index stays warm
memory_budget = "50MB"      # Minimal footprint, queries still work
```

When budget < index size, ruzz pre-warms the most important index pages (term dictionaries, posting list heads) and lets the OS handle the rest via mmap. Queries that hit cold pages cost a disk read (~100μs on SSD) instead of a memory lookup (~100ns). Still fast. Just not _absurdly_ fast.

## Performance

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

## Why not just use...

**Postgres pg_trgm?** — Works until you hit a short or common query and wait 3 seconds. ruzz has no pathological cases — every query is bounded.

**Elasticsearch?** — Powerful, but you're running a JVM cluster with YAML config for what might be a single-binary problem.

**MeiliSearch / Typesense?** — Both solid. But RAM-only (no memory budget), no CSV import, and MeiliSearch doesn't expose memory controls.

**SQLite FTS5?** — No fuzzy matching. Exact tokens only.

## Roadmap

- [x] Document store — full records behind compact search rows
- [ ] Zero-downtime re-imports (generational index + store, atomic swap)
- [ ] Incremental delta imports (append/update without full rebuild)
- [ ] JSON import (native nested-document sources)
- [ ] Direct Postgres/MySQL import
- [ ] Disk-optimized tree index for reduced memory footprint

## Built with

- [Tantivy](https://github.com/quickwit-oss/tantivy) — search engine library (the engine behind [Quickwit](https://quickwit.io))
- [Axum](https://github.com/tokio-rs/axum) — async web framework
- [Rust](https://www.rust-lang.org/)

## License

Apache 2.0.
