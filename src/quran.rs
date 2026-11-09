//! Quran recitation kept on the Pi, for any reciter on mp3quran.net.
//!
//! * **Reciters:** the catalogue comes from `https://www.mp3quran.net/api/v3/reciters` (about 300
//!   recitations by 244 reciters), cached in `<data>/quran/reciters.json` and refreshed monthly.
//!   Each recitation (a reciter's "moshaf") is keyed by its server path, e.g.
//!   `abdulrahman-sudais-r1`. Some recitations don't have all 114 surahs.
//! * **Files:** one MP3 per surah (0.4–90 MB) in `<data>/quran/<key>/NNN.mp3`. A surah downloads
//!   the first time it is played (and plays while it downloads, see `stream::local`); "Download
//!   all" fetches the rest of the chosen recitation in the background. One download at a time,
//!   rate-limited so the Pi's Wi-Fi (shared with the Bluetooth chip) and the live stream are never
//!   starved; interrupted downloads resume (`curl -C -` on `NNN.mp3.part`).
//! * **Playlists:** named, shared, optionally pinned to a reciter and scheduled on weekdays.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

/// The default recitation (Abdul Rahman Al-Sudais, Hafs, murattal).
pub const DEFAULT_RECITER: &str = "abdulrahman-sudais-r1";
const CATALOG_URL: &str = "https://www.mp3quran.net/api/v3/reciters?language=eng";
/// Well above real time (16 KB/s) yet gentle on the 2.4 GHz link the Bluetooth chip shares.
const RATE_LIMIT: &str = "400K";
/// Shown first in the picker: the best-known reciters.
const POPULAR: [&str; 12] = [
    "abdulrahman-sudais-r1", "mishary-alafasy-r1", "saad-ghamdi-r1", "maher-muaiqly-r1", "abdulbasit-abdulsamad-r3",
    "mahmoud-husary-r1", "muhammad-minshawi-r1", "saud-shuraim-r1", "yasser-dosari-r1", "ahmad-ajmi-r1",
    "abubakr-shatri-r1", "nasser-qatami-r1",
];

pub const SURAHS: [&str; 114] = [
    "Al-Fātiḥah", "Al-Baqarah", "Āl ʿImrān", "An-Nisāʾ", "Al-Māʾidah", "Al-Anʿām", "Al-Aʿrāf", "Al-Anfāl", "At-Tawbah", "Yūnus",
    "Hūd", "Yūsuf", "Ar-Raʿd", "Ibrāhīm", "Al-Ḥijr", "An-Naḥl", "Al-Isrāʾ", "Al-Kahf", "Maryam", "Ṭā Hā",
    "Al-Anbiyāʾ", "Al-Ḥajj", "Al-Muʾminūn", "An-Nūr", "Al-Furqān", "Ash-Shuʿarāʾ", "An-Naml", "Al-Qaṣaṣ", "Al-ʿAnkabūt", "Ar-Rūm",
    "Luqmān", "As-Sajdah", "Al-Aḥzāb", "Sabaʾ", "Fāṭir", "Yā Sīn", "Aṣ-Ṣāffāt", "Ṣād", "Az-Zumar", "Ghāfir",
    "Fuṣṣilat", "Ash-Shūrā", "Az-Zukhruf", "Ad-Dukhān", "Al-Jāthiyah", "Al-Aḥqāf", "Muḥammad", "Al-Fatḥ", "Al-Ḥujurāt", "Qāf",
    "Adh-Dhāriyāt", "Aṭ-Ṭūr", "An-Najm", "Al-Qamar", "Ar-Raḥmān", "Al-Wāqiʿah", "Al-Ḥadīd", "Al-Mujādilah", "Al-Ḥashr", "Al-Mumtaḥanah",
    "Aṣ-Ṣaff", "Al-Jumuʿah", "Al-Munāfiqūn", "At-Taghābun", "Aṭ-Ṭalāq", "At-Taḥrīm", "Al-Mulk", "Al-Qalam", "Al-Ḥāqqah", "Al-Maʿārij",
    "Nūḥ", "Al-Jinn", "Al-Muzzammil", "Al-Muddaththir", "Al-Qiyāmah", "Al-Insān", "Al-Mursalāt", "An-Nabaʾ", "An-Nāziʿāt", "ʿAbasa",
    "At-Takwīr", "Al-Infiṭār", "Al-Muṭaffifīn", "Al-Inshiqāq", "Al-Burūj", "Aṭ-Ṭāriq", "Al-Aʿlā", "Al-Ghāshiyah", "Al-Fajr", "Al-Balad",
    "Ash-Shams", "Al-Layl", "Aḍ-Ḍuḥā", "Ash-Sharḥ", "At-Tīn", "Al-ʿAlaq", "Al-Qadr", "Al-Bayyinah", "Az-Zalzalah", "Al-ʿĀdiyāt",
    "Al-Qāriʿah", "At-Takāthur", "Al-ʿAṣr", "Al-Humazah", "Al-Fīl", "Quraysh", "Al-Māʿūn", "Al-Kawthar", "Al-Kāfirūn", "An-Naṣr",
    "Al-Masad", "Al-Ikhlāṣ", "Al-Falaq", "An-Nās",
];

