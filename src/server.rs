use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
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
/// Max items per /resolve batch. Exact keys resolve in ~0.1ms each, so this
/// is bounded by response size rather than by query cost.
const MAX_RESOLVE_BATCH: usize = 250;
/// Max items per /match batch. Each item is a separate ranked query costing
/// ~15ms on real company names — two orders of magnitude more than an exact
/// key — so this limit is much lower and is about CPU, not bytes.
const MAX_MATCH_BATCH: usize = 25;
/// Longest query string accepted per /match item. Fuzzy cost scales with the
/// number of trigrams, so a limit on item count alone does not bound work.
const MAX_MATCH_QUERY_LEN: usize = 200;
/// Candidates returned for an ambiguous exact match, or per /match item.
const DEFAULT_CANDIDATES: usize = 5;
const MAX_CANDIDATES: usize = 25;

/// Cached /search responses before the cache resets. Entries are result
/// pages (full=true is never cached), so worst case is modest.
const SEARCH_CACHE_CAP: usize = 512;

pub struct AppState {
    pub engine: SearchEngine,
    pub started_at: Instant,
    /// Query counters and the resource-sample ring behind /activity's
    /// search-load and resources views. Filled by `resource_sampler`.
    pub metrics: crate::metrics::Metrics,
    /// Replayed /search responses — valid for the process lifetime because
    /// the index is: imports swap directories, and a running server only
    /// sees the new one on restart.
    search_cache: std::sync::Mutex<HashMap<String, serde_json::Value>>,
    /// Caps concurrent query threads at the core count. Excess requests
    /// queue on the semaphore instead of spawning ever more blocking
    /// threads that fight over the same CPUs.
    query_permits: tokio::sync::Semaphore,
}

impl AppState {
    pub fn new(engine: SearchEngine) -> Self {
        let permits = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            engine,
            started_at: Instant::now(),
            metrics: crate::metrics::Metrics::default(),
            search_cache: std::sync::Mutex::new(HashMap::new()),
            query_permits: tokio::sync::Semaphore::new(permits),
        }
    }
}

/// Run CPU-bound engine work off the async runtime.
///
/// Handlers used to call the engine inline, so a handful of slow queries
/// stalled every in-flight request — including /health, which makes a load
/// balancer read a busy server as a dead one. Anything that traverses the
/// index, decompresses the store, or scans the filesystem belongs in here;
/// the async threads only parse requests and serialize responses.
async fn run_blocking<T, F>(state: &Arc<AppState>, f: F) -> anyhow::Result<T>
where
    F: FnOnce(&AppState) -> T + Send + 'static,
    T: Send + 'static,
{
    let _permit = state
        .query_permits
        .acquire()
        .await
        .expect("query semaphore is never closed");
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || f(&state))
        .await
        .map_err(|e| anyhow::anyhow!("query task failed: {}", e))
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
        .route("/resolve", post(handle_resolve))
        .route("/match", post(handle_match))
        .route("/fields", get(handle_fields))
        .route("/openapi.json", get(handle_openapi))
        .route("/api", get(handle_api_index))
        .route("/stats", get(handle_stats))
        .route("/activity", get(handle_activity))
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

