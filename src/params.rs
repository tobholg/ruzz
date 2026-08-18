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

fn control(
    name: &str,
    value_type: &'static str,
    description: &str,
    example: Option<&str>,
) -> ParamSpec {
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
            FieldType::Text if fc.search == Some(SearchMode::Substring) => "substring",
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
            FieldType::Text => match fc.search {
                Some(SearchMode::Fuzzy) => {
                    "Searched by the `q` parameter (fuzzy). Not an exact-match filter.".to_string()
                }
                Some(SearchMode::Substring) => {
                    "Case-insensitive substring search — matches anywhere in the value. Needs at least 3 characters. Not included in `q`.".to_string()
                }
                None => "Text field.".to_string(),
            },
        };

        let description = if fc.multi {
            format!(
                "{description} A document may hold several values; a filter matches any of them."
            )
        } else {
            description
        };
        // The schema author's own note comes first: it says what the field
        // means, which matters more than how it is matched.
        let description = match fc.description.as_deref() {
            Some(note) if !note.trim().is_empty() => format!("{} {description}", note.trim()),
            _ => description,
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
                    // A threshold is typed here, so the unit has to be here too.
                    description: match fc.description.as_deref() {
                        Some(note) if !note.trim().is_empty() => format!(
                            "{} bound (inclusive) for `{}`. {}",
                            word,
                            fc.name,
                            note.trim()
                        ),
                        _ => format!("{} bound (inclusive) for `{}`.", word, fc.name),
                    },
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

// ---------------------------------------------------------------------------
// Generated documentation
// ---------------------------------------------------------------------------

/// Endpoints this server exposes, given its configuration.
pub fn endpoints(engine: &SearchEngine) -> Vec<(&'static str, &'static str)> {
    let mut list = vec![
        (
            "GET /search",
            "Fuzzy search with filters, ranges, sorting and paging.",
        ),
        (
            "GET /lookup",
            "Exact-match lookup returning at most one row.",
        ),
        (
            "POST /resolve",
            "Bulk exact resolution, max 250 items. Each item reports matched, not_found or ambiguous — never a best guess.",
        ),
        (
            "POST /match",
            "Bulk fuzzy matching, max 25 items, returning ranked candidates per item for the caller to choose between.",
        ),
        (
            "GET /fields",
            "Machine-readable list of every accepted query parameter.",
        ),
        (
            "GET /docs",
            "This documentation, as Markdown (no parameters).",
        ),
        ("GET /openapi.json", "OpenAPI 3.1 description of the API."),
        (
            "GET /stats",
            "Runtime stats: documents, index size, memory, schema.",
        ),
        ("GET /health", "Liveness probe."),
    ];
    if engine.store.is_some() {
        list.insert(2, ("GET /doc/{ref}", "One full document by its `_ref`."));
        list.insert(
            3,
            (
                "GET /docs?refs=1,2,3",
                "Batch full-document fetch, max 256 refs.",
            ),
        );
        list.insert(
            4,
            (
                "GET /doc?field=value",
                "Resolve exact filters to one full document.",
            ),
        );
    }
    list
}

/// Single-page Markdown reference generated from the live schema — intended
/// to be fetched whole by an LLM or pasted into a client's docs.
pub fn markdown_docs(engine: &SearchEngine) -> String {
    let params = all_params(engine);
    let mut out = String::with_capacity(8192);

    out.push_str("# Search API\n\n");
    out.push_str(&format!(
        "{} documents. Every response is JSON. All parameters are optional; \
combining them narrows the result set (AND), while comma-separated values \
within one parameter widen it (OR).\n\n",
        engine.reader.searcher().num_docs()
    ));

    out.push_str("## Endpoints\n\n");
    for (route, description) in endpoints(engine) {
        out.push_str(&format!("- `{}` — {}\n", route, description));
    }

    out.push_str("\n## Bulk endpoints\n\n");
    out.push_str("`POST /resolve` joins exact keys to records — the bulk lookup an importer needs. Up to 250 items, each with your own `id` echoed back.\n\n");
    out.push_str("```json\n{\n  \"items\": [\n    {\"id\": \"row-1\", \"filters\": {\"org_number\": \"923609016\", \"country_code\": \"NO\"}},\n    {\"id\": \"row-2\", \"filters\": {\"org_number\": \"982463718\", \"country_code\": \"NO\"}}\n  ],\n  \"full\": true\n}\n```\n\n");
    out.push_str("Every item comes back with an explicit `status`, never a best guess:\n\n");
    out.push_str("- `matched` — exactly one record, in `document`\n");
    out.push_str("- `not_found` — nothing matched; create a record or flag the row\n");
    out.push_str("- `ambiguous` — more than one matched. `count` is the true number and `candidates` holds the first few. The key is not unique in this data, so choosing for you would attach your row to an arbitrary record\n");
    out.push_str("- `invalid` — the item could not identify anything: an unknown field, no filters, or a blank value. A blank value matches every document, so it is refused rather than resolved\n\n");
    out.push_str("`POST /match` is the fuzzy sibling for names, capped at 25 items because each one is a separate ranked query costing roughly a hundred times more than an exact key. It returns ranked `candidates` per item and never decides between them.\n\n");
    out.push_str("```json\n{\n  \"items\": [{\"id\": \"row-1\", \"q\": \"Equinor\", \"filters\": {\"country_code\": \"NO\"}}],\n  \"candidates\": 5\n}\n```\n\n");
    out.push_str("Do not store `_ref` as a foreign key: refs are ordinal and change on every re-import. Key on your own identifier and re-resolve.\n\n");
    out.push_str("\n## Response shape\n\n");
    out.push_str("```json\n{\n  \"took_ms\": 4.1,\n  \"count\": 44678,\n  \"returned\": 20,\n  \"offset\": 0,\n  \"limit\": 20,\n  \"has_more\": true,\n  \"results\": []\n}\n```\n\n");
    out.push_str("- `count` — documents matching the current search state, exact and uncapped. Pass `count=false` to skip computing it.\n");
    out.push_str("- `returned` — rows in this response.\n");
    out.push_str(
        "- `total` — deprecated alias of `returned`; use `count` for the number of matches.\n",
    );
    out.push_str(
        "- `_score` on each row — relevance, driven by `q` only. Filters never affect ranking.\n\n",
    );

    let section = |title: &str, kind: &str, out: &mut String| {
        let rows: Vec<&ParamSpec> = params.iter().filter(|p| p.kind == kind).collect();
        if rows.is_empty() {
            return;
        }
        out.push_str(&format!("## {}\n\n", title));
        out.push_str("| Parameter | Type | Multi-value | Description |\n|---|---|---|---|\n");
        for p in rows {
            let values = match &p.values {
                Some(v) if v.len() <= 12 => format!(" Values: {}.", v.join(", ")),
                Some(v) => format!(" {} distinct values, see /fields.", v.len()),
                None => String::new(),
            };
            out.push_str(&format!(
                "| `{}` | {} | {} | {}{} |\n",
                p.name,
                p.value_type,
                if p.multi_value { "yes" } else { "no" },
                p.description.replace('|', "\\|"),
                values
            ));
        }
        out.push('\n');
    };
    section("Control parameters", "control", &mut out);
    section("Filters", "filter", &mut out);
    section("Numeric ranges", "range", &mut out);

    out.push_str("## Examples\n\n```\n");
    let first_filter = params
        .iter()
        .find(|p| p.kind == "filter" && p.multi_value && p.values.is_some())
        .or_else(|| params.iter().find(|p| p.kind == "filter" && p.multi_value));
    out.push_str("/search?q=amazn&limit=10\n");
    if let Some(f) = first_filter {
        let example_value = f
            .values
            .as_ref()
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| "VALUE".to_string());
        out.push_str(&format!(
            "/search?{}={}                 # exact filter\n",
            f.name, example_value
        ));
        let second_value = f
            .values
            .as_ref()
            .and_then(|v| v.get(1))
            .cloned()
            .unwrap_or_else(|| "OTHER".to_string());
        out.push_str(&format!(
            "/search?{}={},{}           # OR between values\n",
            f.name, example_value, second_value
        ));
    }
    if let Some(r) = params.iter().find(|p| p.kind == "range") {
        let base = r.name.trim_end_matches("_min").trim_end_matches("_max");
        out.push_str(&format!(
            "/search?{}_min=1000&{}_max=5000  # inclusive numeric range\n",
            base, base
        ));
        out.push_str(&format!("/search?sort_by={}&sort_order=desc\n", base));
    }
    out.push_str("```\n\n");
    out.push_str("Unknown parameters are reported in `ignored_parameters`, or rejected outright when the server runs with `strict_params`.\n");
    out
}

