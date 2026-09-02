use std::collections::HashMap;
use std::sync::Arc;

use tantivy::collector::{Count, MultiCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, ConstScoreQuery, EmptyQuery, Occur, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{DocAddress, Index, IndexReader, ReloadPolicy, Term};

use crate::config::{Config, FieldType, SearchMode};
use crate::field_meta::{
    canonicalize_filter_value, json_boolean_value, load_stored_field_metadata,
    runtime_metadata_for_field, RuntimeFieldMetadata,
};
use crate::schema::REF_FIELD;
use crate::store::{self, StoreReader};

/// A filter value resolved against its field's type. Text fields index a
/// string term; numeric fields index an f64, and the two are not
/// interchangeable — building a text term for a numeric field yields a term
/// that exists nowhere in the index.
enum FilterValue {
    Text(String),
    Number(f64),
}

impl FilterValue {
    fn into_term(self, field: Field) -> Term {
        match self {
            FilterValue::Text(value) => Term::from_field_text(field, &value),
            FilterValue::Number(value) => Term::from_field_f64(field, value),
        }
    }
}

pub struct SearchEngine {
    pub index: Index,
    pub reader: IndexReader,
    pub schema: Schema,
    pub field_map: HashMap<String, Field>,
    pub field_configs: HashMap<String, crate::config::FieldConfig>,
    /// Enum value lists and the like, read from the index's metadata file.
    /// An incremental update rewrites that file, so this is reloaded when
    /// the searcher generation moves — see `field_values`.
    field_metadata: std::sync::RwLock<HashMap<String, RuntimeFieldMetadata>>,
    metadata_generation: std::sync::atomic::AtomicU64,
    pub config: Arc<Config>,
    pub store: Option<StoreReader>,
    pub store_status: StoreStatus,
    ref_field: Option<Field>,
    /// Keyword fields the index folded to lowercase. Filters are folded to
    /// match only for these, so a new binary on an old index keeps its
    /// original exact-match behaviour.
    case_insensitive_fields: std::collections::HashSet<String>,
    /// Fuzzy text was folded (not just lowercased) at index time; queries
    /// must fold the same way. False on indexes built before folding.
    folded: bool,
    /// Fuzzy field name → its `_prefix_*` typeahead shadow field, present
    /// only on indexes that carry them.
    prefix_field_map: HashMap<String, Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreStatus {
    Disabled,
    Ok,
    Error(String),
}

impl StoreStatus {
    pub fn as_str(&self) -> &str {
        match self {
            StoreStatus::Disabled => "disabled",
            StoreStatus::Ok => "ok",
            StoreStatus::Error(e) => e,
        }
    }
}

#[derive(serde::Serialize)]
pub struct SearchResult {
    pub took_ms: f64,
    /// Deprecated: number of rows in this response. Kept unchanged so existing
    /// clients keep working — use `returned` for this, `count` for the number
    /// of matches.
    pub total: usize,
    /// Rows in this response.
    pub returned: usize,
    /// Documents matching the current search state (query + every filter),
    /// independent of limit/offset. Exact and uncapped. Absent when the
    /// caller passes count=false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pagination: Option<PaginationInfo>,
    pub results: Vec<serde_json::Value>,
}

#[derive(serde::Serialize)]
pub struct PaginationInfo {
    pub offset: usize,
    pub limit: usize,
    pub returned: usize,
    pub total: usize,
    pub total_relation: &'static str,
    pub has_more: bool,
}

pub struct RangeFilter {
    pub field: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

pub enum SortOrder {
    Relevance,
    FieldAsc(String),
    FieldDesc(String),
}

/// Deepest reachable result page (offset + limit). This bounds the cost of
/// deep pagination — it does not bound `count`, which is always exact.
pub const MAX_PAGINATION_WINDOW: usize = 100_000;

/// A boolean clause: how it must occur, and the query itself.
type Clause = (Occur, Box<dyn Query>);
/// Hits as (score, address) — score is BM25, a sort value, or a similarity.
type Hits = Vec<(f64, DocAddress)>;
/// Rerank candidates as (similarity, bm25, address).
type RankedHits = Vec<(f64, f64, DocAddress)>;

/// Driving trigrams per query word. One edit destroys at most three of a
/// word's trigrams, so of any four at least one survives — recall at edit
/// distance 1. Seven survive two edits; the wide drive is the adaptive
/// second pass for queries the narrow one fails.
const RARE_DRIVE_TERMS: usize = 4;
const WIDE_DRIVE_TERMS: usize = 7;

/// Candidate pool for reranking: enough beyond the page that similarity can
/// genuinely reorder, small enough that fetching the candidates' stored
/// values stays cheap.
const RERANK_POOL_MIN: usize = 50;
const RERANK_POOL_MAX: usize = 200;

/// Wrap a filter clause so it matches without contributing to relevance.
fn const_score(query: Box<dyn Query>) -> Box<dyn Query> {
    Box::new(ConstScoreQuery::new(query, 1.0))
}

/// Open the document store and verify it belongs to this index: the
/// generation stamped into the index dir at import time must match the store
/// meta, and doc counts must line up.
fn open_store(config: &Config, reader: &IndexReader) -> anyhow::Result<StoreReader> {
    let store_path = config.resolve_store_path();
    if store_path == config.legacy_store_path() && store_path != config.default_store_path() {
        eprintln!(
            "note: reading the document store from the legacy path {}. \
             New imports write to {} instead, which cannot collide with another \
             index in the same directory. Rename it, or pin it with [store] path.",
            store_path.display(),
            config.default_store_path().display()
        );
    }
    let cache_bytes = store::parse_size(&config.store.cache).unwrap_or(0);
    let store = StoreReader::open(&store_path, cache_bytes)?;

    let pairing = store::read_index_pairing(&config.server.index_path).ok_or_else(|| {
        anyhow::anyhow!("index has no store pairing file — re-run import with [store] enabled")
    })?;
    if pairing.generation != store.generation() {
        anyhow::bail!(
            "store generation {} does not match index generation {} — re-run import",
            store.generation(),
            pairing.generation
        );
    }
    let num_docs = reader.searcher().num_docs();
    match pairing.doc_count {
        // Incremental updates leave superseded versions in the store, so the
        // live index holds fewer docs than the store holds refs. The pairing
        // records how many refs the index may hand out; the store must reach
        // at least that far (more is a harmless in-flight append).
        Some(refs_issued) => {
            if store.doc_count() < refs_issued {
                anyhow::bail!(
                    "store holds {} docs but the index references up to {} — re-run import",
                    store.doc_count(),
                    refs_issued
                );
            }
        }
        // Pre-incremental pairing: refs were exactly the doc count.
        None => {
            if store.doc_count() != num_docs {
                anyhow::bail!(
                    "store holds {} docs but index holds {} — re-run import",
                    store.doc_count(),
                    num_docs
                );
            }
        }
    }
    Ok(store)
}

impl SearchEngine {
    pub fn open(config: Arc<Config>) -> anyhow::Result<Self> {
        let index = Index::open_in_dir(&config.server.index_path)?;
        crate::import::register_trigram_tokenizer_pub(&index);

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let schema = index.schema();
        let mut field_map = HashMap::new();
        let mut field_configs = HashMap::new();
        for fc in &config.schema.fields {
            if let Ok(field) = schema.get_field(&fc.name) {
                field_map.insert(fc.name.clone(), field);
            }
            field_configs.insert(fc.name.clone(), fc.clone());
        }

        let stored_metadata = load_stored_field_metadata(&config.server.index_path)?;
        let case_insensitive_fields: std::collections::HashSet<String> = stored_metadata
            .case_insensitive_fields
            .iter()
            .cloned()
            .collect();
        let field_metadata = runtime_field_metadata(&config, &stored_metadata);
        let metadata_generation = reader.searcher().generation().generation_id();

        let folded = stored_metadata.folded_fuzzy;
        let prefix_field_map: HashMap<String, Field> = if stored_metadata.prefix_fields {
            config
                .schema
                .fields
                .iter()
                .filter(|fc| fc.search == Some(SearchMode::Fuzzy))
                .filter_map(|fc| {
                    schema
                        .get_field(&crate::schema::prefix_field_name(&fc.name))
                        .ok()
                        .map(|field| (fc.name.clone(), field))
                })
                .collect()
        } else {
            HashMap::new()
        };

        let ref_field = schema.get_field(REF_FIELD).ok();
        let (store, store_status) = if config.store.enabled {
            match open_store(&config, &reader) {
                Ok(store) => (Some(store), StoreStatus::Ok),
                Err(e) => {
                    let msg = format!("{:#}", e);
                    eprintln!("⚠ document store unavailable: {} (search still works; doc endpoints disabled)", msg);
                    (None, StoreStatus::Error(msg))
                }
            }
        } else {
            (None, StoreStatus::Disabled)
        };

        Ok(Self {
            index,
            reader,
            schema,
            field_map,
            field_configs,
            field_metadata: std::sync::RwLock::new(field_metadata),
            metadata_generation: std::sync::atomic::AtomicU64::new(metadata_generation),
            config,
            store,
            store_status,
            ref_field,
            case_insensitive_fields,
            folded,
            prefix_field_map,
        })
    }

    /// Runtime metadata for one field (enum values, truncation), current as
    /// of the latest index generation. Values discovered by an incremental
    /// update used to stay invisible until restart: the metadata was read
    /// once at open, while the update had rewritten the file underneath.
    pub fn field_values(&self, name: &str) -> Option<RuntimeFieldMetadata> {
        self.refresh_field_metadata();
        self.field_metadata.read().unwrap().get(name).cloned()
    }

    fn refresh_field_metadata(&self) {
        use std::sync::atomic::Ordering;
        let generation = self.reader.searcher().generation().generation_id();
        if self.metadata_generation.load(Ordering::Relaxed) == generation {
            return;
        }
        // Two threads may both reload after the same commit; the second
        // just writes the same map again.
        if let Ok(stored) = load_stored_field_metadata(&self.config.server.index_path) {
            *self.field_metadata.write().unwrap() = runtime_field_metadata(&self.config, &stored);
        }
        self.metadata_generation
            .store(generation, Ordering::Relaxed);
    }

    /// Normalize query text the way this index normalized its fuzzy terms.
    /// Old indexes lowercased; new ones fold — mixing the two silently
    /// breaks matching, so the stored metadata decides.
    fn normalize(&self, text: &str) -> String {
        if self.folded {
            crate::analyze::fold(text)
        } else {
            text.to_lowercase()
        }
    }

    /// Fold a filter value the same way the index folded its terms.
    fn match_case(&self, field: &str, value: String) -> String {
        if self.case_insensitive_fields.contains(field) {
            return value.to_lowercase();
        }
        // An enum outside that set belongs to an index built before enums
        // preserved their casing: its terms were upper-cased on the way in,
        // so a filter has to be upper-cased to meet them. Without this a
        // binary upgrade would silently stop matching every enum filter on
        // every existing index.
        if matches!(
            self.field_configs.get(field).map(|f| &f.field_type),
            Some(FieldType::Enum)
        ) {
            return value.to_uppercase();
        }
        value
    }

    /// Terms a filter should match, resolved against the field's type.
    ///
    /// `None` means the caller supplied no value at all (`city=`), which an
    /// unselected UI control sends and which must not constrain the query.
    /// `Some([])` means values were supplied but none can ever match, which
    /// must constrain the query to nothing — the alternative is dropping the
    /// clause and returning the entire index, which reads as success.
    fn resolve_filter_values(
        &self,
        key: &str,
        field_config: &crate::config::FieldConfig,
        raw: &str,
    ) -> Option<Vec<FilterValue>> {
        let parts: Vec<&str> = raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.is_empty() {
            return None;
        }
        Some(
            parts
                .into_iter()
                .filter_map(|part| match field_config.field_type {
                    // Numeric fields hold an f64. A text term built from "40"
                    // is a different term entirely and can never match one.
                    FieldType::Number => part.parse::<f64>().ok().map(FilterValue::Number),
                    _ => canonicalize_filter_value(&field_config.field_type, part)
                        .map(|value| FilterValue::Text(self.match_case(key, value))),
                })
                .collect(),
        )
    }

    /// One MUST clause for an exact filter, or None when the filter is not a
    /// constraint. Shared by `/search` and the `/lookup` + `/doc` path so the
    /// two cannot drift apart.
    fn exact_clause(
        &self,
        field: Field,
        values: Vec<FilterValue>,
        const_scored: bool,
    ) -> Box<dyn Query> {
        let wrap = |query: Box<dyn Query>| {
            if const_scored {
                const_score(query)
            } else {
                query
            }
        };
        let mut terms = values.into_iter().map(|value| {
            let term = value.into_term(field);
            Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>
        });
        match terms.next() {
            // Nothing matchable was asked for: unsatisfiable, not unfiltered.
            None => Box::new(EmptyQuery),
            Some(first) => {
                let rest: Vec<Box<dyn Query>> = terms.collect();
                if rest.is_empty() {
                    wrap(first)
                } else {
                    let clauses: Vec<(Occur, Box<dyn Query>)> = std::iter::once(first)
                        .chain(rest)
                        .map(|query| (Occur::Should, query))
                        .collect();
                    wrap(Box::new(BooleanQuery::new(clauses)))
                }
            }
        }
    }

    /// Substring query over one field: every trigram of the value must be
    /// present. Basic record option — the clause is const-scored, so freqs
    /// and positions would be decoded for nothing.
    fn trigram_query(&self, field: Field, text: &str) -> Option<BooleanQuery> {
        let ngrams = query_trigrams(&self.normalize(text));
        if ngrams.is_empty() {
            return None;
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = ngrams
            .iter()
            .map(|ng| {
                let term = Term::from_field_text(field, ng);
                let query: Box<dyn Query> =
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Must, query)
            })
            .collect();
        Some(BooleanQuery::new(clauses))
    }

    /// Fuzzy `q` as ONE flat Should-union with a TermQuery per fuzzy field
    /// per selected trigram: for each query word, the `RARE_DRIVE_TERMS`
    /// rarest of that word's trigrams by document frequency (all of them
    /// when the word has that few).
    ///
    /// Two properties carry the whole fuzzy path:
    ///
    /// Flat matters: tantivy specializes a union into its block-WAND top-k
    /// pruning path only when every clause of a single BooleanQuery is a raw
    /// term query. A union of per-field unions never qualified, so every
    /// document sharing even one trigram with the query was fully
    /// BM25-scored.
    ///
    /// Rare matters: traversal cost is the summed posting lengths of the
    /// clauses, and the rarest trigrams are orders of magnitude cheaper than
    /// the commonest while carrying nearly all of the IDF weight — the
    /// trigrams dropped are exactly the ones BM25 scores lowest. Recall is
    /// pigeonholed: one edit destroys at most three of a word's trigrams, so
    /// of any four at least one survives in every document within edit
    /// distance 1 of that word. Documents sharing only the query's common
    /// trigrams — the bulk of the old candidate set — stop matching, which
    /// is the point. Ties on document frequency break on the trigram itself
    /// so the selection cannot vary between runs.
    ///
    /// WithFreqs, not positions: BM25 needs term frequency, and nothing
    /// reads positions.
    /// Returns the flat fuzzy clauses and whether any word was capped —
    /// i.e. had more trigrams than `drive`, so widening the drive could
    /// genuinely admit more candidates.
    fn fuzzy_clauses(
        &self,
        searcher: &tantivy::Searcher,
        fields: &[(String, Field)],
        text: &str,
        drive: usize,
    ) -> (Vec<Clause>, bool) {
        let normalized = self.normalize(text);
        let mut capped = false;
        let mut seen: std::collections::HashSet<Term> = std::collections::HashSet::new();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for word in normalized.split_whitespace() {
            let mut ngrams = generate_ngrams(word, 3, 3);
            ngrams.sort_unstable();
            ngrams.dedup();
            if ngrams.is_empty() {
                // Too short for a trigram — the first keystrokes of a search
                // box. New indexes carry edge-prefix shadow fields exactly
                // for this; old ones keep matching nothing here.
                for (name, _) in fields {
                    if let Some(&prefix_field) = self.prefix_field_map.get(name) {
                        let term = Term::from_field_text(prefix_field, word);
                        if seen.insert(term.clone()) {
                            clauses.push((
                                Occur::Should,
                                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
                                    as Box<dyn Query>,
                            ));
                        }
                    }
                }
                continue;
            }
            if ngrams.len() > drive {
                capped = true;
            }
            for (_, field) in fields {
                let field = *field;
                let mut ranked: Vec<(u64, &String)> = ngrams
                    .iter()
                    .map(|ng| {
                        let term = Term::from_field_text(field, ng);
                        let df = if ngrams.len() > drive {
                            searcher.doc_freq(&term).unwrap_or(u64::MAX)
                        } else {
                            0 // short word: everything is kept, skip the lookups
                        };
                        (df, ng)
                    })
                    .collect();
                ranked.sort();
                for (_, ng) in ranked.into_iter().take(drive) {
                    let term = Term::from_field_text(field, ng);
                    if seen.insert(term.clone()) {
                        clauses.push((
                            Occur::Should,
                            Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
                                as Box<dyn Query>,
                        ));
                    }
                }
            }
        }
        (clauses, capped)
    }

    /// The MUST clauses for exact, substring and range filters — rebuildable,
    /// because the rerank fallback composes the query twice.
    fn filter_clauses(
        &self,
        filters: &HashMap<String, String>,
        range_filters: &[RangeFilter],
    ) -> Vec<(Occur, Box<dyn Query>)> {
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (key, value) in filters {
            if let (Some(&field), Some(field_config)) =
                (self.field_map.get(key), self.field_configs.get(key))
            {
                // Substring fields are trigram-indexed: match every trigram
                // of the value rather than the value as one term.
                if field_config.search == Some(SearchMode::Substring) {
                    match self.trigram_query(field, value) {
                        Some(query) => subqueries.push((Occur::Must, const_score(Box::new(query)))),
                        // Under three characters there are no trigrams to
                        // match. Returning everything would be a silent lie.
                        None => subqueries.push((Occur::Must, Box::new(EmptyQuery))),
                    }
                    continue;
                }
                // Filters are wrapped in ConstScoreQuery so they contribute no
                // relevance. Without this, BM25 gives rarer values a higher
                // IDF, which sorts a multi-value filter into blocks — a query
                // for BERGEN,STAVANGER returns every STAVANGER row before the
                // first BERGEN one, so a normal page looks like the OR was
                // ignored. Only `q` should influence ranking.
                let Some(values) = self.resolve_filter_values(key, field_config, value) else {
                    continue;
                };
                subqueries.push((Occur::Must, self.exact_clause(field, values, true)));
            }
        }
        for rf in range_filters {
            if self.field_map.contains_key(&rf.field) {
                let min = rf.min.unwrap_or(f64::MIN);
                let max = rf.max.unwrap_or(f64::MAX);
                let range_query = RangeQuery::new_f64_bounds(
                    rf.field.clone(),
                    std::ops::Bound::Included(min),
                    std::ops::Bound::Included(max),
                );
                subqueries.push((Occur::Must, const_score(Box::new(range_query))));
            }
        }
        subqueries
    }

    fn fuzzy_field_handles(&self) -> Vec<(String, Field)> {
        self.config
            .schema
            .fields
            .iter()
            .filter(|fc| fc.search == Some(SearchMode::Fuzzy))
            .filter_map(|fc| {
                self.field_map
                    .get(&fc.name)
                    .map(|&field| (fc.name.clone(), field))
            })
            .collect()
    }

    /// One retrieval + similarity pass for the reranked path. Returns hits
    /// as (similarity, bm25, address) sorted best-first, the exact count if
    /// asked for, whether any word's driving was capped (so widening could
    /// help), and how many candidates the pool actually yielded.
    #[allow(clippy::too_many_arguments)]
    fn rerank_attempt(
        &self,
        searcher: &tantivy::Searcher,
        fuzzy_fields: &[(String, Field)],
        normalized_query: &str,
        query_text: &str,
        filters: &HashMap<String, String>,
        range_filters: &[RangeFilter],
        drive: usize,
        pool: usize,
        need_count: bool,
    ) -> anyhow::Result<(RankedHits, Option<usize>, bool, usize)> {
        let (clauses, capped) = self.fuzzy_clauses(searcher, fuzzy_fields, query_text, drive);
        let subqueries = self.filter_clauses(filters, range_filters);
        let (query, prunable) = compose_query(Some(clauses), subqueries);
        let (docs, total) = collect_relevance(searcher, &*query, prunable, pool, 0, need_count)?;
        let fetched = docs.len();

        let mut scored: RankedHits = Vec::with_capacity(docs.len());
        for (bm25, addr) in docs {
            let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
            let mut best = 0.0f64;
            for (_, field) in fuzzy_fields {
                let value: String = doc
                    .get_all(*field)
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if value.is_empty() {
                    continue;
                }
                let sim =
                    crate::analyze::name_similarity(normalized_query, &self.normalize(&value));
                best = best.max(sim);
            }
            scored.push((best, bm25, addr));
        }
        // Similarity decides, BM25 breaks ties among equally-similar names
        // (an exact duplicate set, say). NaN cannot occur: both are finite.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        Ok((scored, total, capped, fetched))
    }

    /// Relevance-sorted fuzzy search with reranking: retrieve a candidate
    /// pool by trigram BM25 (cheap recall), re-order it by true string
    /// similarity against the stored values (precision), and page from
    /// that. `_score` on this path is the similarity, 0..1.
    ///
    /// When the best candidate is still weak and the rare-trigram driving
    /// was capped, retry once with a wider drive: four driving terms
    /// guarantee recall at one edit (pigeonhole over three destroyed
    /// trigrams), seven guarantee it at two — the adaptive pass buys
    /// edit-distance-2 tolerance only on the queries that need it, instead
    /// of taxing every query with the wider union.
    #[allow(clippy::too_many_arguments)]
    fn search_reranked(
        &self,
        start: std::time::Instant,
        searcher: &tantivy::Searcher,
        query_text: &str,
        filters: &HashMap<String, String>,
        range_filters: &[RangeFilter],
        limit: usize,
        offset: usize,
        pool: usize,
        include_pagination: bool,
        want_count: bool,
        include_full: bool,
    ) -> anyhow::Result<SearchResult> {
        const WEAK_SIM: f64 = 0.55;
        let need_count = want_count || include_pagination;
        let fuzzy_fields = self.fuzzy_field_handles();
        let normalized_query = self.normalize(query_text);

        let (mut scored, mut total, capped, mut fetched) = self.rerank_attempt(
            searcher,
            &fuzzy_fields,
            &normalized_query,
            query_text,
            filters,
            range_filters,
            RARE_DRIVE_TERMS,
            pool,
            need_count,
        )?;

        let best_sim = |s: &RankedHits| s.first().map_or(0.0, |hit| hit.0);
        if capped && best_sim(&scored) < WEAK_SIM {
            let (wide, wide_total, _, wide_fetched) = self.rerank_attempt(
                searcher,
                &fuzzy_fields,
                &normalized_query,
                query_text,
                filters,
                range_filters,
                WIDE_DRIVE_TERMS,
                pool,
                need_count,
            )?;
            if best_sim(&wide) > best_sim(&scored) {
                scored = wide;
                total = wide_total;
                fetched = wide_fetched;
            }
        }

        let page: Vec<(f64, DocAddress)> = scored
            .iter()
            .skip(offset)
            .take(limit)
            .map(|&(sim, _, addr)| (sim, addr))
            .collect();
        let results = self.render_hits(searcher, &page, include_full)?;
        let returned = results.len();
        let pagination = if include_pagination {
            total.map(|total| build_pagination_info(total, limit, offset, returned))
        } else {
            None
        };
        let has_more = match total {
            Some(count) => offset.saturating_add(returned) < count,
            // A full pool means the candidate set was truncated there.
            None => fetched == pool || scored.len() > offset + returned,
        };
        let took = start.elapsed().as_secs_f64() * 1000.0;
        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total: returned,
            returned,
            count: if want_count { total } else { None },
            offset,
            limit,
            has_more,
            pagination,
            results,
        })
    }

    /// Hits → response rows, fetching each document once. The score slot is
    /// whatever the caller ranked by: BM25, a fast-field sort value, or the
    /// rerank similarity.
    fn render_hits(
        &self,
        searcher: &tantivy::Searcher,
        docs: &[(f64, DocAddress)],
        include_full: bool,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(docs.len());
        for (score_or_val, doc_address) in docs {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            let mut obj = serde_json::Map::new();

            for fc in &self.config.schema.fields {
                if let Ok(field) = self.schema.get_field(&fc.name) {
                    let val = doc.get_first(field);
                    match fc.field_type {
                        FieldType::Text | FieldType::Keyword | FieldType::Enum => {
                            // A multi field holds several terms; returning
                            // only the first would hide the rest from callers.
                            if fc.multi {
                                let values: Vec<String> = doc
                                    .get_all(field)
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect();
                                obj.insert(
                                    fc.name.clone(),
                                    serde_json::Value::Array(
                                        values.into_iter().map(serde_json::Value::String).collect(),
                                    ),
                                );
                                continue;
                            }
                            let text = val.and_then(|v| v.as_str()).unwrap_or("");
                            obj.insert(
                                fc.name.clone(),
                                serde_json::Value::String(text.to_string()),
                            );
                        }
                        FieldType::Boolean => {
                            if let Some(text) = val.and_then(|v| v.as_str()) {
                                obj.insert(fc.name.clone(), json_boolean_value(text));
                            } else {
                                obj.insert(fc.name.clone(), serde_json::Value::Null);
                            }
                        }
                        FieldType::Number => {
                            // A stored zero is a value; only a field that was
                            // never filed is null. Zero used to render as
                            // null, making real zeros unreadable.
                            match val.and_then(|v| v.as_f64()) {
                                Some(num) => obj.insert(fc.name.clone(), serde_json::json!(num)),
                                None => obj.insert(fc.name.clone(), serde_json::Value::Null),
                            };
                        }
                    }
                }
            }

            obj.insert("_score".to_string(), serde_json::json!(score_or_val));

            if let Some(doc_ref) = self.doc_ref_of(&doc) {
                obj.insert("_ref".to_string(), serde_json::json!(doc_ref));
                if include_full {
                    obj.insert("_full".to_string(), self.full_as_value(doc_ref));
                }
            }

            results.push(serde_json::Value::Object(obj));
        }
        Ok(results)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        query_text: &str,
        filters: &HashMap<String, String>,
        range_filters: &[RangeFilter],
        sort: &SortOrder,
        limit: usize,
        offset: usize,
        include_pagination: bool,
        want_count: bool,
        include_full: bool,
    ) -> anyhow::Result<SearchResult> {
        let start = std::time::Instant::now();
        let searcher = self.reader.searcher();

        // Relevance-sorted fuzzy queries go through the rerank path — unless
        // the caller pages past the pool, where relevance order is the only
        // one that can be produced incrementally.
        let pool = (limit * 3).clamp(RERANK_POOL_MIN, RERANK_POOL_MAX);
        if !query_text.is_empty() && matches!(sort, SortOrder::Relevance) && offset + limit <= pool
        {
            return self.search_reranked(
                start,
                &searcher,
                query_text,
                filters,
                range_filters,
                limit,
                offset,
                pool,
                include_pagination,
                want_count,
                include_full,
            );
        }

        let fuzzy_fields = self.fuzzy_field_handles();
        let subqueries = self.filter_clauses(filters, range_filters);
        let fuzzy = if query_text.is_empty() {
            None
        } else {
            Some(
                self.fuzzy_clauses(&searcher, &fuzzy_fields, query_text, RARE_DRIVE_TERMS)
                    .0,
            )
        };
        let (query, prunable) = compose_query(fuzzy, subqueries);

        // Determine sort field for fast-field sorting
        let sort_field_name = match sort {
            SortOrder::FieldAsc(f) | SortOrder::FieldDesc(f) => Some(f.as_str()),
            SortOrder::Relevance => None,
        };

        let sort_field_type = sort_field_name
            .and_then(|name| self.field_configs.get(name).map(|fc| fc.field_type.clone()));
        let is_numeric_sort = sort_field_type == Some(FieldType::Number);
        // Keyword, enum and boolean fields carry a string fast-field column,
        // which is what global sorting needs. Text fields do not: they used
        // to be "sorted" by alphabetizing whichever page relevance returned,
        // so every page was ordered internally but pages did not connect.
        // An honest error beats globally shuffled data.
        let is_string_sort = matches!(
            sort_field_type,
            Some(FieldType::Keyword) | Some(FieldType::Enum) | Some(FieldType::Boolean)
        );
        if let Some(name) = sort_field_name {
            if !is_numeric_sort && !is_string_sort {
                anyhow::bail!(
                    "cannot sort by '{}': only keyword, enum, boolean and number \
                     fields are sortable",
                    name
                );
            }
        }

        let need_count = want_count || include_pagination;

        // Execute query with appropriate collector
        let (docs, matched_total): (Vec<(f64, DocAddress)>, Option<usize>) = if is_numeric_sort {
            // Not tantivy's order_by_fast_field: that drops documents
            // without a value, so a doc with no revenue vanished from every
            // revenue-sorted listing. The collector sorts them last instead,
            // matching the string-sort contract.
            let field_name = sort_field_name.unwrap();
            let ascending = matches!(sort, SortOrder::FieldAsc(_));
            let collector = crate::sort::TopByF64Field::new(field_name, offset + limit, ascending);
            let (top, total) = if need_count {
                let mut collectors = MultiCollector::new();
                let docs_handle = collectors.add_collector(collector);
                let count_handle = collectors.add_collector(Count);
                let mut multi_fruit = searcher.search(&*query, &collectors)?;
                let total = count_handle.extract(&mut multi_fruit);
                (docs_handle.extract(&mut multi_fruit), Some(total))
            } else {
                (searcher.search(&*query, &collector)?, None)
            };
            let docs = top
                .into_iter()
                .skip(offset)
                .map(|hit| (hit.value.unwrap_or(0.0), hit.address))
                .collect();
            (docs, total)
        } else if is_string_sort {
            let field_name = sort_field_name.unwrap();
            let ascending = matches!(sort, SortOrder::FieldAsc(_));
            let collector = crate::sort::TopByStrField::new(field_name, offset + limit, ascending);
            let (top, total) = if need_count {
                let mut collectors = MultiCollector::new();
                let docs_handle = collectors.add_collector(collector);
                let count_handle = collectors.add_collector(Count);
                let mut multi_fruit = searcher.search(&*query, &collectors)?;
                let total = count_handle.extract(&mut multi_fruit);
                (docs_handle.extract(&mut multi_fruit), Some(total))
            } else {
                (searcher.search(&*query, &collector)?, None)
            };
            // The sort value itself is in the document; _score carries no
            // information on this path.
            let docs = top
                .into_iter()
                .skip(offset)
                .map(|hit| (0.0, hit.address))
                .collect();
            (docs, total)
        } else {
            collect_relevance(&searcher, &*query, prunable, limit, offset, need_count)?
        };

        let results = self.render_hits(&searcher, &docs, include_full)?;
        let returned = results.len();
        let pagination = if include_pagination {
            matched_total
                .map(|matched_total| build_pagination_info(matched_total, limit, offset, returned))
        } else {
            None
        };
        let has_more = match matched_total {
            Some(count) => offset.saturating_add(returned) < count,
            // Without a count the best signal is a full page
            None => returned == limit,
        };
        let took = start.elapsed().as_secs_f64() * 1000.0;

        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total: returned,
            returned,
            count: if want_count { matched_total } else { None },
            offset,
            limit,
            has_more,
            pagination,
            results,
        })
    }

    pub fn lookup(
        &self,
        filters: &HashMap<String, String>,
        include_full: bool,
    ) -> anyhow::Result<SearchResult> {
        let start = std::time::Instant::now();
        let searcher = self.reader.searcher();

        let query = self.exact_query(filters);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1))?;

        let mut results = Vec::new();
        for (_score, doc_address) in &top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            let mut obj = serde_json::Map::new();
            for fc in &self.config.schema.fields {
                if let Ok(field) = self.schema.get_field(&fc.name) {
                    let val = doc.get_first(field);
                    match fc.field_type {
                        FieldType::Text | FieldType::Keyword | FieldType::Enum => {
                            // A multi field holds several terms; returning
                            // only the first would hide the rest from callers.
                            if fc.multi {
                                let values: Vec<String> = doc
                                    .get_all(field)
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect();
                                obj.insert(
                                    fc.name.clone(),
                                    serde_json::Value::Array(
                                        values.into_iter().map(serde_json::Value::String).collect(),
                                    ),
                                );
                                continue;
                            }
                            let text = val.and_then(|v| v.as_str()).unwrap_or("");
                            obj.insert(
                                fc.name.clone(),
                                serde_json::Value::String(text.to_string()),
                            );
                        }
                        FieldType::Boolean => {
                            if let Some(text) = val.and_then(|v| v.as_str()) {
                                obj.insert(fc.name.clone(), json_boolean_value(text));
                            } else {
                                obj.insert(fc.name.clone(), serde_json::Value::Null);
                            }
                        }
                        FieldType::Number => {
                            // A stored zero is a value; only a field that was
                            // never filed is null. Zero used to render as
                            // null, making real zeros unreadable.
                            match val.and_then(|v| v.as_f64()) {
                                Some(num) => obj.insert(fc.name.clone(), serde_json::json!(num)),
                                None => obj.insert(fc.name.clone(), serde_json::Value::Null),
                            };
                        }
                    }
                }
            }

            if let Some(doc_ref) = self.doc_ref_of(&doc) {
                obj.insert("_ref".to_string(), serde_json::json!(doc_ref));
                if include_full {
                    obj.insert("_full".to_string(), self.full_as_value(doc_ref));
                }
            }

            results.push(serde_json::Value::Object(obj));
        }

        let took = start.elapsed().as_secs_f64() * 1000.0;
        let returned = results.len();
        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total: returned,
            returned,
            count: Some(returned),
            offset: 0,
            limit: 1,
            has_more: false,
            pagination: None,
            results,
        })
    }

    // -- document store access ------------------------------------------------

    /// Raw JSON text of one full document, by ref.
    pub fn get_full(&self, doc_ref: u64) -> anyhow::Result<Option<String>> {
        let Some(store) = self.store.as_ref() else {
            anyhow::bail!("document store not available");
        };
        let Some(bytes) = store.get(doc_ref)? else {
            return Ok(None);
        };
        Ok(Some(String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!("stored document {} is not valid UTF-8", doc_ref)
        })?))
    }

    pub fn get_full_many(&self, refs: &[u64]) -> anyhow::Result<Vec<Option<String>>> {
        refs.iter().map(|&r| self.get_full(r)).collect()
    }

    /// Resolve exact-match filters to a full document: term query → first hit's
    /// `_ref` → store. Returns (matched_count, first_match).
    pub fn resolve_full(
        &self,
        filters: &HashMap<String, String>,
    ) -> anyhow::Result<(usize, Option<(u64, String)>)> {
        if self.store.is_none() {
            anyhow::bail!("document store not available");
        }
        let searcher = self.reader.searcher();
        let query = self.exact_query(filters);

        let mut collectors = MultiCollector::new();
        let docs_handle = collectors.add_collector(TopDocs::with_limit(1));
        let count_handle = collectors.add_collector(Count);
        let mut fruit = searcher.search(&query, &collectors)?;
        let matched = count_handle.extract(&mut fruit);
        let top_docs = docs_handle.extract(&mut fruit);

        let Some((_score, doc_address)) = top_docs.first() else {
            return Ok((matched, None));
        };
        let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
        let doc_ref = self
            .doc_ref_of(&doc)
            .ok_or_else(|| anyhow::anyhow!("matched document has no _ref"))?;
        let full = self
            .get_full(doc_ref)?
            .ok_or_else(|| anyhow::anyhow!("ref {} missing from store", doc_ref))?;
        Ok((matched, Some((doc_ref, full))))
    }

    fn exact_query(&self, filters: &HashMap<String, String>) -> BooleanQuery {
        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (key, value) in filters {
            if let (Some(&field), Some(field_config)) =
                (self.field_map.get(key), self.field_configs.get(key))
            {
                let Some(values) = self.resolve_filter_values(key, field_config, value) else {
                    continue;
                };
                subqueries.push((Occur::Must, self.exact_clause(field, values, false)));
            }
        }
        BooleanQuery::new(subqueries)
    }

    fn doc_ref_of(&self, doc: &tantivy::TantivyDocument) -> Option<u64> {
        let rf = self.ref_field?;
        doc.get_first(rf).and_then(|v| v.as_u64())
    }

    /// Full document as a JSON value for embedding into search results.
    /// Parse errors degrade to a string; a missing doc degrades to null.
    fn full_as_value(&self, doc_ref: u64) -> serde_json::Value {
        let Some(store) = self.store.as_ref() else {
            return serde_json::Value::Null;
        };
        match store.get(doc_ref) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
            }),
            _ => serde_json::Value::Null,
        }
    }
}

