//! Shared application state. Everything the UI shows lives in one `Status` value
//! published through a `watch` channel, so the UI gets push updates for free.

use crate::catalog::Station;
use crate::config::{Paths, Settings};
use crate::history::History;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{mpsc, watch};

/// "HH:MM" in the device's local time for a unix-ms timestamp (falls back to "later").
pub fn fmt_local_time(ms: u64) -> String {
    std::process::Command::new("date")
        .args(["-d", &format!("@{}", ms / 1000), "+%H:%M"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "later".into())
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No masjid selected.
    Idle,
    /// Watching for the selected masjid to go live.
    Waiting,
    /// Live signal received, opening the stream.
    Connecting,
    Playing,
    /// Stream dropped mid-session; reconnecting.
    Reconnecting,
    /// User pressed stop; resumes automatically next time the masjid goes live.
    Stopped,
    /// Playing a past recording (live resumes automatically afterwards).
    Recording,
}

/// The recording currently playing on the speaker.
#[derive(Clone, Debug, Serialize)]
pub struct RecordingStatus {
    pub mount: String,
    pub title: String,
    pub position_s: u64,
    pub duration_s: Option<u64>,
    pub paused: bool,
    /// "recording" (a past broadcast) or "quran".
    pub kind: &'static str,
    /// Quran: the surah number.
    pub surah: Option<u16>,
    /// Quran: the playlist being played, if any.
    pub playlist: Option<crate::quran::PlaylistCtx>,
    /// Quran: download progress (percent) while the surah is still downloading.
    pub loaded_pct: Option<u8>,
    /// Quran: the reciter (display name).
    pub reciter: Option<String>,
    /// Quran: the recitation key.
    pub reciter_key: Option<String>,
    /// Quran: playback speed.
    pub speed: f32,
}

/// The Quran was interrupted by a live broadcast and continues when it closes.
#[derive(Clone, Debug, Serialize)]
pub struct QuranResume {
    pub surah: u16,
    pub title: String,
    pub position_s: u64,
    /// Resume by itself (false if the user had paused it).
    pub auto: bool,
    pub playlist: Option<crate::quran::PlaylistCtx>,
    /// Recitation key.
    pub reciter: String,
    pub speed: f32,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Signals {
    pub sse_connected: bool,
    pub sse_last_event_ms: u64,
    pub poll_ok: bool,
    pub poll_last_ms: u64,
    /// Selected mount is listed as live by the Icecast status poll.
    pub poll_live: bool,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct OutputStatus {
    /// "bluetooth" | "wired" | "none"
    pub kind: String,
    pub device: String,
    pub label: String,
    pub open: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct BtStatus {
    pub available: bool,
    pub powered: bool,
    pub device: Option<String>,
    pub name: Option<String>,
    pub connected: bool,
    pub battery: Option<u8>,
    pub scanning: bool,
    /// Controller reset in progress.
    pub recovering: bool,
    /// The speaker refuses audio (lost pairing): it must be re-paired.
    pub needs_repair: bool,
    pub bluealsa: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub version: &'static str,
    pub phase: Phase,
    pub mount: Option<String>,
    pub station_name: Option<String>,
    pub station_location: Option<String>,
    pub phase_since_ms: u64,
    pub codec: Option<String>,
    pub stream_url: Option<String>,
    pub trigger: Option<String>,
    pub last_heard_ms: Option<u64>,
    pub autoplay: bool,
    pub volume: u8,
    pub keep_awake: bool,
    pub signals: Signals,
    pub output: OutputStatus,
    pub bt: BtStatus,
    pub password_set: bool,
    pub started_ms: u64,
    pub live_count: usize,
    /// Audio waiting in the jitter buffer while playing (ms).
    pub buffer_ms: u64,
    /// Set while livemasjid.com is refusing us and we are staying away.
    pub site_blocked_until_ms: Option<u64>,
    /// Set while a past recording is playing.
    pub recording: Option<RecordingStatus>,
    /// Set while the Quran waits for a live broadcast to end.
    pub quran_resume: Option<QuranResume>,
    /// A scheduled playlist waiting for the live broadcast to end (its name).
    pub quran_pending: Option<String>,
    /// The Quran's own volume (percent of the main volume).
    pub quran_volume: u8,
    /// Visitors may listen on their own device.
    pub browser_listening: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub t: u64,
    pub level: &'static str,
    pub msg: String,
}

/// What the Icecast status says about a live mount.
#[derive(Clone, Debug, Serialize)]
pub struct LiveInfo {
    pub server_type: String,
    pub stream_start: Option<String>,
    pub listeners: Option<u64>,
    pub title: Option<String>,
}

#[derive(Debug)]
pub enum PlayerCmd {
    PlayNow,
    Stop,
    TestTone,
    /// Selected mount changed (or settings that affect playback).
    Reconfigure,
    /// Play a listed recording on the speaker.
    PlayRecording { mount: String, title: String, url: String, duration_s: Option<u64> },
    PauseRecording(bool),
    StopRecording,
    /// Play a Quran surah (downloading it first if needed) from a position.
    PlayQuran { surah: u16, position_ms: u64, playlist: Option<crate::quran::PlaylistCtx>, reciter: String, speed: f32 },
    /// Change the playing Quran's speed.
    QuranSpeed(f32),
    /// A playlist's scheduled time has come (playlist id).
    ScheduledPlaylist(String),
    /// Seek within the playing surah.
    QuranSeek(u64),
    /// Previous (-1) / next (+1) surah.
    QuranStep(i32),
}

/// Live/offline evidence for the selected mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    SseOnline,
    PollOnline,
    PollOffline,
}

pub struct AppState {
    pub paths: Paths,
    pub settings: RwLock<Settings>,
    pub status: watch::Sender<Status>,
    pub catalog: RwLock<Vec<Station>>,
    pub catalog_fetched_ms: AtomicU64,
    /// mount (without extension) -> info for each extension variant
    pub live: RwLock<crate::monitor::LiveMap>,
    pub history: Mutex<History>,
    pub logs: Mutex<VecDeque<LogLine>>,
    pub player_tx: mpsc::UnboundedSender<PlayerCmd>,
    pub signal_tx: mpsc::UnboundedSender<Signal>,
    /// Bumped whenever the selected mount changes so monitors restart.
    pub mount_gen: watch::Sender<u64>,
    pub volume: Arc<AtomicU8>,
    pub http: reqwest::Client,
    pub sink: crate::sink::SinkHandle,
    /// Heartbeats for the systemd watchdog (ms timestamps).
    pub hb_player: AtomicU64,
    /// Back-off state for www.livemasjid.com.
    pub site: crate::site::SiteGuard,
    /// Salah times and recordings (caches + per-site guards).
    pub integrations: crate::integrations::Integrations,
    /// Quran recitation stored on the Pi.
    pub quran: Arc<crate::quran::Library>,
}

pub type App = Arc<AppState>;

impl AppState {
    pub fn log(&self, level: &'static str, msg: impl Into<String>) {
        let msg = msg.into();
        match level {
            "error" => tracing::error!("{msg}"),
            "warn" => tracing::warn!("{msg}"),
            _ => tracing::info!("{msg}"),
        }
        let mut l = self.logs.lock().unwrap();
        if l.len() >= 300 {
            l.pop_front();
        }
        l.push_back(LogLine { t: now_ms(), level, msg });
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Mutate settings, persist them and refresh the derived status fields.
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> anyhow::Result<Settings> {
        let snapshot = {
            let mut s = self.settings.write().unwrap();
            f(&mut s);
            s.sanitize();
            s.save(&self.paths.settings)?;
            s.clone()
        };
        self.volume.store(snapshot.volume, Ordering::Relaxed);
        self.sink.quran_volume.store(snapshot.quran_volume, Ordering::Relaxed);
        self.sink.set_keep_awake(snapshot.keep_awake);
        let (loc, last) = self.station_meta(snapshot.mount.as_deref());
        self.status.send_modify(|st| {
            st.autoplay = snapshot.autoplay;
            st.volume = snapshot.volume;
            st.quran_volume = snapshot.quran_volume;
            st.keep_awake = snapshot.keep_awake;
            st.password_set = snapshot.password_hash.is_some();
            st.browser_listening = snapshot.browser_listening;
            st.mount = snapshot.mount.clone();
            st.station_name = snapshot.station_name.clone();
            st.station_location = loc;
            st.last_heard_ms = last;
            st.bt.device = snapshot.bt_device.clone();
            st.bt.name = snapshot.bt_name.clone();
        });
        Ok(snapshot)
    }

    pub fn station_meta(&self, mount: Option<&str>) -> (Option<String>, Option<u64>) {
        let Some(m) = mount else { return (None, None) };
        let loc = self
            .catalog
            .read()
            .unwrap()
            .iter()
            .find(|s| s.mount == m)
            .map(|s| s.location.clone())
            .filter(|l| !l.is_empty());
        let last = self.history.lock().unwrap().last_heard(m);
        (loc, last)
    }

    pub fn set_phase(&self, phase: Phase) {
        self.status.send_if_modified(|st| {
            if st.phase != phase {
                st.phase = phase;
                st.phase_since_ms = now_ms();
                true
            } else {
                false
            }
        });
    }
}
