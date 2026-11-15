//! Quran recitation kept on the Pi, for any reciter on mp3quran.net.
//!
//! * **Reciters** ([`reciters`]): the catalogue from `https://www.mp3quran.net/api/v3/reciters`,
//!   cached in `<data>/quran/reciters.json` and refreshed monthly. Some recitations don't have
//!   all 114 surahs.
//! * **Files:** one MP3 per surah (0.4–90 MB) in `<data>/quran/<key>/NNN.mp3`. A surah downloads
//!   the first time it is played (and plays while it downloads, see `stream::local`); "Download
//!   all" fetches the rest of the chosen recitation in the background ([`download`]).
//! * **Playlists** ([`playlists`]): named, shared, optionally pinned to a reciter and scheduled.

mod download;
mod playlists;
mod reciters;

pub use download::{Curl, Fetcher};
pub use playlists::{Playlist, PlaylistCtx};
pub use reciters::Reciter;

use crate::sync::LockExt;
use download::Queue;
use playlists::PlaylistStore;
use reciters::Catalog;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// The default recitation (Abdul Rahman Al-Sudais, Hafs, murattal).
pub const DEFAULT_RECITER: &str = "abdulrahman-sudais-r1";

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

/// A recitation's surah: what a download or a file is about.
pub type Item = (String, u16);

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

/// Size of the file at `p`, if it exists.
fn file_len(p: &Path) -> Option<u64> {
    std::fs::metadata(p).map(|m| m.len()).ok()
}

/// The recitations on the Pi: catalogue, files, downloads and playlists.
pub struct Library<F = Curl> {
    root: PathBuf,
    queue: Mutex<Queue>,
    wake: tokio::sync::Notify,
    catalog: Catalog,
    playlists: PlaylistStore,
    fetcher: F,
}

impl Library<Curl> {
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Self::with_fetcher(data_dir, Curl)
    }
}

impl<F: Fetcher> Library<F> {
    pub fn with_fetcher(data_dir: &Path, fetcher: F) -> Arc<Self> {
        let root = data_dir.join("quran");
        let _ = std::fs::create_dir_all(&root);
        // Before reciters existed, Al-Sudais lived in quran/sudais.
        let old = root.join("sudais");
        if old.is_dir() && !root.join(DEFAULT_RECITER).exists() {
            let _ = std::fs::rename(&old, root.join(DEFAULT_RECITER));
        }
        Arc::new(Self {
            catalog: Catalog::load(root.join("reciters.json")),
            playlists: PlaylistStore::load(data_dir.join("quran-playlists.json")),
            root,
            queue: Mutex::new(Queue::default()),
            wake: tokio::sync::Notify::new(),
            fetcher,
        })
    }

    // -------------------------------------------------------------- reciters

    /// All recitations (the built-in default if the catalogue hasn't been fetched yet).
    pub fn reciters(&self) -> Vec<Reciter> {
        self.catalog.all()
    }

    pub fn reciter(&self, key: &str) -> Option<Reciter> {
        self.catalog.get(key)
    }

    /// A usable recitation for `key` (falls back to the default).
    pub fn resolve(&self, key: Option<&str>) -> Reciter {
        self.catalog.resolve(key)
    }

    /// Keep the reciter catalogue fresh. Runs for the life of the app.
    pub async fn refresh_catalog(self: Arc<Self>) {
        self.catalog.keep_fresh().await;
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
        let have = file_len(&self.part_path(key, n)).unwrap_or(0);
        let total = self.queue.locked().totals.get(&(key.to_string(), n)).copied()?;
        u8::try_from((have * 100 / total.max(1)).min(99)).ok()
    }

    /// Expected size of the surah file, if known.
    pub fn total(&self, key: &str, n: u16) -> Option<u64> {
        file_len(&self.final_path(key, n)).or_else(|| self.queue.locked().totals.get(&(key.to_string(), n)).copied())
    }

