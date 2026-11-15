//! The download queue: one surah at a time. The surah being listened to goes first (whatever
//! else was downloading yields and resumes later from its `.part` file), then "Download all"
//! works through the rest of a recitation. Failures back off from 15 s to 10 min.
//!
//! The network side is the [`Fetcher`] trait: [`Curl`] on the Pi, a fake in the tests.

use super::{Item, Library, name, valid};
use crate::sync::LockExt;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;

/// Well above real time (16 KB/s) yet gentle on the 2.4 GHz link the Bluetooth chip shares.
const RATE_LIMIT: &str = "400K";
/// A finished file smaller than this is an error page, not a recitation.
const MIN_FILE: u64 = 1024;

/// Downloads files over the network.
pub trait Fetcher: Send + Sync + 'static {
    /// The size of the file at `url`, if the server says.
    fn content_length(&self, url: &str) -> impl Future<Output = Option<u64>> + Send;

    /// Download `url` into `dest`, continuing a partial file. Gives up soon after `cancel` is set.
    ///
    /// # Errors
    /// A short reason when the download failed or was cancelled.
    fn fetch(&self, url: &str, dest: &Path, cancel: &AtomicBool) -> impl Future<Output = Result<(), String>> + Send;
}

/// Downloads with `curl`: resumable, rate-limited, https only.
pub struct Curl;

impl Fetcher for Curl {
    async fn content_length(&self, url: &str) -> Option<u64> {
        curl_content_length(url, "=https").await
    }

    async fn fetch(&self, url: &str, dest: &Path, cancel: &AtomicBool) -> Result<(), String> {
        curl_fetch(url, dest, cancel, "=https").await
    }
}

/// [`Curl::content_length`], allowing only the protocols in `proto` (tests use a local http server).
async fn curl_content_length(url: &str, proto: &str) -> Option<u64> {
    let out = Command::new("curl")
        .args(["-sSI", "-L", "--max-redirs", "3", "--proto", proto, "--max-time", "20", "-A", crate::integrations::USER_AGENT, url])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    last_content_length(&String::from_utf8_lossy(&out.stdout))
}

/// [`Curl::fetch`], allowing only the protocols in `proto`.
async fn curl_fetch(url: &str, dest: &Path, cancel: &AtomicBool, proto: &str) -> Result<(), String> {
    let mut child = Command::new("curl")
        .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", proto, "--retry", "2"])
        .args(["--limit-rate", RATE_LIMIT, "--speed-limit", "1000", "--speed-time", "60", "-C", "-"])
        .args(["-A", crate::integrations::USER_AGENT, "-o"])
        .arg(dest)
        .arg(url)
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
                if status.success() {
                    return Ok(());
                }
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = e.read_to_string(&mut err).await;
                }
                let err = err.trim().lines().last().unwrap_or("").to_string();
                return Err(if err.is_empty() { format!("curl exit {status}") } else { err });
            }
            () = tokio::time::sleep(Duration::from_millis(500)) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill().await;
                    return Err("cancelled".into());
                }
            }
        }
    }
}

/// With redirects there are several header blocks; the last Content-Length wins.
fn last_content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .filter_map(|(_, v)| v.trim().parse().ok())
        .next_back()
}

/// Back-off after `failures` failed downloads in a row: 15 s doubling to 10 min.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs((15u64 << failures.min(6)).min(600))
}

#[derive(Default)]
pub(super) struct Queue {
    /// Content lengths learned from the server.
    pub totals: HashMap<Item, u64>,
    /// Surah being downloaded, and its cancel flag.
    pub active: Option<(Item, Arc<AtomicBool>)>,
    /// Requested to play: downloaded before anything else.
    pub priority: VecDeque<Item>,
    /// "Download all" for this recitation.
    pub all: Option<String>,
    pub error: Option<String>,
}

impl<F: Fetcher> Library<F> {
    /// Make sure this surah is (being) downloaded, ahead of "download all".
    pub fn request(&self, key: &str, n: u16) {
        if !valid(n) || self.is_ready(key, n) {
            return;
        }
        let item = (key.to_string(), n);
        let mut q = self.queue.locked();
        if q.active.as_ref().is_some_and(|(a, _)| *a == item) {
            return;
        }
        // Only the surah being listened to matters now; earlier requests stay as partial files.
        q.priority.clear();
        q.priority.push_back(item);
        // Whatever else is downloading yields; it resumes from its .part later.
        if let Some(((k, a), cancel)) = &q.active {
            tracing::info!("Quran: pausing download of {k} {a} for {key} {n}");
            cancel.store(true, Ordering::Relaxed);
        }
        drop(q);
        self.wake.notify_one();
    }