pub fn valid(n: u16) -> bool {
    (1..=114).contains(&n)
}

pub fn name(n: u16) -> &'static str {
    SURAHS.get(usize::from(n).wrapping_sub(1)).copied().unwrap_or("")
}

/// Recitation keys are used in paths: lowercase letters, digits and '-' only.
pub fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 80 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One recitation (a reciter's moshaf) on mp3quran.net.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reciter {
    pub key: String,
    pub name: String,
    /// "Murattal", "Mujawwad", "Warsh, Murattal" … (empty when the reciter has only one).
    pub style: String,
    /// Base URL; surah files are `<server>NNN.mp3`.
    pub server: String,
    /// Surahs this recitation has.
    pub surahs: Vec<u16>,
    #[serde(default)]
    pub popular: bool,
}

impl Reciter {
    pub fn label(&self) -> String {
        if self.style.is_empty() { self.name.clone() } else { format!("{} ({})", self.name, self.style) }
    }

    pub fn has(&self, n: u16) -> bool {
        self.surahs.contains(&n)
    }
}

fn builtin_sudais() -> Reciter {
    Reciter {
        key: DEFAULT_RECITER.into(),
        name: "Abdulrahman Alsudaes".into(),
        style: String::new(),
        server: "https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/".into(),
        surahs: (1..=114).collect(),
        popular: true,
    }
}

/// Key from a server URL: "https://cdn.mp3quran.net/audio/maher-muaiqly/r1/" → "maher-muaiqly-r1".
fn key_of(server: &str) -> Option<String> {
    let rest = server.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if host != "mp3quran.net" && !host.ends_with(".mp3quran.net") {
        return None; // only ever download from mp3quran's servers
    }
    let path = path.strip_prefix("audio/").unwrap_or(path);
    let key: String = path.trim_matches('/').to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    valid_key(&key).then_some(key)
}

/// Short style label from a moshaf name ("Rewayat Hafs A'n Assem - Murattal" → "Murattal").
fn style_of(moshaf: &str) -> String {
    let (riwayah, kind) = moshaf.split_once(" - ").unwrap_or(("", moshaf));
    let kind = kind.replace("Almusshaf Al Mojawwad", "Mujawwad").replace("Almusshaf Al Mo'lim", "Teaching");
    let riwayah = riwayah.trim_start_matches("Rewayat ").split(" A'n ").next().unwrap_or("").trim().to_string();
    if riwayah.is_empty() || riwayah == "Hafs" { kind.trim().to_string() } else { format!("{riwayah}, {}", kind.trim()) }
}

