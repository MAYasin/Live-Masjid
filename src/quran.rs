//! Quran recitation (Abdul Rahman Al-Sudais) kept on the Pi.
//!
//! Surahs come from mp3quran.net (one 128 kbps MP3 per surah, 0.4–90 MB) and are stored in
//! `<data>/quran/sudais/NNN.mp3`. A surah downloads the first time it is played (and plays
//! while it downloads, see `stream::local`); "Download all" fetches the rest in the
//! background. One download at a time, rate-limited so the Pi's Wi-Fi (shared with the
//! Bluetooth chip) and the live stream are never starved; interrupted downloads resume
//! (`curl -C -` on `NNN.mp3.part`).

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

pub const RECITER: &str = "Abdul Rahman Al-Sudais";
const BASE_URL: &str = "https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/";
/// Well above real time (16 KB/s) yet gentle on the 2.4 GHz link the Bluetooth chip shares.
const RATE_LIMIT: &str = "400K";

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

#[derive(Clone, Debug, Serialize)]
pub struct SurahInfo {
    pub n: u16,
    pub name: &'static str,
    /// "none" | "queued" | "downloading" | "partial" | "ready"
    pub state: &'static str,
    pub bytes: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LibraryInfo {
    pub reciter: &'static str,
    pub surahs: Vec<SurahInfo>,
    pub ready: usize,
    pub stored_bytes: u64,
    pub downloading_all: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct Inner {
    /// Content lengths learned from the server.
    totals: HashMap<u16, u64>,
    /// Surah being downloaded, and its cancel flag.
    active: Option<(u16, Arc<AtomicBool>)>,
    /// Requested to play: downloaded before anything else.
    priority: VecDeque<u16>,
    /// "Download all" is on.
    all: bool,
    error: Option<String>,
}

pub struct Library {
    dir: PathBuf,
    inner: Mutex<Inner>,
    wake: tokio::sync::Notify,
}

impl Library {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        let dir = data_dir.join("quran").join("sudais");
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(Self { dir, inner: Mutex::new(Inner::default()), wake: tokio::sync::Notify::new() })
    }

    pub fn final_path(&self, n: u16) -> PathBuf {
        self.dir.join(format!("{n:03}.mp3"))
    }

    pub fn part_path(&self, n: u16) -> PathBuf {
        self.dir.join(format!("{n:03}.mp3.part"))
    }

    pub fn is_ready(&self, n: u16) -> bool {
        self.final_path(n).is_file()
    }

    /// Expected size of the surah file, if known.
    pub fn total(&self, n: u16) -> Option<u64> {
        if let Ok(m) = std::fs::metadata(self.final_path(n)) {
            return Some(m.len());
        }
        self.inner.lock().unwrap().totals.get(&n).copied()
    }

    /// Make sure `n` is (being) downloaded, ahead of "download all".
    pub fn request(&self, n: u16) {
        if !valid(n) || self.is_ready(n) {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        if g.active.as_ref().is_some_and(|(a, _)| *a == n) {
            return;
        }
        // Only the surah being listened to matters now; earlier requests stay as partial files.
        g.priority.clear();
        g.priority.push_back(n);
        // Whatever else is downloading yields; it resumes from its .part later.
        if let Some((a, cancel)) = &g.active {
            tracing::info!("Quran: pausing download of surah {a} for surah {n}");
            cancel.store(true, Ordering::Relaxed);
        }
        drop(g);
        self.wake.notify_one();
    }

    pub fn download_all(&self, on: bool) {
        let mut g = self.inner.lock().unwrap();
        g.all = on;
        g.error = None;
        if !on {
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

    /// Delete one surah (or all with `None`). Never the one playing (`keep`).
    pub fn delete(&self, n: Option<u16>, keep: Option<u16>) -> usize {
        let targets: Vec<u16> = match n {
            Some(n) => vec![n],
            None => (1..=114).collect(),
        };
        let mut g = self.inner.lock().unwrap();
        if n.is_none() {
            g.all = false;
            g.priority.clear();
        }
        let mut removed = 0;
        for t in targets.into_iter().filter(|&t| Some(t) != keep) {
            if let Some((a, cancel)) = &g.active {
                if *a == t {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            g.priority.retain(|&p| p != t);
            let a = std::fs::remove_file(self.final_path(t)).is_ok();
            let b = std::fs::remove_file(self.part_path(t)).is_ok();
            removed += usize::from(a || b);
        }
        removed
    }

    pub fn info(&self) -> LibraryInfo {
        let g = self.inner.lock().unwrap();
        let mut surahs = Vec::with_capacity(114);
        let (mut ready, mut stored) = (0, 0);
        for n in 1..=114u16 {
            let fin = std::fs::metadata(self.final_path(n)).map(|m| m.len()).ok();
            let part = std::fs::metadata(self.part_path(n)).map(|m| m.len()).ok();
            let active = g.active.as_ref().is_some_and(|(a, _)| *a == n);
            let queued = g.priority.contains(&n);
            let (state, bytes) = match (fin, part) {
                (Some(b), _) => {
                    ready += 1;
                    ("ready", b)
                }
                (None, p) if active => ("downloading", p.unwrap_or(0)),
                (None, p) if queued || g.all => ("queued", p.unwrap_or(0)),
                (None, Some(p)) => ("partial", p),
                (None, None) => ("none", 0),
            };
            stored += fin.or(part).unwrap_or(0);
            let total = fin.or_else(|| g.totals.get(&n).copied());
            surahs.push(SurahInfo { n, name: name(n), state, bytes, total });
        }
        LibraryInfo { reciter: RECITER, surahs, ready, stored_bytes: stored, downloading_all: g.all && ready < 114, error: g.error.clone() }
    }

    /// Next surah to fetch: requested ones first, then (with "download all") in order.
    fn next_job(&self) -> Option<u16> {
        let mut g = self.inner.lock().unwrap();
        while let Some(&n) = g.priority.front() {
            if self.is_ready(n) {
                g.priority.pop_front();
            } else {
                return Some(n);
            }
        }
        if g.all {
            let next = (1..=114).find(|&n| !self.is_ready(n));
            if next.is_none() {
                g.all = false;
            }
            return next;
        }
        None
    }

    /// The download worker. Runs for the life of the app.
    pub async fn run(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            let Some(n) = self.next_job() else {
                self.wake.notified().await;
                continue;
            };
            let cancel = Arc::new(AtomicBool::new(false));
            self.inner.lock().unwrap().active = Some((n, cancel.clone()));
            let result = self.download(n, &cancel).await;
            let backoff = {
                let mut g = self.inner.lock().unwrap();
                g.active = None;
                match result {
                    Ok(()) => {
                        failures = 0;
                        g.error = None;
                        g.priority.retain(|&p| p != n);
                        None
                    }
                    Err(e) if cancel.load(Ordering::Relaxed) => {
                        tracing::debug!("Quran: download of {n} paused ({e})");
                        None
                    }
                    Err(e) => {
                        failures += 1;
                        tracing::warn!("Quran: download of surah {n} failed: {e}");
                        g.error = Some(format!("Download of {} failed: {e}", name(n)));
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

    async fn download(&self, n: u16, cancel: &AtomicBool) -> Result<(), String> {
        let url = format!("{BASE_URL}{n:03}.mp3");
        let part = self.part_path(n);
        // Learn the size (for progress, and to finish a .part that is already complete).
        let known = self.inner.lock().unwrap().totals.get(&n).copied();
        let total = match known {
            Some(t) => Some(t),
            None => content_length(&url).await,
        };
        if let Some(t) = total {
            self.inner.lock().unwrap().totals.insert(n, t);
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
        std::fs::rename(&part, self.final_path(n)).map_err(|e| e.to_string())?;
        tracing::info!("Quran: surah {n} ({}) downloaded, {} KB", name(n), got / 1024);
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
    #[test]
    fn surah_names() {
        assert_eq!(super::SURAHS.len(), 114);
        assert_eq!(super::name(1), "Al-Fātiḥah");
        assert_eq!(super::name(114), "An-Nās");
        assert_eq!(super::name(0), "");
        assert!(!super::valid(115));
    }
}
