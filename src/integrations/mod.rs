//! Third-party data shown in the UI: salah times (masjidscreen.co.za) and recordings
//! (livemasjid.com). Designed so we can never be the reason a site blocks us:
//!
//! * the **Pi** fetches, never the browser — opening the UI from five phones costs nothing;
//! * every result (including "not found" and errors) is **cached** with a generous TTL;
//! * at most **one request per site at a time**, honest User-Agent, short timeouts;
//! * a 403/429 trips that site's [`SiteGuard`] (6 → 12 → 24 h back-off, persisted) and the UI
//!   shows a fallback instead.
//!
//! Requests go through `curl` (installed on every device by `install.sh`) so HTTPS works
//! with the system's TLS in every build.

pub mod masjidboard;
pub mod recordings;
pub mod salah;

use crate::site::SiteGuard;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Command;

pub const USER_AGENT: &str = concat!("livemasjid-pi/", env!("CARGO_PKG_VERSION"), " (+https://github.com/MAYasin/Live-Masjid)");

/// Outcome of a fetch, as cached and shown to the UI.
#[derive(Clone, Debug)]
pub enum Fetched {
    Ok(String),
    /// 404: the site doesn't have this masjid.
    NotFound,
    /// The site refused us (403/429) or we're backing off.
    Blocked { until_ms: u64 },
    Failed(String),
}

/// GET `url` with curl. Returns the body for 2xx; classifies the rest.
pub async fn fetch(url: &str, timeout: Duration, guard: &SiteGuard) -> Fetched {
    if let Some(until_ms) = guard.blocked_until() {
        return Fetched::Blocked { until_ms };
    }
    let secs = timeout.as_secs().max(1).to_string();
    let fut = Command::new("curl")
        .args(["-sS", "-L", "--max-redirs", "3", "--proto", "=https,http", "--max-time", &secs, "-A", USER_AGENT])
        .args(["-H", "Accept: application/json, application/rss+xml, application/xml, */*"])
        .args(["-w", "\n%{http_code}", url])
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(timeout + Duration::from_secs(2), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Fetched::Failed(format!("curl: {e}")),
        Err(_) => return Fetched::Failed("timed out".into()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_ref()));
    match code.trim().parse::<u16>().unwrap_or(0) {
        200..=299 => {
            guard.ok();
            Fetched::Ok(body.to_string())
        }
        404 => Fetched::NotFound,
        403 | 429 => Fetched::Blocked { until_ms: guard.refused() },
        0 => Fetched::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        c => Fetched::Failed(format!("HTTP {c}")),
    }
}

/// Small TTL cache: one entry per key, each with its own expiry.
pub struct Cache<T: Clone> {
    map: Mutex<HashMap<String, (Instant, T)>>,
}

impl<T: Clone> Default for Cache<T> {
    fn default() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
}

impl<T: Clone> Cache<T> {
    pub fn get(&self, key: &str) -> Option<T> {
        let map = self.map.lock().unwrap();
        map.get(key).filter(|(exp, _)| Instant::now() < *exp).map(|(_, v)| v.clone())
    }

    pub fn put(&self, key: &str, value: T, ttl: Duration) {
        let mut map = self.map.lock().unwrap();
        if map.len() > 500 {
            map.retain(|_, (exp, _)| Instant::now() < *exp);
        }
        map.insert(key.to_string(), (Instant::now() + ttl, value));
    }
}

/// Serialises requests per site, so a burst of UI requests becomes one fetch at a time.
pub struct OneAtATime(tokio::sync::Mutex<()>);

impl Default for OneAtATime {
    fn default() -> Self {
        Self(tokio::sync::Mutex::new(()))
    }
}

impl OneAtATime {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }
}

/// Everything the integrations share, stored in `AppState`.
pub struct Integrations {
    pub masjidscreen: SiteGuard,
    pub masjidscreen_gate: OneAtATime,
    pub salah: Cache<salah::SalahTimes>,
    pub masjidboard: SiteGuard,
    pub masjidboard_gate: OneAtATime,
    pub boards: Cache<masjidboard::Lookup>,
    pub livemasjid_gate: OneAtATime,
    pub recordings: Cache<recordings::RecordingList>,
    /// Last good recording lists, on disk (`recordings.json`).
    pub recordings_saved: recordings::Store,
}

impl Integrations {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            masjidscreen: SiteGuard::load(data_dir.join("site-masjidscreen.json")),
            masjidscreen_gate: OneAtATime::default(),
            salah: Cache::default(),
            masjidboard: SiteGuard::load(data_dir.join("site-masjidboard.json")),
            masjidboard_gate: OneAtATime::default(),
            boards: Cache::default(),
            livemasjid_gate: OneAtATime::default(),
            recordings: Cache::default(),
            recordings_saved: recordings::Store::load(data_dir.join("recordings.json")),
        }
    }
}