/// Per-field runtime metadata worth carrying: only fields that have values
/// (or a truncation flag) get an entry.
fn runtime_field_metadata(
    config: &Config,
    stored: &crate::field_meta::StoredFieldMetadata,
) -> HashMap<String, RuntimeFieldMetadata> {
    let mut field_metadata = HashMap::new();
    for fc in &config.schema.fields {
        let meta = runtime_metadata_for_field(fc, stored.fields.get(&fc.name));
        if !meta.values.is_empty() || meta.truncated {
            field_metadata.insert(fc.name.clone(), meta);
        }
    }
    field_metadata
}

/// Compose the top-level query from optional fuzzy clauses and filter MUSTs.
///
/// `prunable`: the whole query is one flat scored term union — the only
/// shape tantivy's block-WAND pruning accepts — so it is used bare.
/// Wrapping it in an outer BooleanQuery, even alone under a single Must,
/// produces a generic scorer, and that is exactly what the over-threshold
/// arm does on purpose: WAND pays per-clause bookkeeping on every block,
/// and past a handful of trigrams that overhead outgrows the skipping it
/// buys (measured crossover ~6 clauses on a 5M-doc corpus). With filters
/// present the top level is an intersection and cannot prune regardless.
///
/// `fuzzy: Some(vec![])` means a query was given but nothing can match it
/// (too short on an index without prefix fields): unsatisfiable, never
/// unfiltered.
fn compose_query(
    fuzzy: Option<Vec<(Occur, Box<dyn Query>)>>,
    mut subqueries: Vec<(Occur, Box<dyn Query>)>,
) -> (Box<dyn Query>, bool) {
    const WAND_MAX_CLAUSES: usize = 6;
    match fuzzy {
        Some(clauses) if clauses.is_empty() => {
            subqueries.push((Occur::Must, Box::new(EmptyQuery)));
            (Box::new(BooleanQuery::new(subqueries)), false)
        }
        Some(clauses) if subqueries.is_empty() && clauses.len() <= WAND_MAX_CLAUSES => {
            (Box::new(BooleanQuery::new(clauses)), true)
        }
        Some(clauses) => {
            subqueries.push((Occur::Must, Box::new(BooleanQuery::new(clauses))));
            (Box::new(BooleanQuery::new(subqueries)), false)
        }
        // If no subqueries at all (browse mode), use AllQuery
        None if subqueries.is_empty() => (Box::new(AllQuery), false),
        None => (Box::new(BooleanQuery::new(subqueries)), false),
    }
}

