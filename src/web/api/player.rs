//! Playback controls and settings.

use crate::state::{App, PlayerCmd, Signal};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::json;

pub async fn action(State(app): State<App>, Path(action): Path<String>) -> ApiResult {
    let cmd = match action.as_str() {
        "play" => PlayerCmd::PlayNow,
        "stop" => PlayerCmd::Stop,
        "test" => PlayerCmd::TestTone,
        _ => return Err(ApiError::bad("unknown action")),
    };
    let _ = app.player_tx.send(cmd);
    ok()
}

/// Settings that aren't already part of the live status.
pub async fn config(State(app): State<App>) -> ApiResult {
    let s = app.settings();
    Ok(Json(json!({ "codec": s.codec, "live_delay_s": s.live_delay_s, "poll_secs": s.poll_secs, "fallback_output": s.fallback_output, "bt_quality": s.bt_quality, "browser_listening": s.browser_listening })))
}

#[derive(Deserialize)]
pub struct SettingsPatch {
    autoplay: Option<bool>,
    volume: Option<u8>,
    keep_awake: Option<bool>,
    codec: Option<String>,
    live_delay_s: Option<u32>,
    auto_update: Option<bool>,
    quran_volume: Option<u8>,
    poll_secs: Option<u64>,
    fallback_output: Option<String>,
    bt_quality: Option<String>,
    browser_listening: Option<bool>,
}

pub async fn settings(State(app): State<App>, Json(p): Json<SettingsPatch>) -> ApiResult {
    let autoplay_on = p.autoplay == Some(true);
    let output_changed = p.fallback_output.is_some();
    let quality = p.bt_quality.clone().filter(|q| matches!(q.as_str(), "medium" | "high"));
    if p.bt_quality.is_some() && quality.is_none() {
        return Err(ApiError::bad("quality must be medium or high"));
    }
    app.update_settings(|s| {
        if let Some(v) = p.autoplay { s.autoplay = v; }
        if let Some(v) = p.volume { s.volume = v; }
        if let Some(v) = p.keep_awake { s.keep_awake = v; }
        if let Some(v) = p.codec { s.codec = v; }
        if let Some(v) = p.live_delay_s { s.live_delay_s = v.clamp(2, 30); }
        if let Some(v) = p.auto_update { s.auto_update = v; }
        if let Some(v) = p.quran_volume { s.quran_volume = v.min(100); }
        if let Some(v) = p.poll_secs { s.poll_secs = v; }
        if let Some(v) = p.fallback_output { s.fallback_output = v; }
        if let Some(v) = quality.clone() { s.bt_quality = v; }
        if let Some(v) = p.browser_listening { s.browser_listening = v; }
    })?;
    // Applies immediately: a buffer that is now too long is trimmed on the next packet.
    crate::stream::LIVE_DELAY_MS.store(u64::from(app.settings().live_delay_s) * 1000, std::sync::atomic::Ordering::Relaxed);
    if output_changed {
        app.sink.retarget(crate::bluetooth::compute_targets(&app));
    }
    if let Some(q) = quality {
        crate::bluetooth::set_quality(&app, &q).await?;
    }
    // Auto-play just switched on and the masjid is already live: start now.
    if autoplay_on {
        let live = app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m));
        if live {
            let _ = app.signal_tx.send(Signal::PollOnline);
        }
    }
    ok()
}
