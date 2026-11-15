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
    /// Seconds behind the live edge to play (3 = near live, 8 = balanced, 20 = extra steady, the default).
    /// Icecast's connect burst is ~17–28 s of audio; we keep only this much of it.
    pub live_delay_s: u32,
    /// Bluetooth SBC encoder quality: "high" (default) or "medium" (shown as "Low").
    pub bt_quality: String,
    /// Let people who open the UI play the stream / recordings on their own device (on by default).
    /// The Pi's own output is unaffected.
    pub browser_listening: bool,
    /// Icecast status poll interval in seconds (backup to the server-sent events).
    /// Kept polite: the livemasjid.com page itself polls every 60 s.
    pub poll_secs: u64,
    /// Salah times source per masjid (mount → masjidboardlive board id, or "none" for
    /// masjidscreen only). Masjids not listed are matched automatically.
    pub salah_boards: std::collections::BTreeMap<String, String>,
    /// Install new releases automatically (the hourly timer; "Update now" works either way).
    pub auto_update: bool,
    /// Quran volume (0–100), independent of the broadcast volume (recitations are mastered louder).
    pub quran_volume: u8,
    /// Default Quran playback speed (0.5–2.0, pitch kept); playlists may set their own.
    pub quran_speed: f32,
    /// Default Quran recitation (mp3quran key); playlists may pin their own.
    pub quran_reciter: String,
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
            live_delay_s: 20,
            bt_quality: "high".into(),
            browser_listening: true,
            poll_secs: 20,
            salah_boards: Default::default(),
            quran_reciter: crate::quran::DEFAULT_RECITER.into(),
            auto_update: true,
            quran_volume: 100,
            quran_speed: 1.0,
            password_hash: None,
            secret: random_hex(32),
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    use rand::Rng;
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    hex(&b)
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}"); // writing to a String can't fail
        s
    })
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
        self.live_delay_s = self.live_delay_s.clamp(2, 30);
        self.quran_volume = self.quran_volume.min(100);
        self.quran_speed = if self.quran_speed.is_finite() { self.quran_speed.clamp(0.5, 2.0) } else { 1.0 };
        if !crate::quran::valid_key(&self.quran_reciter) {
            self.quran_reciter = crate::quran::DEFAULT_RECITER.into();
        }
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
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
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
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
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
        Self::in_dir(&data_dir(std::env::var_os("LIVEMASJID_DATA"), std::env::var_os("STATE_DIRECTORY")))
    }

    /// The files in data folder `dir` (created if missing).
    fn in_dir(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self {
            settings: dir.join("settings.json"),
            catalog: dir.join("catalog.json"),
            history: dir.join("history.jsonl"),
            site: dir.join("site.json"),
        }
    }
}

/// The data folder: `LIVEMASJID_DATA`, else systemd's `STATE_DIRECTORY`, else `./data`.
fn data_dir(data: Option<std::ffi::OsString>, state: Option<std::ffi::OsString>) -> PathBuf {
    data.or(state).map_or_else(|| PathBuf::from("data"), PathBuf::from)
}