    /// Start (Some) / stop (None) downloading every surah of a recitation.
    pub fn download_all(&self, key: Option<&str>) {
        let mut q = self.queue.locked();
        q.all = key.map(str::to_string);
        q.error = None;
        // Stopping ends background work, but keeps a surah someone is listening to.
        if key.is_none()
            && let Some((a, cancel)) = &q.active
            && !q.priority.contains(a)
        {
            cancel.store(true, Ordering::Relaxed);
        }
        drop(q);
        self.wake.notify_one();
    }

    /// Next surah to fetch: requested ones first, then (with "download all") in order.
    fn next_job(&self) -> Option<Item> {
        let mut q = self.queue.locked();
        while let Some(item) = q.priority.front().cloned() {
            if self.is_ready(&item.0, item.1) {
                q.priority.pop_front();
            } else {
                return Some(item);
            }
        }
        if let Some(key) = q.all.clone() {
            let reciter = self.resolve(Some(&key));
            let next = reciter.surahs.iter().copied().find(|&n| !self.is_ready(&key, n));
            if next.is_none() {
                q.all = None;
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
            let wait = self.run_job(&item, &mut failures).await;
            if let Some(wait) = wait {
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    () = self.wake.notified() => {}
                }
            }
        }
    }

    /// Download one surah and record the outcome; returns how long to back off, if at all.
    async fn run_job(&self, item: &Item, failures: &mut u32) -> Option<Duration> {
        let cancel = Arc::new(AtomicBool::new(false));
        self.queue.locked().active = Some((item.clone(), cancel.clone()));
        let result = self.download(item, &cancel).await;
        let mut q = self.queue.locked();
        q.active = None;
        match result {
            Ok(()) => {
                *failures = 0;
                q.error = None;
                q.priority.retain(|p| p != item);
                None
            }
            Err(e) if cancel.load(Ordering::Relaxed) => {
                tracing::debug!("Quran: download of {item:?} paused ({e})");
                None
            }
            Err(e) => {
                *failures += 1;
                tracing::warn!("Quran: download of {} surah {} failed: {e}", item.0, item.1);
                q.error = Some(format!("Download of {} failed: {e}", name(item.1)));
                // Network down? Wait before trying again.
                Some(backoff(*failures))
            }
        }
    }

