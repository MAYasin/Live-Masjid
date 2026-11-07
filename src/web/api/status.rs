//! Health, live status (snapshot + server-sent events), history and activity log.

use crate::state::{App, now_ms};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Used by the installer/updater to decide whether a new build is healthy.
pub async fn health(State(app): State<App>) -> Response {
    let player = now_ms().saturating_sub(app.hb_player.load(Ordering::Relaxed));
    let sink = now_ms().saturating_sub(app.sink.heartbeat.load(Ordering::Relaxed));
    if player > 30_000 || sink > 30_000 {
        return (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response();
    }
    (StatusCode::OK, crate::VERSION).into_response()
}

pub async fn state(State(app): State<App>) -> Json<Value> {
    Json(serde_json::to_value(&*app.status.borrow()).unwrap_or_default())
}

/// Pushes the full status whenever it changes (coalesced to ≤ 8 updates/s).
pub async fn events(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = app.status.subscribe();
    let (tx, mut out) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(4);
    tokio::spawn(async move {
        loop {
            let json = serde_json::to_string(&*rx.borrow_and_update()).unwrap_or_default();
            if tx.send(Ok(Event::default().event("status").data(json))).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
            if rx.changed().await.is_err() {
                return;
            }
        }
    });
    let stream = futures_util::stream::poll_fn(move |cx| out.poll_recv(cx));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    limit: Option<usize>,
    mount: Option<String>,
}

pub async fn history(State(app): State<App>, Query(q): Query<HistoryQuery>) -> Json<Value> {
    let h = app.history.lock().unwrap();
    Json(json!({ "sessions": h.recent(q.limit.unwrap_or(200).min(2000), q.mount.as_deref()) }))
}

pub async fn logs(State(app): State<App>) -> Json<Value> {
    let l = app.logs.lock().unwrap();
    Json(json!({ "logs": l.iter().rev().collect::<Vec<_>>() }))
}

/// Device health for the System page (network, Bluetooth, resources, app usage).
pub async fn system(State(app): State<App>) -> Json<crate::sysinfo::SystemInfo> {
    Json(crate::sysinfo::collect(&app).await)
}
