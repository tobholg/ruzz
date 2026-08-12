//! The API's parameter surface, derived from the configured schema.
//!
//! One source of truth for three things: validating incoming query strings,
//! suggesting corrections for typos, and generating `/fields`, `/docs` and
//! `/openapi.json`. Anything accepted by `/search` appears here.

use crate::config::{FieldType, SearchMode};
use crate::search::SearchEngine;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ParamSpec {
    pub name: String,
    /// "control" (paging, sorting, output) or "filter" / "range"
    pub kind: &'static str,
    #[serde(rename = "type")]
    pub value_type: &'static str,
    pub description: String,
    /// Comma-separated values are OR'ed together
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub multi_value: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sortable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
}

fn control(name: &str, value_type: &'static str, description: &str, example: Option<&str>) -> ParamSpec {
    ParamSpec {
        name: name.to_string(),
        kind: "control",
        value_type,
        description: description.to_string(),
        multi_value: false,
        sortable: false,
        values: None,
        example: example.map(|s| s.to_string()),
    }
}

/// Control parameters that exist regardless of schema.
pub fn control_params(store_enabled: bool) -> Vec<ParamSpec> {
    let mut specs = vec![
        control("q", "string", "Fuzzy full-text query across all fuzzy-search fields. Typo tolerant.", Some("q=amazn")),
        control("limit", "integer", "Rows to return (1-1000, default 20).", Some("limit=50")),
        control("offset", "integer", "Rows to skip for paging. offset + limit must be <= 100000.", Some("offset=100")),
        control("sort_by", "string", "Field to sort by. Numeric fields sort natively; omit for relevance order.", Some("sort_by=revenue")),
        control("sort_order", "string", "\"asc\" or \"desc\" (default desc). Only meaningful with sort_by.", Some("sort_order=desc")),
        control("count", "boolean", "Include `count`, the exact number of matches for this search state (default true). Pass false to skip it on broad filter-only browses.", Some("count=false")),
        control("include_pagination", "boolean", "Also emit the legacy `pagination` object (default false).", None),
    ];
    if store_enabled {
        specs.push(control(
            "full",
            "boolean",
            "Attach each hit's full document from the document store as `_full` (requires limit <= 100).",
            Some("full=true"),
        ));
    }
    specs
}

/// Every parameter this server accepts, control params first.
pub fn all_params(engine: &SearchEngine) -> Vec<ParamSpec> {
    let mut specs = control_params(engine.store.is_some());

    for fc in &engine.config.schema.fields {
        let value_type = match fc.field_type {
            FieldType::Text => "text",
            FieldType::Keyword => "keyword",
            FieldType::Number => "number",
            FieldType::Enum => "enum",
            FieldType::Boolean => "boolean",
        };
        let values = engine
            .field_metadata
            .get(&fc.name)
            .filter(|m| !m.values.is_empty())
            .map(|m| m.values.clone());

        let description = match fc.field_type {
            FieldType::Number => "Numeric field. Exact match, or use the _min / _max range parameters.".to_string(),
            FieldType::Boolean => "Boolean filter. Accepts true/false, yes/no, 1/0.".to_string(),
            FieldType::Enum => "Exact-match filter over a fixed value set. Comma-separated values are OR'ed.".to_string(),
            FieldType::Keyword => "Exact-match filter. Comma-separated values are OR'ed.".to_string(),
            FieldType::Text => {
                if fc.search == Some(SearchMode::Fuzzy) {
                    "Searched by the `q` parameter (fuzzy). Not an exact-match filter.".to_string()
                } else {
                    "Text field.".to_string()
                }
            }
        };

        specs.push(ParamSpec {
            name: fc.name.clone(),
            kind: "filter",
            value_type,
            description,
            multi_value: !matches!(fc.field_type, FieldType::Text),
            sortable: true,
            values,
            example: None,
        });

        if fc.field_type == FieldType::Number {
            for (suffix, word) in [("_min", "Lower"), ("_max", "Upper")] {
                specs.push(ParamSpec {
                    name: format!("{}{}", fc.name, suffix),
                    kind: "range",
                    value_type: "number",
                    description: format!("{} bound (inclusive) for `{}`.", word, fc.name),
                    multi_value: false,
                    sortable: false,
                    values: None,
                    example: None,
                });
            }
        }
    }

    specs
}

/// Parameters accepted but not part of the documented surface.
const IMPLICIT_PARAMS: &[&str] = &["token"];

/// Every accepted parameter name, built once per request.
pub struct KnownParams {
    names: Vec<String>,
}

impl KnownParams {
    pub fn build(engine: &SearchEngine) -> Self {
        let mut names: Vec<String> = all_params(engine).into_iter().map(|p| p.name).collect();
        names.extend(IMPLICIT_PARAMS.iter().map(|s| s.to_string()));
        Self { names }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    /// Closest known parameter to `unknown`, when one is near enough to be a
    /// plausible typo rather than a different word.
    pub fn suggest(&self, unknown: &str) -> Option<String> {
        let target = unknown.to_lowercase();
        // Roughly one edit per four characters, capped — "registerd_town"
        // suggests "registered_town" while "colour" suggests nothing.
        let budget = (target.len() / 4).clamp(1, 3);
        self.names
            .iter()
            .map(|name| (levenshtein(&target, &name.to_lowercase()), name))
            .filter(|(distance, _)| *distance <= budget)
            .min_by_key(|(distance, name)| (*distance, name.len()))
            .map(|(_, name)| name.clone())
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::levenshtein;

    #[test]
    fn measures_edit_distance() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("registerd_town", "registered_town"), 1);
        assert_eq!(levenshtein("revenu_min", "revenue_min"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
