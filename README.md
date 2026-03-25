# ruzz ⚡

**Fuzzy search that doesn't make you wait.**

Drop in your CSVs. Get sub-millisecond fuzzy search over millions of records. No Elasticsearch cluster. No 500MB of node_modules. Just one binary and a config file.

```
$ ruzz run
✓ 1,155,509 rows indexed in 2.9s

⚡ ruzz server listening on http://localhost:8888
```

Yeah. Under 3 seconds. Go make coffee with all that time you saved.

---

## What is this?

ruzz is a fast, embeddable fuzzy search engine built in Rust. It eats CSV files for breakfast and serves typo-tolerant search results before you finish typing.

**The pitch:** You have 50 million company records across 13 countries in CSV files. You want to search them. Postgres `pg_trgm` takes 3 seconds on a bad query. Elasticsearch needs a PhD to configure. ruzz does it in 0.3ms and you set it up in 2 minutes.

## Features

- **🔍 Fuzzy search** — typos, partial matches, unicode. "potencon" finds "PROTENCON AS". Your users can't spell, and that's okay.
- **⚡ Fast** — sub-millisecond to low-millisecond on 1M+ documents. No pathological cases. Every query is fast, not just the easy ones.
- **📁 CSV import** — point at your files, define a column mapping, done. Multiple files with different schemas? Different column names per country? Handled.
- **🎛 Memory budget** — tell ruzz how much RAM it can use. `50MB`, `2GB`, `50%`, `unlimited`. It figures out the rest. Run on a $5 VPS or a beefy server, same binary.
- **🔎 Filters** — exact match on keywords, numeric range filtering, sort by any field. Fuzzy search + filter by country + sort by employees desc? One query.
- **🖥 Web dashboard** — ships with a built-in search UI. Dark mode. Light mode. Looks nice. You're welcome.
- **📊 Stats endpoint** — memory usage, index size, document count, uptime. Know exactly what your search engine is doing.

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
    { name = "country_code", type = "keyword" },
    { name = "org_number", type = "keyword" },
    { name = "nace", type = "keyword" },
    { name = "employees", type = "keyword" },
    { name = "city", type = "keyword" },
    { name = "address", type = "text" },
]

# Each source maps CSV columns to schema fields
[[sources]]
path = "data/norway.csv"
defaults = { country_code = "NO" }
mapping = { name = "organisasjonsnavn", org_number = "organisasjonsnummer", nace = "naeringskode1" }

[[sources]]
path = "data/sweden.csv"
defaults = { country_code = "SE" }
mapping = { name = "företagsnamn", org_number = "organisationsnummer", nace = "sni_kod" }

# Reuse mappings for countries with the same CSV structure
[[sources]]
path = "data/germany.csv"
defaults = { country_code = "DE" }
use_mapping = "eu_standard"

[mappings.eu_standard]
name = "company_name"
org_number = "registration_number"
nace = "nace_code"
```

## API

### `GET /search`

Fuzzy search with optional filters and sorting.

```bash
# Basic fuzzy search
curl 'localhost:8888/search?q=equinor&limit=10'

# With filters
curl 'localhost:8888/search?q=abax&country_code=NO&city=LARVIK'

# With numeric range
curl 'localhost:8888/search?q=tech&employees_min=100&employees_max=5000'

# With sorting (override relevance ranking)
curl 'localhost:8888/search?q=energy&sort_by=employees&sort_order=desc'
```

### `GET /lookup`

Exact match lookup. Lightning fast.

```bash
curl 'localhost:8888/lookup?country_code=NO&org_number=923609016'
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

Tested with 1.15M Norwegian companies (Brønnøysundregistrene open data):

| Metric | Value |
|---|---|
| Import speed | **2.9 seconds** (1.15M rows, 16 fields) |
| Index size | 545 MB |
| Memory (full) | ~400 MB |
| Memory (50MB budget) | ~110 MB |
| Fuzzy search (p50) | **0.3 - 2ms** |
| Fuzzy search (p99) | **< 12ms** |
| Exact lookup | **< 0.1ms** |

For comparison, Postgres `pg_trgm` on the same dataset: 2ms - 3000ms depending on query. The variance is the problem ruzz solves.

## Why not just use...

**Postgres pg_trgm?** — Great until you search "aba" and wait 3 seconds because the trigram posting lists are enormous. ruzz doesn't have pathological cases.

**Elasticsearch?** — You need a JVM, a cluster, YAML files, and a therapist. ruzz is one binary.

**MeiliSearch?** — Actually good! But RAM-only, no memory budget control, and you can't point it at a CSV.

**Typesense?** — Also good! Also RAM-only, also no CSV import, also priced by RAM tier.

**SQLite FTS5?** — No fuzzy search. "protencon" won't find "PROTENCON".

## Roadmap

- [ ] Live index updates (append without full rebuild)
- [ ] Direct Postgres/MySQL import
- [ ] HTTP streaming for large result sets
- [ ] Phonetic matching (Soundex/Metaphone)
- [ ] Custom scoring functions
- [ ] Disk-based tree index with level-pinning (the real endgame)

## Built with

- [Tantivy](https://github.com/quickwit-oss/tantivy) — search engine library
- [Axum](https://github.com/tokio-rs/axum) — web framework
- [Rust](https://www.rust-lang.org/) — because life's too short for garbage collection pauses

## License

MIT. Do whatever you want. If you build something cool with it, tell us.

---

*Built by [Recursion Endeavours](https://recursion-endeavours.com). We make fast things faster.*
