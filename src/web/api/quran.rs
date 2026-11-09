//! Quran stored on the Pi: reciters, library, playlists, playback controls, downloads.
//! Pause and stop use the recording endpoints (`/recordings/pause`, `/recordings/stop`).

use crate::quran::{self, LibraryInfo, Playlist, Reciter};
use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct ReciterQuery {
    reciter: Option<String>,
}

/// The recitation asked for, or the app's default.
fn reciter_key(app: &App, asked: Option<&str>) -> String {
    asked.filter(|k| quran::valid_key(k)).map(str::to_string).unwrap_or_else(|| app.settings().quran_reciter)
}

/// Surahs of one recitation (default: the app's reciter) with download states.
pub async fn library(State(app): State<App>, Query(q): Query<ReciterQuery>) -> ApiResult<Json<LibraryInfo>> {
    let key = reciter_key(&app, q.reciter.as_deref());
    let lib = app.quran.clone();
    // Reads a few hundred file sizes: off the async threads.
    let info = tokio::task::spawn_blocking(move || lib.info(&key)).await.map_err(ApiError::bad)?;
    Ok(Json(info))
}

/// Every recitation, popular first, plus the default.
pub async fn reciters(State(app): State<App>) -> Json<serde_json::Value> {
    let list: Vec<Reciter> = app.quran.reciters();
    Json(json!({ "default": app.settings().quran_reciter, "reciters": list }))
}

#[derive(Deserialize)]
pub struct SetReciter {
    reciter: String,
}

/// Choose the app's default reciter.
pub async fn set_reciter(State(app): State<App>, Json(r): Json<SetReciter>) -> ApiResult {
    let rc = app.quran.reciter(&r.reciter).ok_or_else(|| ApiError::bad("Unknown reciter"))?;
    app.update_settings(|s| s.quran_reciter = rc.key.clone())?;
    app.log("info", format!("Quran reciter: {}", rc.label()));
    ok()
}

fn surah(n: u16) -> Result<u16, ApiError> {
    if quran::valid(n) { Ok(n) } else { Err(ApiError::bad("no such surah")) }
}

#[derive(Deserialize)]
pub struct PlayReq {
    /// A surah on its own, or…
    surah: Option<u16>,
    /// …a playlist, from entry `index`.
    playlist: Option<String>,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    position_s: u64,
    /// Recitation (default: the playlist's, else the app's).
    reciter: Option<String>,
    /// Speed (default: the playlist's, else the app's).
    speed: Option<f32>,
}

pub async fn play(State(app): State<App>, Json(r): Json<PlayReq>) -> ApiResult {
    let (n, playlist, pinned, pinned_speed) = match (&r.playlist, r.surah) {
        (Some(id), _) => {
            let p = app.quran.playlist(id).ok_or_else(|| ApiError::bad("That playlist no longer exists"))?;
            let ctx = quran::PlaylistCtx::new(&p, r.index);
            (ctx.surah(), Some(ctx), p.reciter, p.speed)
        }
        (None, Some(n)) => (surah(n)?, None, None, None),
        (None, None) => return Err(ApiError::bad("choose a surah or a playlist")),
    };
    let key = reciter_key(&app, r.reciter.as_deref().or(pinned.as_deref()));
    if !app.quran.resolve(Some(&key)).has(n) {
        return Err(ApiError::bad(format!("This reciter doesn't have {}", quran::name(n))));
    }
    let speed = r.speed.or(pinned_speed).unwrap_or_else(|| app.settings().quran_speed).clamp(0.5, 2.0);
    let _ = app.player_tx.send(PlayerCmd::PlayQuran { surah: n, position_ms: r.position_s * 1000, playlist, reciter: key, speed });
    ok()
}

pub async fn playlists(State(app): State<App>) -> Json<Vec<Playlist>> {
    Json(app.quran.playlists())
}