/// Collect a relevance-ordered page, with the count when asked for.
fn collect_relevance(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    prunable: bool,
    limit: usize,
    offset: usize,
    need_count: bool,
) -> anyhow::Result<(Hits, Option<usize>)> {
    if need_count && prunable {
        // Two passes beat one here. TopDocs on its own is the only
        // collector arrangement tantivy prunes with block-WAND, so the
        // ranked page skips most of the candidate set; Count on its own
        // disables scoring entirely. A MultiCollector would instead
        // BM25-score every document sharing a trigram with the query —
        // millions of docs on a large index.
        let docs: Vec<(f64, DocAddress)> = searcher
            .search(query, &TopDocs::with_limit(limit).and_offset(offset))?
            .into_iter()
            .map(|(score, addr)| (score as f64, addr))
            .collect();
        let total = searcher.search(query, &Count)?;
        Ok((docs, Some(total)))
    } else if need_count {
        // Count rides along with TopDocs: for filtered or unscored queries
        // nothing can prune anyway, so one shared traversal is the cheapest
        // way to get both.
        let mut collectors = MultiCollector::new();
        let docs_handle = collectors.add_collector(TopDocs::with_limit(limit).and_offset(offset));
        let count_handle = collectors.add_collector(Count);
        let mut multi_fruit = searcher.search(query, &collectors)?;
        let total = count_handle.extract(&mut multi_fruit);
        let docs = docs_handle
            .extract(&mut multi_fruit)
            .into_iter()
            .map(|(score, addr)| (score as f64, addr))
            .collect();
        Ok((docs, Some(total)))
    } else {
        let collector = TopDocs::with_limit(limit).and_offset(offset);
        let docs = searcher
            .search(query, &collector)?
            .into_iter()
            .map(|(score, addr)| (score as f64, addr))
            .collect();
        Ok((docs, None))
    }
}

