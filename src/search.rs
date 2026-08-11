use std::collections::HashMap;
use std::sync::Arc;

use tantivy::collector::{Count, MultiCollector, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RangeQuery, TermQuery};
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
    pub total: usize,
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

pub const MAX_REPORTED_TOTAL: usize = 100_000;

/// Open the document store and verify it belongs to this index: the
/// generation stamped into the index dir at import time must match the store
/// meta, and doc counts must line up.
fn open_store(config: &Config, reader: &IndexReader) -> anyhow::Result<StoreReader> {
    let store_path = config.store_path();
    let cache_bytes = store::parse_size(&config.store.cache).unwrap_or(0);
    let store = StoreReader::open(&store_path, cache_bytes)?;

    let pairing = store::read_index_pairing(&config.server.index_path)
        .ok_or_else(|| anyhow::anyhow!(
            "index has no store pairing file — re-run import with [store] enabled"
        ))?;
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
        })
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
                let values: Vec<String> = value
                    .split(',')
                    .filter_map(|s| canonicalize_filter_value(&field_config.field_type, s))
                    .collect();
                if values.len() == 1 {
                    let term = Term::from_field_text(field, &values[0]);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, Box::new(term_query)));
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
                    subqueries.push((Occur::Must, Box::new(BooleanQuery::new(or_clauses))));
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
                subqueries.push((Occur::Must, Box::new(range_query)));
            }
        }

        // Fuzzy search with trigrams
        if !query_text.is_empty() && !fuzzy_fields.is_empty() {
            let normalized = query_text.to_lowercase();
            let ngrams = generate_ngrams(&normalized, 3, 3);

            let mut ngram_queries: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for field in &fuzzy_fields {
                for ng in &ngrams {
                    let term = Term::from_field_text(*field, ng);
                    let tq = TermQuery::new(term, IndexRecordOption::WithFreqsAndPositions);
                    ngram_queries.push((Occur::Should, Box::new(tq)));
                }
            }

            if !ngram_queries.is_empty() {
                let ngram_bool = BooleanQuery::new(ngram_queries);
                subqueries.push((Occur::Must, Box::new(ngram_bool)));
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

        // Execute query with appropriate collector
        let (docs, matched_total): (Vec<(f64, DocAddress)>, Option<usize>) = if is_numeric_sort {
            let field_name = sort_field_name.unwrap();
            let order = match sort {
                SortOrder::FieldAsc(_) => Order::Asc,
                _ => Order::Desc,
            };
            if include_pagination {
                let mut collectors = MultiCollector::new();
                let docs_handle = collectors.add_collector(
                    TopDocs::with_limit(limit)
                        .and_offset(offset)
                        .order_by_fast_field::<f64>(field_name, order),
                );
                let count_handle = collectors.add_collector(Count);
                let mut multi_fruit = searcher.search(&*query, &collectors)?;
                let total = count_handle.extract(&mut multi_fruit);
                let docs = docs_handle
                    .extract(&mut multi_fruit)
                    .into_iter()
                    .map(|(val, addr)| (val, addr))
                    .collect();
                (docs, Some(total))
            } else {
                let collector = TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_fast_field::<f64>(field_name, order);
                let docs = searcher
                    .search(&*query, &collector)?
                    .into_iter()
                    .map(|(val, addr)| (val, addr))
                    .collect();
                (docs, None)
            }
        } else {
            if include_pagination {
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

        let total = results.len();
        let pagination = matched_total.map(|matched_total| {
            build_pagination_info(matched_total, limit, offset, results.len())
        });
        let took = start.elapsed().as_secs_f64() * 1000.0;

        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total,
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
        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total: results.len(),
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
    let total = matched_total.min(MAX_REPORTED_TOTAL);
    let total_relation = if matched_total > MAX_REPORTED_TOTAL {
        "gte"
    } else {
        "eq"
    };
    let has_more = matched_total > offset.saturating_add(returned);

    PaginationInfo {
        offset,
        limit,
        returned,
        total,
        total_relation,
        has_more,
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
mod tests {
    use super::{build_pagination_info, RangeFilter, SearchEngine, SortOrder, MAX_REPORTED_TOTAL};
    use crate::config::{
        Config, FieldConfig, FieldType, SchemaConfig, ServerConfig, SourceConfig, StoreConfig,
    };
    use crate::schema::build_schema;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tantivy::{doc, Index};

    #[test]
    fn builds_capped_pagination_metadata() {
        let pagination = build_pagination_info(MAX_REPORTED_TOTAL + 25, 1000, 99_000, 1000);
        assert_eq!(pagination.total, MAX_REPORTED_TOTAL);
        assert_eq!(pagination.total_relation, "gte");
        assert!(pagination.has_more);
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
            },
            schema: SchemaConfig {
                fields: vec![
                    FieldConfig {
                        name: "city".to_string(),
                        field_type: FieldType::Enum,
                        search: None,
                        values: None,
                        max_values: None,
                    },
                    FieldConfig {
                        name: "revenue".to_string(),
                        field_type: FieldType::Number,
                        search: None,
                        values: None,
                        max_values: None,
                    },
                    FieldConfig {
                        name: "name".to_string(),
                        field_type: FieldType::Keyword,
                        search: None,
                        values: None,
                        max_values: None,
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

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn test_index_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ruzz-{prefix}-{unique}"))
    }
}
