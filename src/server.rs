use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use sysinfo::System;
use tower_http::cors::CorsLayer;

use crate::search::SearchEngine;

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
    // Skip auth for /health (load balancer probes)
    if req.uri().path() == "/health" {
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
    sort_by: Option<String>,      // field name to sort by
    sort_order: Option<String>,   // "asc" or "desc"
    #[serde(flatten)]
    extra: HashMap<String, String>,
}

async fn handle_search(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Json<serde_json::Value> {
    let query_text = params.q.unwrap_or_default();
    let limit = params.limit.unwrap_or(20);

    let mut filters = params.extra.clone();
    filters.remove("limit");
    filters.remove("sort_by");
    filters.remove("sort_order");

    // Extract range filters: keys ending in _min or _max
    let mut range_filters: Vec<crate::search::RangeFilter> = Vec::new();
    let range_keys: Vec<String> = filters.keys()
        .filter(|k| k.ends_with("_min") || k.ends_with("_max"))
        .cloned()
        .collect();

    // Collect unique field base names
    let mut range_fields: HashMap<String, (Option<f64>, Option<f64>)> = HashMap::new();
    for key in &range_keys {
        let value = filters.remove(key).unwrap_or_default();
        let num = value.parse::<f64>().ok();
        if let Some(n) = num {
            if let Some(base) = key.strip_suffix("_min") {
                let entry = range_fields.entry(base.to_string()).or_insert((None, None));
                entry.0 = Some(n);
            } else if let Some(base) = key.strip_suffix("_max") {
                let entry = range_fields.entry(base.to_string()).or_insert((None, None));
                entry.1 = Some(n);
            }
        }
    }

    for (field, (min, max)) in range_fields {
        range_filters.push(crate::search::RangeFilter { field, min, max });
    }

    // Sort
    let sort = match (params.sort_by.as_deref(), params.sort_order.as_deref()) {
        (Some(field), Some("asc")) => crate::search::SortOrder::FieldAsc(field.to_string()),
        (Some(field), _) => crate::search::SortOrder::FieldDesc(field.to_string()), // default desc
        _ => crate::search::SortOrder::Relevance,
    };

    match state.engine.search(&query_text, &filters, &range_filters, &sort, limit) {
        Ok(result) => Json(serde_json::to_value(result).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(serde::Deserialize)]
struct LookupParams {
    #[serde(flatten)]
    filters: HashMap<String, String>,
}

async fn handle_lookup(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LookupParams>,
) -> Json<serde_json::Value> {
    match state.engine.lookup(&params.filters) {
        Ok(result) => Json(serde_json::to_value(result).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

async fn handle_stats(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
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
    let segment_count = state.engine.index.searchable_segment_metas()
        .map(|s| s.len())
        .unwrap_or(0);

    let num_docs = state.engine.reader.searcher().num_docs();

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
                serde_json::json!({
                    "name": f.name,
                    "type": format!("{:?}", f.field_type).to_lowercase(),
                    "search": f.search.as_ref().map(|s| format!("{:?}", s).to_lowercase()),
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
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    if hours > 0 {
        format!("{}h {}m {}s", hours, mins, s)
    } else if mins > 0 {
        format!("{}m {}s", mins, s)
    } else {
        format!("{}s", s)
    }
}
