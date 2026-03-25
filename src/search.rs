use std::collections::HashMap;
use std::sync::Arc;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{Index, IndexReader, ReloadPolicy, Term};

use crate::config::{Config, FieldType, SearchMode};

pub struct SearchEngine {
    pub index: Index,
    pub reader: IndexReader,
    pub schema: Schema,
    pub field_map: HashMap<String, Field>,
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
        for fc in &config.schema.fields {
            if let Ok(field) = schema.get_field(&fc.name) {
                field_map.insert(fc.name.clone(), field);
            }
        }

        Ok(Self { index, reader, schema, field_map, config })
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

        // Exact filters FIRST as MUST clauses — narrows candidate set before fuzzy
        for (key, value) in filters {
            if let Some(&field) = self.field_map.get(key) {
                // Support comma-separated values: country_code=NO,SE,DK
                let values: Vec<&str> = value.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                if values.len() == 1 {
                    let term = Term::from_field_text(field, values[0]);
                    let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                    subqueries.push((Occur::Must, Box::new(term_query)));
                } else if values.len() > 1 {
                    // OR across multiple values
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

        // Fuzzy search with trigrams only (faster than 2-4 gram range)
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

        if subqueries.is_empty() {
            let took = start.elapsed().as_secs_f64() * 1000.0;
            return Ok(SearchResult { took_ms: (took * 100.0).round() / 100.0, total: 0, results: vec![] });
        }

        let query = BooleanQuery::new(subqueries);

        // Over-fetch when we need post-filtering or re-ranking
        let has_range = !range_filters.is_empty();
        let has_sort = !matches!(sort, SortOrder::Relevance);
        let fetch_limit = if has_range || has_sort { (limit * 10).max(200) } else { limit };

        let top_docs = searcher.search(&query, &TopDocs::with_limit(fetch_limit))?;

        let mut results: Vec<serde_json::Value> = Vec::with_capacity(top_docs.len());

        for (score, doc_address) in &top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(*doc_address)?;
            let mut obj = serde_json::Map::new();

            for fc in &self.config.schema.fields {
                if let Ok(field) = self.schema.get_field(&fc.name) {
                    if let Some(value) = doc.get_first(field) {
                        if let Some(text) = value.as_str() {
                            obj.insert(fc.name.clone(), serde_json::Value::String(text.to_string()));
                        }
                    }
                }
            }

            obj.insert("_score".to_string(), serde_json::json!(score));
            results.push(serde_json::Value::Object(obj));
        }

        // Apply range filters
        if has_range {
            results.retain(|r| {
                for rf in range_filters {
                    let val = r.get(&rf.field)
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok());

                    match val {
                        Some(v) => {
                            if let Some(min) = rf.min {
                                if v < min { return false; }
                            }
                            if let Some(max) = rf.max {
                                if v > max { return false; }
                            }
                        }
                        None => {
                            // If field is empty/missing and we have a min filter, exclude it
                            if rf.min.is_some() { return false; }
                        }
                    }
                }
                true
            });
        }

        // Apply sort
        match sort {
            SortOrder::Relevance => {} // already sorted by score
            SortOrder::FieldAsc(field_name) => {
                results.sort_by(|a, b| {
                    let va = a.get(field_name).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::MIN);
                    let vb = b.get(field_name).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::MIN);
                    va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortOrder::FieldDesc(field_name) => {
                results.sort_by(|a, b| {
                    let va = a.get(field_name).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::MIN);
                    let vb = b.get(field_name).and_then(|v| v.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::MIN);
                    vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }

        // Trim to requested limit
        results.truncate(limit);
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
            if let Some(&field) = self.field_map.get(key) {
                let term = Term::from_field_text(field, value);
                let term_query = TermQuery::new(term, IndexRecordOption::Basic);
                subqueries.push((Occur::Must, Box::new(term_query)));
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
                    if let Some(value) = doc.get_first(field) {
                        if let Some(text) = value.as_str() {
                            obj.insert(fc.name.clone(), serde_json::Value::String(text.to_string()));
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
