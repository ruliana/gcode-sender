//! The web UI. The files in `web/` are compiled into the binary, so the
//! server is the only thing that has to be copied onto the machine. That
//! includes the libraries and fonts the page uses (`web/vendor/`): the page
//! loads nothing from anywhere else, so it works on a machine with no
//! internet access.

use axum::Router;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const WOFF2: &str = "font/woff2";

/// Every file the page uses: its path, content type and contents.
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/", HTML, include_bytes!("../../../web/index.html")),
    ("/style.css", CSS, include_bytes!("../../../web/style.css")),
    ("/lib.js", JS, include_bytes!("../../../web/lib.js")),
    ("/preview.js", JS, include_bytes!("../../../web/preview.js")),
    ("/app.js", JS, include_bytes!("../../../web/app.js")),
    (
        "/vendor/alpine-csp-3.17.5.min.js",
        JS,
        include_bytes!("../../../web/vendor/alpine-csp-3.17.5.min.js"),
    ),
    (
        "/vendor/d3-7.9.0.min.js",
        JS,
        include_bytes!("../../../web/vendor/d3-7.9.0.min.js"),
    ),
    (
        "/vendor/fonts/DSEG7Classic-Bold.woff2",
        WOFF2,
        include_bytes!("../../../web/vendor/fonts/DSEG7Classic-Bold.woff2"),
    ),
    (
        "/vendor/fonts/barlow-condensed-latin-500-normal.woff2",
        WOFF2,
        include_bytes!("../../../web/vendor/fonts/barlow-condensed-latin-500-normal.woff2"),
    ),
    (
        "/vendor/fonts/barlow-condensed-latin-700-normal.woff2",
        WOFF2,
        include_bytes!("../../../web/vendor/fonts/barlow-condensed-latin-700-normal.woff2"),
    ),
];

/// The page may only run its own scripts and styles and talk to its own
/// origin. It cannot be framed, so another site cannot drive it by overlay.
/// Alpine is its CSP build, which needs no `unsafe-eval`.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    ASSETS
        .iter()
        .fold(Router::new(), |router, &(path, content_type, body)| {
            router.route(path, get(move || async move { asset(body, content_type) }))
        })
}

fn asset(body: &'static [u8], content_type: &'static str) -> Response {
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
