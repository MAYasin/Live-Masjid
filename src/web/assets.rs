//! The web UI, compiled into the binary from `ui/`.
//!
//! Files are served with an ETag equal to the build version, so browsers revalidate
//! cheaply (304) and always pick up a new UI after an update.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Embed `ui/<path>` files: `(url path, content type, contents)`.
macro_rules! embed {
    ($($path:literal),* $(,)?) => {
        &[$(($path, content_type($path), include_str!(concat!("../../ui/", $path)))),*]
    };
}

static FILES: &[(&str, &str, &str)] = embed![
    "css/base.css",
    "css/layout.css",
    "css/components.css",
    "css/views.css",
    "js/main.js",
    "js/api.js",
    "js/store.js",
    "js/format.js",
    "js/dom.js",
    "js/views/now.js",
    "js/views/masjids.js",
    "js/views/speaker.js",
    "js/views/history.js",
    "js/views/settings.js",
];

const INDEX: &str = include_str!("../../ui/index.html");
const ICON: &str = include_str!("../../ui/icon.svg");

const fn content_type(path: &str) -> &'static str {
    let b = path.as_bytes();
    let n = b.len();
    if n > 4 && b[n - 4] == b'.' && b[n - 3] == b'c' && b[n - 2] == b's' && b[n - 1] == b's' {
        "text/css; charset=utf-8"
    } else if n > 3 && b[n - 3] == b'.' && b[n - 2] == b'j' && b[n - 1] == b's' {
        "text/javascript; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    }
}

fn etag() -> String {
    format!("\"{}\"", crate::VERSION)
}

fn serve(headers: &HeaderMap, content_type: &'static str, body: &'static str) -> Response {
    let tag = etag();
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(tag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    (
        [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache"), (header::ETAG, tag.as_str())],
        body,
    )
        .into_response()
}

pub async fn index(headers: HeaderMap) -> Response {
    serve(&headers, "text/html; charset=utf-8", INDEX)
}

pub async fn icon(headers: HeaderMap) -> Response {
    serve(&headers, "image/svg+xml", ICON)
}

pub async fn file(headers: HeaderMap, axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match FILES.iter().find(|(p, _, _)| *p == path) {
        Some((_, ct, body)) => serve(&headers, ct, body),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn manifest() -> Response {
    let m = serde_json::json!({
        "name": "Live Masjid", "short_name": "Live Masjid", "start_url": "/", "display": "standalone",
        "background_color": "#0a0a0a", "theme_color": "#0a0a0a",
        "icons": [{ "src": "/icon.svg", "sizes": "any", "type": "image/svg+xml" }]
    });
    ([(header::CONTENT_TYPE, "application/manifest+json")], m.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_types() {
        assert!(super::content_type("a/b.css").starts_with("text/css"));
        assert!(super::content_type("a/b.js").starts_with("text/javascript"));
        assert!(super::FILES.iter().all(|(p, _, body)| !body.is_empty() && !p.starts_with('/')));
    }
}
