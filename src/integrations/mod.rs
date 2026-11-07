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
    pub livemasjid_gate: OneAtATime,
    pub recordings: Cache<recordings::RecordingList>,
}

impl Integrations {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            masjidscreen: SiteGuard::load(data_dir.join("site-masjidscreen.json")),
            masjidscreen_gate: OneAtATime::default(),
            salah: Cache::default(),
            livemasjid_gate: OneAtATime::default(),
            recordings: Cache::default(),
        }
    }
}

/// Mount names are used in URLs: allow only what livemasjid uses.
pub fn valid_mount(m: &str) -> bool {
    !m.is_empty() && m.len() <= 80 && m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_expires() {
        let c: Cache<u32> = Cache::default();
        c.put("a", 1, Duration::from_secs(60));
        c.put("b", 2, Duration::ZERO);
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("b"), None);
        assert!(valid_mount("lakewood") && !valid_mount("a/b") && !valid_mount(""));
    }
}
