//! Web UI + JSON API. The UI is a single embedded HTML file; live updates are pushed
//! over server-sent events from the shared `Status` watch channel.

use crate::config::{ct_eq, hash_password, verify_password};
use crate::state::{App, PlayerCmd, now_ms};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::time::Duration;

const INDEX: &str = include_str!("../ui/index.html");
const COOKIE: &str = "lm_session";

type R = Result<Response, (StatusCode, String)>;

fn bad(msg: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.to_string())
}

pub fn router(app: App) -> Router {
    let api = Router::new()
        .route("/state", get(state))
        .route("/events", get(events))
        .route("/stations", get(stations))
        .route("/stations/refresh", post(stations_refresh))
        .route("/select", post(select))
        .route("/settings", post(settings))
        .route("/config", get(config_view))
        .route("/player/{action}", post(player))
        .route("/history", get(history))
        .route("/logs", get(logs))
        .route("/bt/devices", get(bt_devices))
        .route("/bt/scan", post(bt_scan))
        .route("/bt/use", post(bt_use))
        .route("/bt/stop", post(bt_stop))
        .route("/bt/forget", post(bt_forget))
        .route("/outputs", get(outputs))
        .route("/password", post(password))
        .route("/logout", post(logout))
        .route("/system/{action}", post(system))
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .nest("/api", api)
        .with_state(app)
}

fn session_token(app: &App) -> Option<String> {
    use sha2::{Digest, Sha256};
    let s = app.settings.read().unwrap();
    let hash = s.password_hash.as_ref()?;
    Some(crate::config::hex(&Sha256::digest(format!("{}|{hash}", s.secret).as_bytes())))
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(name)?.strip_prefix('='))
}

async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Some(tok) = session_token(&app) {
        let ok = cookie_value(req.headers(), COOKIE).is_some_and(|c| ct_eq(c.as_bytes(), tok.as_bytes()));
        if !ok {
            return (StatusCode::UNAUTHORIZED, "login required").into_response();
        }
    }
    next.run(req).await
}

fn set_cookie(tok: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("{COOKIE}={tok}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}")).unwrap()
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX,
    )
        .into_response()
}

async fn manifest() -> Response {
    let m = json!({
        "name": "Live Masjid", "short_name": "Live Masjid", "start_url": "/", "display": "standalone",
        "background_color": "#0a0a0a", "theme_color": "#0a0a0a",
        "icons": [{"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml"}]
    });
    ([(header::CONTENT_TYPE, "application/manifest+json")], m.to_string()).into_response()
}

async fn icon() -> Response {
    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="12" fill="#18181b"/><g fill="#fafafa"><path d="M32 13c-3.5 5-11 7.5-11 15v3h22v-3c0-7.5-7.5-10-11-15z"/><rect x="17" y="33" width="30" height="16" rx="1.5"/><circle cx="32" cy="10" r="2"/></g><path d="M29 49v-6a3 3 0 0 1 6 0v6z" fill="#18181b"/></svg>"##;
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "max-age=86400")], SVG).into_response()
}

