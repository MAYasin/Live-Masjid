//! Optional password protection: a salted password hash in settings, and an HttpOnly
//! session cookie derived from it (rotating the secret signs everyone out).

use super::{ApiError, ok};
use crate::config::{ct_eq, hash_password, hex, random_hex, verify_password};
use crate::state::App;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::time::Duration;

const COOKIE: &str = "lm_session";
const COOKIE_MAX_AGE: u64 = 400 * 86_400;

/// The cookie value for the current password, or `None` when no password is set.
fn session_token(app: &App) -> Option<String> {
    use sha2::{Digest, Sha256};
    let s = app.settings.read().unwrap();
    let hash = s.password_hash.as_ref()?;
    Some(hex(&Sha256::digest(format!("{}|{hash}", s.secret).as_bytes())))
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
}

fn set_cookie(resp: &mut Response, token: &str, max_age: u64) {
    let v = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}");
    if let Ok(v) = HeaderValue::from_str(&v) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
}

pub async fn require_session(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Some(token) = session_token(&app) {
        let valid = cookie(req.headers(), COOKIE).is_some_and(|c| ct_eq(c.as_bytes(), token.as_bytes()));
        if !valid {
            return ApiError::unauthorized("login required").into_response();
        }
    }
    next.run(req).await
}

#[derive(Deserialize)]
pub struct Login {
    password: String,
}

pub async fn login(State(app): State<App>, Json(r): Json<Login>) -> Result<Response, ApiError> {
    let Some(stored) = app.settings().password_hash else {
        return Ok(Json(serde_json::json!({ "ok": true })).into_response());
    };
    let valid = tokio::task::spawn_blocking(move || verify_password(&r.password, &stored)).await.unwrap_or(false);
    if !valid {
        tokio::time::sleep(Duration::from_millis(600)).await; // slow down guessing
        return Err(ApiError::unauthorized("Wrong password"));
    }
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    set_cookie(&mut resp, &session_token(&app).unwrap_or_default(), COOKIE_MAX_AGE);
    Ok(resp)
}

pub async fn logout() -> Response {
    let mut resp = Json(serde_json::json!({ "ok": true })).into_response();
    set_cookie(&mut resp, "", 0);
    resp
}

#[derive(Deserialize)]
pub struct NewPassword {
    new_password: String,
}

/// Set, change or (with an empty string) remove the password. Caller is authenticated.
pub async fn set_password(State(app): State<App>, Json(r): Json<NewPassword>) -> Result<Response, ApiError> {
    let pw = r.new_password;
    if !pw.is_empty() && pw.len() < 4 {
        return Err(ApiError::bad("Password must be at least 4 characters"));
    }
    let hash = if pw.is_empty() {
        None
    } else {
        Some(tokio::task::spawn_blocking(move || hash_password(&pw, None)).await.map_err(ApiError::bad)?)
    };
    let set = hash.is_some();
    app.update_settings(|s| {
        s.password_hash = hash;
        s.secret = random_hex(32); // signs out every other browser
    })?;
    app.log("info", if set { "UI password changed" } else { "UI password removed" });
    let mut resp = ok()?.into_response();
    if let Some(token) = session_token(&app) {
        set_cookie(&mut resp, &token, COOKIE_MAX_AGE);
    }
    Ok(resp)
}