/// OpenAPI 3.1 description of /search, generated from the same specs.
pub fn openapi(engine: &SearchEngine) -> serde_json::Value {
    let parameters: Vec<serde_json::Value> = all_params(engine)
        .iter()
        .map(|p| {
            let mut schema = serde_json::Map::new();
            schema.insert(
                "type".to_string(),
                serde_json::json!(match p.value_type {
                    "integer" => "integer",
                    "number" => "number",
                    "boolean" => "boolean",
                    _ => "string",
                }),
            );
            if let Some(values) = &p.values {
                schema.insert("enum".to_string(), serde_json::json!(values));
            }
            serde_json::json!({
                "name": p.name,
                "in": "query",
                "required": false,
                "description": p.description,
                "schema": serde_json::Value::Object(schema),
            })
        })
        .collect();

    serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "ruzz search API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": format!("Fuzzy search over {} documents.", engine.reader.searcher().num_docs()),
        },
        "paths": {
            "/search": {
                "get": {
                    "summary": "Fuzzy search with filters, ranges, sorting and paging",
                    "parameters": parameters,
                    "responses": {
                        "200": {
                            "description": "Matching rows",
                            "content": { "application/json": { "schema": {
                                "type": "object",
                                "properties": {
                                    "took_ms": { "type": "number" },
                                    "count": { "type": "integer", "description": "Exact number of matches for this search state" },
                                    "returned": { "type": "integer" },
                                    "offset": { "type": "integer" },
                                    "limit": { "type": "integer" },
                                    "has_more": { "type": "boolean" },
                                    "results": { "type": "array", "items": { "type": "object" } }
                                }
                            }}}
                        },
                        "400": { "description": "Unknown or invalid parameters (strict_params mode)" }
                    }
                }
            },
            "/fields": { "get": { "summary": "List every accepted query parameter" } },
            "/docs": { "get": { "summary": "Markdown documentation for this API" } },
            "/stats": { "get": { "summary": "Runtime stats and schema" } },
            "/health": { "get": { "summary": "Liveness probe" } }
        }
    })
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

    /// A schema note has to reach the range parameters, not just the field:
    /// `revenue_min=100000000` is where someone gets the unit wrong, and the
    /// engine has no way to know the figures arrived in thousands.
    #[test]
    fn schema_notes_reach_the_field_and_its_range_bounds() {
        let dir = std::env::temp_dir().join(format!(
            "ruzz-schema-note-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = crate::search::tests::engine_with_field(
            &dir,
            crate::config::FieldConfig {
                name: "revenue".to_string(),
                field_type: crate::config::FieldType::Number,
                search: None,
                values: None,
                max_values: None,
                case_sensitive: false,
                multi: false,
                separator: None,
                description: Some("Thousands of NOK.".to_string()),
            },
        );

        let specs = super::all_params(&engine);
        for name in ["revenue", "revenue_min", "revenue_max"] {
            let spec = specs
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(
                spec.description.contains("Thousands of NOK."),
                "{name} lost the schema note: {}",
                spec.description
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