/// Parse the mp3quran catalogue into recitations (popular first, then by name).
pub fn parse_catalog(body: &str) -> Option<Vec<Reciter>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut out = Vec::new();
    for r in v.get("reciters")?.as_array()? {
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        let moshafs = r.get("moshaf").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        let several = moshafs.len() > 1;
        for m in moshafs {
            let Some(server) = m.get("server").and_then(|s| s.as_str()) else { continue };
            let Some(key) = key_of(server) else { continue };
            let surahs: Vec<u16> = m.get("surah_list").and_then(|s| s.as_str()).unwrap_or("")
                .split(',').filter_map(|s| s.trim().parse().ok()).filter(|&n| valid(n)).collect();
            if name.is_empty() || surahs.is_empty() {
                continue;
            }
            let style = if several { style_of(m.get("name").and_then(|s| s.as_str()).unwrap_or("")) } else { String::new() };
            let server = if server.ends_with('/') { server.to_string() } else { format!("{server}/") };
            out.push(Reciter { popular: POPULAR.contains(&key.as_str()), key, name: name.clone(), style, server, surahs });
        }
    }
    if out.is_empty() {
        return None;
    }
    out.sort_by(|a, b| {
        let rank = |r: &Reciter| POPULAR.iter().position(|p| *p == r.key).unwrap_or(usize::MAX);
        rank(a).cmp(&rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.style.cmp(&b.style))
    });
    out.dedup_by(|a, b| a.key == b.key);
    Some(out)
}

#[derive(Clone, Debug, Serialize)]
pub struct SurahInfo {
    pub n: u16,
    pub name: &'static str,
    /// "none" | "queued" | "downloading" | "partial" | "ready" | "unavailable" (not in this recitation)
    pub state: &'static str,
    pub bytes: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LibraryInfo {
    pub reciter: Reciter,
    pub surahs: Vec<SurahInfo>,
    pub ready: usize,
    pub available: usize,
    /// All recitations, across reciters.
    pub stored_bytes: u64,
    pub downloading_all: bool,
    pub error: Option<String>,
}

/// A recitation's surah: what a download or a file is about.
pub type Item = (String, u16);

/// A user's named selection of surahs, played in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub surahs: Vec<u16>,
    /// Start again from the first surah after the last.
    pub repeat: bool,
    /// Recitation to use (None = the app's default reciter).
    #[serde(default)]
    pub reciter: Option<String>,
    /// Play automatically on these weekdays (0 = Sunday) at `time` ("HH:MM").
    #[serde(default)]
    pub days: Vec<u8>,
    #[serde(default)]
    pub time: Option<String>,
    /// Playback speed (None = the app's default).
    #[serde(default)]
    pub speed: Option<f32>,
}

impl Playlist {
    pub fn scheduled(&self) -> bool {
        self.time.is_some() && !self.days.is_empty()
    }
}

/// What is playing from a playlist (a snapshot, so editing the playlist doesn't disturb it).
#[derive(Clone, Debug, Serialize)]
pub struct PlaylistCtx {
    pub id: String,
    pub name: String,
    pub surahs: Vec<u16>,
    pub index: usize,
    pub repeat: bool,
}

impl PlaylistCtx {
    pub fn new(p: &Playlist, index: usize) -> Self {
        Self { id: p.id.clone(), name: p.name.clone(), surahs: p.surahs.clone(), index: index.min(p.surahs.len().saturating_sub(1)), repeat: p.repeat }
    }

    pub fn surah(&self) -> u16 {
        self.surahs[self.index]
    }

    /// The entry `by` steps away (-1/+1); wraps around only with repeat. None past the end.
    pub fn step(&self, by: i32) -> Option<Self> {
        let len = self.surahs.len() as i64;
        let mut i = self.index as i64 + i64::from(by);
        if i >= len {
            if !self.repeat {
                return None;
            }
            i = 0;
        }
        if i < 0 {
            i = if self.repeat { len - 1 } else { 0 };
        }
        Some(Self { index: i as usize, ..self.clone() })
    }
}

