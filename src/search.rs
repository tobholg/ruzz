use std::collections::HashMap;
use std::sync::Arc;

use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{DocAddress, Index, IndexReader, Order, ReloadPolicy, Term};

use crate::config::{Config, FieldType, SearchMode};
use crate::field_meta::{
    canonicalize_filter_value, json_boolean_value, load_stored_field_metadata,
    runtime_metadata_for_field, RuntimeFieldMetadata,
};

pub struct SearchEngine {
    pub index: Index,
    pub reader: IndexReader,
    pub schema: Schema,
    pub field_map: HashMap<String, Field>,
    pub field_configs: HashMap<String, crate::config::FieldConfig>,
    pub field_metadata: HashMap<String, RuntimeFieldMetadata>,
    pub config: Arc<Config>,
}

#[derive(serde::Serialize)]
pub struct SearchResult {
    pub took_ms: f64,
    pub total: usize,
    pub results: Vec<serde_json::Value>,
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

        Ok(Self { index, reader, schema, field_map, field_configs, field_metadata, config })
    }

    pub fn search(
        &self,
        query_text: &str,
        filters: &HashMap<String, String>,
        range_filters: &[RangeFilter],
        sort: &SortOrder,
        limit: usize,
    ) -> anyhow::Result<SearchResult> {
        let start = std::time::Instant::now();
        let searcher = self.reader.searcher();

        let fuzzy_fields: Vec<Field> = self.config.schema.fields.iter()
            .filter(|fc| fc.search == Some(SearchMode::Fuzzy))
            .filter_map(|fc| self.field_map.get(&fc.name).copied())
            .collect();

        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        // Exact filters FIRST as MUST clauses
        for (key, value) in filters {
            if let (Some(&field), Some(field_config)) = (self.field_map.get(key), self.field_configs.get(key)) {
                let values: Vec<String> = value.split(',')
                    .filter_map(|s| canonicalize_filter_value(&field_config.field_type, s))
                    .collect();
                if values.len() == 1 {
                    let term = Term::from_field_text(field, &values[0]);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, Box::new(term_query)));
                } else if values.len() > 1 {
                    let or_clauses: Vec<(Occur, Box<dyn Query>)> = values.iter()
                        .map(|v| {
                            let term = Term::from_field_text(field, v);
                            let tq: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
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
        let is_numeric_sort = sort_field_name.map(|name| {
            self.config.schema.fields.iter().any(|fc| fc.name == name && fc.field_type == FieldType::Number)
        }).unwrap_or(false);

        // Execute query with appropriate collector
        let docs: Vec<(f64, DocAddress)> = if is_numeric_sort {
            let field_name = sort_field_name.unwrap();
            let order = match sort {
                SortOrder::FieldAsc(_) => Order::Asc,
                _ => Order::Desc,
            };
            let collector = TopDocs::with_limit(limit).order_by_fast_field::<f64>(field_name, order);
            searcher.search(&*query, &collector)?
                .into_iter()
                .map(|(val, addr)| (val, addr))
                .collect()
        } else {
            let collector = TopDocs::with_limit(limit);
            searcher.search(&*query, &collector)?
                .into_iter()
                .map(|(score, addr)| (score as f64, addr))
                .collect()
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
                            obj.insert(fc.name.clone(), serde_json::Value::String(text.to_string()));
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
        let took = start.elapsed().as_secs_f64() * 1000.0;

        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total,
            results,
        })
    }

    pub fn lookup(
        &self,
        filters: &HashMap<String, String>,
    ) -> anyhow::Result<SearchResult> {
        let start = std::time::Instant::now();
        let searcher = self.reader.searcher();

        let mut subqueries: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for (key, value) in filters {
            if let (Some(&field), Some(field_config)) = (self.field_map.get(key), self.field_configs.get(key)) {
                if let Some(normalized) = canonicalize_filter_value(&field_config.field_type, value) {
                    let term = Term::from_field_text(field, &normalized);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, Box::new(term_query)));
                }
            }
        }

        let query = BooleanQuery::new(subqueries);
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
                            obj.insert(fc.name.clone(), serde_json::Value::String(text.to_string()));
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
            results.push(serde_json::Value::Object(obj));
        }

        let took = start.elapsed().as_secs_f64() * 1000.0;
        Ok(SearchResult {
            took_ms: (took * 100.0).round() / 100.0,
            total: results.len(),
            results,
        })
    }
}

fn generate_ngrams(text: &str, min_n: usize, max_n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut ngrams = Vec::new();
    for n in min_n..=max_n {
        if chars.len() < n { continue; }
        for i in 0..=(chars.len() - n) {
            let ng: String = chars[i..i + n].iter().collect();
            ngrams.push(ng);
        }
    }
    ngrams
}