/// Mount names are used in URLs: allow only what livemasjid uses.
pub fn valid_mount(m: &str) -> bool {
    !m.is_empty() && m.len() <= 80 && m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// A local HTTP server for tests (127.0.0.1 only, never a real host).
#[cfg(test)]
pub(crate) mod test_http {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers every request with `route(path)` → (status code, body); status 0 holds the
    /// connection open without answering. Returns the base URL and a request counter.
    pub async fn serve<F>(route: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let route = Arc::new(route);
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let route = route.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    let text = String::from_utf8_lossy(&req);
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (code, body) = route(&path);
                    if code == 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        return;
                    }
                    let response = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// A URL on 127.0.0.1 where nothing listens.
    pub fn refused_url() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> SiteGuard {
        SiteGuard::default()
    }

    #[tokio::test]
    async fn fetch_classifies_answers() {
        let (base, hits) = test_http::serve(|path| match path {
            "/ok" => (200, "hello\nworld".into()),
            "/missing" => (404, "no".into()),
            "/forbidden" => (403, "".into()),
            "/slow-down" => (429, "".into()),
            _ => (500, "boom".into()),
        })
        .await;
        let g = guard();
        let t = Duration::from_secs(5);
        assert!(matches!(fetch(&format!("{base}/ok"), t, &g).await, Fetched::Ok(b) if b == "hello\nworld"));
        assert!(matches!(fetch(&format!("{base}/missing"), t, &g).await, Fetched::NotFound));
        assert!(matches!(fetch(&format!("{base}/other"), t, &g).await, Fetched::Failed(e) if e == "HTTP 500"));
        assert!(g.blocked_until().is_none());
        assert!(matches!(fetch(&format!("{base}/forbidden"), t, &g).await, Fetched::Blocked { .. }));
        let until = g.blocked_until().expect("a 403 trips the guard");
        // While blocked, nothing is sent at all.
        let before = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(fetch(&format!("{base}/ok"), t, &g).await, Fetched::Blocked { until_ms } if until_ms == until));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), before);

        let g2 = guard();
        assert!(matches!(fetch(&format!("{base}/slow-down"), t, &g2).await, Fetched::Blocked { .. }));
        assert!(g2.blocked_until().is_some(), "429 trips it too");
    }

    #[tokio::test]
    async fn fetch_reports_connection_failures() {
        let g = guard();
        match fetch(&format!("{}/x", test_http::refused_url()), Duration::from_secs(3), &g).await {
            Fetched::Failed(e) => assert!(!e.is_empty(), "curl's own error is passed on"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(g.blocked_until().is_none(), "a network failure is not a refusal");
    }

    #[tokio::test]
    async fn fetch_gives_up_on_a_server_that_never_answers() {
        let (base, _) = test_http::serve(|_| (0, String::new())).await;
        let started = Instant::now();
        let r = fetch(&format!("{base}/hang"), Duration::from_millis(500), &guard()).await;
        assert!(matches!(r, Fetched::Failed(_)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn one_at_a_time_serialises() {
        let gate = OneAtATime::default();
        let first = gate.lock().await;
        assert!(gate.0.try_lock().is_err());
        drop(first);
        let _second = gate.lock().await;
    }

    #[test]
    fn integrations_load_from_the_data_folder() {
        let dir = std::env::temp_dir().join(format!("lm-int-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let it = Integrations::new(&dir);
        assert!(it.masjidscreen.blocked_until().is_none() && it.masjidboard.blocked_until().is_none());
        assert!(it.salah.get("x").is_none() && it.boards.get("x").is_none() && it.recordings.get("x").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cache_expires() {
        let c: Cache<u32> = Cache::default();
        c.put("a", 1, Duration::from_secs(60));
        c.put("b", 2, Duration::ZERO);
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("b"), None);
        assert!(valid_mount("lakewood") && !valid_mount("a/b") && !valid_mount(""));
    }

    #[test]
    fn cache_overwrites_and_misses() {
        let c: Cache<u32> = Cache::default();
        c.put("a", 1, Duration::from_secs(60));
        c.put("a", 2, Duration::from_secs(60));
        assert_eq!(c.get("a"), Some(2));
        assert_eq!(c.get("missing"), None);
    }

    #[test]
    fn cache_prunes_expired_entries_when_large() {
        let c: Cache<u32> = Cache::default();
        {
            let mut m = c.map.lock().unwrap();
            m.insert("keep".into(), (Instant::now() + Duration::from_secs(60), 7));
            for i in 0..600 {
                m.insert(format!("k{i}"), (Instant::now() - Duration::from_secs(1), i));
            }
        }
        c.put("fresh", 9, Duration::from_secs(60));
        assert_eq!(c.map.lock().unwrap().len(), 2, "expired entries dropped");
        assert_eq!(c.get("keep"), Some(7));
        assert_eq!(c.get("fresh"), Some(9));
    }

    #[test]
    fn mount_names_are_url_safe() {
        assert!(valid_mount("lakewood-kauthar"));
        assert!(valid_mount("a_b.c-d"));
        assert!(valid_mount(&"a".repeat(80)));
        assert!(!valid_mount(&"a".repeat(81)), "too long");
        assert!(!valid_mount("a b"));
        assert!(!valid_mount("a/b"));
        assert!(!valid_mount("münchen"));
        assert!(!valid_mount(""));
    }
}
