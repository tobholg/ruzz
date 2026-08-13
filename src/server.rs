use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde_json::value::RawValue;
use sysinfo::System;
use tower_http::cors::CorsLayer;

use crate::search::{SearchEngine, StoreStatus};

const DEFAULT_SEARCH_LIMIT: usize = 20;
const MAX_SEARCH_LIMIT: usize = 1000;
/// Max results that may be hydrated inline with full=true
const MAX_FULL_LIMIT: usize = 100;
/// Max refs per /docs batch request
const MAX_DOC_BATCH: usize = 256;

pub struct AppState {
    pub engine: SearchEngine,
    pub started_at: Instant,
}

pub fn create_router(state: Arc<AppState>) -> Router {
    let auth_token = state.engine.config.server.auth_token.clone();

    let mut app = Router::new()
        .route("/", get(handle_dashboard))
        .route("/search", get(handle_search))
        .route("/lookup", get(handle_lookup))
        .route("/doc", get(handle_doc_resolve))
        .route("/doc/{doc_ref}", get(handle_doc_by_ref))
        .route("/docs", get(handle_docs_batch))
        .route("/fields", get(handle_fields))
        .route("/openapi.json", get(handle_openapi))
        .route("/api", get(handle_api_index))
        .route("/stats", get(handle_stats))
        .route("/health", get(handle_health))
        .with_state(state);

    if let Some(token) = auth_token {
        app = app.layer(middleware::from_fn(move |req, next| {
            let token = token.clone();
            auth_middleware(token, req, next)
        }));
    }

    app.layer(CorsLayer::permissive())
}

async fn auth_middleware(token: String, req: Request, next: Next) -> Response {
    // Skip auth for /health and / (dashboard handles auth via JS)
    let path = req.uri().path();
    if path == "/health" || path == "/" {
        return next.run(req).await;
    }

    // Check Authorization: Bearer <token> header
    if let Some(auth_header) = req.headers().get("authorization") {
        if let Ok(val) = auth_header.to_str() {
            if val.strip_prefix("Bearer ").map(|t| t.trim()) == Some(token.as_str()) {
                return next.run(req).await;
            }
        }
    }

    // Check ?token=<token> query param
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                if val == token {
                    return next.run(req).await;
                }
            }
        }
    }

    // Unauthorized
    let body = serde_json::json!({"error": "unauthorized", "message": "Provide Authorization: Bearer <token> header or ?token=<token> param"});
    let mut resp = axum::response::Json(body).into_response();
    *resp.status_mut() = axum::http::StatusCode::UNAUTHORIZED;
    resp
}

async fn handle_dashboard() -> axum::response::Html<&'static str> {
    crate::dashboard::dashboard_html()
}

#[derive(serde::Deserialize)]
struct SearchParams {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    include_pagination: Option<bool>,
    sort_by: Option<String>,    // field name to sort by
    sort_order: Option<String>, // "asc" or "desc"
    /// Hydrate each hit with its full document from the store as "_full"
    full: Option<bool>,
    /// Return `count` (matches for this search state). Default true; pass
    /// false to skip the count on broad filter-only browses.
    count: Option<bool>,
    #[serde(flatten)]
    extra: HashMap<String, String>,
}

