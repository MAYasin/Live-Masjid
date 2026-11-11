//! The download queue: one surah at a time. The surah being listened to goes first (whatever
//! else was downloading yields and resumes later from its `.part` file), then "Download all"
//! works through the rest of a recitation. Failures back off from 15 s to 10 min.
//!
//! The network side is the [`Fetcher`] trait: [`Curl`] on the Pi, a fake in the tests.

use super::{Item, Library, name, valid};
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
        let out = Command::new("curl")
            .args(["-sSI", "-L", "--max-redirs", "3", "--proto", "=https", "--max-time", "20", "-A", crate::integrations::USER_AGENT, url])
            .kill_on_drop(true)
            .output()
            .await
            .ok()?;
        last_content_length(&String::from_utf8_lossy(&out.stdout))
    }

    async fn fetch(&self, url: &str, dest: &Path, cancel: &AtomicBool) -> Result<(), String> {
        let mut child = Command::new("curl")
            .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", "=https", "--retry", "2"])
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
        let mut q = self.queue.lock().unwrap();
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
        let mut q = self.queue.lock().unwrap();
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
        let mut q = self.queue.lock().unwrap();
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
        self.queue.lock().unwrap().active = Some((item.clone(), cancel.clone()));
        let result = self.download(item, &cancel).await;
        let mut q = self.queue.lock().unwrap();
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
        let known = self.queue.lock().unwrap().totals.get(item).copied();
        let total = match known {
            Some(t) => Some(t),
            None => self.fetcher.content_length(&url).await,
        };
        if let Some(t) = total {
            self.queue.lock().unwrap().totals.insert(item.clone(), t);
        }
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if total.is_none_or(|t| have < t) {
            self.fetcher.fetch(&url, &part, cancel).await?;
        }
        let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
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
        fetched: Mutex<Vec<String>>,
    }

    impl Fetcher for Fake {
        async fn content_length(&self, _url: &str) -> Option<u64> {
            Some(self.size)
        }

        async fn fetch(&self, url: &str, dest: &Path, _cancel: &AtomicBool) -> Result<(), String> {
            self.fetched.lock().unwrap().push(url.to_string());
            if self.fail {
                return Err("network unreachable".into());
            }
            std::fs::write(dest, vec![0u8; usize::try_from(self.size).unwrap()]).map_err(|e| e.to_string())
        }
    }

    fn library(size: u64, fail: bool) -> (Arc<Library<Fake>>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lm-quran-test-{}", crate::config::random_hex(6)));
        let lib = Library::with_fetcher(&dir, Fake { size, fail, fetched: Mutex::new(Vec::new()) });
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
        assert!(lib.fetcher.fetched.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