    /// Delete one surah of a recitation, or every download of every recitation (`None`).
    /// Never the one playing (`keep`).
    pub fn delete(&self, what: Option<Item>, keep: Option<&Item>) -> usize {
        let all = what.is_none();
        let targets: Vec<Item> = match what {
            Some(i) => vec![i],
            None => std::fs::read_dir(&self.root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|k| valid_key(k))
                .flat_map(|k| (1..=114).map(move |n| (k.clone(), n)))
                .collect(),
        };
        let mut q = self.queue.locked();
        if all {
            q.all = None;
            q.priority.clear();
        }
        let mut removed = 0;
        for t in targets.into_iter().filter(|t| Some(t) != keep) {
            if let Some((a, cancel)) = &q.active
                && *a == t
            {
                cancel.store(true, Ordering::Relaxed);
            }
            q.priority.retain(|p| *p != t);
            let a = std::fs::remove_file(self.final_path(&t.0, t.1)).is_ok();
            let b = std::fs::remove_file(self.part_path(&t.0, t.1)).is_ok();
            removed += usize::from(a || b);
        }
        removed
    }

    /// Bytes of recitations stored, across reciters (walks the folders: call off the async threads when possible).
    pub fn stored_bytes(&self) -> u64 {
        let size = |p: PathBuf| {
            std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len())
                .sum::<u64>()
        };
        std::fs::read_dir(&self.root).into_iter().flatten().flatten().filter(|e| e.path().is_dir()).map(|e| size(e.path())).sum()
    }

    pub fn info(&self, key: &str) -> LibraryInfo {
        let reciter = self.resolve(Some(key));
        let key = reciter.key.clone();
        let q = self.queue.locked();
        let mut surahs = Vec::with_capacity(114);
        let mut ready = 0;
        for n in 1..=114u16 {
            if !reciter.has(n) {
                surahs.push(SurahInfo { n, name: name(n), state: "unavailable", bytes: 0, total: None });
                continue;
            }
            let item = (key.clone(), n);
            let fin = file_len(&self.final_path(&key, n));
            let part = file_len(&self.part_path(&key, n));
            let active = q.active.as_ref().is_some_and(|(a, _)| *a == item);
            let queued = q.priority.contains(&item) || q.all.as_deref() == Some(key.as_str());
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
            let total = fin.or_else(|| q.totals.get(&item).copied());
            surahs.push(SurahInfo { n, name: name(n), state, bytes, total });
        }
        let available = reciter.surahs.len();
        let downloading_all = q.all.as_deref() == Some(key.as_str()) && ready < available;
        let error = q.error.clone();
        drop(q);
        LibraryInfo { stored_bytes: self.stored_bytes(), reciter, surahs, ready, available, downloading_all, error }
    }

    // -------------------------------------------------------------- playlists

    pub fn playlists(&self) -> Vec<Playlist> {
        self.playlists.all()
    }

    pub fn playlist(&self, id: &str) -> Option<Playlist> {
        self.playlists.get(id)
    }

    /// Create (unknown id) or replace a playlist. Validates and saves to disk.
    ///
    /// # Errors
    /// A sentence for the user (invalid playlist, too many, or the file couldn't be written).
    pub fn save_playlist(&self, p: Playlist) -> Result<Playlist, String> {
        let p = playlists::validate(p, |k| self.reciter(k).is_some())?;
        self.playlists.save(p)
    }

    /// # Errors
    /// When the playlists file couldn't be written.
    pub fn delete_playlist(&self, id: &str) -> Result<bool, String> {
        self.playlists.delete(id)
    }
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
    fn surah_numbers_are_one_to_114() {
        assert!(valid(1) && valid(114));
        assert!(!valid(0) && !valid(115) && !valid(u16::MAX));
        assert_eq!(name(115), "");
        assert_eq!(name(36), "Yā Sīn");
    }

    #[test]
    fn recitation_keys_are_path_safe() {
        assert!(valid_key(DEFAULT_RECITER));
        assert!(valid_key("a-b-9"));
        assert!(valid_key(&"a".repeat(80)));
        assert!(!valid_key(""));
        assert!(!valid_key(&"a".repeat(81)));
        assert!(!valid_key("Sudais"), "uppercase is rejected");
        assert!(!valid_key("a/b"));
        assert!(!valid_key("a b"));
        assert!(!valid_key("é"));
    }

    fn library() -> (Arc<Library>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lm-lib-{}", crate::config::random_hex(6)));
        (Library::new(&dir), dir)
    }

    #[test]
    fn the_old_sudais_folder_is_adopted() {
        let dir = std::env::temp_dir().join(format!("lm-lib-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(dir.join("quran").join("sudais")).unwrap();
        std::fs::write(dir.join("quran").join("sudais").join("001.mp3"), b"x").unwrap();
        let lib = Library::new(&dir);
        assert!(lib.is_ready(DEFAULT_RECITER, 1));
        assert!(!dir.join("quran").join("sudais").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn progress_and_totals() {
        let (lib, dir) = library();
        let key = DEFAULT_RECITER;
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        assert_eq!(lib.progress(key, 2), None, "size unknown");
        std::fs::write(lib.part_path(key, 2), vec![0u8; 512]).unwrap();
        lib.queue.locked().totals.insert((key.to_string(), 2), 1024);
        assert_eq!(lib.progress(key, 2), Some(50));
        assert_eq!(lib.total(key, 2), Some(1024));
        std::fs::write(lib.final_path(key, 2), vec![0u8; 1000]).unwrap();
        assert_eq!(lib.progress(key, 2), None, "ready");
        assert_eq!(lib.total(key, 2), Some(1000), "the file's own size");
        assert_eq!(lib.total(key, 3), None);
        assert_eq!(lib.stored_bytes(), 1512);
        assert_eq!(lib.reciters().len(), 1);
        assert_eq!(lib.reciter(key).unwrap().key, key);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_one_or_everything_but_the_playing_surah() {
        let (lib, dir) = library();
        let key = DEFAULT_RECITER;
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        std::fs::create_dir_all(lib.dir("other-r1")).unwrap();
        for n in [1, 2, 3] {
            std::fs::write(lib.final_path(key, n), b"x").unwrap();
        }
        std::fs::write(lib.part_path("other-r1", 5), b"x").unwrap();
        std::fs::create_dir_all(lib.root.join("Not-A-Key")).unwrap();

        assert_eq!(lib.delete(Some((key.to_string(), 1)), None), 1);
        assert_eq!(lib.delete(Some((key.to_string(), 1)), None), 0, "already gone");

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut q = lib.queue.locked();
            q.active = Some((("other-r1".to_string(), 5), cancel.clone()));
            q.priority.push_back((key.to_string(), 3));
            q.all = Some(key.to_string());
        }
        assert_eq!(lib.delete(None, Some(&(key.to_string(), 2))), 2);
        assert!(cancel.load(Ordering::Relaxed), "the download being deleted stops");
        assert!(lib.is_ready(key, 2), "the playing surah is kept");
        let q = lib.queue.locked();
        assert!(q.all.is_none() && q.priority.is_empty());
        drop(q);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn info_reports_each_surah_state() {
        let (lib, dir) = library();
        let key = DEFAULT_RECITER;
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        std::fs::write(lib.final_path(key, 1), b"ready").unwrap();
        std::fs::write(lib.part_path(key, 2), b"pa").unwrap();
        std::fs::write(lib.part_path(key, 3), b"par").unwrap();
        {
            let mut q = lib.queue.locked();
            q.active = Some(((key.to_string(), 2), Arc::new(std::sync::atomic::AtomicBool::new(false))));
            q.priority.push_back((key.to_string(), 4));
        }
        let info = lib.info(key);
        let state = |n: usize| info.surahs[n - 1].state;
        assert_eq!([state(1), state(2), state(3), state(4), state(5)], ["ready", "downloading", "partial", "queued", "none"]);
        assert_eq!((info.ready, info.available, info.downloading_all), (1, 114, false));
        assert_eq!(info.surahs[0].total, Some(5));
        lib.download_all(Some(key));
        assert!(lib.info(key).downloading_all);

        // A recitation that lacks some surahs.
        let partial = r#"[{"key":"partial-r1","name":"P","style":"","server":"https://cdn.mp3quran.net/p/","surahs":[1]}]"#;
        std::fs::write(lib.root.join("reciters.json"), partial).unwrap();
        let lib = Library::new(&dir);
        let info = lib.info("partial-r1");
        assert_eq!((info.surahs[0].state, info.surahs[1].state, info.available), ("none", "unavailable", 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn playlists_are_validated_and_stored() {
        let (lib, dir) = library();
        let p = Playlist { name: "Fajr".into(), surahs: vec![1, 36], ..Playlist::default() };
        let saved = lib.save_playlist(p.clone()).unwrap();
        assert_eq!(lib.playlists().len(), 1);
        assert_eq!(lib.playlist(&saved.id).unwrap().name, "Fajr");
        assert!(lib.save_playlist(Playlist { reciter: Some("nobody".into()), ..p }).is_err());
        assert!(lib.delete_playlist(&saved.id).unwrap());
        assert!(lib.playlists().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
