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
    /// Only broadcasts that started in [from, to) (ms).
    from: Option<u64>,
    to: Option<u64>,
    /// "Fajr" | "Zuhr" (incl. Jumu'ah) | "Asr" | "Maghrib" | "Esha" | "other" (no label).
    salah: Option<String>,
}

/// Broadcasts, newest first, filtered by masjid, salah and time range; with the range's totals and
/// per-day totals for the whole retained year (calendar dots, month summaries). Sessions recorded
/// before salah labels existed get an estimate from the followed masjid's current times
/// (`salah_estimated`).
pub async fn history(State(app): State<App>, Query(q): Query<HistoryQuery>) -> Json<Value> {
    use crate::history::{per_day, salah_matches};
    let all = app.history.lock().unwrap().recent(usize::MAX, q.mount.as_deref());
    let last = all.first().map(|s| json!({ "ms": s.end_ms, "live": s.reason.is_empty() }));
    let followed = app.settings().mount;
    let needs_estimate = all.iter().any(|s| s.salah.is_none() && Some(&s.mount) == followed.as_ref());
    let times = match (&followed, needs_estimate) {
        (Some(m), true) => Some(crate::integrations::salah::get(&app, m).await).filter(|t| t.status == "ok"),
        _ => None,
    };
    let offset = crate::salah_match::utc_offset_s();
    // Each session with its label: recorded, or estimated.
    let labelled: Vec<(crate::history::Session, bool)> = all
        .into_iter()
        .map(|mut s| {
            let mut estimated = false;
            if s.salah.is_none()
                && Some(&s.mount) == followed.as_ref()
                && let Some(t) = &times
            {
                let (weekday, minute) = crate::salah_match::local_day_minute(s.start_ms, offset);
                s.salah = crate::salah_match::salah_for(t, weekday, minute);
                estimated = s.salah.is_some();
            }
            (s, estimated)
        })
        .filter(|(s, _)| q.salah.as_deref().is_none_or(|f| salah_matches(s.salah.as_deref(), f)))
        .collect();
    let days = per_day(labelled.iter().map(|(s, _)| s), offset);
    let in_range: Vec<&(crate::history::Session, bool)> = labelled
        .iter()
        .filter(|(s, _)| q.from.is_none_or(|f| s.start_ms >= f) && q.to.is_none_or(|t| s.start_ms < t))
        .collect();
    let listened_ms: u64 = in_range.iter().map(|(s, _)| s.end_ms.saturating_sub(s.start_ms)).sum();
    let sessions: Vec<Value> = in_range
        .iter()
        .take(q.limit.unwrap_or(150).min(5000))
        .map(|(s, estimated)| {
            let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
            if *estimated {
                v["salah_estimated"] = json!(true);
            }
            v
        })
        .collect();
    Json(json!({ "sessions": sessions, "total": in_range.len(), "listened_ms": listened_ms, "days": days, "last": last }))
}

pub async fn logs(State(app): State<App>) -> Json<Value> {
    let l = app.logs.lock().unwrap();
    Json(json!({ "logs": l.iter().rev().collect::<Vec<_>>() }))
}

/// Device health for the System page (network, Bluetooth, resources, app usage).
pub async fn system(State(app): State<App>) -> Json<crate::sysinfo::SystemInfo> {
    Json(crate::sysinfo::collect(&app).await)
}