async fn health(State(app): State<App>) -> Response {
    let age = now_ms().saturating_sub(app.hb_player.load(std::sync::atomic::Ordering::Relaxed));
    let sink_age = now_ms().saturating_sub(app.sink.heartbeat.load(std::sync::atomic::Ordering::Relaxed));
    if age > 30_000 || sink_age > 30_000 {
        return (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response();
    }
    (StatusCode::OK, env!("CARGO_PKG_VERSION")).into_response()
}

async fn state(State(app): State<App>) -> Json<Value> {
    Json(serde_json::to_value(&*app.status.borrow()).unwrap_or_default())
}

async fn events(State(app): State<App>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = app.status.subscribe();
    let stream = async_stream(move |tx| async move {
        loop {
            let json = serde_json::to_string(&*rx.borrow_and_update()).unwrap_or_default();
            if tx.send(Ok(Event::default().event("status").data(json))).await.is_err() {
                return;
            }
            // Coalesce bursts of updates.
            tokio::time::sleep(Duration::from_millis(120)).await;
            if rx.changed().await.is_err() {
                return;
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Tiny channel-backed stream helper (avoids pulling in async-stream).
fn async_stream<F, Fut>(f: F) -> impl futures_util::Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<Event, Infallible>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(f(tx));
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

async fn stations(State(app): State<App>) -> Json<Value> {
    let live = app.live.read().unwrap();
    let hist = app.history.lock().unwrap();
    let last = hist.all_last_heard();
    let selected = app.settings().mount;
    let catalog = app.catalog.read().unwrap();
    let mut list: Vec<Value> = catalog
        .iter()
        .map(|s| {
            json!({
                "mount": s.mount, "name": s.name, "location": s.location, "genre": s.genre,
                "live": crate::monitor::is_live(&live, &s.mount),
                "last_heard_ms": last.get(&s.mount),
                "selected": selected.as_deref() == Some(&s.mount),
            })
        })
        .collect();
    // Live mounts that aren't in the directory (yet).
    for m in live.keys() {
        if !catalog.iter().any(|s| s.mount == *m) {
            list.push(json!({"mount": m, "name": m, "location": "", "genre": "Other", "live": true,
                "last_heard_ms": last.get(m), "selected": selected.as_deref() == Some(m)}));
        }
    }
    Json(json!({
        "stations": list,
        "fetched_ms": app.catalog_fetched_ms.load(std::sync::atomic::Ordering::Relaxed),
    }))
}

async fn stations_refresh(State(app): State<App>) -> R {
    let n = crate::catalog::refresh(&app).await.map_err(bad)?;
    Ok(Json(json!({"count": n})).into_response())
}

#[derive(Deserialize)]
struct SelectReq {
    mount: Option<String>,
}

async fn select(State(app): State<App>, Json(req): Json<SelectReq>) -> R {
    let mount = req.mount.map(|m| m.trim().trim_start_matches('/').to_string()).filter(|m| !m.is_empty());
    if let Some(m) = &mount {
        if m.len() > 80 || !m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err(bad("invalid mount"));
        }
    }
    let name = mount.as_ref().map(|m| {
        app.catalog.read().unwrap().iter().find(|s| &s.mount == m).map(|s| s.name.clone()).unwrap_or_else(|| m.clone())
    });
    app.update_settings(|s| {
        s.mount = mount.clone();
        s.station_name = name.clone();
    })
    .map_err(bad)?;
    app.mount_gen.send_modify(|g| *g += 1);
    let _ = app.player_tx.send(PlayerCmd::Reconfigure);
    app.log("info", format!("Selected masjid: {}", name.as_deref().unwrap_or("none")));
    Ok(Json(json!({"ok": true})).into_response())
}

async fn config_view(State(app): State<App>) -> Json<Value> {
    let s = app.settings();
    Json(json!({"codec": s.codec, "poll_secs": s.poll_secs, "fallback_output": s.fallback_output}))
}

#[derive(Deserialize)]
struct SettingsReq {
    autoplay: Option<bool>,
    volume: Option<u8>,
    keep_awake: Option<bool>,
    codec: Option<String>,
    poll_secs: Option<u64>,
    fallback_output: Option<String>,
}

async fn settings(State(app): State<App>, Json(r): Json<SettingsReq>) -> R {
    let resume = r.autoplay == Some(true);
    let out_changed = r.fallback_output.is_some();
    app.update_settings(|s| {
        if let Some(v) = r.autoplay { s.autoplay = v; }
        if let Some(v) = r.volume { s.volume = v; }
        if let Some(v) = r.keep_awake { s.keep_awake = v; }
        if let Some(v) = r.codec { s.codec = v; }
        if let Some(v) = r.poll_secs { s.poll_secs = v; }
        if let Some(v) = r.fallback_output { s.fallback_output = v; }
    })
    .map_err(bad)?;
    if out_changed {
        app.sink.retarget(crate::bluetooth::compute_targets(&app));
    }
    if resume {
        // Autoplay just enabled: if the masjid is already live, start now.
        let live = app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m));
        if live {
            let _ = app.signal_tx.send(crate::state::Signal::PollOnline);
        }
    }
    Ok(Json(json!({"ok": true})).into_response())
}

async fn player(State(app): State<App>, axum::extract::Path(action): axum::extract::Path<String>) -> R {
    let cmd = match action.as_str() {
        "play" => PlayerCmd::PlayNow,
        "stop" => PlayerCmd::Stop,
        "test" => PlayerCmd::TestTone,
        _ => return Err(bad("unknown action")),
    };
    let _ = app.player_tx.send(cmd);
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct HistQ {
    limit: Option<usize>,
    mount: Option<String>,
}

async fn history(State(app): State<App>, Query(q): Query<HistQ>) -> Json<Value> {
    let h = app.history.lock().unwrap();
    Json(json!({ "sessions": h.recent(q.limit.unwrap_or(200).min(2000), q.mount.as_deref()) }))
}

async fn logs(State(app): State<App>) -> Json<Value> {
    let l = app.logs.lock().unwrap();
    Json(json!({ "logs": l.iter().rev().collect::<Vec<_>>() }))
}

async fn bt_devices(State(app): State<App>) -> R {
    let list = crate::bluetooth::list(&app).await.map_err(bad)?;
    Ok(Json(json!({ "devices": list })).into_response())
}

async fn bt_scan(State(app): State<App>) -> R {
    tokio::spawn(crate::bluetooth::scan(app.clone(), 20));
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct MacReq {
    mac: String,
}

async fn bt_use(State(app): State<App>, Json(r): Json<MacReq>) -> R {
    let msg = crate::bluetooth::use_device(&app, &r.mac).await.map_err(bad)?;
    Ok(Json(json!({"ok": true, "message": msg})).into_response())
}

async fn bt_stop(State(app): State<App>, Json(r): Json<MacReq>) -> R {
    crate::bluetooth::stop_using(&app, &r.mac, false).await.map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn bt_forget(State(app): State<App>, Json(r): Json<MacReq>) -> R {
    crate::bluetooth::stop_using(&app, &r.mac, true).await.map_err(bad)?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn outputs() -> Json<Value> {
    let cards: Vec<Value> = crate::bluetooth::wired_cards()
        .into_iter()
        .map(|(id, desc)| json!({"device": format!("default:CARD={id}"), "label": desc}))
        .collect();
    Json(json!({ "cards": cards }))
}

#[derive(Deserialize)]
struct LoginReq {
    password: String,
}

async fn login(State(app): State<App>, Json(r): Json<LoginReq>) -> R {
    let stored = app.settings().password_hash;
    let Some(stored) = stored else {
        return Ok(Json(json!({"ok": true})).into_response());
    };
    let ok = tokio::task::spawn_blocking(move || verify_password(&r.password, &stored)).await.unwrap_or(false);
    if !ok {
        tokio::time::sleep(Duration::from_millis(600)).await;
        return Err((StatusCode::UNAUTHORIZED, "wrong password".into()));
    }
    let tok = session_token(&app).unwrap_or_default();
    let mut resp = Json(json!({"ok": true})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie(&tok, 400 * 86400));
    Ok(resp)
}

async fn logout() -> Response {
    let mut resp = Json(json!({"ok": true})).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, set_cookie("", 0));
    resp
}

#[derive(Deserialize)]
struct PasswordReq {
    new_password: String,
}

async fn password(State(app): State<App>, Json(r): Json<PasswordReq>) -> R {
    // Caller is already authenticated (middleware). Empty = remove password.
    let pw = r.new_password;
    if !pw.is_empty() && pw.len() < 4 {
        return Err(bad("password must be at least 4 characters"));
    }
    let hash = if pw.is_empty() {
        None
    } else {
        Some(tokio::task::spawn_blocking(move || hash_password(&pw, None)).await.map_err(bad)?)
    };
    let set = hash.is_some();
    app.update_settings(|s| {
        s.password_hash = hash;
        s.secret = crate::config::random_hex(32); // invalidates other sessions
    })
    .map_err(bad)?;
    let mut resp = Json(json!({"ok": true})).into_response();
    if let Some(tok) = session_token(&app) {
        resp.headers_mut().insert(header::SET_COOKIE, set_cookie(&tok, 400 * 86400));
    }
    app.log("info", if set { "UI password changed" } else { "UI password removed" });
    Ok(resp)
}

async fn system(State(app): State<App>, axum::extract::Path(action): axum::extract::Path<String>) -> R {
    match action.as_str() {
        "restart" => {
            app.log("info", "Restart requested from UI");
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::process::exit(0); // systemd restarts us
            });
        }
        "update" => {
            let out = tokio::process::Command::new("sudo")
                .args(["-n", "/usr/bin/systemctl", "start", "--no-block", "livemasjid-update.service"])
                .output()
                .await
                .map_err(bad)?;
            if !out.status.success() {
                return Err(bad(String::from_utf8_lossy(&out.stderr).trim().to_string()));
            }
            app.log("info", "Update check started");
        }
        _ => return Err(bad("unknown action")),
    }
    Ok(Json(json!({"ok": true})).into_response())
}
