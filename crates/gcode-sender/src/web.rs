//! The web UI. The files in `web/` are compiled into the binary, so the
//! server is the only thing that has to be copied onto the machine.

use axum::Router;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const INDEX_HTML: &str = include_str!("../../../web/index.html");
const APP_JS: &str = include_str!("../../../web/app.js");
const STYLE_CSS: &str = include_str!("../../../web/style.css");

/// The page may only run its own scripts and styles and talk to its own
/// origin. It cannot be framed, so another site cannot drive it by overlay.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    connect-src 'self'; img-src 'self' data:; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/",
            get(|| async { asset(INDEX_HTML, "text/html; charset=utf-8") }),
        )
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }),
        )
        .route(
            "/style.css",
            get(|| async { asset(STYLE_CSS, "text/css; charset=utf-8") }),
        )
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