/// Constant-time byte equality: the comparison must not leak how much of
/// the token matched. A length mismatch returns early — length is not the
/// secret.
fn token_matches(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
            if let Some(provided) = val.strip_prefix("Bearer ") {
                if token_matches(provided.trim(), &token) {
                    return next.run(req).await;
                }
            }
        }
    }

    // Check ?token=<token> query param
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("token=") {
                if token_matches(val, &token) {
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
) -> Response {
    let started = Instant::now();
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
        return bad_request(
            "pagination_window_too_large",
            format!(
                "offset + limit must be <= {}",
                crate::search::MAX_PAGINATION_WINDOW
            ),
        );
    }

    if include_full {
        if state.engine.store.is_none() {
            return store_unavailable(&state.engine);
        }
        if limit > MAX_FULL_LIMIT {
            return bad_request(
                "full_limit_too_large",
                format!("full=true requires limit <= {}", MAX_FULL_LIMIT),
            );
        }
    }

    // Repeated identical requests are the norm for a dashboard (every
    // keystroke re-fires, tabs re-open, several people watch one deploy).
    // The index is immutable for the life of the process — imports build
    // into a staging directory and swap, which a running server only picks
    // up on restart — so a response can be replayed verbatim. Keyed on the
    // raw request inputs, before any parsing, so every derived behavior
    // (ignored parameters included) is covered by the key.
    let cache_key = if include_full || limit > 100 {
        None // bound entry size: no full documents, no thousand-row pages
    } else {
        let mut extra: Vec<(&String, &String)> = params.extra.iter().collect();
        extra.sort();
        Some(format!(
            "{}\x01{:?}\x01{:?}\x01{}\x01{}\x01{}\x01{}\x01{:?}",
            query_text,
            params.sort_by,
            params.sort_order,
            limit,
            offset,
            include_pagination,
            want_count,
            extra
        ))
    };
    if let Some(key) = &cache_key {
        if let Some(mut value) = state.search_cache.lock().unwrap().get(key).cloned() {
            if let Some(obj) = value.as_object_mut() {
                let took = (started.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0;
                obj.insert("took_ms".to_string(), serde_json::json!(took));
                obj.insert("cached".to_string(), serde_json::json!(true));
            }
            state
                .metrics
                .record_query(started.elapsed().as_secs_f64() * 1000.0);
            return Json(value).into_response();
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
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_parameters",
                "message": message,
                "unknown_parameters": unknown_params,
                "invalid_parameters": invalid_params,
                "did_you_mean": suggestions,
            })),
        )
            .into_response();
    }

    // Sort
    let sort = match (params.sort_by.as_deref(), params.sort_order.as_deref()) {
        (Some(field), Some("asc")) => crate::search::SortOrder::FieldAsc(field.to_string()),
        (Some(field), _) => crate::search::SortOrder::FieldDesc(field.to_string()), // default desc
        _ => crate::search::SortOrder::Relevance,
    };

    let outcome = run_blocking(&state, move |app| {
        app.engine.search(
            &query_text,
            &filters,
            &range_filters,
            &sort,
            limit,
            offset,
            include_pagination,
            want_count,
            include_full,
        )
    })
    .await
    .and_then(|result| result);

    match outcome {
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
            if let Some(key) = cache_key {
                let mut cache = state.search_cache.lock().unwrap();
                // Crude but sufficient eviction: reset when full. Entries
                // are pages of at most 1000 rows without full documents.
                if cache.len() >= SEARCH_CACHE_CAP {
                    cache.clear();
                }
                cache.insert(key, value.clone());
            }
            state
                .metrics
                .record_query(started.elapsed().as_secs_f64() * 1000.0);
            Json(value).into_response()
        }
        Err(e) => engine_error(e),
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
) -> Response {
    let include_full = params.full.unwrap_or(false);
    params.filters.remove("full");
    if include_full && state.engine.store.is_none() {
        return store_unavailable(&state.engine);
    }
    let filters = params.filters;
    let outcome = run_blocking(&state, move |app| app.engine.lookup(&filters, include_full))
        .await
        .and_then(|result| result);
    match outcome {
        Ok(result) => Json(serde_json::to_value(result).unwrap()).into_response(),
        Err(e) => engine_error(e),
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
    let outcome = run_blocking(&state, move |app| app.engine.get_full(doc_ref))
        .await
        .and_then(|result| result);
    match outcome {
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
    let outcome = run_blocking(&state, move |app| app.engine.resolve_full(&filters))
        .await
        .and_then(|result| result);
    match outcome {
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
/// One input row of a bulk request, carrying the caller's own identifier.
///
/// `id` is echoed back untouched. Array position is preserved too, but an
/// importer that dedupes or filters client-side cannot rely on position, and
/// correlating by it silently misaligns when they do.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveItem {
    #[serde(default)]
    id: Option<String>,
    /// Exact field filters that together identify one record.
    filters: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveRequest {
    items: Vec<ResolveItem>,
    /// Attach the full document from the store to each matched record.
    #[serde(default)]
    full: bool,
    /// Candidates to return for an ambiguous item (default 5, max 25).
    #[serde(default)]
    candidates: Option<usize>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchItem {
    #[serde(default)]
    id: Option<String>,
    /// The text to match, e.g. a company name.
    q: String,
    /// Exact filters scoping the match — country_code and the like.
    #[serde(default)]
    filters: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchRequest {
    items: Vec<MatchItem>,
    #[serde(default)]
    full: bool,
    /// Candidates per item (default 5, max 25).
    #[serde(default)]
    candidates: Option<usize>,
}

/// Parse a bulk request body, reporting failures as JSON.
///
/// Axum's own `Json` rejection is `text/plain`, so a client that parses every
/// response as JSON breaks on exactly the requests it most needs to read. The
/// serde message is worth surfacing too: it names the offending field and
/// lists the valid ones, which is the body-level equivalent of the
/// did-you-mean already offered for query parameters.
fn parse_body<T: serde::de::DeserializeOwned>(
    body: &axum::body::Bytes,
) -> Result<T, Box<Response>> {
    serde_json::from_slice::<T>(body).map_err(|e| {
        Box::new(bad_request(
            "invalid_body",
            format!("could not parse the JSON body: {}", e),
        ))
    })
}

/// How far clear the top candidate is of the next one, as a fraction of the
/// top score. `null` when there is nothing to compare against.
fn top_margin(results: &[serde_json::Value]) -> Option<f64> {
    let score = |row: &serde_json::Value| row.get("_score").and_then(|v| v.as_f64());
    let first = score(results.first()?)?;
    match results.get(1).and_then(score) {
        _ if first <= 0.0 => None,
        None => Some(1.0),
        Some(second) => Some(((first - second) / first).clamp(0.0, 1.0)),
    }
}

/// An engine error is almost always the caller's (an unsortable field, an
/// impossible parameter) — 400. Anything else is genuinely internal.
fn engine_error(e: anyhow::Error) -> Response {
    let message = e.to_string();
    let status = if message.starts_with("cannot sort by") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn bad_request(error: &str, message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error, "message": message })),
    )
        .into_response()
}

/// Reject a filter set that cannot identify anything, before it reaches the
/// query layer.
///
/// An empty value means "no constraint" everywhere else in this API, which is
/// right for an unselected dropdown and catastrophic here: the item would
/// match the whole index and, under first-match semantics, join to an
/// arbitrary record. Real data contains such keys — 412 rows in one country
/// file carry a blank organisation number — so this is a live hazard, not a
/// hypothetical one.
fn validate_filters(
    engine: &SearchEngine,
    filters: &HashMap<String, String>,
) -> Result<(), (&'static str, String)> {
    if filters.is_empty() {
        return Err(("no_filters", "provide at least one filter".to_string()));
    }
    for (key, value) in filters {
        if !engine.field_map.contains_key(key) {
            return Err((
                "unknown_field",
                format!("'{}' is not a field in this index; see /fields", key),
            ));
        }
        if value.trim().is_empty() {
            return Err((
                "empty_value",
                format!(
                    "'{}' has an empty value; an empty filter matches every document, \
                     which cannot identify a record",
                    key
                ),
            ));
        }
    }
    Ok(())
}

/// Deterministic resolution of exact keys to records — the bulk join an
/// importer needs. Never returns a "best guess": an item matching more than
/// one record is reported as ambiguous with its candidates, because a wrong
/// join corrupts the destination silently while a reported one does not.
async fn handle_resolve(State(state): State<Arc<AppState>>, body: axum::body::Bytes) -> Response {
    let req: ResolveRequest = match parse_body(&body) {
        Ok(req) => req,
        Err(response) => return *response,
    };
    let engine = &state.engine;
    if req.items.is_empty() {
        return bad_request("no_items", "provide at least one item".to_string());
    }
    if req.items.len() > MAX_RESOLVE_BATCH {
        return bad_request(
            "batch_too_large",
            format!(
                "at most {} items per request, got {}",
                MAX_RESOLVE_BATCH,
                req.items.len()
            ),
        );
    }
    if req.full && engine.store.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(store_unavailable_body(engine)),
        )
            .into_response();
    }
    let candidates = req
        .candidates
        .unwrap_or(DEFAULT_CANDIDATES)
        .clamp(1, MAX_CANDIDATES);

    let start = Instant::now();
    let outcome = run_blocking(&state, move |app| {
        let engine = &app.engine;
        let mut results = Vec::with_capacity(req.items.len());
        let mut matched = 0usize;
        let mut not_found = 0usize;
        let mut ambiguous = 0usize;
        let mut invalid = 0usize;

        for item in &req.items {
            if let Err((error, message)) = validate_filters(engine, &item.filters) {
                invalid += 1;
                results.push(serde_json::json!({
                    "id": item.id, "status": "invalid", "error": error, "message": message,
                }));
                continue;
            }
            // Ask for one more than we will report, so "exactly one" is
            // decided by the count rather than by the page size.
            let outcome = engine.search(
                "",
                &item.filters,
                &[],
                &crate::search::SortOrder::Relevance,
                candidates,
                0,
                false,
                true,
                req.full,
            );
            match outcome {
                Ok(found) => {
                    let count = found.count.unwrap_or(found.returned);
                    if count == 0 {
                        not_found += 1;
                        results.push(serde_json::json!({
                            "id": item.id, "status": "not_found", "count": 0,
                        }));
                    } else if count == 1 {
                        matched += 1;
                        results.push(serde_json::json!({
                            "id": item.id, "status": "matched", "count": 1,
                            "document": found.results.into_iter().next(),
                        }));
                    } else {
                        ambiguous += 1;
                        results.push(serde_json::json!({
                            "id": item.id, "status": "ambiguous", "count": count,
                            "candidates": found.results,
                        }));
                    }
                }
                Err(e) => {
                    invalid += 1;
                    results.push(serde_json::json!({
                        "id": item.id, "status": "error", "message": e.to_string(),
                    }));
                }
            }
        }

        serde_json::json!({
            "took_ms": round_ms(start),
            "summary": {
                "items": req.items.len(),
                "matched": matched,
                "not_found": not_found,
                "ambiguous": ambiguous,
                "invalid": invalid,
            },
            "results": results,
        })
    })
    .await;

    match outcome {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Ranked candidate matching for names — record linkage rather than a join.
/// Returns candidates for the caller to choose between; it never decides.
async fn handle_match(State(state): State<Arc<AppState>>, body: axum::body::Bytes) -> Response {
    let req: MatchRequest = match parse_body(&body) {
        Ok(req) => req,
        Err(response) => return *response,
    };
    let engine = &state.engine;
    if req.items.is_empty() {
        return bad_request("no_items", "provide at least one item".to_string());
    }
    if req.items.len() > MAX_MATCH_BATCH {
        return bad_request(
            "batch_too_large",
            format!(
                "at most {} items per request, got {} — each item is a separate \
                 ranked query, so this limit is much lower than /resolve's",
                MAX_MATCH_BATCH,
                req.items.len()
            ),
        );
    }
    if req.full && engine.store.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(store_unavailable_body(engine)),
        )
            .into_response();
    }
    let candidates = req
        .candidates
        .unwrap_or(DEFAULT_CANDIDATES)
        .clamp(1, MAX_CANDIDATES);

    let start = Instant::now();
    let outcome = run_blocking(&state, move |app| {
        let engine = &app.engine;
        let mut results = Vec::with_capacity(req.items.len());
        let mut with_hits = 0usize;

        for item in &req.items {
            let query = item.q.trim();
            if query.is_empty() {
                results.push(serde_json::json!({
                    "id": item.id, "status": "invalid", "error": "empty_query",
                    "message": "q is empty; an empty query cannot rank anything",
                }));
                continue;
            }
            if query.chars().count() > MAX_MATCH_QUERY_LEN {
                results.push(serde_json::json!({
                    "id": item.id, "status": "invalid", "error": "query_too_long",
                    "message": format!("q is limited to {} characters", MAX_MATCH_QUERY_LEN),
                }));
                continue;
            }
            if !item.filters.is_empty() {
                if let Err((error, message)) = validate_filters(engine, &item.filters) {
                    results.push(serde_json::json!({
                        "id": item.id, "status": "invalid", "error": error, "message": message,
                    }));
                    continue;
                }
            }
            // No count: trigram matching gives almost every document a weak
            // score, so the "number of matches" for a name runs to hundreds
            // of thousands and means nothing a caller should act on.
            // Reporting it would invite exactly the wrong reading. Skipping
            // it is also free speed, since nothing has to traverse the whole
            // matching set.
            let outcome = engine.search(
                query,
                &item.filters,
                &[],
                &crate::search::SortOrder::Relevance,
                candidates,
                0,
                false,
                false,
                req.full,
            );
            match outcome {
                Ok(found) => {
                    if !found.results.is_empty() {
                        with_hits += 1;
                    }
                    results.push(serde_json::json!({
                        "id": item.id,
                        "status": if found.results.is_empty() { "not_found" } else { "candidates" },
                        "returned": found.results.len(),
                        // Relative distance between the best candidate and
                        // the runner-up. _score is a 0-1 name similarity
                        // since the rerank stage, so it carries meaning on
                        // its own now; margin still says whether the leader
                        // stands alone (1.0) or sits in a tie the caller
                        // should look at (~0).
                        "margin": top_margin(&found.results),
                        "candidates": found.results,
                    }));
                }
                Err(e) => results.push(serde_json::json!({
                    "id": item.id, "status": "error", "message": e.to_string(),
                })),
            }
        }

        serde_json::json!({
            "took_ms": round_ms(start),
            "summary": { "items": req.items.len(), "with_candidates": with_hits },
            "results": results,
        })
    })
    .await;

    match outcome {
        Ok(body) => Json(body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

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
    let batch = refs.clone();
    let outcome = run_blocking(&state, move |app| app.engine.get_full_many(&batch))
        .await
        .and_then(|result| result);
    match outcome {
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
    // The sysinfo refresh and the index-directory walk are blocking work
    // too — a stats poller must not be able to stall queries, or vice versa.
    let outcome = run_blocking(&state, move |state| {
        // Refresh only what the response reports. new_all()+refresh_all()
        // walked every process on the machine on every poll.
        let pid = sysinfo::get_current_pid().ok();
        let mut sys = System::new();
        sys.refresh_memory();
        if let Some(pid) = pid {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        }
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

        serde_json::json!({
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
                "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            },
                "dashboard": {
                    "name": state.engine.config.dashboard.name,
                    "columns": state.engine.config.dashboard.columns,
                    "filters": state.engine.config.dashboard.filters,
                },
                "schema": {
                    "fields": state.engine.config.schema.fields.iter().map(|f| {
                        let metadata = state.engine.field_metadata.get(&f.name);
                        serde_json::json!({
                            "name": f.name,
                            "type": format!("{:?}", f.field_type).to_lowercase(),
                            "search": f.search.as_ref().map(|s| s.as_str()),
                            "description": f.description,
                            "values": metadata.map(|m| m.values.clone()).unwrap_or_default(),
                            "values_truncated": metadata.map(|m| m.truncated).unwrap_or(false),
                        })
                    }).collect::<Vec<_>>(),
                },
        })
    })
    .await;

    Json(outcome.unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })))
}

/// Operation history for the Activity tab and external monitoring: recent
/// events (newest first), per-day aggregates for the heatmap, and the
/// store's accumulated dead weight (refs issued vs live docs — superseded
/// versions an incremental update left behind, reclaimed by a full import).
async fn handle_activity(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let outcome = run_blocking(&state, move |state| {
        let config = &state.engine.config;
        let log = crate::activity::read_log(&crate::activity::activity_path(config));
        let live_docs = state.engine.reader.searcher().num_docs();

        let index_size = dir_size(&config.server.index_path).unwrap_or(0);
        let store_size = if config.store.enabled {
            dir_size(&config.resolve_store_path()).unwrap_or(0)
        } else {
            0
        };

        let store = state.engine.store.as_ref().map(|store| {
            let refs_issued = store.doc_count();
            let superseded = refs_issued.saturating_sub(live_docs);
            let stats = store.stats();
            let cache_lookups = stats.cache_hits + stats.cache_misses;
            let cache_hit_pct = if cache_lookups > 0 {
                stats.cache_hits as f64 * 100.0 / cache_lookups as f64
            } else {
                0.0
            };
            serde_json::json!({
                "refs_issued": refs_issued,
                "superseded": superseded,
                "superseded_pct": if refs_issued > 0 {
                    superseded as f64 * 100.0 / refs_issued as f64
                } else {
                    0.0
                },
                "size_bytes": store_size,
                "size_human": format_bytes(store_size),
                "compression_ratio": stats.raw_bytes.max(1) as f64
                    / stats.compressed_bytes.max(1) as f64,
                "cache_hit_pct": cache_hit_pct,
            })
        });

        // The latest ring sample plus the decimated series. Empty until the
        // sampler has run (it fires immediately on serve).
        let samples = state.metrics.samples(crate::metrics::MAX_POINTS_SERVED);
        let latest = state.metrics.latest();
        let resources = latest.map(|s| {
            serde_json::json!({
                "rss_bytes": s.rss_bytes,
                "rss_human": format_bytes(s.rss_bytes),
                "available_mem": s.available_mem,
                "total_mem": s.total_mem,
                "disk_free": s.disk_free,
                "disk_total": s.disk_total,
                "cpu_pct": s.cpu_pct,
                "uptime_seconds": state.started_at.elapsed().as_secs(),
                "uptime_human": format_duration(state.started_at.elapsed().as_secs()),
            })
        });

        serde_json::json!({
            "events": log.events,
            "days": log.days,
            "total_events": log.total_events,
            "documents": live_docs,
            "index_bytes": index_size,
            "index_human": format_bytes(index_size),
            "disk_bytes": index_size + store_size,
            "disk_human": format_bytes(index_size + store_size),
            "store": store,
            "resources": resources,
            "search": {
                "total_queries": state.metrics.total_queries(),
                "sample_interval_secs": crate::metrics::SAMPLE_INTERVAL_SECS,
                "samples": samples,
            },
        })
    })
    .await;

    Json(outcome.unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })))
}

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Background task feeding the metrics ring: every SAMPLE_INTERVAL_SECS it
/// takes one cheap, targeted sysinfo reading (this process + memory + the
/// index volume — never the every-process walk /stats was cured of) and
/// folds the interval's query counters into a sample. Spawned by `serve`;
/// the ring simply stays empty when nothing spawns it (tests, embedders).
pub async fn resource_sampler(state: Arc<AppState>) {
    let pid = sysinfo::get_current_pid().ok();
    // One persistent System: process CPU usage is measured between two
    // refreshes, so a fresh System every tick would always read 0.
    let mut sys = System::new();
    let index_path = state
        .engine
        .config
        .server
        .index_path
        .canonicalize()
        .unwrap_or_else(|_| state.engine.config.server.index_path.clone());

    let mut last_tick = Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        crate::metrics::SAMPLE_INTERVAL_SECS,
    ));
    // First tick fires immediately, so the page has a sample from startup.
    loop {
        interval.tick().await;
        sys.refresh_memory();
        if let Some(pid) = pid {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        }
        let process = pid.and_then(|p| sys.process(p));
        let (disk_free, disk_total) = index_volume_space(&index_path);

        let reading = crate::metrics::ResourceReading {
            rss_bytes: process.map(|p| p.memory()).unwrap_or(0),
            available_mem: sys.available_memory(),
            total_mem: sys.total_memory(),
            disk_free,
            disk_total,
            cpu_pct: process.map(|p| p.cpu_usage()).unwrap_or(0.0),
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let elapsed = last_tick.elapsed().as_secs_f64();
        last_tick = Instant::now();
        state.metrics.sample_tick(now_unix, elapsed, reading);
    }
}

