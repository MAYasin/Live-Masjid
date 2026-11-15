//! Optional password protection: a salted password hash in settings, and an HttpOnly
//! session cookie derived from it (rotating the secret signs everyone out).

use crate::sync::RwLockExt;
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
    let s = app.settings.read_lock();
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

/// A too-short password, unless it's empty (which removes the password).
fn password_problem(pw: &str) -> Option<&'static str> {
    (!pw.is_empty() && pw.len() < 4).then_some("Password must be at least 4 characters")
}

/// Set, change or (with an empty string) remove the password. Caller is authenticated.
pub async fn set_password(State(app): State<App>, Json(r): Json<NewPassword>) -> Result<Response, ApiError> {
    let pw = r.new_password;
    if let Some(msg) = password_problem(&pw) {
        return Err(ApiError::bad(msg));
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::COOKIE, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn finds_the_session_cookie() {
        assert_eq!(cookie(&headers(&["a=1; lm_session=abc; b=2"]), COOKIE), Some("abc"));
        assert_eq!(cookie(&headers(&["x=1", "lm_session=def"]), COOKIE), Some("def"));
        assert_eq!(cookie(&headers(&[" lm_session = spaced "]), COOKIE), None);
        assert_eq!(cookie(&headers(&["lm_session="]), COOKIE), Some(""));
        assert_eq!(cookie(&HeaderMap::new(), COOKIE), None);
    }

    #[test]
    fn cookie_name_must_match_exactly() {
        assert_eq!(cookie(&headers(&["lm_session_extra=no"]), COOKIE), None);
        assert_eq!(cookie(&headers(&["xlm_session=no"]), COOKIE), None);
    }

    #[test]
    fn sets_a_strict_httponly_cookie() {
        let mut resp = Response::new(Body::empty());
        set_cookie(&mut resp, "token123", 400);
        let v = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert_eq!(v, "lm_session=token123; Path=/; HttpOnly; SameSite=Strict; Max-Age=400");
    }

    #[test]
    fn logout_expires_the_cookie() {
        let mut resp = Response::new(Body::empty());
        set_cookie(&mut resp, "", 0);
        let v = resp.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(v.starts_with("lm_session=; "));
        assert!(v.ends_with("Max-Age=0"));
    }

    #[test]
    fn an_unsendable_token_sets_no_cookie() {
        let mut resp = Response::new(Body::empty());
        set_cookie(&mut resp, "bad
token", 10);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn login_without_a_password_and_too_short_passwords() {
        let dir = std::env::temp_dir().join(format!("lm-auth-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let resp = login(State(app.clone()), Json(Login { password: "anything".into() })).await.ok().expect("open");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_none(), "no session without a password");
        let Err(e) = set_password(State(app.clone()), Json(NewPassword { new_password: "abc".into() })).await else { panic!("short password") };
        assert_eq!(e.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(app.settings().password_hash.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn password_length_rule() {
        assert_eq!(password_problem(""), None, "empty removes the password");
        assert!(password_problem("a").is_some());
        assert!(password_problem("abc").is_some());
        assert_eq!(password_problem("abcd"), None);
    }
}
