//! Health, live status (snapshot + server-sent events), history and activity log.

use crate::sync::LockExt;
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
    if unhealthy(player, sink) {
        return (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response();
    }
    (StatusCode::OK, crate::VERSION).into_response()
}

/// The player and the sink both heartbeat; either one stale for over 30 s means trouble.
fn unhealthy(player_ms: u64, sink_ms: u64) -> bool {
    player_ms > 30_000 || sink_ms > 30_000
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
    let all = app.history.locked().recent(usize::MAX, q.mount.as_deref());
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
    let l = app.logs.locked();
    Json(json!({ "logs": l.iter().rev().collect::<Vec<_>>() }))
}

/// Device health for the System page (network, Bluetooth, resources, app usage).
pub async fn system(State(app): State<App>) -> Json<crate::sysinfo::SystemInfo> {
    Json(crate::sysinfo::collect(&app).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_app;

    fn app() -> App {
        let dir = std::env::temp_dir().join(format!("livemasjid-status-api-{}", crate::config::random_hex(6)));
        test_app(&dir)
    }

    #[test]
    fn heartbeat_grace_period() {
        assert!(!unhealthy(0, 0));
        assert!(!unhealthy(30_000, 30_000), "exactly 30 s is still healthy");
        assert!(unhealthy(30_001, 0), "a stale player is unhealthy");
        assert!(unhealthy(0, 30_001), "a stale sink is unhealthy");
        assert!(unhealthy(u64::MAX, u64::MAX));
    }

    #[tokio::test]
    async fn state_and_logs_reflect_the_app() {
        let app = app();
        app.log("info", "hello");
        let Json(v) = super::state(State(app.clone())).await;
        assert_eq!(v["version"], "test");
        assert_eq!(v["phase"], "idle");
        let Json(l) = super::logs(State(app.clone())).await;
        assert_eq!(l["logs"][0]["msg"], "hello");
    }

    #[tokio::test]
    async fn empty_history_has_zero_totals() {
        let app = app();
        let q = HistoryQuery { limit: None, mount: None, from: None, to: None, salah: None };
        let Json(v) = super::history(State(app), Query(q)).await;
        assert_eq!(v["total"], 0);
        assert_eq!(v["listened_ms"], 0);
        assert!(v["sessions"].as_array().expect("array").is_empty());
        assert!(v["days"].as_object().expect("object").is_empty());
    }

    #[tokio::test]
    async fn health_is_ok_when_heartbeats_are_fresh() {
        let app = app();
        assert_eq!(super::health(State(app)).await.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_fails_when_the_player_stalls() {
        let app = app();
        app.hb_player.store(0, Ordering::Relaxed);
        assert_eq!(super::health(State(app)).await.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn events_push_the_status_and_its_changes() {
        use futures_util::StreamExt;
        let app = app();
        let resp = super::events(State(app.clone())).await.into_response();
        assert_eq!(resp.headers()[axum::http::header::CONTENT_TYPE], "text/event-stream");
        let mut body = resp.into_body().into_data_stream();
        let first = body.next().await.expect("first event").expect("bytes");
        let first = String::from_utf8_lossy(&first).into_owned();
        assert!(first.contains("event: status") && first.contains("\"phase\":\"idle\""), "{first}");
        app.set_phase(crate::state::Phase::Playing);
        let second = tokio::time::timeout(Duration::from_secs(5), body.next()).await.expect("pushed").expect("event").expect("bytes");
        assert!(String::from_utf8_lossy(&second).contains("\"phase\":\"playing\""));
        // A closed page ends the pusher on its next change.
        drop(body);
        app.set_phase(crate::state::Phase::Idle);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// Salah times for `mount` that put a broadcast starting now in Asr, cached so nothing is fetched.
    fn cache_times_covering_now(app: &App, mount: &str) {
        use crate::integrations::{masjidboard::Lookup, salah};
        let (_, minute) = crate::salah_match::local_day_minute(now_ms(), crate::salah_match::utc_offset_s());
        let hhmm = format!("{:02}:{:02}", minute / 60, minute % 60);
        let asr = salah::Salah { key: "asr".into(), label: "Asr".into(), athaan: Some(hhmm.clone()), jamaat: Some(hhmm) };
        let times = salah::SalahTimes { status: "ok".into(), date: Some(salah::today()), times: vec![asr], ..Default::default() };
        let it = &app.integrations;
        it.boards.put(mount, Lookup::NotFound, Duration::from_secs(3600));
        it.salah.put(mount, times, Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn history_labels_filters_and_pages() {
        let app = app();
        app.update_settings(|s| s.mount = Some("followed".into())).unwrap();
        cache_times_covering_now(&app, "followed");
        {
            let mut h = app.history.locked();
            h.begin("other", "Other", "opus", "poll");
            let id = h.begin("followed", "Followed", "opus", "sse");
            h.finish(id, "ended", 0);
            let id = h.begin("followed", "Followed", "opus", "sse");
            h.set_salah(id, Some("Fajr".into()));
        }
        let query = |salah: Option<&str>, from: Option<u64>, to: Option<u64>, limit: Option<usize>| HistoryQuery {
            limit,
            mount: None,
            from,
            to,
            salah: salah.map(str::to_string),
        };
        let Json(v) = super::history(State(app.clone()), Query(query(None, None, None, None))).await;
        assert_eq!(v["total"], 3);
        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(sessions[0]["salah"], "Fajr", "a recorded label is kept");
        assert!(sessions[0].get("salah_estimated").is_none());
        assert_eq!(sessions[1]["salah"], "Asr", "estimated from today's times");
        assert_eq!(sessions[1]["salah_estimated"], true);
        assert!(sessions[2].get("salah").is_none(), "only the followed masjid is estimated");
        assert_eq!(v["last"]["live"], true, "the newest session is still going");
        assert_eq!(v["days"].as_object().unwrap().len(), 1);

        let Json(v) = super::history(State(app.clone()), Query(query(Some("Asr"), None, None, None))).await;
        assert_eq!(v["total"], 1);
        let Json(v) = super::history(State(app.clone()), Query(query(Some("other"), None, None, None))).await;
        assert_eq!(v["total"], 1);
        let Json(v) = super::history(State(app.clone()), Query(query(None, None, None, Some(1)))).await;
        assert_eq!(v["total"], 3);
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1, "limited");
        let Json(v) = super::history(State(app.clone()), Query(query(None, Some(now_ms() + 60_000), None, None))).await;
        assert_eq!(v["total"], 0, "nothing started in the future");
        let Json(v) = super::history(State(app.clone()), Query(query(None, None, Some(1), None))).await;
        assert_eq!(v["total"], 0, "nothing started before 1970");
    }
}