fn build_pagination_info(
    matched_total: usize,
    limit: usize,
    offset: usize,
    returned: usize,
) -> PaginationInfo {
    PaginationInfo {
        offset,
        limit,
        returned,
        // Counts are exact and uncapped; the relation stays in the response
        // so clients that branch on it keep working.
        total: matched_total,
        total_relation: "eq",
        has_more: matched_total > offset.saturating_add(returned),
    }
}

/// Deduplicated trigrams of ALREADY-NORMALIZED text — callers must run the
/// engine's `normalize` first so query and index bytes agree.
fn query_trigrams(normalized: &str) -> Vec<String> {
    let mut ngrams = generate_ngrams(normalized, 3, 3);
    ngrams.sort_unstable();
    ngrams.dedup();
    ngrams
}

fn generate_ngrams(text: &str, min_n: usize, max_n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut ngrams = Vec::new();
    for n in min_n..=max_n {
        if chars.len() < n {
            continue;
        }
        for i in 0..=(chars.len() - n) {
            let ng: String = chars[i..i + n].iter().collect();
            ngrams.push(ng);
        }
    }
    ngrams
}

#[cfg(test)]
pub mod tests {
    use super::{build_pagination_info, RangeFilter, SearchEngine, SortOrder};
    use crate::config::{
        Config, DashboardConfig, FieldConfig, FieldType, SchemaConfig, ServerConfig, SourceConfig,
        StoreConfig,
    };
    use crate::schema::build_schema;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tantivy::{doc, Index};