#[derive(Deserialize)]
pub struct SavePlaylist {
    id: Option<String>,
    name: String,
    surahs: Vec<u16>,
    #[serde(default)]
    repeat: bool,
    reciter: Option<String>,
    #[serde(default)]
    days: Vec<u8>,
    time: Option<String>,
    speed: Option<f32>,
}

pub async fn save_playlist(State(app): State<App>, Json(r): Json<SavePlaylist>) -> ApiResult<Json<Playlist>> {
    let p = Playlist {
        id: r.id.unwrap_or_default(),
        name: r.name,
        surahs: r.surahs,
        repeat: r.repeat,
        reciter: r.reciter.filter(|k| !k.is_empty()),
        days: r.days,
        time: r.time.filter(|t| !t.is_empty()),
        speed: r.speed,
    };
    app.quran.save_playlist(p).map(Json).map_err(ApiError::bad)
}

#[derive(Deserialize)]
pub struct PlaylistRef {
    id: String,
}

pub async fn delete_playlist(State(app): State<App>, Json(r): Json<PlaylistRef>) -> ApiResult {
    app.quran.delete_playlist(&r.id).map_err(ApiError::bad)?;
    ok()
}

#[derive(Deserialize)]
pub struct SeekReq {
    position_s: u64,
}

pub async fn seek(State(app): State<App>, Json(r): Json<SeekReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::QuranSeek(r.position_s * 1000));
    ok()
}

#[derive(Deserialize)]
pub struct StepReq {
    by: i32,
}

pub async fn step(State(app): State<App>, Json(r): Json<StepReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::QuranStep(r.by.clamp(-1, 1)));
    ok()
}

#[derive(Deserialize)]
pub struct DownloadReq {
    /// Download one surah, or start/stop downloading all of the recitation.
    surah: Option<u16>,
    all: Option<bool>,
    reciter: Option<String>,
}

pub async fn download(State(app): State<App>, Json(r): Json<DownloadReq>) -> ApiResult {
    let key = reciter_key(&app, r.reciter.as_deref());
    if let Some(n) = r.surah {
        app.quran.request(&key, surah(n)?);
    }
    if let Some(on) = r.all {
        app.quran.download_all(on.then_some(key.as_str()));
        app.log("info", if on { "Quran: downloading all surahs in the background" } else { "Quran: stopped downloading all" });
    }
    ok()
}

#[derive(Deserialize)]
pub struct DeleteReq {
    surah: Option<u16>,
    reciter: Option<String>,
}

/// Remove downloads (one surah of a recitation, or everything) to free space.
/// The surah playing is kept.
pub async fn delete(State(app): State<App>, Json(r): Json<DeleteReq>) -> ApiResult {
    let what = match r.surah {
        Some(n) => Some((reciter_key(&app, r.reciter.as_deref()), surah(n)?)),
        None => None,
    };
    let playing = app.status.borrow().recording.as_ref().and_then(|x| Some((x.reciter_key.clone()?, x.surah?)));
    let lib = app.quran.clone();
    let removed = tokio::task::spawn_blocking(move || lib.delete(what, playing)).await.map_err(ApiError::bad)?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

#[derive(Deserialize)]
pub struct SpeedReq {
    speed: f32,
}

/// Change the playing Quran's speed. It also becomes the default speed, unless the playing
/// playlist has its own.
pub async fn speed(State(app): State<App>, Json(r): Json<SpeedReq>) -> ApiResult {
    if !r.speed.is_finite() || !(0.5..=2.0).contains(&r.speed) {
        return Err(ApiError::bad("Speed must be between 0.5× and 2×"));
    }
    let playlist = app.status.borrow().recording.as_ref().and_then(|x| x.playlist.as_ref().map(|p| p.id.clone()));
    let pinned = playlist.and_then(|id| app.quran.playlist(&id)).is_some_and(|p| p.speed.is_some());
    if !pinned {
        app.update_settings(|s| s.quran_speed = r.speed)?;
    }
    let _ = app.player_tx.send(PlayerCmd::QuranSpeed(r.speed));
    ok()
}
