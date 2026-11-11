//! The web UI, compiled into the binary from `ui/`.
//!
//! Files are served with an ETag derived from their *content*, so browsers revalidate
//! cheaply (304) and always pick up a changed file — even between dev builds that share
//! a version label (an earlier version-based ETag served stale JS after dev deploys).
//!
//! Development (`LIVEMASJID_UI_DIR` set, see `platform`): files are read from that folder on every
//! request, and the page gets a small script that reloads it when any UI file changes.

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
    "js/local.js",
    "js/dom.js",
    "js/components.js",
    "js/views/now.js",
    "js/views/masjids.js",
    "js/views/speaker.js",
    "js/views/history.js",
    "js/views/settings.js",
    "js/views/system.js",
    "js/views/salah.js",
    "js/views/recordings.js",
    "js/views/masjid.js",
    "js/views/quran.js",
    "js/views/playback.js",
    "js/views/updates.js",
    "js/views/security.js",
    "js/miniplayer.js",
    "js/install.js",
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

/// FNV-1a 64-bit hash of the file contents as a quoted ETag.
fn etag(body: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in body.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

fn serve(headers: &HeaderMap, content_type: &'static str, body: &str) -> Response {
    let tag = etag(body);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(tag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    (
        [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache"), (header::ETAG, tag.as_str())],
        body.to_string(),
    )
        .into_response()
}

/// Development: a UI file from disk (only the files a release embeds, so a file missing from
/// `FILES` is noticed during development too).
fn from_disk(path: &str) -> Option<String> {
    std::fs::read_to_string(crate::platform::ui_dir()?.join(path)).ok()
}

/// Development: polls `/__dev/stamp` and reloads the page when it changes.
const RELOAD_SCRIPT: &str = r#"<script>(() => { let last = null;
  setInterval(async () => { try { const s = await (await fetch("/__dev/stamp")).text();
    if (last && s !== last) location.reload(); last = s; } catch {} }, 700); })();</script>"#;

pub async fn index(headers: HeaderMap) -> Response {
    match from_disk("index.html") {
        Some(html) => serve(&headers, "text/html; charset=utf-8", &html.replace("</body>", &format!("{RELOAD_SCRIPT}</body>"))),
        None => serve(&headers, "text/html; charset=utf-8", INDEX),
    }
}

/// Development: the newest modification time under the UI folder (the page reloads when it changes).
pub async fn dev_stamp() -> Response {
    fn newest(dir: &std::path::Path) -> u128 {
        let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
        entries
            .map(|e| {
                let p = e.path();
                if p.is_dir() {
                    return newest(&p);
                }
                let modified = e.metadata().and_then(|m| m.modified()).ok();
                modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis())
            })
            .max()
            .unwrap_or(0)
    }
    match crate::platform::ui_dir() {
        Some(dir) => ([(header::CACHE_CONTROL, "no-store")], newest(dir).to_string()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// App icons (PNG) for phones: iOS home screen, Android/Chrome install.
static ICONS: &[(&str, &[u8])] = &[
    ("icon-192.png", include_bytes!("../../ui/icons/icon-192.png")),
    ("icon-512.png", include_bytes!("../../ui/icons/icon-512.png")),
    ("icon-maskable-512.png", include_bytes!("../../ui/icons/icon-maskable-512.png")),
    ("apple-touch-icon.png", include_bytes!("../../ui/icons/apple-touch-icon.png")),
];

pub async fn png(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match ICONS.iter().find(|(n, _)| *n == name) {
        Some((_, body)) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=604800")], *body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn icon(headers: HeaderMap) -> Response {
    serve(&headers, "image/svg+xml", ICON)
}

pub async fn file(headers: HeaderMap, axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match FILES.iter().find(|(p, _, _)| *p == path) {
        Some((p, ct, body)) => serve(&headers, ct, &from_disk(p).unwrap_or_else(|| (*body).to_string())),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn manifest() -> Response {
    let m = serde_json::json!({
        "id": "/", "name": "Live Masjid", "short_name": "Live Masjid", "start_url": "/", "scope": "/",
        "display": "standalone", "orientation": "portrait",
        "description": "Live masjid broadcasts, salah times and Quran on your speaker",
        "background_color": "#0a0a0a", "theme_color": "#0a0a0a",
        "icons": [
            { "src": "/icons/icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any" },
            { "src": "/icons/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any" },
            { "src": "/icons/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable" },
            { "src": "/icon.svg", "sizes": "any", "type": "image/svg+xml" }
        ],
        "shortcuts": [
            { "name": "Quran", "url": "/#quran", "icons": [{ "src": "/icons/icon-192.png", "sizes": "192x192" }] },
            { "name": "Masjids", "url": "/#masjids", "icons": [{ "src": "/icons/icon-192.png", "sizes": "192x192" }] }
        ]
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
        assert_ne!(super::etag("a"), super::etag("b"), "content changes must change the ETag");
        assert_eq!(super::etag("same"), super::etag("same"));
    }
}
