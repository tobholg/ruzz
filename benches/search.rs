//! Search latency benchmarks over a deterministic synthetic corpus.
//!
//! Every query-path optimization ruzz carries — block-WAND on short trigram
//! unions, rare-trigram driving on long ones, split count passes, fast-field
//! sorting — was bought with measurements; these benches keep them honest.
//!
//! The corpus is generated with a fixed seed and imported once into
//! `target/ruzz-bench/`, then reused across runs (delete that directory to
//! force a rebuild, e.g. after an index-format change — the fixture dir is
//! versioned for exactly that reason). Size defaults to 200k docs; set
//! RUZZ_BENCH_DOCS to bench at another scale. Absolute numbers move with
//! corpus size and hardware — the value is in comparing runs on the same
//! machine (`cargo bench` before and after a change, or criterion's own
//! baseline feature).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;

use ruzz::config::{
    Config, DashboardConfig, FieldConfig, FieldType, SchemaConfig, SearchMode, ServerConfig,
    SourceConfig, StoreConfig,
};
use ruzz::import::run_import;
use ruzz::search::{RangeFilter, SearchEngine, SortOrder};

/// Bump when the corpus generator or schema changes, so stale fixtures
/// rebuild instead of silently benchmarking the wrong data.
const FIXTURE_VERSION: u32 = 2;
const DEFAULT_DOCS: usize = 200_000;

// ── deterministic corpus ────────────────────────────────────────────────────

/// xorshift64* — tiny, seeded, dependency-free. Not statistical-grade, which
/// is fine: the corpus only needs to be realistic-ish and identical per run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const FIRST: &[&str] = &[
    "Nor", "Berg", "Fjord", "Vest", "Øst", "Sol", "Havn", "Stor", "Lille", "Nord", "Sør", "Mid",
    "Grøn", "Blå", "Rød", "Sten", "Skog", "Elv", "Ama", "Aker", "Tele", "Hydro", "Marin", "Agri",
    "Bygg", "Kraft", "Data", "Trans", "Inter", "Euro",
];
const SECOND: &[&str] = &[
    "sen",
    "vik",
    "land",
    "berg",
    "dal",
    "nes",
    "gruppen",
    "consulting",
    "zone",
    "tech",
    "mat",
    "bygg",
    "eiendom",
    "invest",
    "logistikk",
    "service",
    "handel",
    "partner",
    "system",
    "verk",
    "holding",
    "produkt",
    "design",
    "media",
    "energi",
];
const SUFFIX: &[&str] = &[
    "AS",
    "ASA",
    "ANS",
    "DA",
    "Group AS",
    "Norge AS",
    "Holding AS",
    "Invest AS",
    "",
];
const CITIES: &[&str] = &[
    "OSLO",
    "BERGEN",
    "TRONDHEIM",
    "STAVANGER",
    "TROMSØ",
    "DRAMMEN",
    "KRISTIANSAND",
    "FREDRIKSTAD",
    "SANDNES",
    "BODØ",
];

fn write_corpus(path: &PathBuf, docs: usize) {
    let mut rng = Rng(0x5EED_CAFE_F00D_D00D);
    let mut out = String::with_capacity(docs * 48);
    out.push_str("org_number,company_name,city,revenue,bankrupt\n");
    for i in 0..docs {
        let mut name = String::new();
        name.push_str(FIRST[rng.below(FIRST.len())]);
        name.push_str(SECOND[rng.below(SECOND.len())]);
        if rng.chance(50) {
            name.push(' ');
            name.push_str(FIRST[rng.below(FIRST.len())]);
            name.push_str(SECOND[rng.below(SECOND.len())]);
        }
        let suffix = SUFFIX[rng.below(SUFFIX.len())];
        if !suffix.is_empty() {
            name.push(' ');
            name.push_str(suffix);
        }
        let revenue = if rng.chance(15) {
            String::new()
        } else {
            (rng.next() % 10_000_000).to_string()
        };
        let bankrupt = if rng.chance(10) { "true" } else { "false" };
        writeln!(
            out,
            "{},{},{},{},{}",
            900_000_000 + i,
            name,
            CITIES[rng.below(CITIES.len())],
            revenue,
            bankrupt
        )
        .unwrap();
    }
    std::fs::write(path, out).unwrap();
}

// ── fixture: build once, reuse across runs ──────────────────────────────────

