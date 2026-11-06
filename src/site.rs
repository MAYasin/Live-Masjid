//! Good-citizen guard for www.livemasjid.com (behind Cloudflare).
//!
//! The website is only *nice to have*: it provides the instant "online" push and the
//! masjid directory. Playback itself only needs the public Icecast servers. So if the
//! site refuses us (403 / 429 / Cloudflare 1006 "IP banned"), we stop calling it entirely
//! for hours — never retry in a loop, never disguise the client. The back-off is saved to
//! disk so restarts and reboots don't reset it.

use crate::state::now_ms;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

const HOUR_MS: u64 = 3_600_000;

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
struct State {
    blocked_until_ms: u64,
    strikes: u32,
}

#[derive(Default)]
pub struct SiteGuard {
    state: Mutex<State>,
    path: Option<PathBuf>,
}

impl SiteGuard {
    /// Restore the back-off state from `path` (if present) and persist changes there.
    pub fn load(path: PathBuf) -> Self {
        let state = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { state: Mutex::new(state), path: Some(path) }
    }

    fn save(&self, s: State) {
        if let (Some(path), Ok(bytes)) = (&self.path, serde_json::to_vec(&s)) {
            let _ = crate::config::write_atomic(path, &bytes);
        }
    }

    /// May we send a request to the website right now?
    pub fn allowed(&self) -> bool {
        now_ms() >= self.state.lock().unwrap().blocked_until_ms
    }

    pub fn blocked_until(&self) -> Option<u64> {
        let t = self.state.lock().unwrap().blocked_until_ms;
        (t > now_ms()).then_some(t)
    }

    /// Record a refusal. Pauses all website traffic for 6 h, then 12 h, then 24 h.
    /// Returns the time (ms) until which we stay away.
    pub fn refused(&self) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.strikes += 1;
        let hours = match s.strikes {
            1 => 6,
            2 => 12,
            _ => 24,
        };
        s.blocked_until_ms = now_ms() + hours * HOUR_MS;
        self.save(*s);
        s.blocked_until_ms
    }

    /// A successful website request: forget past strikes.
    pub fn ok(&self) {
        let mut s = self.state.lock().unwrap();
        if s.strikes != 0 || s.blocked_until_ms != 0 {
            *s = State::default();
            self.save(*s);
        }
    }
}

/// Does this error mean "you are not welcome / slow down"?
pub fn is_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|s| matches!(s.as_u16(), 403 | 429))
}

pub fn is_website(url: &str) -> bool {
    url.contains("://www.livemasjid.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalates_and_resets() {
        let g = SiteGuard::default();
        assert!(g.allowed());
        let first = g.refused();
        assert!(!g.allowed());
        assert!(first >= now_ms() + 5 * HOUR_MS);
        let second = g.refused();
        assert!(second > first);
        g.ok();
        assert!(g.allowed());
        assert!(is_website("https://www.livemasjid.com/stateQuery.php"));
        assert!(!is_website("http://relay2.livemasjid.com:8000/x"));
    }

    #[test]
    fn survives_restart() {
        let path = std::env::temp_dir().join(format!("lm-site-test-{}.json", std::process::id()));
        let until = SiteGuard::load(path.clone()).refused();
        let reloaded = SiteGuard::load(path.clone());
        assert_eq!(reloaded.blocked_until(), Some(until));
        let _ = std::fs::remove_file(path);
    }
}
