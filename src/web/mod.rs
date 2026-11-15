//! HTTP layer: static UI assets, JSON API, and live status over server-sent events.

mod api;
mod assets;
mod auth;
mod guard;

use crate::state::App;
use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

/// Error returned by API handlers: a status code plus a human-readable message.
pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn bad(msg: impl std::fmt::Display) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.to_string())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self(StatusCode::UNAUTHORIZED, msg.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::bad(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

pub type ApiResult<T = axum::Json<serde_json::Value>> = Result<T, ApiError>;

pub fn ok() -> ApiResult {
    Ok(axum::Json(serde_json::json!({ "ok": true })))
}

pub fn router(app: App) -> Router {
    let protected = Router::new()
        .route("/api/state", get(api::status::state))
        .route("/api/events", get(api::status::events))
        .route("/api/history", get(api::status::history))
        .route("/api/logs", get(api::status::logs))
        .route("/api/system", get(api::status::system))
        .route("/api/salah", get(api::extras::salah_times))
        .route("/api/salah/board", post(api::extras::salah_board))
        .route("/api/recordings", get(api::extras::recording_list))
        .route("/api/quran", get(api::quran::library))
        .route("/api/quran/play", post(api::quran::play))
        .route("/api/quran/reciters", get(api::quran::reciters).post(api::quran::set_reciter))
        .route("/api/quran/playlists", get(api::quran::playlists).post(api::quran::save_playlist))
        .route("/api/quran/playlists/remove", post(api::quran::delete_playlist))
        .route("/api/quran/seek", post(api::quran::seek))
        .route("/api/quran/speed", post(api::quran::speed))
        .route("/api/quran/step", post(api::quran::step))
        .route("/api/quran/download", post(api::quran::download))
        .route("/api/quran/delete", post(api::quran::delete))
        .route("/api/recordings/play", post(api::extras::play))
        .route("/api/recordings/pause", post(api::extras::pause))
        .route("/api/recordings/stop", post(api::extras::stop))
        .route("/api/listen", get(api::extras::listen))
        .route("/api/listen/live.wav", get(api::extras::listen_live))
        .route("/api/recordings/url", get(api::extras::recording_url))
        .route("/api/stations", get(api::stations::list))
        .route("/api/stations/refresh", post(api::stations::refresh))
        .route("/api/select", post(api::stations::select))
        .route("/api/config", get(api::player::config))
        .route("/api/settings", post(api::player::settings))
        .route("/api/player/{action}", post(api::player::action))
        .route("/api/bt/devices", get(api::speaker::devices))
        .route("/api/bt/scan", post(api::speaker::scan))
        .route("/api/bt/use", post(api::speaker::use_device))
        .route("/api/bt/stop", post(api::speaker::release))
        .route("/api/bt/forget", post(api::speaker::forget))
        .route("/api/bt/repair", post(api::speaker::repair))
        .route("/api/outputs", get(api::speaker::outputs))
        .route("/api/password", post(auth::set_password))
        .route("/api/logout", post(auth::logout))
        .route("/api/system/updates", get(api::system::updates))
        .route("/api/system/{action}", post(api::system::action))
        .layer(middleware::from_fn_with_state(app.clone(), auth::require_session));

    Router::new()
        .route("/", get(assets::index))
        .route("/manifest.webmanifest", get(assets::manifest))
        .route("/icon.svg", get(assets::icon))
        .route("/__dev/stamp", get(assets::dev_stamp))
        .route("/icons/{name}", get(assets::png))
        .route("/assets/{*path}", get(assets::file))
        .route("/api/health", get(api::status::health))
        .route("/api/login", post(auth::login))
        // Merged with full /api paths rather than nested: nesting inserts a catch-all
        // fallback route, and matchit 0.8.4 (pinned by axum 0.8) crashed while checking it
        // for conflicts in one build (SIGSEGV formatting a null route string).
        .merge(protected)
        // Outermost: every request (assets too) must name the Pi, and changes must come from this page.
        .layer(middleware::from_fn(guard::check))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use std::path::PathBuf;
    use tower::ServiceExt;

    async fn split(resp: Response) -> (StatusCode, String) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lm-web-{tag}-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(method: Method, uri: &str) -> axum::http::request::Builder {
        Request::builder().method(method).uri(uri).header(header::HOST, "localhost")
    }

    async fn call(app: &App, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let resp = router(app.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn post_json(uri: &str, body: &str) -> Request<Body> {
        request(Method::POST, uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[tokio::test]
    async fn guards_hosts_origins_and_serves_assets() {
        let dir = temp_dir("guard");
        let app = crate::state::test_app(&dir);

        let (status, headers, body) = call(&app, request(Method::GET, "/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().contains("text/html"));
        assert!(body.contains("<!doctype html") || body.contains("<!DOCTYPE html"));

        let (status, _, body) = call(&app, request(Method::GET, "/api/state").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"version\""));

        for asset in ["/manifest.webmanifest", "/icon.svg", "/icons/icon-192.png", "/assets/js/main.js", "/api/health"] {
            let (status, _, _) = call(&app, request(Method::GET, asset).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::OK, "{asset}");
        }

        let (status, _, _) = call(&app, request(Method::GET, "/missing").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A foreign Host (DNS rebinding) and a cross-site POST are refused.
        let req = Request::builder().method(Method::GET).uri("/").header(header::HOST, "evil.example").body(Body::empty()).unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::FORBIDDEN);

        let req = request(Method::POST, "/api/player/play")
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::FORBIDDEN);

        // An Origin naming this host is fine.
        let req = request(Method::POST, "/api/player/play")
            .header(header::ORIGIN, "http://localhost")
            .body(Body::empty())
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::OK);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn password_session_flow() {
        let dir = temp_dir("auth");
        let app = crate::state::test_app(&dir);

        let (status, headers, _) = call(&app, post_json("/api/password", r#"{"new_password":"abcd"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
        assert!(cookie.starts_with("lm_session="));

        let (status, _, _) = call(&app, request(Method::GET, "/api/state").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let req = request(Method::GET, "/api/state").header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::OK);

        let req = request(Method::POST, "/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"password":"wrong"}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::UNAUTHORIZED);

        let req = request(Method::POST, "/api/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"password":"abcd"}"#))
            .unwrap();
        let (status, headers, _) = call(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.contains_key(header::SET_COOKIE));

        let req = request(Method::POST, "/api/logout").header(header::COOKIE, &cookie).body(Body::empty()).unwrap();
        let (status, headers, _) = call(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().contains("Max-Age=0"));

        let req = request(Method::POST, "/api/password")
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"new_password":""}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::OK);
        let (status, _, _) = call(&app, request(Method::GET, "/api/state").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "the password was removed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn api_surface_reads_and_updates() {
        let dir = temp_dir("api");
        let app = crate::state::test_app(&dir);

        let (status, _, body) = call(&app, request(Method::GET, "/api/config").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"live_delay_s\""));

        let req = request(Method::POST, "/api/settings")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"live_delay_s":99,"quran_volume":150,"poll_secs":30}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::OK);
        let s = app.settings();
        assert_eq!(s.live_delay_s, 30, "clamped to the UI range");
        assert_eq!(s.quran_volume, 100);
        assert_eq!(s.poll_secs, 30);

        let req = request(Method::POST, "/api/settings")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"bt_quality":"low"}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::BAD_REQUEST);

        let (status, _, body) = call(&app, request(Method::GET, "/api/stations").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"stations\""));

        let req = request(Method::POST, "/api/select")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"mount":"/lakewood"}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::OK);
        assert_eq!(app.settings().mount.as_deref(), Some("lakewood"));

        let req = request(Method::POST, "/api/select")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"mount":"../etc"}"#))
            .unwrap();
        assert_eq!(call(&app, req).await.0, StatusCode::BAD_REQUEST);

        for uri in ["/api/history", "/api/logs", "/api/system", "/api/system/updates", "/api/outputs", "/api/listen", "/api/quran", "/api/quran/reciters", "/api/quran/playlists"] {
            let (status, _, _) = call(&app, request(Method::GET, uri).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
        }

        for (uri, want) in [("/api/player/play", StatusCode::OK), ("/api/player/nope", StatusCode::BAD_REQUEST), ("/api/system/clear-history", StatusCode::OK), ("/api/system/nope", StatusCode::BAD_REQUEST)] {
            let (status, _, _) = call(&app, request(Method::POST, uri).body(Body::empty()).unwrap()).await;
            assert_eq!(status, want, "{uri}");
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn errors_carry_status_and_message() {
        let (status, body) = split(ApiError::bad("nope").into_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "nope");

        let (status, body) = split(ApiError::unauthorized("login required").into_response()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "login required");

        let (status, body) = split(ApiError::from(anyhow::anyhow!("boom")).into_response()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "boom");
    }

    #[tokio::test]
    async fn ok_is_json() {
        let json = ok().ok().expect("ok() is Ok");
        let (status, body) = split(json.into_response()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"ok":true}"#);
    }
}