    #[test]
    fn reports_exact_uncapped_totals() {
        // Counts used to clamp at 100k and report "gte"; they are now exact
        // however large the match set is.
        let pagination = build_pagination_info(17_635_175, 1000, 99_000, 1000);
        assert_eq!(pagination.total, 17_635_175);
        assert_eq!(pagination.total_relation, "eq");
        assert!(pagination.has_more);

        let last_page = build_pagination_info(2000, 1000, 1000, 1000);
        assert!(!last_page.has_more);
    }

    #[test]
    fn paginates_numeric_filter_queries_globally() {
        let dir = test_index_dir("numeric-pagination");

        let config = Arc::new(Config {
            server: ServerConfig {
                port: 8888,
                bind: "0.0.0.0".to_string(),
                index_path: dir.clone(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                primary_key: None,
                fields: vec![
                    FieldConfig {
                        name: "city".to_string(),
                        field_type: FieldType::Enum,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                    FieldConfig {
                        name: "revenue".to_string(),
                        field_type: FieldType::Number,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                    FieldConfig {
                        name: "name".to_string(),
                        field_type: FieldType::Keyword,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                ],
            },
            sources: Vec::<SourceConfig>::new(),
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        });

        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);

        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            writer
                .add_document(doc!(city => "OSLO", revenue => 200.0, name => "A"))
                .unwrap();
            writer
                .add_document(doc!(city => "OSLO", revenue => 150.0, name => "B"))
                .unwrap();
            writer
                .add_document(doc!(city => "OSLO", revenue => 120.0, name => "C"))
                .unwrap();
            writer
                .add_document(doc!(city => "BERGEN", revenue => 500.0, name => "D"))
                .unwrap();
            writer.commit().unwrap();
        }

        // A real import always records which fields fold case; without it
        // this fixture is a combination that cannot occur.
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let mut filters = HashMap::new();
        filters.insert("city".to_string(), "OSLO".to_string());
        let range_filters = vec![RangeFilter {
            field: "revenue".to_string(),
            min: Some(100.0),
            max: None,
        }];

        let result = engine
            .search(
                "",
                &filters,
                &range_filters,
                &SortOrder::FieldDesc("revenue".to_string()),
                2,
                1,
                true,
                true,
                false,
            )
            .unwrap();

        assert_eq!(result.total, 2);
        let pagination = result.pagination.expect("pagination metadata");
        assert_eq!(pagination.total, 3);
        assert_eq!(pagination.total_relation, "eq");
        assert!(!pagination.has_more);
        assert_eq!(result.results[0]["revenue"], serde_json::json!(150.0));
        assert_eq!(result.results[1]["revenue"], serde_json::json!(120.0));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multi_value_filters_do_not_influence_ranking() {
        // Regression: BM25 gives a rarer filter value a higher IDF, which used
        // to sort a multi-value filter into per-value blocks (all BERGEN rows
        // before the first OSLO row), making an OR filter look like it had
        // been ignored on the first page of results.
        let dir = test_index_dir("filter-scoring");
        let config = test_config(&dir);

        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            // OSLO is common, BERGEN is rare — the IDF gap that caused the bug
            for i in 0..8 {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 100.0, name => format!("O{i}")))
                    .unwrap();
            }
            writer
                .add_document(doc!(city => "BERGEN", revenue => 100.0, name => "B0"))
                .unwrap();
            writer.commit().unwrap();
        }

        // A real import always records which fields fold case; without it
        // this fixture is a combination that cannot occur.
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let mut filters = HashMap::new();
        filters.insert("city".to_string(), "OSLO,BERGEN".to_string());
        let result = engine
            .search(
                "",
                &filters,
                &[],
                &SortOrder::Relevance,
                9,
                0,
                true,
                true,
                false,
            )
            .unwrap();

        assert_eq!(result.results.len(), 9, "OR filter matches every row");
        let scores: Vec<f64> = result
            .results
            .iter()
            .map(|r| r["_score"].as_f64().unwrap())
            .collect();
        assert!(
            scores
                .windows(2)
                .all(|w| (w[0] - w[1]).abs() < f64::EPSILON),
            "filter clauses must score uniformly, got {scores:?}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keyword_filters_match_regardless_of_case() {
        let dir = test_index_dir("case-insensitive");
        let config = test_config(&dir);
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            // `name` is a keyword field holding mixed-case text
            writer
                .add_document(doc!(city => "OSLO", revenue => 10.0, name => "Forusbeen AS"))
                .unwrap();
            writer.commit().unwrap();
        }
        // Written as if the import had recorded the folding
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["name".to_string(), "city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        for probe in ["Forusbeen AS", "forusbeen as", "FORUSBEEN AS"] {
            let mut filters = HashMap::new();
            filters.insert("name".to_string(), probe.to_string());
            let result = engine
                .search(
                    "",
                    &filters,
                    &[],
                    &SortOrder::Relevance,
                    5,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap();
            assert_eq!(result.returned, 1, "casing {probe:?} should still match");
            assert_eq!(
                result.results[0]["name"], "Forusbeen AS",
                "stored value keeps its original casing"
            );
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn case_folding_is_skipped_for_indexes_that_did_not_fold() {
        // A binary upgrade alone must not change query semantics: without the
        // metadata marker the filter value is used verbatim.
        let dir = test_index_dir("case-legacy");
        let config = test_config(&dir);
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            writer
                .add_document(doc!(city => "OSLO", revenue => 10.0, name => "Mixed Case"))
                .unwrap();
            writer.commit().unwrap();
        }
        // No metadata file at all — the pre-upgrade situation, which is the
        // whole point of this test. Writing one here would destroy it.
        let engine = SearchEngine::open(config).unwrap();
        assert!(engine.case_insensitive_fields.is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn substring_field_matches_anywhere_and_stays_out_of_q() {
        let dir = test_index_dir("substring");
        let mut config = test_config(&dir);
        // `name` becomes a substring-searchable text field
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Text;
            field.search = Some(crate::config::SearchMode::Substring);
        }

        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for value in ["Forusbeen 50", "Storgata 1", "Nedre Forusbeen 12"] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 1.0, name => value))
                    .unwrap();
            }
            writer.commit().unwrap();
        }