async fn handle_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Json<serde_json::Value> {
    let query_text = params.q.unwrap_or_default();
    let limit = params
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let offset = params.offset.unwrap_or(0);
    let include_pagination = params.include_pagination.unwrap_or(false);
    let want_count = params.count.unwrap_or(true);
    let include_full = params.full.unwrap_or(false);

    if offset.saturating_add(limit) > crate::search::MAX_PAGINATION_WINDOW {
        return Json(serde_json::json!({
            "error": "pagination_window_too_large",
            "message": format!(
                "offset + limit must be <= {}",
                crate::search::MAX_PAGINATION_WINDOW
            ),
        }));
    }

    if include_full {
        if state.engine.store.is_none() {
            return Json(store_unavailable_body(&state.engine));
        }
        if limit > MAX_FULL_LIMIT {
            return Json(serde_json::json!({
                "error": "full_limit_too_large",
                "message": format!("full=true requires limit <= {}", MAX_FULL_LIMIT),
            }));
        }
    }

    let mut filters = params.extra.clone();
    filters.remove("limit");
    filters.remove("offset");
    filters.remove("include_pagination");
    filters.remove("sort_by");
    filters.remove("sort_order");
    filters.remove("full");
    filters.remove("count");

    let mut unknown_params: Vec<String> = Vec::new();
    let mut invalid_params: Vec<String> = Vec::new();

    // Extract range filters: keys ending in _min or _max
    let mut range_filters: Vec<crate::search::RangeFilter> = Vec::new();
    let range_keys: Vec<String> = filters
        .keys()
        .filter(|k| k.ends_with("_min") || k.ends_with("_max"))
        .cloned()
        .collect();

    // Collect unique field base names
    let mut range_fields: HashMap<String, (Option<f64>, Option<f64>)> = HashMap::new();
    for key in &range_keys {
        let value = filters.remove(key).unwrap_or_default();
        let Some(base) = key
            .strip_suffix("_min")
            .or_else(|| key.strip_suffix("_max"))
        else {
            continue;
        };
        // A range suffix on something that is not a numeric field is a typo,
        // not a filter — report the key the caller actually sent.
        if !state.engine.field_map.contains_key(base) {
            unknown_params.push(key.clone());
            continue;
        }
        let Ok(num) = value.parse::<f64>() else {
            invalid_params.push(format!("{}={}", key, value));
            continue;
        };
        let entry = range_fields.entry(base.to_string()).or_insert((None, None));
        if key.ends_with("_min") {
            entry.0 = Some(num);
        } else {
            entry.1 = Some(num);
        }
    }

    for (field, (min, max)) in range_fields {
        range_filters.push(crate::search::RangeFilter { field, min, max });
    }

    // Anything left in `filters` that is not a schema field is a typo or a
    // stale parameter. Previously these were dropped in silence, so a
    // misspelled filter quietly returned unfiltered results.
    let known = crate::params::KnownParams::build(&state.engine);
    for key in filters.keys() {
        if !known.contains(key) {
            unknown_params.push(key.clone());
        }
    }
    unknown_params.sort();
    invalid_params.sort();

    if (!unknown_params.is_empty() || !invalid_params.is_empty())
        && state.engine.config.server.strict_params
    {
        let suggestions: serde_json::Map<String, serde_json::Value> = unknown_params
            .iter()
            .filter_map(|p| {
                known
                    .suggest(p)
                    .map(|s| (p.clone(), serde_json::Value::String(s)))
            })
            .collect();
        let mut message = String::new();
        if !unknown_params.is_empty() {
            message.push_str(&format!(
                "Unknown query parameter(s): {}. ",
                unknown_params.join(", ")
            ));
        }
        if !invalid_params.is_empty() {
            message.push_str(&format!(
                "Invalid value(s): {}. ",
                invalid_params.join(", ")
            ));
        }
        message.push_str("See /fields for the full list.");
        return Json(serde_json::json!({
            "error": "invalid_parameters",
            "message": message,
            "unknown_parameters": unknown_params,
            "invalid_parameters": invalid_params,
            "did_you_mean": suggestions,
        }));
    }

    // Sort
    let sort = match (params.sort_by.as_deref(), params.sort_order.as_deref()) {
        (Some(field), Some("asc")) => crate::search::SortOrder::FieldAsc(field.to_string()),
        (Some(field), _) => crate::search::SortOrder::FieldDesc(field.to_string()), // default desc
        _ => crate::search::SortOrder::Relevance,
    };

    match state.engine.search(
        &query_text,
        &filters,
        &range_filters,
        &sort,
        limit,
        offset,
        include_pagination,
        want_count,
        include_full,
    ) {
        Ok(result) => {
            let mut value = serde_json::to_value(result).unwrap();
            let ignored: Vec<String> = unknown_params
                .iter()
                .chain(invalid_params.iter())
                .cloned()
                .collect();
            if !ignored.is_empty() {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("ignored_parameters".to_string(), serde_json::json!(ignored));
                }
            }
            Json(value)
        }
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(serde::Deserialize)]
struct LookupParams {
    full: Option<bool>,
    #[serde(flatten)]
    filters: HashMap<String, String>,
}

