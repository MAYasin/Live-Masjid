//! Quran recitation stored on the Pi: library, playback controls, downloads.
//! Pause and stop use the recording endpoints (`/recordings/pause`, `/recordings/stop`).

use crate::quran::{self, LibraryInfo};
use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::json;

pub async fn library(State(app): State<App>) -> ApiResult<Json<LibraryInfo>> {
    let lib = app.quran.clone();
    // Reads 228 file sizes: off the async threads.
    let info = tokio::task::spawn_blocking(move || lib.info()).await.map_err(ApiError::bad)?;
    Ok(Json(info))
}

fn surah(n: u16) -> Result<u16, ApiError> {
    if quran::valid(n) { Ok(n) } else { Err(ApiError::bad("no such surah")) }
}

#[derive(Deserialize)]
pub struct PlayReq {
    surah: u16,
    #[serde(default)]
    position_s: u64,
}

pub async fn play(State(app): State<App>, Json(r): Json<PlayReq>) -> ApiResult {
    let n = surah(r.surah)?;
    let _ = app.player_tx.send(PlayerCmd::PlayQuran { surah: n, position_ms: r.position_s * 1000 });
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
    /// Download one surah, or start/stop downloading all.
    surah: Option<u16>,
    all: Option<bool>,
}

pub async fn download(State(app): State<App>, Json(r): Json<DownloadReq>) -> ApiResult {
    if let Some(n) = r.surah {
        app.quran.request(surah(n)?);
    }
    if let Some(on) = r.all {
        app.quran.download_all(on);
        app.log("info", if on { "Quran: downloading all surahs in the background" } else { "Quran: stopped downloading all" });
    }
    ok()
}

#[derive(Deserialize)]
pub struct DeleteReq {
    surah: Option<u16>,
}

/// Remove downloaded surahs (one, or all) to free space. The playing surah is kept.
pub async fn delete(State(app): State<App>, Json(r): Json<DeleteReq>) -> ApiResult {
    let n = r.surah.map(surah).transpose()?;
    let playing = app.status.borrow().recording.as_ref().and_then(|r| r.surah);
    let lib = app.quran.clone();
    let removed = tokio::task::spawn_blocking(move || lib.delete(n, playing)).await.map_err(ApiError::bad)?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}
