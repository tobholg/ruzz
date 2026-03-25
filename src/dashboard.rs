use axum::response::Html;

pub fn dashboard_html() -> Html<&'static str> {
    Html(include_str!("dashboard.html"))
}