/// Free/total bytes of the volume holding the index: the disk whose mount
/// point is the longest prefix of the index path. (0, 0) when no mount
/// matches — the dashboard hides the disk views rather than lying.
fn index_volume_space(index_path: &std::path::Path) -> (u64, u64) {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| index_path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| (disk.available_space(), disk.total_space()))
        .unwrap_or((0, 0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FieldConfig, FieldType};

    fn engine(dir: &std::path::Path) -> SearchEngine {
        crate::search::tests::engine_with_field(
            dir,
            FieldConfig {
                name: "org_number".to_string(),
                field_type: FieldType::Keyword,
                search: None,
                values: None,
                max_values: None,
                case_sensitive: false,
                multi: false,
                separator: None,
                description: None,
            },
        )
    }

    fn filters(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The hazard this endpoint exists to avoid. Everywhere else in the API an
    /// empty value means "no constraint" — correct for an unselected dropdown.
    /// In a bulk join it would match the entire index, and returning a "best
    /// guess" from that would silently attach an importer's row to an
    /// arbitrary company. Real feeds carry such keys, so this must be refused
    /// rather than resolved.
    #[test]
    fn a_blank_key_is_refused_rather_than_matching_everything() {
        let dir = std::env::temp_dir().join(format!(
            "ruzz-resolve-validate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let engine = engine(&dir);

        assert!(validate_filters(&engine, &filters(&[("org_number", "998877")])).is_ok());

        for blank in ["", "   ", "\t"] {
            let err = validate_filters(&engine, &filters(&[("org_number", blank)]))
                .expect_err("a blank key must not be accepted");
            assert_eq!(err.0, "empty_value", "blank {:?}", blank);
        }

        // No filters at all is the same hazard wearing a different hat.
        let err = validate_filters(&engine, &HashMap::new()).expect_err("empty set");
        assert_eq!(err.0, "no_filters");

        // A misspelled field would otherwise be ignored and resolve against
        // the remaining filters, quietly widening the match.
        let err = validate_filters(&engine, &filters(&[("org_nummer", "998877")]))
            .expect_err("unknown field");
        assert_eq!(err.0, "unknown_field");

        // One good filter does not excuse a blank one alongside it.
        let err = validate_filters(
            &engine,
            &filters(&[("org_number", "998877"), ("country_code", "")]),
        )
        .expect_err("blank alongside good");
        assert!(matches!(err.0, "empty_value" | "unknown_field"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
