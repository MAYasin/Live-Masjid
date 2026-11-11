//! HTTP layer: static UI assets, JSON API, and live status over server-sent events.

mod api;
mod assets;
mod auth;

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
        .with_state(app)
}