    async fn download(&self, item: &Item, cancel: &AtomicBool) -> Result<(), String> {
        let (key, n) = (item.0.as_str(), item.1);
        let reciter = self.reciter(key).ok_or("unknown reciter")?;
        if !reciter.has(n) {
            return Err("this reciter doesn't have that surah".into());
        }
        let url = reciter.url(n);
        let _ = std::fs::create_dir_all(self.dir(key));
        let part = self.part_path(key, n);
        // Learn the size (for progress, and to finish a .part that is already complete).
        let known = self.queue.locked().totals.get(item).copied();
        let total = match known {
            Some(t) => Some(t),
            None => self.fetcher.content_length(&url).await,
        };
        if let Some(t) = total {
            self.queue.locked().totals.insert(item.clone(), t);
        }
        let have = std::fs::metadata(&part).map_or(0, |m| m.len());
        if total.is_none_or(|t| have < t) {
            self.fetcher.fetch(&url, &part, cancel).await?;
        }
        let got = std::fs::metadata(&part).map_or(0, |m| m.len());
        if total.is_some_and(|t| got != t) || got < MIN_FILE {
            return Err(format!("incomplete ({got} bytes)"));
        }
        std::fs::rename(&part, self.final_path(key, n)).map_err(|e| e.to_string())?;
        tracing::info!("Quran: {key} surah {n} ({}) downloaded, {} KB", name(n), got / 1024);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serves files of a fixed size, or fails; remembers what was asked for.
    struct Fake {
        size: u64,
        fail: bool,
        /// Behave as if the download was paused while running.
        cancelled: bool,
        fetched: Mutex<Vec<String>>,
    }

    impl Fetcher for Fake {
        async fn content_length(&self, _url: &str) -> Option<u64> {
            Some(self.size)
        }

        async fn fetch(&self, url: &str, dest: &Path, cancel: &AtomicBool) -> Result<(), String> {
            self.fetched.locked().push(url.to_string());
            if self.cancelled {
                cancel.store(true, Ordering::Relaxed);
                return Err("cancelled".into());
            }
            if self.fail {
                return Err("network unreachable".into());
            }
            std::fs::write(dest, vec![0u8; usize::try_from(self.size).unwrap()]).map_err(|e| e.to_string())
        }
    }

    fn fake(size: u64, fail: bool) -> Fake {
        Fake { size, fail, cancelled: false, fetched: Mutex::new(Vec::new()) }
    }

    fn library(size: u64, fail: bool) -> (Arc<Library<Fake>>, std::path::PathBuf) {
        fake_library(fake(size, fail))
    }

    fn fake_library(fake: Fake) -> (Arc<Library<Fake>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lm-quran-test-{}", crate::config::random_hex(6)));
        let lib = Library::with_fetcher(&dir, fake);
        (lib, dir)
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn content_length_takes_the_last_header_block() {
        let h = "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 4096\r\n";
        assert_eq!(last_content_length(h), Some(4096));
        assert_eq!(last_content_length("HTTP/1.1 200 OK\r\n"), None);
    }

    #[test]
    fn backoff_doubles_up_to_ten_minutes() {
        assert_eq!([1, 2, 3, 5, 6, 20].map(|f| backoff(f).as_secs()), [30, 60, 120, 480, 600, 600]);
    }

    #[test]
    fn requested_surah_goes_first_then_download_all_in_order() {
        let (lib, dir) = library(2048, false);
        let key = super::super::DEFAULT_RECITER;
        assert_eq!(lib.next_job(), None);
        lib.download_all(Some(key));
        assert_eq!(lib.next_job(), Some((key.to_string(), 1)));
        lib.request(key, 36);
        assert_eq!(lib.next_job(), Some((key.to_string(), 36)));
        lib.request(key, 67);
        assert_eq!(lib.next_job(), Some((key.to_string(), 67)), "only the latest request is kept");

        let mut failures = 0;
        assert_eq!(block_on(lib.run_job(&(key.to_string(), 67), &mut failures)), None);
        assert!(lib.is_ready(key, 67));
        assert_eq!(lib.next_job(), Some((key.to_string(), 1)), "back to download-all");
        lib.download_all(None);
        assert_eq!(lib.next_job(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failures_back_off_and_are_reported() {
        let (lib, dir) = library(2048, true);
        let item = (super::super::DEFAULT_RECITER.to_string(), 112);
        let mut failures = 0;
        assert_eq!(block_on(lib.run_job(&item, &mut failures)), Some(Duration::from_secs(30)));
        assert_eq!(failures, 1);
        let info = lib.info(&item.0);
        assert!(info.error.unwrap().contains("Al-Ikhlāṣ"));
        assert!(!lib.is_ready(&item.0, 112));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tiny_file_is_not_a_recitation() {
        let (lib, dir) = library(100, false);
        let item = (super::super::DEFAULT_RECITER.to_string(), 1);
        let mut failures = 0;
        assert!(block_on(lib.run_job(&item, &mut failures)).is_some());
        assert!(!lib.is_ready(&item.0, 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_complete_part_file_is_finished_without_fetching() {
        let (lib, dir) = library(2048, false);
        let key = super::super::DEFAULT_RECITER;
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        std::fs::write(lib.part_path(key, 2), vec![0u8; 2048]).unwrap();
        let mut failures = 0;
        assert_eq!(block_on(lib.run_job(&(key.to_string(), 2), &mut failures)), None);
        assert!(lib.is_ready(key, 2));
        assert!(lib.fetcher.fetched.locked().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn requests_for_ready_invalid_or_active_surahs_change_nothing() {
        let (lib, dir) = library(2048, false);
        let key = super::super::DEFAULT_RECITER;
        lib.request(key, 0);
        lib.request(key, 115);
        assert_eq!(lib.next_job(), None, "invalid surahs are ignored");
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        std::fs::write(lib.final_path(key, 3), b"x").unwrap();
        lib.request(key, 3);
        assert_eq!(lib.next_job(), None, "already downloaded");

        // A surah that is downloading isn't queued again.
        let cancel = Arc::new(AtomicBool::new(false));
        lib.queue.locked().active = Some(((key.to_string(), 5), cancel.clone()));
        lib.request(key, 5);
        assert!(lib.queue.locked().priority.is_empty());
        assert!(!cancel.load(Ordering::Relaxed));
        // Another surah pauses it.
        lib.request(key, 6);
        assert!(cancel.load(Ordering::Relaxed), "the active download yields");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_download_all_cancels_background_work_but_not_a_listened_surah() {
        let (lib, dir) = library(2048, false);
        let key = super::super::DEFAULT_RECITER;
        let bg = Arc::new(AtomicBool::new(false));
        lib.queue.locked().active = Some(((key.to_string(), 10), bg.clone()));
        lib.download_all(Some(key));
        lib.download_all(None);
        assert!(bg.load(Ordering::Relaxed), "background download stopped");

        let listened = Arc::new(AtomicBool::new(false));
        {
            let mut q = lib.queue.locked();
            q.active = Some(((key.to_string(), 11), listened.clone()));
            q.priority.push_back((key.to_string(), 11));
        }
        lib.download_all(None);
        assert!(!listened.load(Ordering::Relaxed), "the surah being listened to continues");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn finished_requests_are_skipped_and_download_all_ends() {
        let (lib, dir) = library(2048, false);
        let key = super::super::DEFAULT_RECITER;
        lib.request(key, 7);
        std::fs::create_dir_all(lib.dir(key)).unwrap();
        std::fs::write(lib.final_path(key, 7), b"x").unwrap();
        assert_eq!(lib.next_job(), None, "a request that finished meanwhile is dropped");
        assert!(lib.queue.locked().priority.is_empty());

        for n in 1..=114 {
            std::fs::write(lib.final_path(key, n), b"x").unwrap();
        }
        lib.download_all(Some(key));
        assert_eq!(lib.next_job(), None);
        assert!(lib.queue.locked().all.is_none(), "nothing left: download-all ends");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_paused_download_is_not_a_failure() {
        let (lib, dir) = fake_library(Fake { cancelled: true, ..fake(2048, false) });
        let mut failures = 0;
        let item = (super::super::DEFAULT_RECITER.to_string(), 9);
        assert_eq!(block_on(lib.run_job(&item, &mut failures)), None);
        assert_eq!(failures, 0);
        assert!(lib.info(&item.0).error.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_reciters_and_missing_surahs_fail() {
        let (lib, dir) = library(2048, false);
        let mut failures = 0;
        assert!(block_on(lib.run_job(&("nobody".to_string(), 1), &mut failures)).is_some());
        assert!(lib.queue.locked().error.as_deref().unwrap().contains("unknown reciter"));
        // A recitation without surah 2.
        let partial = r#"[{"key":"partial-r1","name":"P","style":"","server":"https://cdn.mp3quran.net/p/","surahs":[1]}]"#;
        std::fs::write(dir.join("quran").join("reciters.json"), partial).unwrap();
        let lib = Library::with_fetcher(&dir, fake(2048, false));
        assert!(block_on(lib.run_job(&("partial-r1".to_string(), 2), &mut failures)).is_some());
        assert!(lib.info("partial-r1").error.unwrap().contains("doesn't have"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_known_size_is_not_asked_again() {
        let (lib, dir) = library(2048, false);
        let item = (super::super::DEFAULT_RECITER.to_string(), 4);
        lib.queue.locked().totals.insert(item.clone(), 2048);
        let mut failures = 0;
        assert_eq!(block_on(lib.run_job(&item, &mut failures)), None);
        assert!(lib.is_ready(&item.0, 4));
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        for _ in 0..150 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn the_worker_downloads_requests_and_wakes_from_a_back_off() {
        let key = super::super::DEFAULT_RECITER;
        let (lib, dir) = library(2048, false);
        let task = tokio::spawn(lib.clone().run());
        lib.request(key, 113);
        wait_until(|| lib.is_ready(key, 113)).await;
        assert!(lib.is_ready(key, 113));
        task.abort();
        let _ = std::fs::remove_dir_all(dir);

        let (lib, dir) = library(2048, true);
        let task = tokio::spawn(lib.clone().run());
        lib.request(key, 114);
        wait_until(|| lib.queue.locked().error.is_some()).await;
        // Backing off now (30 s); a new request wakes it at once.
        lib.request(key, 112);
        wait_until(|| lib.fetcher.fetched.locked().len() >= 2).await;
        assert!(lib.fetcher.fetched.locked().len() >= 2, "woken from the back-off");
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn curl_downloads_reports_errors_and_can_be_cancelled() {
        let body = "x".repeat(2048);
        let (base, _) = crate::integrations::test_http::serve(move |path| match path {
            "/ok.mp3" => (200, body.clone()),
            "/hang.mp3" => (0, String::new()),
            _ => (404, String::new()),
        })
        .await;
        let dir = std::env::temp_dir().join(format!("lm-curl-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let no = AtomicBool::new(false);

        assert_eq!(curl_content_length(&format!("{base}/ok.mp3"), "=http").await, Some(2048));
        let dest = dir.join("ok.part");
        curl_fetch(&format!("{base}/ok.mp3"), &dest, &no, "=http").await.unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 2048);

        let err = curl_fetch(&format!("{base}/missing.mp3"), &dir.join("m.part"), &no, "=http").await.unwrap_err();
        assert!(err.contains("404"), "{err}");
        // The real fetcher only speaks https.
        assert!(Curl.fetch(&format!("{base}/ok.mp3"), &dir.join("h.part"), &no).await.is_err());
        assert_eq!(Curl.content_length(&format!("{base}/ok.mp3")).await, None);

        let cancel = AtomicBool::new(true);
        let err = curl_fetch(&format!("{base}/hang.mp3"), &dir.join("c.part"), &cancel, "=http").await.unwrap_err();
        assert_eq!(err, "cancelled");
        let _ = std::fs::remove_dir_all(dir);
    }
}
