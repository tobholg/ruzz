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
- **🎛 Memory budget** — tell ruzz how much RAM it can use. `50MB`, `2GB`, `50%`, `unlimited`. Run on a $5 VPS or a beefy server, same binary.
- **🔎 Filters** — exact match on keywords, numeric range filtering, sort by any field. Fuzzy search + filter by country + sort by revenue desc? One query.
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
    { name = "country", type = "keyword" },
    { name = "id", type = "keyword" },
    { name = "category", type = "keyword" },
    { name = "employees", type = "keyword" },
    { name = "city", type = "keyword" },
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
```

### `GET /lookup`

Exact match lookup. Lightning fast.

```bash
curl 'localhost:8888/lookup?country=US&id=123456789'
```

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

- [ ] Live index updates (append without full rebuild)
- [ ] Direct Postgres/MySQL import
- [ ] Disk-optimized tree index for reduced memory footprint

## Built with

- [Tantivy](https://github.com/quickwit-oss/tantivy) — search engine library (the engine behind [Quickwit](https://quickwit.io))
- [Axum](https://github.com/tokio-rs/axum) — async web framework
- [Rust](https://www.rust-lang.org/)

## License

Apache 2.0.
