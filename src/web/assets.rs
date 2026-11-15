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
        Some(html) => serve(&headers, "text/html; charset=utf-8", &with_reload(&html)),
        None => serve(&headers, "text/html; charset=utf-8", INDEX),
    }
}

/// Development: the page with the reload script added.
fn with_reload(html: &str) -> String {
    html.replace("</body>", &format!("{RELOAD_SCRIPT}</body>"))
}

/// Development: the newest modification time under the UI folder (the page reloads when it changes).
pub async fn dev_stamp() -> Response {
    stamp(crate::platform::ui_dir())
}

fn stamp(dir: Option<&std::path::Path>) -> Response {
    match dir {
        Some(dir) => ([(header::CACHE_CONTROL, "no-store")], newest(dir).to_string()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Newest modification time (ms) of any file under `dir`, 0 when there is none.
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
    use super::*;

    #[test]
    fn content_types() {
        assert!(super::content_type("a/b.css").starts_with("text/css"));
        assert!(super::content_type("a/b.js").starts_with("text/javascript"));
        assert!(super::FILES.iter().all(|(p, _, body)| !body.is_empty() && !p.starts_with('/')));
        assert_ne!(super::etag("a"), super::etag("b"), "content changes must change the ETag");
        assert_eq!(super::etag("same"), super::etag("same"));
    }

    #[tokio::test]
    async fn serves_files_and_revalidates_with_the_etag() {
        let r = super::file(HeaderMap::new(), axum::extract::Path("css/base.css".to_string())).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "text/css; charset=utf-8");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
        let tag = r.headers()[header::ETAG].to_str().unwrap().to_string();
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, tag.parse().unwrap());
        let r2 = super::file(h, axum::extract::Path("css/base.css".to_string())).await;
        assert_eq!(r2.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn unknown_files_are_404() {
        let r = super::file(HeaderMap::new(), axum::extract::Path("../settings.json".to_string())).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(super::png(axum::extract::Path("nope.png".to_string())).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(super::png(axum::extract::Path("icon-192.png".to_string())).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn index_icon_and_manifest_are_served() {
        let idx = super::index(HeaderMap::new()).await;
        assert_eq!(idx.status(), StatusCode::OK);
        assert_eq!(idx.headers()[header::CONTENT_TYPE], "text/html; charset=utf-8");
        let body = axum::body::to_bytes(idx.into_body(), 4 * 1024 * 1024).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("<html"));
        assert_eq!(super::icon(HeaderMap::new()).await.headers()[header::CONTENT_TYPE], "image/svg+xml");
        let m = super::manifest().await;
        assert_eq!(m.headers()[header::CONTENT_TYPE], "application/manifest+json");
    }

    #[test]
    fn other_files_are_plain_text() {
        assert_eq!(super::content_type("notes.txt"), "text/plain; charset=utf-8");
        assert_eq!(super::content_type(".js"), "text/plain; charset=utf-8", "an extension alone is no file");
    }

    #[test]
    fn development_reload() {
        let page = super::with_reload("<html><body>x</body></html>");
        assert!(page.contains("/__dev/stamp"));
        assert!(page.ends_with("</script></body></html>"));
    }

    #[tokio::test]
    async fn dev_stamp_tracks_the_newest_file() {
        assert_eq!(super::dev_stamp().await.status(), if crate::platform::ui_dir().is_some() { StatusCode::OK } else { StatusCode::NOT_FOUND });
        assert_eq!(super::stamp(None).status(), StatusCode::NOT_FOUND);

        let dir = std::env::temp_dir().join(format!("lm-ui-{}", crate::config::random_hex(6)));
        assert_eq!(super::newest(&dir), 0, "a missing folder");
        std::fs::create_dir_all(dir.join("js/views")).unwrap();
        assert_eq!(super::newest(&dir), 0, "no files");
        std::fs::write(dir.join("index.html"), "x").unwrap();
        std::fs::write(dir.join("js/views/now.js"), "y").unwrap();
        let t = super::newest(&dir);
        assert!(t > 0);
        let r = super::stamp(Some(&dir));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), t.to_string());
        let _ = std::fs::remove_dir_all(dir);
    }
}