/// Password hashing: 100k rounds of salted SHA-256 (~60 ms on a Pi Zero 2 W).
pub fn hash_password(password: &str, salt: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let salt = salt.map_or_else(|| random_hex(16), str::to_owned);
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

#[cfg(test)]
mod password_tests {
    use super::*;

    /// Also guards the build flags: on a Pi without the ARMv8 crypto extensions, SHA-256 compiled
    /// for them dies with SIGILL (this test then crashes on the Pi instead of passing).
    #[test]
    fn password_hash_round_trip() {
        let stored = hash_password("correct horse", None);
        assert!(verify_password("correct horse", &stored));
        assert!(!verify_password("wrong", &stored));
        assert!(!verify_password("correct horse", "no-salt-separator"));
        let (salt, _) = stored.split_once('$').unwrap();
        assert_eq!(hash_password("correct horse", Some(salt)), stored, "same salt, same hash");
        assert_ne!(hash_password("correct horse", None), stored, "a fresh salt each time");
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("livemasjid-{}-{name}", random_hex(8)))
    }

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert_eq!(s.volume, 85);
        assert_eq!(s.live_delay_s, 20);
        assert_eq!(s.poll_secs, 20);
        assert_eq!(s.bt_quality, "high");
        assert_eq!(s.codec, "auto");
        assert_eq!(s.quran_volume, 100);
        assert_eq!(s.quran_speed, 1.0);
        assert_eq!(s.secret.len(), 64);
        assert_eq!(s.quran_reciter, crate::quran::DEFAULT_RECITER);
    }

    #[test]
    fn sanitize_clamps_and_repairs() {
        let mut s = Settings {
            volume: 150,
            poll_secs: 1,
            live_delay_s: 99,
            quran_volume: 101,
            quran_speed: 9.0,
            quran_reciter: "../x".into(),
            bt_quality: "low".into(),
            codec: "aac".into(),
            fallback_output: "  ".into(),
            secret: "short".into(),
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.volume, 100);
        assert_eq!(s.poll_secs, 10);
        assert_eq!(s.live_delay_s, 30);
        assert_eq!(s.quran_volume, 100);
        assert_eq!(s.quran_speed, 2.0);
        assert_eq!(s.quran_reciter, crate::quran::DEFAULT_RECITER);
        assert_eq!(s.bt_quality, "high");
        assert_eq!(s.codec, "auto");
        assert_eq!(s.fallback_output, "auto");
        assert_eq!(s.secret.len(), 64);
        s.quran_speed = f32::NAN;
        s.sanitize();
        assert_eq!(s.quran_speed, 1.0);
        s.poll_secs = 999;
        s.sanitize();
        assert_eq!(s.poll_secs, 300);
        s.live_delay_s = 0;
        s.sanitize();
        assert_eq!(s.live_delay_s, 2);
    }

    #[test]
    fn save_and_load_round_trip() {
        let p = temp_path("settings.json");
        let s = Settings { mount: Some("lakewood".into()), volume: 42, ..Settings::default() };
        s.save(&p).unwrap();
        let loaded = Settings::load(&p);
        assert_eq!(loaded.mount.as_deref(), Some("lakewood"));
        assert_eq!(loaded.volume, 42);
        assert_eq!(loaded.secret, s.secret);
        assert!(!p.with_extension("tmp").exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_missing_file_gives_defaults() {
        let p = temp_path("missing.json");
        let s = Settings::load(&p);
        assert_eq!(s.volume, 85);
        assert!(s.mount.is_none());
    }

    #[test]
    fn a_corrupt_file_is_backed_up() {
        let p = temp_path("corrupt.json");
        std::fs::write(&p, b"{not json").unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.volume, 85);
        let backup = p.with_extension("json.corrupt");
        assert!(backup.exists());
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn load_sanitizes_old_values() {
        let p = temp_path("old.json");
        std::fs::write(&p, br#"{"volume":200,"poll_secs":1,"bt_quality":"low"}"#).unwrap();
        let s = Settings::load(&p);
        assert_eq!(s.volume, 100);
        assert_eq!(s.poll_secs, 10);
        assert_eq!(s.bt_quality, "high");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn write_atomic_replaces_the_file() {
        let p = temp_path("atomic.json");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert!(!p.with_extension("tmp").exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn hex_and_constant_time_compare() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(random_hex(8).len(), 16);
        assert_ne!(random_hex(8), random_hex(8));
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn data_folder_from_the_environment() {
        assert_eq!(data_dir(Some("/a".into()), Some("/b".into())), PathBuf::from("/a"));
        assert_eq!(data_dir(None, Some("/b".into())), PathBuf::from("/b"));
        assert_eq!(data_dir(None, None), PathBuf::from("data"));
        let dir = temp_path("paths");
        let p = Paths::in_dir(&dir);
        assert!(dir.is_dir(), "created");
        assert_eq!(p.settings, dir.join("settings.json"));
        assert_eq!(p.catalog, dir.join("catalog.json"));
        assert_eq!(p.history, dir.join("history.jsonl"));
        assert_eq!(p.site, dir.join("site.json"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
