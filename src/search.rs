use std::collections::HashMap;
use std::sync::Arc;

use tantivy::collector::{Count, MultiCollector, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, ConstScoreQuery, EmptyQuery, Occur, Query, RangeQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{DocAddress, Index, IndexReader, Order, ReloadPolicy, Term};

use crate::config::{Config, FieldType, SearchMode};
use crate::field_meta::{
    canonicalize_filter_value, json_boolean_value, load_stored_field_metadata,
    runtime_metadata_for_field, RuntimeFieldMetadata,
};
use crate::schema::REF_FIELD;
use crate::store::{self, StoreReader};

pub struct SearchEngine {
    pub index: Index,
    pub reader: IndexReader,
    pub schema: Schema,
    pub field_map: HashMap<String, Field>,
    pub field_configs: HashMap<String, crate::config::FieldConfig>,
    pub field_metadata: HashMap<String, RuntimeFieldMetadata>,
    pub config: Arc<Config>,
    pub store: Option<StoreReader>,
    pub store_status: StoreStatus,
    ref_field: Option<Field>,
    /// Keyword fields the index folded to lowercase. Filters are folded to
    /// match only for these, so a new binary on an old index keeps its
    /// original exact-match behaviour.
    case_insensitive_fields: std::collections::HashSet<String>,
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
    if pairing.generation != store.meta.generation {
        anyhow::bail!(
            "store generation {} does not match index generation {} — re-run import",
            store.meta.generation,
            pairing.generation
        );
    }
    let num_docs = reader.searcher().num_docs();
    if store.doc_count() != num_docs {
        anyhow::bail!(
            "store holds {} docs but index holds {} — re-run import",
            store.doc_count(),
            num_docs
        );
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
        let mut field_metadata = HashMap::new();
        for fc in &config.schema.fields {
            let meta = runtime_metadata_for_field(fc, stored_metadata.fields.get(&fc.name));
            if !meta.values.is_empty() || meta.truncated {
                field_metadata.insert(fc.name.clone(), meta);
            }
        }

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
            field_metadata,
            config,
            store,
            store_status,
            ref_field,
            case_insensitive_fields,
        })
    }

    /// Fold a filter value the same way the index folded its terms.
    fn match_case(&self, field: &str, value: String) -> String {
        if self.case_insensitive_fields.contains(field) {
            value.to_lowercase()
        } else {
            value
        }
    }

    /// Trigram query over one field. `occur` is Should for relevance-ranked
    /// fuzzy matching and Must for substring matching, where every trigram of
    /// the value has to be present.
    fn trigram_query(&self, field: Field, text: &str, occur: Occur) -> Option<BooleanQuery> {
        let ngrams = generate_ngrams(&text.to_lowercase(), 3, 3);
        if ngrams.is_empty() {
            return None;
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = ngrams
            .iter()
            .map(|ng| {
                let term = Term::from_field_text(field, ng);
                let query: Box<dyn Query> = Box::new(TermQuery::new(
                    term,
                    IndexRecordOption::WithFreqsAndPositions,
                ));
                (occur, query)
            })
            .collect();
        Some(BooleanQuery::new(clauses))
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

        let fuzzy_fields: Vec<Field> = self
            .config
            .schema
            .fields
            .iter()
            .filter(|fc| fc.search == Some(SearchMode::Fuzzy))
            .filter_map(|fc| self.field_map.get(&fc.name).copied())
            .collect();

        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Exact filters FIRST as MUST clauses
        for (key, value) in filters {
            if let (Some(&field), Some(field_config)) =
                (self.field_map.get(key), self.field_configs.get(key))
            {
                // Substring fields are trigram-indexed: match every trigram
                // of the value rather than the value as one term.
                if field_config.search == Some(SearchMode::Substring) {
                    match self.trigram_query(field, value, Occur::Must) {
                        Some(query) => subqueries.push((Occur::Must, const_score(Box::new(query)))),
                        // Under three characters there are no trigrams to
                        // match. Returning everything would be a silent lie.
                        None => subqueries.push((Occur::Must, Box::new(EmptyQuery))),
                    }
                    continue;
                }
                let values: Vec<String> = value
                    .split(',')
                    .filter_map(|s| canonicalize_filter_value(&field_config.field_type, s))
                    .map(|v| self.match_case(key, v))
                    .collect();
                // Filters are wrapped in ConstScoreQuery so they contribute no
                // relevance. Without this, BM25 gives rarer values a higher
                // IDF, which sorts a multi-value filter into blocks — a query
                // for BERGEN,STAVANGER returns every STAVANGER row before the
                // first BERGEN one, so a normal page looks like the OR was
                // ignored. Only `q` should influence ranking.
                if values.len() == 1 {
                    let term = Term::from_field_text(field, &values[0]);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, const_score(Box::new(term_query))));
                } else if values.len() > 1 {
                    let or_clauses: Vec<(Occur, Box<dyn Query>)> = values
                        .iter()
                        .map(|v| {
                            let term = Term::from_field_text(field, v);
                            let tq: Box<dyn Query> =
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                            (Occur::Should, tq)
                        })
                        .collect();
                    subqueries.push((
                        Occur::Must,
                        const_score(Box::new(BooleanQuery::new(or_clauses))),
                    ));
                }
            }
        }

        // Native range filters on numeric fields
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

        // Fuzzy search with trigrams
        if !query_text.is_empty() {
            let ngram_queries: Vec<(Occur, Box<dyn Query>)> = fuzzy_fields
                .iter()
                .filter_map(|field| {
                    self.trigram_query(*field, query_text, Occur::Should)
                        .map(|q| (Occur::Should, Box::new(q) as Box<dyn Query>))
                })
                .collect();

            if ngram_queries.is_empty() {
                // No fuzzy field, or a query too short to form a trigram.
                // Previously this fell through to AllQuery and returned the
                // entire index for something like q=ab.
                subqueries.push((Occur::Must, Box::new(EmptyQuery)));
            } else {
                subqueries.push((Occur::Must, Box::new(BooleanQuery::new(ngram_queries))));
            }
        }

        // If no subqueries at all (browse mode), use AllQuery
        let query: Box<dyn Query> = if subqueries.is_empty() {
            Box::new(AllQuery)
        } else {
            Box::new(BooleanQuery::new(subqueries))
        };

        // Determine sort field for numeric fast-field sorting
        let sort_field_name = match sort {
            SortOrder::FieldAsc(f) | SortOrder::FieldDesc(f) => Some(f.as_str()),
            SortOrder::Relevance => None,
        };

        // Check if the sort field is a numeric field
        let is_numeric_sort = sort_field_name
            .map(|name| {
                self.config
                    .schema
                    .fields
                    .iter()
                    .any(|fc| fc.name == name && fc.field_type == FieldType::Number)
            })
            .unwrap_or(false);

        // The Count collector rides along with TopDocs; it is nearly free for
        // scored queries (which already traverse the whole matching set) and
        // costs an extra pass only on broad filter-only browses.
        let need_count = want_count || include_pagination;

        // Execute query with appropriate collector
        let (docs, matched_total): (Vec<(f64, DocAddress)>, Option<usize>) = if is_numeric_sort {
            let field_name = sort_field_name.unwrap();
            let order = match sort {
                SortOrder::FieldAsc(_) => Order::Asc,
                _ => Order::Desc,
            };
            if need_count {
                let mut collectors = MultiCollector::new();
                let docs_handle = collectors.add_collector(
                    TopDocs::with_limit(limit)
                        .and_offset(offset)
                        .order_by_fast_field::<f64>(field_name, order),
                );
                let count_handle = collectors.add_collector(Count);
                let mut multi_fruit = searcher.search(&*query, &collectors)?;
                let total = count_handle.extract(&mut multi_fruit);
                let docs = docs_handle.extract(&mut multi_fruit).into_iter().collect();
                (docs, Some(total))
            } else {
                let collector = TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_fast_field::<f64>(field_name, order);
                let docs = searcher.search(&*query, &collector)?.into_iter().collect();
                (docs, None)
            }
        } else {
            if need_count {
                let mut collectors = MultiCollector::new();
                let docs_handle =
                    collectors.add_collector(TopDocs::with_limit(limit).and_offset(offset));
                let count_handle = collectors.add_collector(Count);
                let mut multi_fruit = searcher.search(&*query, &collectors)?;
                let total = count_handle.extract(&mut multi_fruit);
                let docs = docs_handle
                    .extract(&mut multi_fruit)
                    .into_iter()
                    .map(|(score, addr)| (score as f64, addr))
                    .collect();
                (docs, Some(total))
            } else {
                let collector = TopDocs::with_limit(limit).and_offset(offset);
                let docs = searcher
                    .search(&*query, &collector)?
                    .into_iter()
                    .map(|(score, addr)| (score as f64, addr))
                    .collect();
                (docs, None)
            }
        };

        // Build results
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(docs.len());

        for (score_or_val, doc_address) in &docs {
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
                            let num = val.and_then(|v| v.as_f64()).unwrap_or(0.0);
                            if num != 0.0 {
                                obj.insert(fc.name.clone(), serde_json::json!(num));
                            } else {
                                obj.insert(fc.name.clone(), serde_json::Value::Null);
                            }
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

        // For non-numeric sort on text fields, do post-sort
        if !is_numeric_sort {
            if let Some(field_name) = sort_field_name {
                let fname = field_name.to_string();
                match sort {
                    SortOrder::FieldAsc(_) => {
                        results.sort_by(|a, b| {
                            let va = a.get(&fname).and_then(|v| v.as_str()).unwrap_or("");
                            let vb = b.get(&fname).and_then(|v| v.as_str()).unwrap_or("");
                            va.cmp(vb)
                        });
                    }
                    SortOrder::FieldDesc(_) => {
                        results.sort_by(|a, b| {
                            let va = a.get(&fname).and_then(|v| v.as_str()).unwrap_or("");
                            let vb = b.get(&fname).and_then(|v| v.as_str()).unwrap_or("");
                            vb.cmp(va)
                        });
                    }
                    _ => {}
                }
            }
        }

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
                            let num = val.and_then(|v| v.as_f64()).unwrap_or(0.0);
                            if num != 0.0 {
                                obj.insert(fc.name.clone(), serde_json::json!(num));
                            } else {
                                obj.insert(fc.name.clone(), serde_json::Value::Null);
                            }
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
                if let Some(normalized) = canonicalize_filter_value(&field_config.field_type, value)
                {
                    let normalized = self.match_case(key, normalized);
                    let term = Term::from_field_text(field, &normalized);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, Box::new(term_query)));
                }
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
        Config, FieldConfig, FieldType, SchemaConfig, ServerConfig, SourceConfig, StoreConfig,
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
                index_path: dir.clone(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
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
                case_insensitive_fields: vec!["name".to_string()],
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
        // No metadata file at all — the pre-upgrade situation
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
                case_insensitive_fields: vec!["name".to_string()],
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

    fn test_config(dir: &Path) -> Arc<Config> {
        Arc::new(Config {
            server: ServerConfig {
                port: 8888,
                index_path: dir.to_path_buf(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
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
        })
    }

    /// Build an engine over an empty index with one field. Enough for anything
    /// that only reads the schema, without each caller repeating the setup.
    pub fn engine_with_field(dir: &std::path::Path, field: FieldConfig) -> SearchEngine {
        std::fs::create_dir_all(dir).unwrap();
        let config = Arc::new(Config {
            server: ServerConfig {
                port: 8888,
                index_path: dir.to_path_buf(),
                memory_budget: "100%".to_string(),
                auth_token: None,
                strict_params: false,
            },
            schema: SchemaConfig {
                fields: vec![field],
            },
            sources: Vec::new(),
            mappings: HashMap::new(),
            store: crate::config::StoreConfig::default(),
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