#[derive(Default)]
struct Inner {
    /// Content lengths learned from the server.
    totals: HashMap<Item, u64>,
    /// Surah being downloaded, and its cancel flag.
    active: Option<(Item, Arc<AtomicBool>)>,
    /// Requested to play: downloaded before anything else.
    priority: VecDeque<Item>,
    /// "Download all" for this recitation.
    all: Option<String>,
    error: Option<String>,
}

pub struct Library {
    root: PathBuf,
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
    catalog_path: PathBuf,
    catalog: Mutex<Vec<Reciter>>,
    playlists_path: PathBuf,
    playlists: Mutex<Vec<Playlist>>,
}

impl Library {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        let root = data_dir.join("quran");
        let _ = std::fs::create_dir_all(&root);
        // Before reciters existed, Al-Sudais lived in quran/sudais.
        let old = root.join("sudais");
        if old.is_dir() && !root.join(DEFAULT_RECITER).exists() {
            let _ = std::fs::rename(&old, root.join(DEFAULT_RECITER));
        }
        let catalog_path = root.join("reciters.json");
        let catalog: Vec<Reciter> = std::fs::read(&catalog_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let playlists_path = data_dir.join("quran-playlists.json");
        let playlists = std::fs::read(&playlists_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            root,
            inner: Mutex::new(Inner::default()),
            wake: tokio::sync::Notify::new(),
            catalog_path,
            catalog: Mutex::new(catalog),
            playlists_path,
            playlists: Mutex::new(playlists),
        })
    }

    // -------------------------------------------------------------- reciters

    /// All recitations (the built-in default if the catalogue hasn't been fetched yet).
    pub fn reciters(&self) -> Vec<Reciter> {
        let c = self.catalog.lock().unwrap();
        if c.is_empty() { vec![builtin_sudais()] } else { c.clone() }
    }

    pub fn reciter(&self, key: &str) -> Option<Reciter> {
        let found = self.catalog.lock().unwrap().iter().find(|r| r.key == key).cloned();
        found.or_else(|| (key == DEFAULT_RECITER).then(builtin_sudais))
    }

    /// A usable recitation for `key` (falls back to the default).
    pub fn resolve(&self, key: Option<&str>) -> Reciter {
        key.and_then(|k| self.reciter(k)).unwrap_or_else(|| self.reciter(DEFAULT_RECITER).unwrap_or_else(builtin_sudais))
    }

    /// Fetch the reciter catalogue when missing or older than 30 days. Runs for the life of the app.
    pub async fn refresh_catalog(self: Arc<Self>) {
        loop {
            let age = std::fs::metadata(&self.catalog_path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            let due = self.catalog.lock().unwrap().is_empty() || age.is_none_or(|a| a > Duration::from_secs(30 * 86400));
            let mut wait = Duration::from_secs(86400);
            if due {
                let out = Command::new("curl")
                    .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", "=https", "--max-time", "40"])
                    .args(["-A", crate::integrations::USER_AGENT, CATALOG_URL])
                    .kill_on_drop(true)
                    .output()
                    .await;
                match out.ok().filter(|o| o.status.success()).and_then(|o| parse_catalog(&String::from_utf8_lossy(&o.stdout))) {
                    Some(list) => {
                        tracing::info!("Quran: {} recitations available", list.len());
                        if let Ok(body) = serde_json::to_vec(&list) {
                            let _ = crate::config::write_atomic(&self.catalog_path, &body);
                        }
                        *self.catalog.lock().unwrap() = list;
                    }
                    None => {
                        tracing::warn!("Quran: couldn't fetch the reciter list; retrying in an hour");
                        wait = Duration::from_secs(3600);
                    }
                }
            }
            tokio::time::sleep(wait).await;
        }
    }

    // -------------------------------------------------------------- files

    fn dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    pub fn final_path(&self, key: &str, n: u16) -> PathBuf {
        self.dir(key).join(format!("{n:03}.mp3"))
    }

    pub fn part_path(&self, key: &str, n: u16) -> PathBuf {
        self.dir(key).join(format!("{n:03}.mp3.part"))
    }

    pub fn is_ready(&self, key: &str, n: u16) -> bool {
        self.final_path(key, n).is_file()
    }

    /// Download progress of a surah that isn't complete yet (percent), None once ready.
    pub fn progress(&self, key: &str, n: u16) -> Option<u8> {
        if self.is_ready(key, n) {
            return None;
        }
        let have = std::fs::metadata(self.part_path(key, n)).map(|m| m.len()).unwrap_or(0);
        let total = self.inner.lock().unwrap().totals.get(&(key.to_string(), n)).copied()?;
        Some((have * 100 / total.max(1)).min(99) as u8)
    }

    /// Expected size of the surah file, if known.
    pub fn total(&self, key: &str, n: u16) -> Option<u64> {
        if let Ok(m) = std::fs::metadata(self.final_path(key, n)) {
            return Some(m.len());
        }
        self.inner.lock().unwrap().totals.get(&(key.to_string(), n)).copied()
    }

    /// Make sure this surah is (being) downloaded, ahead of "download all".
    pub fn request(&self, key: &str, n: u16) {
        if !valid(n) || self.is_ready(key, n) {
            return;
        }
        let item = (key.to_string(), n);
        let mut g = self.inner.lock().unwrap();
        if g.active.as_ref().is_some_and(|(a, _)| *a == item) {
            return;
        }
        // Only the surah being listened to matters now; earlier requests stay as partial files.
        g.priority.clear();
        g.priority.push_back(item);
        // Whatever else is downloading yields; it resumes from its .part later.
        if let Some(((k, a), cancel)) = &g.active {
            tracing::info!("Quran: pausing download of {k} {a} for {key} {n}");
            cancel.store(true, Ordering::Relaxed);
        }
        drop(g);
        self.wake.notify_one();
    }

    /// Start (Some) / stop (None) downloading every surah of a recitation.
    pub fn download_all(&self, key: Option<&str>) {
        let mut g = self.inner.lock().unwrap();
        g.all = key.map(str::to_string);
        g.error = None;
        if key.is_none() {
            // Stop background work, but keep a surah someone is listening to.
            if let Some((a, cancel)) = &g.active {
                if !g.priority.contains(a) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        drop(g);
        self.wake.notify_one();
    }

    /// Delete one surah of a recitation, or every download of every recitation (`None`).
    /// Never the one playing (`keep`).
    pub fn delete(&self, what: Option<Item>, keep: Option<Item>) -> usize {
        let all = what.is_none();
        let targets: Vec<Item> = match what {
            Some(i) => vec![i],
            None => std::fs::read_dir(&self.root).into_iter().flatten().flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|k| valid_key(k))
                .flat_map(|k| (1..=114).map(move |n| (k.clone(), n)))
                .collect(),
        };
        let mut g = self.inner.lock().unwrap();
        if all {
            g.all = None;
            g.priority.clear();
        }
        let mut removed = 0;
        for t in targets.into_iter().filter(|t| Some(t) != keep.as_ref()) {
            if let Some((a, cancel)) = &g.active {
                if *a == t {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            g.priority.retain(|p| *p != t);
            let a = std::fs::remove_file(self.final_path(&t.0, t.1)).is_ok();
            let b = std::fs::remove_file(self.part_path(&t.0, t.1)).is_ok();
            removed += usize::from(a || b);
        }
        removed
    }

    fn stored_bytes(&self) -> u64 {
        let size = |p: PathBuf| {
            std::fs::read_dir(p).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum::<u64>()
        };
        std::fs::read_dir(&self.root).into_iter().flatten().flatten().filter(|e| e.path().is_dir()).map(|e| size(e.path())).sum()
    }

    pub fn info(&self, key: &str) -> LibraryInfo {
        let reciter = self.resolve(Some(key));
        let key = reciter.key.clone();
        let g = self.inner.lock().unwrap();
        let mut surahs = Vec::with_capacity(114);
        let mut ready = 0;
        for n in 1..=114u16 {
            if !reciter.has(n) {
                surahs.push(SurahInfo { n, name: name(n), state: "unavailable", bytes: 0, total: None });
                continue;
            }
            let item = (key.clone(), n);
            let fin = std::fs::metadata(self.final_path(&key, n)).map(|m| m.len()).ok();
            let part = std::fs::metadata(self.part_path(&key, n)).map(|m| m.len()).ok();
            let active = g.active.as_ref().is_some_and(|(a, _)| *a == item);
            let queued = g.priority.contains(&item) || g.all.as_deref() == Some(key.as_str());
            let (state, bytes) = match (fin, part) {
                (Some(b), _) => {
                    ready += 1;
                    ("ready", b)
                }
                (None, p) if active => ("downloading", p.unwrap_or(0)),
                (None, p) if queued => ("queued", p.unwrap_or(0)),
                (None, Some(p)) => ("partial", p),
                (None, None) => ("none", 0),
            };
            let total = fin.or_else(|| g.totals.get(&item).copied());
            surahs.push(SurahInfo { n, name: name(n), state, bytes, total });
        }
        let available = reciter.surahs.len();
        let downloading_all = g.all.as_deref() == Some(key.as_str()) && ready < available;
        let error = g.error.clone();
        drop(g);
        LibraryInfo { stored_bytes: self.stored_bytes(), reciter, surahs, ready, available, downloading_all, error }
    }

    // -------------------------------------------------------------- playlists

    pub fn playlists(&self) -> Vec<Playlist> {
        self.playlists.lock().unwrap().clone()
    }

    pub fn playlist(&self, id: &str) -> Option<Playlist> {
        self.playlists.lock().unwrap().iter().find(|p| p.id == id).cloned()
    }

    /// Create (unknown id) or replace a playlist. Validates and saves to disk.
    pub fn save_playlist(&self, mut p: Playlist) -> Result<Playlist, String> {
        p.name = p.name.trim().to_string();
        if p.name.is_empty() || p.name.chars().count() > 60 {
            return Err("Give the playlist a name (up to 60 characters)".into());
        }
        if p.surahs.is_empty() || p.surahs.len() > 300 || !p.surahs.iter().all(|&n| valid(n)) {
            return Err("Add at least one surah".into());
        }
        if p.reciter.as_deref().is_some_and(|k| self.reciter(k).is_none()) {
            return Err("Unknown reciter".into());
        }
        if p.speed.is_some_and(|v| !(0.5..=2.0).contains(&v)) {
            return Err("Speed must be between 0.5× and 2×".into());
        }
        p.days.retain(|d| *d <= 6);
        p.days.sort_unstable();
        p.days.dedup();
        match &p.time {
            Some(t) => {
                let ok = t.len() == 5 && t.as_bytes()[2] == b':'
                    && t[..2].parse::<u8>().is_ok_and(|h| h < 24) && t[3..].parse::<u8>().is_ok_and(|m| m < 60);
                if !ok {
                    return Err("Pick a time like 11:00".into());
                }
                if p.days.is_empty() {
                    return Err("Pick at least one day for the schedule".into());
                }
            }
            None => p.days.clear(),
        }
        let mut list = self.playlists.lock().unwrap();
        match list.iter_mut().find(|x| x.id == p.id) {
            Some(x) => *x = p.clone(),
            None => {
                if list.len() >= 50 {
                    return Err("That's the maximum of 50 playlists".into());
                }
                p.id = crate::config::random_hex(6);
                list.push(p.clone());
            }
        }
        self.persist(&list)?;
        Ok(p)
    }

    pub fn delete_playlist(&self, id: &str) -> Result<bool, String> {
        let mut list = self.playlists.lock().unwrap();
        let before = list.len();
        list.retain(|p| p.id != id);
        self.persist(&list)?;
        Ok(list.len() != before)
    }

    fn persist(&self, list: &[Playlist]) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
        crate::config::write_atomic(&self.playlists_path, &body).map_err(|e| format!("couldn't save playlists: {e}"))
    }

    // -------------------------------------------------------------- downloader

    /// Next surah to fetch: requested ones first, then (with "download all") in order.
    fn next_job(&self) -> Option<Item> {
        let mut g = self.inner.lock().unwrap();
        while let Some(item) = g.priority.front().cloned() {
            if self.is_ready(&item.0, item.1) {
                g.priority.pop_front();
            } else {
                return Some(item);
            }
        }
        if let Some(key) = g.all.clone() {
            let reciter = self.resolve(Some(&key));
            let next = reciter.surahs.iter().copied().find(|&n| !self.is_ready(&key, n));
            if next.is_none() {
                g.all = None;
            }
            return next.map(|n| (key, n));
        }
        None
    }

    /// The download worker. Runs for the life of the app.
    pub async fn run(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            let Some(item) = self.next_job() else {
                self.wake.notified().await;
                continue;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            self.inner.lock().unwrap().active = Some((item.clone(), cancel.clone()));
            let result = self.download(&item, &cancel).await;
            let backoff = {
                let mut g = self.inner.lock().unwrap();
                g.active = None;
                match result {
                    Ok(()) => {
                        failures = 0;
                        g.error = None;
                        g.priority.retain(|p| *p != item);
                        None
                    }
                    Err(e) if cancel.load(Ordering::Relaxed) => {
                        tracing::debug!("Quran: download of {item:?} paused ({e})");
                        None
                    }
                    Err(e) => {
                        failures += 1;
                        tracing::warn!("Quran: download of {} surah {} failed: {e}", item.0, item.1);
                        g.error = Some(format!("Download of {} failed: {e}", name(item.1)));
                        // Back off (network down?) before trying again: 15 s doubling to 10 min.
                        Some(Duration::from_secs((15u64 << failures.min(6)).min(600)))
                    }
                }
            };
            if let Some(wait) = backoff {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = self.wake.notified() => {}
                }
            }
        }
    }

    async fn download(&self, item: &Item, cancel: &AtomicBool) -> Result<(), String> {
        let (key, n) = (item.0.as_str(), item.1);
        let reciter = self.reciter(key).ok_or("unknown reciter")?;
        if !reciter.has(n) {
            return Err("this reciter doesn't have that surah".into());
        }
        let url = format!("{}{n:03}.mp3", reciter.server);
        let _ = std::fs::create_dir_all(self.dir(key));
        let part = self.part_path(key, n);
        // Learn the size (for progress, and to finish a .part that is already complete).
        let known = self.inner.lock().unwrap().totals.get(item).copied();
        let total = match known {
            Some(t) => Some(t),
            None => content_length(&url).await,
        };
        if let Some(t) = total {
            self.inner.lock().unwrap().totals.insert(item.clone(), t);
        }
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if total.is_none_or(|t| have < t) {
            let mut child = Command::new("curl")
                .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", "=https", "--retry", "2"])
                .args(["--limit-rate", RATE_LIMIT, "--speed-limit", "1000", "--speed-time", "60", "-C", "-"])
                .args(["-A", crate::integrations::USER_AGENT, "-o"])
                .arg(&part)
                .arg(&url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("curl: {e}"))?;
            loop {
                tokio::select! {
                    status = child.wait() => {
                        let status = status.map_err(|e| e.to_string())?;
                        if !status.success() {
                            let mut err = String::new();
                            if let Some(mut e) = child.stderr.take() {
                                use tokio::io::AsyncReadExt;
                                let _ = e.read_to_string(&mut err).await;
                            }
                            let err = err.trim().lines().last().unwrap_or("").to_string();
                            return Err(if err.is_empty() { format!("curl exit {status}") } else { err });
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        if cancel.load(Ordering::Relaxed) {
                            let _ = child.kill().await;
                            return Err("cancelled".into());
                        }
                    }
                }
            }
        }
        let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if total.is_some_and(|t| got != t) || got < 1024 {
            return Err(format!("incomplete ({got} bytes)"));
        }
        std::fs::rename(&part, self.final_path(key, n)).map_err(|e| e.to_string())?;
        tracing::info!("Quran: {key} surah {n} ({}) downloaded, {} KB", name(n), got / 1024);
        Ok(())
    }
}

async fn content_length(url: &str) -> Option<u64> {
    let out = Command::new("curl")
        .args(["-sSI", "-L", "--max-redirs", "3", "--proto", "=https", "--max-time", "20", "-A", crate::integrations::USER_AGENT, url])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    // With redirects there are several header blocks; the last Content-Length wins.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .filter_map(|(_, v)| v.trim().parse().ok())
        .last()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surah_names() {
        assert_eq!(SURAHS.len(), 114);
        assert_eq!(name(1), "Al-Fātiḥah");
        assert_eq!(name(114), "An-Nās");
        assert_eq!(name(0), "");
        assert!(!valid(115));
    }

    #[test]
    fn playlist_steps() {
        let p = Playlist { id: "a".into(), name: "Morning".into(), surahs: vec![1, 2, 36], repeat: false, reciter: None, days: vec![], time: None, speed: None };
        let c = PlaylistCtx::new(&p, 0);
        assert_eq!(c.step(1).unwrap().surah(), 2);
        assert_eq!(c.step(-1).unwrap().surah(), 1); // stays at the start
        let last = PlaylistCtx::new(&p, 2);
        assert!(last.step(1).is_none()); // ends without repeat
        let r = PlaylistCtx { repeat: true, ..last };
        assert_eq!(r.step(1).unwrap().surah(), 1); // wraps with repeat
        assert_eq!(PlaylistCtx::new(&p, 9).index, 2);
    }

    #[test]
    fn catalog_parsing() {
        let body = r#"{"reciters":[
          {"id":102,"name":"Maher Al Meaqli","moshaf":[
            {"id":133,"name":"Rewayat Hafs A'n Assem - Almusshaf Al Mojawwad","server":"https://cdn.mp3quran.net/audio/maher-muaiqly/r3/","surah_list":"1,2"},
            {"id":102,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://cdn.mp3quran.net/audio/maher-muaiqly/r1/","surah_list":"1,2"}]},
          {"id":54,"name":"Abdulrahman  Alsudaes","moshaf":[{"id":54,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/","surah_list":"1,2,3,114"}]},
          {"id":9,"name":"Elsewhere","moshaf":[{"id":9,"name":"x","server":"https://evil.example/audio/x/","surah_list":"1"}]},
          {"id":10,"name":"Two Riwayat","moshaf":[{"id":1,"name":"Rewayat Warsh A'n Nafi' - Murattal","server":"https://server15.mp3quran.net/warsh1/","surah_list":"1"},{"id":2,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://server15.mp3quran.net/hafs1","surah_list":"1"}]}
        ]}"#;
        let list = parse_catalog(body).unwrap();
        assert_eq!(list[0].key, "abdulrahman-sudais-r1"); // popular first
        assert_eq!(list[0].name, "Abdulrahman Alsudaes"); // whitespace tidied
        assert!(list[0].has(114) && !list[0].has(5));
        assert!(!list.iter().any(|r| r.name == "Elsewhere")); // only mp3quran servers
        assert_eq!(list.iter().find(|r| r.key == "maher-muaiqly-r3").unwrap().style, "Mujawwad");
        assert_eq!(list.iter().find(|r| r.key == "warsh1").unwrap().style, "Warsh, Murattal");
        assert_eq!(list.iter().find(|r| r.key == "hafs1").unwrap().server, "https://server15.mp3quran.net/hafs1/");
        assert!(valid_key("maher-muaiqly-r1") && !valid_key("../x") && !valid_key(""));
    }
}
