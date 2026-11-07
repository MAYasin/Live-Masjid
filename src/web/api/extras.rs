//! Salah times and recordings. All third-party fetching happens on the Pi, cached
//! (see `crate::integrations`), so browsing the UI never adds load to those sites.

use crate::integrations::{recordings, salah, valid_mount};
use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct MountQuery {
    mount: Option<String>,
}

/// The requested mount, or the selected one.
fn mount_or_selected(app: &App, q: MountQuery) -> Result<String, ApiError> {
    let m = q.mount.or_else(|| app.settings().mount).ok_or_else(|| ApiError::bad("no masjid selected"))?;
    if !valid_mount(&m) {
        return Err(ApiError::bad("invalid masjid"));
    }
    Ok(m)
}

pub async fn salah_times(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult<Json<salah::SalahTimes>> {
    let mount = mount_or_selected(&app, q)?;
    Ok(Json(salah::get(&app, &mount).await))
}

pub async fn recording_list(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult<Json<recordings::RecordingList>> {
    let mount = mount_or_selected(&app, q)?;
    Ok(Json(recordings::list(&app, &mount).await))
}

#[derive(Deserialize)]
pub struct PlayReq {
    mount: String,
    id: String,
}

pub async fn play(State(app): State<App>, Json(r): Json<PlayReq>) -> ApiResult {
    if !valid_mount(&r.mount) {
        return Err(ApiError::bad("invalid masjid"));
    }
    // Only recordings we listed ourselves can be played (no arbitrary URLs).
    let rec = recordings::find(&app, &r.mount, &r.id).await.ok_or_else(|| ApiError::bad("recording not found; refresh the list"))?;
    let _ = app.player_tx.send(PlayerCmd::PlayRecording { mount: r.mount, title: rec.title, url: rec.url, duration_s: rec.duration_s });
    ok()
}

#[derive(Deserialize)]
pub struct PauseReq {
    pause: bool,
}

pub async fn pause(State(app): State<App>, Json(r): Json<PauseReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::PauseRecording(r.pause));
    ok()
}

pub async fn stop(State(app): State<App>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::StopRecording);
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ listening in the browser

fn browser_listening_enabled(app: &App) -> Result<(), ApiError> {
    if app.settings().browser_listening {
        Ok(())
    } else {
        Err(ApiError::bad("Listening in the browser is turned off (Settings)"))
    }
}

/// Stream URLs for the visitor's own device. MP3 first (plays everywhere, incl. iOS Safari),
/// relay before origin (keeps load off the origin). The Pi's own output is untouched.
pub async fn listen(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult {
    browser_listening_enabled(&app)?;
    let mount = mount_or_selected(&app, q)?;
    let base = mount.strip_suffix(".mp3").unwrap_or(&mount).to_string();
    let live = crate::monitor::is_live(&app.live.read().unwrap(), &base);
    let mut sources = Vec::new();
    for (path, kind) in [(format!("{base}.mp3"), "audio/mpeg"), (base.clone(), "audio/ogg")] {
        for (host, port) in crate::net::STREAM_HOSTS.iter().rev() {
            sources.push(json!({ "url": format!("http://{host}:{port}/{path}"), "type": kind }));
        }
    }
    Ok(Json(json!({ "mount": base, "live": live, "sources": sources })))
}

#[derive(Deserialize)]
pub struct RecordingRef {
    mount: String,
    id: String,
}

/// The file URL of a listed recording, for playing it on the visitor's device.
pub async fn recording_url(State(app): State<App>, Query(r): Query<RecordingRef>) -> ApiResult {
    browser_listening_enabled(&app)?;
    if !valid_mount(&r.mount) {
        return Err(ApiError::bad("invalid masjid"));
    }
    let rec = recordings::find(&app, &r.mount, &r.id).await.ok_or_else(|| ApiError::bad("recording not found; refresh the list"))?;
    Ok(Json(json!({ "url": rec.url, "title": rec.title })))
}