fn corpus_size() -> usize {
    std::env::var("RUZZ_BENCH_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DOCS)
}

fn bench_config(dir: &std::path::Path) -> Arc<Config> {
    let field = |name: &str, ty: FieldType, search: Option<SearchMode>| FieldConfig {
        name: name.to_string(),
        field_type: ty,
        search,
        values: None,
        max_values: None,
        case_sensitive: false,
        multi: false,
        separator: None,
        description: None,
    };
    let mapping: HashMap<String, String> = [
        ("name", "company_name"),
        ("org_number", "org_number"),
        ("city", "city"),
        ("revenue", "revenue"),
        ("bankrupt", "bankrupt"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();

    Arc::new(Config {
        server: ServerConfig {
            port: 0,
            bind: "127.0.0.1".to_string(),
            index_path: dir.join("index"),
            memory_budget: "100%".to_string(),
            auth_token: None,
            strict_params: false,
            doc_cache: None,
            default_count: true,
        },
        schema: SchemaConfig {
            primary_key: None,
            fields: vec![
                field("name", FieldType::Text, Some(SearchMode::Fuzzy)),
                field("org_number", FieldType::Keyword, None),
                field("city", FieldType::Keyword, None),
                field("revenue", FieldType::Number, None),
                field("bankrupt", FieldType::Boolean, None),
            ],
        },
        sources: vec![SourceConfig {
            path: dir.join("corpus.csv"),
            defaults: HashMap::new(),
            mapping,
            use_mapping: None,
            sidecar: None,
            format: None,
        }],
        mappings: HashMap::new(),
        store: StoreConfig::default(),
        dashboard: DashboardConfig::default(),
    })
}

fn fixture() -> SearchEngine {
    let docs = corpus_size();
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ruzz-bench-{}-v{}", docs, FIXTURE_VERSION));
    let marker = dir.join("fixture-ready");
    let config = bench_config(&dir);
    if !marker.exists() {
        eprintln!("building bench fixture: {} docs (one-time)…", docs);
        std::fs::create_dir_all(&dir).unwrap();
        write_corpus(&dir.join("corpus.csv"), docs);
        run_import(&config).expect("bench corpus import");
        std::fs::write(&marker, b"ok").unwrap();
    }
    let engine = SearchEngine::open(config).expect("open bench index");
    // RUZZ_BENCH_FILTER_STRATEGY=intersect|postfilter forces how filters
    // run under a text query, to measure the two paths against each other.
    match std::env::var("RUZZ_BENCH_FILTER_STRATEGY").as_deref() {
        Ok("intersect") => engine.set_filter_strategy(ruzz::search::FilterStrategy::Intersect),
        Ok("postfilter") => engine.set_filter_strategy(ruzz::search::FilterStrategy::PostFilter),
        _ => {}
    }
    engine
}

// ── benches ─────────────────────────────────────────────────────────────────

struct Query {
    id: &'static str,
    q: &'static str,
    filters: &'static [(&'static str, &'static str)],
    ranges: &'static [(&'static str, f64, f64)],
    sort: fn() -> SortOrder,
    count: bool,
}

fn relevance() -> SortOrder {
    SortOrder::Relevance
}

const QUERIES: &[Query] = &[
    // Short trigram union — the block-WAND fast path, typeahead's shape.
    Query {
        id: "fuzzy_short",
        q: "bergsen",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: false,
    },
    // Same, with the exact count (split TopDocs + Count execution).
    Query {
        id: "fuzzy_short_count",
        q: "bergsen",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Long query — rare-trigram driving keeps this flat.
    Query {
        id: "fuzzy_long",
        q: "interconsulting",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: false,
    },
    Query {
        id: "fuzzy_long_count",
        q: "interconsulting",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Fuzzy under a moderately selective filter (one city of several).
    Query {
        id: "fuzzy_filtered",
        q: "bergsen",
        filters: &[("city", "BERGEN")],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Fuzzy under a broad filter (~90% of documents) — the shape where an
    // intersection buys nothing and post-filtering under WAND pays.
    Query {
        id: "fuzzy_filtered_broad",
        q: "bergsen",
        filters: &[("bankrupt", "false")],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Text query with a field sort: the whole match set (trigram
    // membership) through the global sort collector, plus the exact count.
    Query {
        id: "fuzzy_sorted",
        q: "bergsen",
        filters: &[],
        ranges: &[],
        sort: || SortOrder::FieldDesc("revenue".to_string()),
        count: true,
    },
    // Typeahead: too short for a trigram, served by the edge-prefix field.
    Query {
        id: "typeahead_2char",
        q: "be",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: false,
    },
    // Folded query: diacritic normalization on both sides.
    Query {
        id: "fuzzy_folded",
        q: "sorland",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: false,
    },
    // No plausible match: driving terms empty out, should be near-free.
    Query {
        id: "fuzzy_no_match",
        q: "xqzwvbjk",
        filters: &[],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Filter-only browse with exact count.
    Query {
        id: "filter_browse",
        q: "",
        filters: &[("city", "BERGEN"), ("bankrupt", "FALSE")],
        ranges: &[],
        sort: relevance,
        count: true,
    },
    // Numeric range + fast-field sort.
    Query {
        id: "range_sorted",
        q: "",
        filters: &[("city", "BERGEN")],
        ranges: &[("revenue", 100_000.0, 5_000_000.0)],
        sort: || SortOrder::FieldDesc("revenue".to_string()),
        count: true,
    },
    // Global string sort over the whole corpus — the honest worst case.
    Query {
        id: "string_sort_browse",
        q: "",
        filters: &[],
        ranges: &[],
        sort: || SortOrder::FieldAsc("city".to_string()),
        count: true,
    },
];

fn bench_search(c: &mut Criterion) {
    let engine = fixture();
    let mut group = c.benchmark_group(format!("search_{}docs", corpus_size()));
    group.sample_size(40);
    group.warm_up_time(std::time::Duration::from_millis(800));
    group.measurement_time(std::time::Duration::from_secs(2));

    for query in QUERIES {
        let filters: HashMap<String, String> = query
            .filters
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let ranges: Vec<RangeFilter> = query
            .ranges
            .iter()
            .map(|(field, min, max)| RangeFilter {
                field: field.to_string(),
                min: Some(*min),
                max: Some(*max),
            })
            .collect();
        group.bench_function(query.id, |b| {
            b.iter_batched(
                query.sort,
                |sort| {
                    let result = engine
                        .search(
                            black_box(query.q),
                            black_box(&filters),
                            &ranges,
                            &sort,
                            20,
                            0,
                            false,
                            query.count,
                            false,
                        )
                        .expect("bench query");
                    black_box(result.returned)
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