async fn handle_lookup(
    State(state): State<Arc<AppState>>,
    Query(mut params): Query<LookupParams>,
) -> Json<serde_json::Value> {
    let include_full = params.full.unwrap_or(false);
    params.filters.remove("full");
    if include_full && state.engine.store.is_none() {
        return Json(store_unavailable_body(&state.engine));
    }
    match state.engine.lookup(&params.filters, include_full) {
        Ok(result) => Json(serde_json::to_value(result).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

// -- document store endpoints -------------------------------------------------

fn store_unavailable_body(engine: &SearchEngine) -> serde_json::Value {
    let message = match &engine.store_status {
        StoreStatus::Disabled => {
            "document store is not enabled — set [store] enabled = true and re-import".to_string()
        }
        StoreStatus::Error(e) => format!("document store unavailable: {}", e),
        StoreStatus::Ok => "document store unavailable".to_string(),
    };
    serde_json::json!({ "error": "store_unavailable", "message": message })
}

fn store_unavailable(engine: &SearchEngine) -> Response {
    (StatusCode::CONFLICT, Json(store_unavailable_body(engine))).into_response()
}

/// Wrap raw stored JSON so it passes through to the response verbatim
/// (byte-for-byte, original key order) instead of being re-parsed.
fn raw_json(text: String) -> Box<RawValue> {
    RawValue::from_string(text.clone()).unwrap_or_else(|_| {
        // Stored bytes weren't valid JSON (possible in sidecar mode beyond the
        // validated first line) — degrade to a JSON string of the raw text.
        serde_json::value::to_raw_value(&text).expect("string is valid JSON")
    })
}

#[derive(serde::Serialize)]
struct DocResponse {
    took_ms: f64,
    #[serde(rename = "_ref")]
    doc_ref: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<usize>,
    full: Box<RawValue>,
}

fn round_ms(start: Instant) -> f64 {
    (start.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}

/// GET /doc/{ref} — one full document by ref
async fn handle_doc_by_ref(
    State(state): State<Arc<AppState>>,
    Path(doc_ref): Path<u64>,
) -> Response {
    if state.engine.store.is_none() {
        return store_unavailable(&state.engine);
    }
    let start = Instant::now();
    match state.engine.get_full(doc_ref) {
        Ok(Some(full)) => Json(DocResponse {
            took_ms: round_ms(start),
            doc_ref,
            matched: None,
            full: raw_json(full),
        })
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": format!("no document with ref {}", doc_ref),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /doc?field=value&... — exact-match resolve, then hydrate the first match
async fn handle_doc_resolve(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if state.engine.store.is_none() {
        return store_unavailable(&state.engine);
    }
    let mut filters = params;
    filters.remove("token");
    if filters.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing_filters",
                "message": "provide at least one exact-match filter, e.g. /doc?country_code=NO&org_number=923609016",
            })),
        )
            .into_response();
    }

    let start = Instant::now();
    match state.engine.resolve_full(&filters) {
        Ok((_matched, None)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": "no document matched the given filters",
            })),
        )
            .into_response(),
        Ok((matched, Some((doc_ref, full)))) => Json(DocResponse {
            took_ms: round_ms(start),
            doc_ref,
            matched: Some(matched),
            full: raw_json(full),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct DocsBatchParams {
    /// Absent means the caller wants the API documentation, not documents.
    /// `/docs?refs=…` keeps its original batch-fetch behaviour.
    refs: Option<String>,
}

#[derive(serde::Serialize)]
struct DocsBatchEntry {
    #[serde(rename = "_ref")]
    doc_ref: u64,
    full: Box<RawValue>,
}

/// GET /docs?refs=1,2,3 — batch fetch, order-preserving, null for missing refs
async fn handle_docs_batch(
    State(state): State<Arc<AppState>>,
    Query(params): Query<DocsBatchParams>,
) -> Response {
    let Some(refs_param) = params.refs else {
        // Bare /docs is where people look for documentation
        return handle_api_docs(State(state)).await;
    };
    if state.engine.store.is_none() {
        return store_unavailable(&state.engine);
    }

    let mut refs: Vec<u64> = Vec::new();
    for part in refs_param.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<u64>() {
            Ok(r) => refs.push(r),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_ref",
                        "message": format!("'{}' is not a valid ref", part),
                    })),
                )
                    .into_response();
            }
        }
    }
    if refs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing_refs",
                "message": "provide refs as a comma-separated list, e.g. /docs?refs=42,17",
            })),
        )
            .into_response();
    }
    if refs.len() > MAX_DOC_BATCH {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "batch_too_large",
                "message": format!("at most {} refs per request", MAX_DOC_BATCH),
            })),
        )
            .into_response();
    }

    let start = Instant::now();
    match state.engine.get_full_many(&refs) {
        Ok(fulls) => {
            let results: Vec<Option<DocsBatchEntry>> = refs
                .iter()
                .zip(fulls)
                .map(|(&doc_ref, full)| {
                    full.map(|f| DocsBatchEntry {
                        doc_ref,
                        full: raw_json(f),
                    })
                })
                .collect();
            let found = results.iter().filter(|r| r.is_some()).count();
            Json(serde_json::json!({
                "took_ms": round_ms(start),
                "total": found,
                "results": results,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Machine-readable parameter list — the answer to "what can I query?"
async fn handle_fields(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let params = crate::params::all_params(&state.engine);
    Json(serde_json::json!({
        "documents": state.engine.reader.searcher().num_docs(),
        "strict_params": state.engine.config.server.strict_params,
        "parameters": params,
        "docs_url": "/docs",
        "openapi_url": "/openapi.json",
    }))
}

/// Whole-API documentation as Markdown, generated from the live schema.
/// One fetch gives an LLM (or a human) every valid parameter.
async fn handle_api_docs(State(state): State<Arc<AppState>>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        crate::params::markdown_docs(&state.engine),
    )
        .into_response()
}

async fn handle_openapi(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(crate::params::openapi(&state.engine))
}

/// JSON index of the API, for clients that hit the root expecting one.
async fn handle_api_index(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let endpoints: Vec<serde_json::Value> = crate::params::endpoints(&state.engine)
        .into_iter()
        .map(|(route, description)| serde_json::json!({ "route": route, "description": description }))
        .collect();
    Json(serde_json::json!({
        "name": "ruzz",
        "version": env!("CARGO_PKG_VERSION"),
        "documents": state.engine.reader.searcher().num_docs(),
        "endpoints": endpoints,
        "docs_url": "/docs",
        "fields_url": "/fields",
        "openapi_url": "/openapi.json",
    }))
}

async fn handle_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let mut sys = System::new_all();
    sys.refresh_all();

    let pid = sysinfo::get_current_pid().ok();
    let process_info = pid.and_then(|p| sys.process(p));

    let process_memory = process_info.map(|p| p.memory()).unwrap_or(0);
    let process_virtual = process_info.map(|p| p.virtual_memory()).unwrap_or(0);

    let uptime_secs = state.started_at.elapsed().as_secs();
    let index_path = &state.engine.config.server.index_path;

    // Calculate index size on disk
    let index_size = dir_size(index_path).unwrap_or(0);

    // Count segments
    let segment_count = state
        .engine
        .index
        .searchable_segment_metas()
        .map(|s| s.len())
        .unwrap_or(0);

    let num_docs = state.engine.reader.searcher().num_docs();

    let store_stats = match state.engine.store.as_ref() {
        Some(store) => {
            let s = store.stats();
            serde_json::json!({
                "enabled": true,
                "status": "ok",
                "documents": s.doc_count,
                "blocks": s.block_count,
                "generation": s.generation,
                "source_mode": s.source_mode,
                "raw_bytes": s.raw_bytes,
                "raw_human": format_bytes(s.raw_bytes),
                "size_bytes": s.compressed_bytes,
                "size_human": format_bytes(s.compressed_bytes),
                "cache": {
                    "capacity_bytes": s.cache_capacity_bytes,
                    "used_bytes": s.cache_used_bytes,
                    "entries": s.cache_entries,
                    "hits": s.cache_hits,
                    "misses": s.cache_misses,
                },
            })
        }
        None => serde_json::json!({
            "enabled": state.engine.config.store.enabled,
            "status": state.engine.store_status.as_str(),
        }),
    };

    Json(serde_json::json!({
        "status": "online",
        "uptime_seconds": uptime_secs,
        "uptime_human": format_duration(uptime_secs),
        "documents": num_docs,
        "index": {
            "path": index_path.display().to_string(),
            "size_bytes": index_size,
            "size_human": format_bytes(index_size),
            "segments": segment_count,
        },
        "store": store_stats,
        "memory": {
            "rss_bytes": process_memory,
            "rss_human": format_bytes(process_memory),
            "virtual_bytes": process_virtual,
            "virtual_human": format_bytes(process_virtual),
            "budget": state.engine.config.server.memory_budget,
        },
        "system": {
            "total_memory_bytes": sys.total_memory(),
            "total_memory_human": format_bytes(sys.total_memory()),
            "available_memory_bytes": sys.available_memory(),
            "available_memory_human": format_bytes(sys.available_memory()),
            "cpu_count": sys.cpus().len(),
        },
            "schema": {
                "fields": state.engine.config.schema.fields.iter().map(|f| {
                    let metadata = state.engine.field_metadata.get(&f.name);
                    serde_json::json!({
                        "name": f.name,
                        "type": format!("{:?}", f.field_type).to_lowercase(),
                        "search": f.search.as_ref().map(|s| s.as_str()),
                        "values": metadata.map(|m| m.values.clone()).unwrap_or_default(),
                        "values_truncated": metadata.map(|m| m.truncated).unwrap_or(false),
                    })
                }).collect::<Vec<_>>(),
            },
    }))
}

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

fn dir_size(path: &std::path::Path) -> std::io::Result<u64> {
    let mut total = 0;
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                total += dir_size(&entry.path())?;
            }
        }
    }
    Ok(total)
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_duration(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if days >= 60 {
        let months = days / 30;
        let rem_days = days % 30;
        if rem_days > 0 {
            format!("{}mo {}d", months, rem_days)
        } else {
            format!("{}mo", months)
        }
    } else if days > 0 {
        format!("{}d {}h", days, hours % 24)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins, s)
    } else if mins > 0 {
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", s)
    }
}
