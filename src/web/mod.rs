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
        .route("/state", get(api::status::state))
        .route("/events", get(api::status::events))
        .route("/history", get(api::status::history))
        .route("/logs", get(api::status::logs))
        .route("/system", get(api::status::system))
        .route("/salah", get(api::extras::salah_times))
        .route("/salah/board", post(api::extras::salah_board))
        .route("/recordings", get(api::extras::recording_list))
        .route("/quran", get(api::quran::library))
        .route("/quran/play", post(api::quran::play))
        .route("/quran/seek", post(api::quran::seek))
        .route("/quran/step", post(api::quran::step))
        .route("/quran/download", post(api::quran::download))
        .route("/quran/delete", post(api::quran::delete))
        .route("/recordings/play", post(api::extras::play))
        .route("/recordings/pause", post(api::extras::pause))
        .route("/recordings/stop", post(api::extras::stop))
        .route("/listen", get(api::extras::listen))
        .route("/listen/live.wav", get(api::extras::listen_live))
        .route("/recordings/url", get(api::extras::recording_url))
        .route("/stations", get(api::stations::list))
        .route("/stations/refresh", post(api::stations::refresh))
        .route("/select", post(api::stations::select))
        .route("/config", get(api::player::config))
        .route("/settings", post(api::player::settings))
        .route("/player/{action}", post(api::player::action))
        .route("/bt/devices", get(api::speaker::devices))
        .route("/bt/scan", post(api::speaker::scan))
        .route("/bt/use", post(api::speaker::use_device))
        .route("/bt/stop", post(api::speaker::release))
        .route("/bt/forget", post(api::speaker::forget))
        .route("/bt/repair", post(api::speaker::repair))
        .route("/outputs", get(api::speaker::outputs))
        .route("/password", post(auth::set_password))
        .route("/logout", post(auth::logout))
        .route("/system/{action}", post(api::system::action))
        .layer(middleware::from_fn_with_state(app.clone(), auth::require_session));

    Router::new()
        .route("/", get(assets::index))
        .route("/manifest.webmanifest", get(assets::manifest))
        .route("/icon.svg", get(assets::icon))
        .route("/assets/{*path}", get(assets::file))
        .route("/api/health", get(api::status::health))
        .route("/api/login", post(auth::login))
        .nest("/api", protected)
        .with_state(app)
}