        // A real import always records which fields fold case; without it
        // this fixture is a combination that cannot occur.
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let search_for = |value: &str| {
            let mut filters = HashMap::new();
            filters.insert("name".to_string(), value.to_string());
            engine
                .search(
                    "",
                    &filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        // Substring, any casing, with or without the house number
        for probe in ["Forusbeen", "forusbeen", "FORUSBEEN", "forusbeen 50"] {
            assert!(
                search_for(probe).returned >= 1,
                "{probe:?} should match by substring"
            );
        }
        assert_eq!(search_for("forusbeen").returned, 2, "matches mid-value too");
        assert_eq!(search_for("storgata").returned, 1);

        // A substring field must not widen the global `q` — that is the whole
        // reason it is a separate search mode.
        let by_q = engine
            .search(
                "forusbeen",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();
        assert_eq!(by_q.returned, 0, "substring fields are excluded from q");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsatisfiable_query_returns_nothing_not_everything() {
        // q=ab has no trigram, so the query cannot be satisfied. It used to
        // fall through to AllQuery and return the whole index.
        let dir = test_index_dir("short-query");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Text;
            field.search = Some(crate::config::SearchMode::Fuzzy);
        }
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for value in ["Alpha", "Beta", "Gamma"] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 1.0, name => value))
                    .unwrap();
            }
            writer.commit().unwrap();
        }

        // A real import always records which fields fold case; without it
        // this fixture is a combination that cannot occur.
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let run = |q: &str| {
            engine
                .search(
                    q,
                    &HashMap::new(),
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };
        assert_eq!(run("ab").returned, 0, "too short to match anything");
        assert_eq!(run("alpha").returned, 1, "normal query still works");
        assert_eq!(run("").returned, 3, "no query at all still browses");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multi_value_field_matches_any_of_its_values() {
        // A person holding several roles must be findable by each of them.
        // Joining the values into one string and matching substrings was
        // unreliable — "Revisor" inside "Styreleder, Revisor" would sometimes
        // miss — so each value is indexed as its own term.
        let dir = test_index_dir("multi-value");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Keyword;
            field.multi = true;
        }

        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            // Written the way import splits a multi cell
            let mut doc = tantivy::TantivyDocument::default();
            doc.add_text(city, "OSLO");
            doc.add_f64(revenue, 1.0);
            for role in ["styreleder", "revisor"] {
                doc.add_text(name, role);
            }
            writer.add_document(doc).unwrap();
            writer
                .add_document(doc!(city => "OSLO", revenue => 1.0, name => "styremedlem"))
                .unwrap();
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["name".to_string(), "city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let search_for = |value: &str| {
            let mut filters = HashMap::new();
            filters.insert("name".to_string(), value.to_string());
            engine
                .search(
                    "",
                    &filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        assert_eq!(
            search_for("styreleder").returned,
            1,
            "matches the first value"
        );
        assert_eq!(
            search_for("revisor").returned,
            1,
            "matches the second value"
        );
        assert_eq!(search_for("Revisor").returned, 1, "still case-insensitive");
        assert_eq!(
            search_for("styremedlem").returned,
            1,
            "single-value doc unaffected"
        );
        // Comma-OR across a multi field returns each matching document once
        assert_eq!(search_for("revisor,styremedlem").returned, 2);

        // The result must expose every value, not just the first
        let result = search_for("revisor");
        let values = result.results[0]["name"]
            .as_array()
            .expect("multi field is an array");
        assert_eq!(values.len(), 2);
        assert!(values.iter().any(|v| v == "revisor"));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The pruning-eligible shape (bare `q`, no filters) takes a different
    /// execution path from `q` + filters: a bare union collects TopDocs and
    /// Count in two passes, filtered queries in one. Both must agree with
    /// each other and return exact counts, whichever fields the union spans.
    #[test]
    fn fuzzy_counts_are_exact_on_both_execution_paths() {
        let dir = test_index_dir("fuzzy-count");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Text;
            field.search = Some(crate::config::SearchMode::Fuzzy);
        }
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for (n, c) in [
                ("Amazon Web Services", "OSLO"),
                ("Amazonia Flowers", "BERGEN"),
                ("Amason Logistics", "OSLO"), // the typo fuzzy search exists for
                ("Beta Bakery", "OSLO"),
            ] {
                writer
                    .add_document(doc!(city => c, revenue => 1.0, name => n))
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let run = |q: &str, filters: &HashMap<String, String>, want_count: bool| {
            engine
                .search(
                    q,
                    filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    want_count,
                    false,
                )
                .unwrap()
        };

        // Bare q: two-pass execution. All three amazon-ish names share
        // trigrams with the query; the bakery shares none.
        let bare = run("amazon", &HashMap::new(), true);
        assert_eq!(bare.count, Some(3), "exact count on the pruned path");
        // Which of the two all-trigram matches wins depends on BM25 length
        // normalization; the stable property is that both beat the one-trigram
        // typo match.
        assert_eq!(
            bare.results[2]["name"], "Amason Logistics",
            "sharing one trigram ranks below sharing all of them"
        );

        // Same q through the filtered (single-pass) path must agree.
        let mut filters = HashMap::new();
        filters.insert("city".to_string(), "OSLO".to_string());
        let filtered = run("amazon", &filters, true);
        assert_eq!(filtered.count, Some(2), "typo match survives the filter");

        // count=false returns the same page without a count.
        let uncounted = run("amazon", &HashMap::new(), false);
        assert_eq!(uncounted.count, None);
        assert_eq!(uncounted.returned, bare.returned);
        assert_eq!(uncounted.results[0]["name"], bare.results[0]["name"]);

        // A query that is nothing but one repeated trigram still works.
        assert_eq!(run("aaaa", &HashMap::new(), true).count, Some(0));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Sorting by a keyword field used to alphabetize each page in
    /// isolation, so pages were internally ordered but did not connect —
    /// page 2 could hold values belonging on page 1. The order must be
    /// global, across pages AND across segments (ordinals from different
    /// segments are not comparable; the merge goes through resolved bytes).
    #[test]
    fn string_sort_is_global_across_pages_and_segments() {
        let dir = test_index_dir("string-sort");
        let config = test_config(&dir);
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            // Two commits → two segments, so the cross-segment merge runs.
            for n in ["delta", "alpha", "echo"] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 1.0, name => n))
                    .unwrap();
            }
            writer.commit().unwrap();
            for n in ["bravo", "charlie"] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 1.0, name => n))
                    .unwrap();
            }
            // And one document with no name at all — it must sort last.
            writer
                .add_document(doc!(city => "OSLO", revenue => 1.0))
                .unwrap();
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let page = |sort: SortOrder, limit: usize, offset: usize| -> Vec<String> {
            engine
                .search(
                    "",
                    &HashMap::new(),
                    &[],
                    &sort,
                    limit,
                    offset,
                    false,
                    true,
                    false,
                )
                .unwrap()
                .results
                .iter()
                .map(|r| r["name"].as_str().unwrap().to_string())
                .collect()
        };

        // Ascending, walked two at a time: pages must connect globally.
        let asc = SortOrder::FieldAsc("name".to_string());
        assert_eq!(page(asc, 2, 0), vec!["alpha", "bravo"]);
        let asc = SortOrder::FieldAsc("name".to_string());
        assert_eq!(page(asc, 2, 2), vec!["charlie", "delta"]);
        let asc = SortOrder::FieldAsc("name".to_string());
        assert_eq!(page(asc, 2, 4), vec!["echo", ""]);

        // Descending reverses the values; the value-less doc stays last.
        let desc = SortOrder::FieldDesc("name".to_string());
        assert_eq!(
            page(desc, 6, 0),
            vec!["echo", "delta", "charlie", "bravo", "alpha", ""]
        );

        // A field with no fast column cannot be sorted globally; that must
        // be an error, not a silently page-local shuffle.
        let err = engine
            .search(
                "",
                &HashMap::new(),
                &[],
                &SortOrder::FieldAsc("no_such_field".to_string()),
                5,
                0,
                false,
                true,
                false,
            )
            .err()
            .expect("sorting by an unknown field must fail")
            .to_string();
        assert!(err.contains("cannot sort by"), "got: {err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Long fuzzy queries (and filtered ones) are driven by their rarest
    /// trigrams. The pigeonhole guarantee: every document within one edit of
    /// a query word stays reachable, while documents sharing only common
    /// trigrams — the bulk of the old candidate set — drop out.
    #[test]
    fn rare_trigram_driving_keeps_typo_matches_and_sheds_noise() {
        let dir = test_index_dir("rare-driving");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Text;
            field.search = Some(crate::config::SearchMode::Fuzzy);
        }
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for (n, c) in [
                ("Storgruppen Konsern", "OSLO"), // the real match
                ("Historie AS", "OSLO"),         // shares only sto/tor — noise
                ("Gruppen Invest", "BERGEN"),    // shares the gruppen tail
                ("Fjordkraft AS", "OSLO"),
            ] {
                writer
                    .add_document(doc!(city => c, revenue => 1.0, name => n))
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let run = |q: &str, filters: &HashMap<String, String>| {
            engine
                .search(
                    q,
                    filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        // 9 trigrams, of which the 4 rarest are kept. The full-name doc and
        // the tail-sharing doc survive; the doc sharing only the two
        // commonest trigrams does not.
        let result = run("storgruppen", &HashMap::new());
        assert_eq!(result.count, Some(2), "common-trigram-only noise is shed");
        assert_eq!(
            result.results[0]["name"], "Storgruppen Konsern",
            "full-union scoring still ranks the real match first"
        );

        // One edit destroys at most three trigrams, so a typo'd query must
        // still reach the real document through its driving terms.
        let typo = run("storgrappen", &HashMap::new());
        assert_eq!(typo.results[0]["name"], "Storgruppen Konsern");

        // A document matching only one word of a multi-word query stays
        // reachable: driving terms are chosen per word.
        let multi = run("blahblah fjordkraft", &HashMap::new());
        assert_eq!(multi.results[0]["name"], "Fjordkraft AS");

        // Filters compose with driving (filtered fuzzy also takes this arm).
        let mut filters = HashMap::new();
        filters.insert("city".to_string(), "OSLO".to_string());
        let filtered = run("storgruppen", &filters);
        assert_eq!(filtered.count, Some(1), "BERGEN tail match filtered out");
        assert_eq!(filtered.results[0]["name"], "Storgruppen Konsern");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The rerank stage: trigram BM25 recalls candidates, true string
    /// similarity orders them. Pins the fix for BM25's length-normalization
    /// quirk (a shorter partial match used to outrank the exact name) and
    /// the adaptive wide-drive pass that recovers edit-distance-2 typos.
    #[test]
    fn rerank_orders_by_similarity_and_recovers_double_typos() {
        let dir = test_index_dir("rerank");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            let field = config
                .schema
                .fields
                .iter_mut()
                .find(|f| f.name == "name")
                .unwrap();
            field.field_type = FieldType::Text;
            field.search = Some(crate::config::SearchMode::Fuzzy);
        }
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for n in [
                "Amazon Web Services",
                "Amazonia Flowers",
                "Interconsulting Partners",
                "Fjordkraft",
            ] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => 1.0, name => n))
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let run = |q: &str| {
            engine
                .search(
                    q,
                    &HashMap::new(),
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        // BM25's field-length normalization used to rank the shorter
        // "Amazonia Flowers" above the exact word match. Similarity does
        // not make that mistake, and reports itself as a 0..1 score.
        let exact = run("amazon");
        assert_eq!(exact.results[0]["name"], "Amazon Web Services");
        let top_score = exact.results[0]["_score"].as_f64().unwrap();
        assert!(
            top_score > 0.99 && top_score <= 1.0,
            "exact word match scores ~1, got {top_score}"
        );

        // Two typos destroy up to six trigrams; the four driving terms of
        // the first pass all miss, and the adaptive wide pass recovers it.
        let double_typo = run("interkonsalting");
        assert_eq!(
            double_typo.results[0]["name"], "Interconsulting Partners",
            "wide drive recovers an edit-distance-2 query"
        );

        // Garbage still returns nothing — both passes come up empty.
        assert_eq!(run("xqzwvbjk").count, Some(0));

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two fuzzy fields flatten into one union; a document must be findable
    /// through either field, and one matching in both must outrank one
    /// matching in a single field (scores sum across the flat union exactly
    /// as they summed across the old nested one).
    #[test]
    fn fuzzy_union_spans_every_fuzzy_field() {
        let dir = test_index_dir("fuzzy-multi-field");
        let mut config = test_config(&dir);
        {
            let config = Arc::get_mut(&mut config).unwrap();
            for name in ["name", "city"] {
                let field = config
                    .schema
                    .fields
                    .iter_mut()
                    .find(|f| f.name == name)
                    .unwrap();
                field.field_type = FieldType::Text;
                field.search = Some(crate::config::SearchMode::Fuzzy);
            }
        }
        let (schema, _) = build_schema(&config.schema, false);
        std::fs::create_dir_all(&dir).unwrap();
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            writer
                .add_document(doc!(city => "Bergen Havn", revenue => 1.0, name => "Bergen Seafood"))
                .unwrap();
            writer
                .add_document(doc!(city => "Oslo", revenue => 1.0, name => "Bergen Byggvarer"))
                .unwrap();
            writer
                .add_document(doc!(city => "Bergen", revenue => 1.0, name => "Fjordkraft"))
                .unwrap();
            writer
                .add_document(doc!(city => "Oslo", revenue => 1.0, name => "Beta Bakery"))
                .unwrap();
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: Vec::new(),
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let result = engine
            .search(
                "bergen",
                &HashMap::new(),
                &[],
                &SortOrder::Relevance,
                10,
                0,
                false,
                true,
                false,
            )
            .unwrap();

        // Matches via name only, city only, and both.
        assert_eq!(result.count, Some(3), "either fuzzy field can match");
        assert_eq!(
            result.results[0]["name"], "Bergen Seafood",
            "matching in both fields outranks matching in one"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_config(dir: &Path) -> Arc<Config> {
        Arc::new(Config {
            server: ServerConfig {
                port: 8888,
                bind: "0.0.0.0".to_string(),
                index_path: dir.to_path_buf(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                primary_key: None,
                fields: vec![
                    FieldConfig {
                        name: "city".to_string(),
                        field_type: FieldType::Enum,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                    FieldConfig {
                        name: "revenue".to_string(),
                        field_type: FieldType::Number,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                    FieldConfig {
                        name: "name".to_string(),
                        field_type: FieldType::Keyword,
                        search: None,
                        values: None,
                        max_values: None,
                        case_sensitive: false,
                        multi: false,
                        separator: None,
                        description: None,
                    },
                ],
            },
            sources: Vec::<SourceConfig>::new(),
            mappings: HashMap::new(),
            store: StoreConfig::default(),
            dashboard: DashboardConfig::default(),
        })
    }

    /// An exact filter on a numeric field used to be dropped on the floor:
    /// the value list came back empty, neither query branch fired, no clause
    /// was added, and every document matched. `age=40` returned the whole
    /// index, and `strict_params` could not catch it because `age` is a real
    /// field. The same shape as the earlier "unsatisfiable query returns
    /// everything" bug, one branch further down.
    #[test]
    fn numeric_filters_match_exactly_and_never_match_everything() {
        let dir = test_index_dir("numeric-exact");
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config(&dir);
        let (schema, _) = crate::schema::build_schema(&config.schema, false);
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            for value in [10.0f64, 20.0, 20.0, 30.0] {
                writer
                    .add_document(doc!(city => "OSLO", revenue => value, name => "acme"))
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let count = |field: &str, value: &str| {
            let mut filters = HashMap::new();
            filters.insert(field.to_string(), value.to_string());
            engine
                .search(
                    "",
                    &filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
                .count
                .unwrap()
        };

        assert_eq!(count("revenue", "20"), 2, "exact numeric match");
        assert_eq!(count("revenue", "10"), 1);
        assert_eq!(
            count("revenue", "20.0"),
            2,
            "same value, different spelling"
        );
        assert_eq!(count("revenue", "99"), 0, "absent value matches nothing");
        assert_eq!(count("revenue", "10,30"), 2, "comma-OR across numbers");

        // The bug: any of these used to return all four documents.
        assert_eq!(count("revenue", "abc"), 0, "unparseable value cannot match");
        assert_ne!(
            count("revenue", "20"),
            4,
            "must not fall back to matching all"
        );

        // An unselected UI control sends an empty value; that is not a
        // constraint and must keep returning everything.
        assert_eq!(count("revenue", ""), 4, "empty value is not a filter");
        assert_eq!(count("city", ""), 4, "empty value is not a filter (text)");
        assert_eq!(count("city", "OSLO"), 4, "text filters unaffected");
        assert_eq!(count("city", "BERGEN"), 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Enums used to be upper-cased on the way in, which bought
    /// case-insensitive matching by destroying the alternative: a region
    /// filed as "Vestland" came back "VESTLAND", and a role code documented
    /// as `dagl` came back `DAGL`. Matching belongs to the index term, as it
    /// already did for keywords.
    #[test]
    fn enum_values_keep_their_casing_and_still_match_either_way() {
        let dir = test_index_dir("enum-case");
        std::fs::create_dir_all(&dir).unwrap();
        let config = test_config(&dir);
        let (schema, _) = crate::schema::build_schema(&config.schema, false);
        let index = tantivy::Index::create_in_dir(&dir, schema.clone()).unwrap();
        crate::import::register_trigram_tokenizer_pub(&index);
        {
            let city = schema.get_field("city").unwrap();
            let revenue = schema.get_field("revenue").unwrap();
            let name = schema.get_field("name").unwrap();
            let mut writer = index.writer(20_000_000).unwrap();
            // Written the way import writes a canonicalised enum: untouched.
            writer
                .add_document(doc!(city => "Vestland", revenue => 1.0, name => "a"))
                .unwrap();
            writer.commit().unwrap();
        }
        crate::field_meta::write_stored_field_metadata(
            &dir,
            &crate::field_meta::StoredFieldMetadata {
                fields: HashMap::new(),
                case_insensitive_fields: vec!["city".to_string(), "name".to_string()],
                folded_fuzzy: false,
                prefix_fields: false,
            },
        )
        .unwrap();

        let engine = SearchEngine::open(config).unwrap();
        let run = |value: &str| {
            let mut filters = HashMap::new();
            filters.insert("city".to_string(), value.to_string());
            engine
                .search(
                    "",
                    &filters,
                    &[],
                    &SortOrder::Relevance,
                    10,
                    0,
                    false,
                    true,
                    false,
                )
                .unwrap()
        };

        for spelling in ["Vestland", "vestland", "VESTLAND", "vEsTlAnD"] {
            assert_eq!(
                run(spelling).count,
                Some(1),
                "filter {spelling:?} should match"
            );
        }
        assert_eq!(run("Trøndelag").count, Some(0));

        // And the value comes back as it was filed, not shouted.
        let stored = run("Vestland").results[0]["city"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            stored, "Vestland",
            "an enum must not rewrite the value it stores"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Build an engine over an empty index with one field. Enough for anything
    /// that only reads the schema, without each caller repeating the setup.
    pub fn engine_with_field(dir: &std::path::Path, field: FieldConfig) -> SearchEngine {
        std::fs::create_dir_all(dir).unwrap();
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 8888,
                bind: "0.0.0.0".to_string(),
                index_path: dir.to_path_buf(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                primary_key: None,
                fields: vec![field],
            },
            sources: Vec::new(),
            mappings: HashMap::new(),
            store: crate::config::StoreConfig::default(),
            dashboard: crate::config::DashboardConfig::default(),
        });
        let (schema, _) = crate::schema::build_schema(&config.schema, false);
        let index = tantivy::Index::create_in_dir(dir, schema).unwrap();
        index
            .writer::<tantivy::TantivyDocument>(15_000_000)
            .unwrap()
            .commit()
            .unwrap();
        SearchEngine::open(config).unwrap()
    }

    fn test_index_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ruzz-{prefix}-{unique}"))
    }
}
