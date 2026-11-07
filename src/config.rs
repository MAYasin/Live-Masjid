//! Persistent settings. Stored as JSON in the data directory and written atomically
//! (write temp file, fsync, rename) so a power cut can never leave a corrupt file.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Selected stream mount (e.g. "lakewood"). `None` until the user picks one.
    pub mount: Option<String>,
    /// Display name of the selected masjid, cached so it survives catalog refresh failures.
    pub station_name: Option<String>,
    /// Start playing automatically whenever the selected masjid goes live.
    pub autoplay: bool,
    /// Software volume 0..=100.
    pub volume: u8,
    /// Preferred Bluetooth speaker MAC address.
    pub bt_device: Option<String>,
    pub bt_name: Option<String>,
    /// Wired fallback when Bluetooth is unavailable: "auto", "none", or an ALSA device string.
    pub fallback_output: String,
    /// Keep the audio output open (feeding silence) while waiting, so playback starts
    /// with zero device wake-up delay and Bluetooth speakers never go to sleep.
    pub keep_awake: bool,
    /// "auto" (Opus first – it goes live ~6 s before the MP3 mirror), "opus" or "mp3".
    pub codec: String,
    /// Bluetooth SBC encoder quality: "high" (default) or "medium" (shown as "Low").
    pub bt_quality: String,
    /// Let people who open the UI play the stream / recordings on their own device (off by default).
    /// The Pi's own output is unaffected.
    pub browser_listening: bool,
    /// Icecast status poll interval in seconds (backup to the server-sent events).
    /// Kept polite: the livemasjid.com page itself polls every 60 s.
    pub poll_secs: u64,
    /// Salted, iterated SHA-256 of the UI password; `None` = no password set.
    pub password_hash: Option<String>,
    /// Random secret used to derive session cookies.
    pub secret: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mount: None,
            station_name: None,
            autoplay: true,
            volume: 85,
            bt_device: None,
            bt_name: None,
            fallback_output: "auto".into(),
            keep_awake: true,
            codec: "auto".into(),
            bt_quality: "high".into(),
            browser_listening: false,
            poll_secs: 20,
            password_hash: None,
            secret: random_hex(32),
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    hex(&b)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
                Ok(mut s) => {
                    s.sanitize();
                    s
                }
                Err(e) => {
                    tracing::error!("settings file corrupt ({e}); keeping a backup and using defaults");
                    let _ = std::fs::copy(path, path.with_extension("json.corrupt"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn sanitize(&mut self) {
        self.volume = self.volume.min(100);
        self.poll_secs = self.poll_secs.clamp(10, 300);
        // "low" was removed: on some speakers its SBC bitpool cannot sustain real time (choppy audio).
        if !matches!(self.bt_quality.as_str(), "medium" | "high") {
            self.bt_quality = "high".into();
        }
        if !matches!(self.codec.as_str(), "auto" | "opus" | "mp3") {
            self.codec = "auto".into();
        }
        if self.fallback_output.trim().is_empty() {
            self.fallback_output = "auto".into();
        }
        if self.secret.len() < 32 {
            self.secret = random_hex(32);
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self).expect("settings serialize"))
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Filesystem locations. `LIVEMASJID_DATA` is set by systemd (StateDirectory).
#[derive(Clone, Debug)]
pub struct Paths {
    pub settings: PathBuf,
    pub catalog: PathBuf,
    pub history: PathBuf,
    pub site: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let dir = std::env::var_os("LIVEMASJID_DATA")
            .or_else(|| std::env::var_os("STATE_DIRECTORY"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        let _ = std::fs::create_dir_all(&dir);
        Self {
            settings: dir.join("settings.json"),
            catalog: dir.join("catalog.json"),
            history: dir.join("history.jsonl"),
            site: dir.join("site.json"),
        }
    }
}

/// Password hashing: 100k rounds of salted SHA-256 (~60 ms on a Pi Zero 2 W).
pub fn hash_password(password: &str, salt: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let salt = salt.map(str::to_owned).unwrap_or_else(|| random_hex(16));
    let mut h: [u8; 32] = Sha256::digest(format!("{salt}:{password}").as_bytes()).into();
    for _ in 0..100_000 {
        h = Sha256::digest(h).into();
    }
    format!("{salt}${}", hex(&h))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let Some((salt, _)) = stored.split_once('$') else { return false };
    ct_eq(hash_password(password, Some(salt)).as_bytes(), stored.as_bytes())
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
