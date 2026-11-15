//! Playback controls and settings.

use crate::sync::RwLockExt;
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

/// The UI offers only these two (real "low" was removed: it underran on the owner's speaker).
fn valid_quality(q: &str) -> bool {
    matches!(q, "medium" | "high")
}

/// Keep the live delay in the range the UI offers (2–30 s).
fn clamp_delay(secs: u32) -> u32 {
    secs.clamp(2, 30)
}

pub async fn settings(State(app): State<App>, Json(p): Json<SettingsPatch>) -> ApiResult {
    let autoplay_on = p.autoplay == Some(true);
    let output_changed = p.fallback_output.is_some();
    let quality = p.bt_quality.as_deref().filter(|q| valid_quality(q)).map(str::to_string);
    if p.bt_quality.is_some() && quality.is_none() {
        return Err(ApiError::bad("quality must be medium or high"));
    }
    app.update_settings(|s| {
        if let Some(v) = p.autoplay { s.autoplay = v; }
        if let Some(v) = p.volume { s.volume = v; }
        if let Some(v) = p.keep_awake { s.keep_awake = v; }
        if let Some(v) = p.codec { s.codec = v; }
        if let Some(v) = p.live_delay_s { s.live_delay_s = clamp_delay(v); }
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
        let live = app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read_lock(), m));
        if live {
            let _ = app.signal_tx.send(Signal::PollOnline);
        }
    }
    ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_medium_and_high_are_accepted() {
        assert!(valid_quality("medium"));
        assert!(valid_quality("high"));
        assert!(!valid_quality("low"));
        assert!(!valid_quality("HIGH"));
        assert!(!valid_quality(""));
    }

    fn app_with_channels() -> (App, std::path::PathBuf, tokio::sync::mpsc::UnboundedReceiver<PlayerCmd>, tokio::sync::mpsc::UnboundedReceiver<Signal>) {
        let dir = std::env::temp_dir().join(format!("lm-player-api-{}", crate::config::random_hex(6)));
        let mut app = crate::state::test_app(&dir);
        let (player_tx, player_rx) = tokio::sync::mpsc::unbounded_channel();
        let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = std::sync::Arc::get_mut(&mut app).expect("sole owner");
        state.player_tx = player_tx;
        state.signal_tx = signal_tx;
        (app, dir, player_rx, signal_rx)
    }

    fn patch(json: &str) -> Json<SettingsPatch> {
        Json(serde_json::from_str(json).expect("patch"))
    }

    #[tokio::test]
    async fn actions_reach_the_player() {
        let (app, dir, mut rx, _) = app_with_channels();
        for (name, want) in [("play", "PlayNow"), ("stop", "Stop"), ("test", "TestTone")] {
            assert!(action(State(app.clone()), Path(name.into())).await.is_ok());
            assert_eq!(format!("{:?}", rx.try_recv().expect("sent")), want);
        }
        assert!(action(State(app.clone()), Path("reboot".into())).await.is_err());
        assert!(rx.try_recv().is_err(), "nothing sent for an unknown action");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn every_setting_is_saved() {
        let (app, dir, _, _) = app_with_channels();
        let body = r#"{"autoplay":false,"volume":40,"keep_awake":true,"codec":"mp3","auto_update":false,
            "quran_volume":70,"poll_secs":45,"fallback_output":"default:CARD=Headphones","browser_listening":false}"#;
        assert!(settings(State(app.clone()), patch(body)).await.is_ok());
        let s = app.settings();
        assert!(!s.autoplay && s.keep_awake && !s.auto_update && !s.browser_listening);
        assert_eq!((s.volume, s.quran_volume, s.poll_secs), (40, 70, 45));
        assert_eq!(s.codec, "mp3");
        assert_eq!(s.fallback_output, "default:CARD=Headphones");
        let Json(v) = config(State(app.clone())).await.ok().expect("config");
        assert_eq!(v["codec"], "mp3");
        assert_eq!(v["browser_listening"], false);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn autoplay_on_starts_a_live_masjid() {
        let (app, dir, _, mut signals) = app_with_channels();
        assert!(settings(State(app.clone()), patch(r#"{"autoplay":true}"#)).await.is_ok());
        assert!(signals.try_recv().is_err(), "no masjid selected");

        app.update_settings(|s| s.mount = Some("m".into())).unwrap();
        assert!(settings(State(app.clone()), patch(r#"{"autoplay":true}"#)).await.is_ok());
        assert!(signals.try_recv().is_err(), "not live");

        let info = crate::state::LiveInfo { server_type: String::new(), stream_start: None, listeners: None, title: None };
        app.live.write_lock().insert("m".into(), vec![("relay".into(), info)]);
        assert!(settings(State(app.clone()), patch(r#"{"autoplay":true}"#)).await.is_ok());
        assert!(matches!(signals.try_recv(), Ok(Signal::PollOnline)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn quality_needs_the_host_in_a_container() {
        if crate::platform::is_native() {
            return; // would reconfigure the real bluez-alsa service
        }
        let (app, dir, _, _) = app_with_channels();
        assert!(settings(State(app.clone()), patch(r#"{"bt_quality":"medium"}"#)).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn live_delay_is_clamped_to_the_ui_range() {
        assert_eq!(clamp_delay(0), 2);
        assert_eq!(clamp_delay(1), 2);
        assert_eq!(clamp_delay(2), 2);
        assert_eq!(clamp_delay(15), 15);
        assert_eq!(clamp_delay(30), 30);
        assert_eq!(clamp_delay(31), 30);
        assert_eq!(clamp_delay(u32::MAX), 30);
    }
}
