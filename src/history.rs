//! Play history: one JSON line per finished session, appended to history.jsonl.
//! An in-progress session is also recorded on start (so "last heard" survives a crash)
//! and rewritten when it ends. Sessions older than a year are pruned (at most 2000 kept).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: u64,
    pub mount: String,
    pub name: String,
    pub start_ms: u64,
    /// Updated while playing; final value when the session ends.
    pub end_ms: u64,
    pub codec: String,
    /// What started it: "sse", "poll", "probe", "manual".
    pub trigger: String,
    /// Why it ended: "ended", "stopped", "switched", "error", "restart".
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub reconnects: u32,
    /// The salah this broadcast belonged to ("Esha", "Jumu'ah"…), from that day's times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salah: Option<String>,
}

/// At most this many sessions (a year of five broadcasts a day is about 1,800).
const KEEP: usize = 4000;
/// Sessions older than this are dropped.
const KEEP_MS: u64 = 365 * 24 * 3600 * 1000;

pub struct History {
    path: PathBuf,
    sessions: Vec<Session>,
    last_heard: HashMap<String, u64>,
    last_persist: u64,
}

impl History {
    pub fn load(path: PathBuf) -> Self {
        let mut sessions: Vec<Session> = Vec::new();
        if let Ok(f) = std::fs::File::open(&path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(s) = serde_json::from_str::<Session>(&line) {
                    // Later lines for the same id supersede earlier ones.
                    if let Some(prev) = sessions.iter_mut().rev().take(4).find(|p| p.id == s.id) {
                        *prev = s;
                    } else {
                        sessions.push(s);
                    }
                }
            }
        }
        for s in sessions.iter_mut().filter(|s| s.reason.is_empty()) {
            s.reason = "restart".into();
        }
        let mut h = Self { path, sessions, last_heard: HashMap::new(), last_persist: 0 };
        h.compact_if_needed(true);
        h.rebuild_index();
        h
    }

    fn rebuild_index(&mut self) {
        self.last_heard.clear();
        for s in &self.sessions {
            let e = self.last_heard.entry(s.mount.clone()).or_default();
            *e = (*e).max(s.end_ms);
        }
    }

    pub fn last_heard(&self, mount: &str) -> Option<u64> {
        self.last_heard.get(mount).copied()
    }

    pub fn all_last_heard(&self) -> &HashMap<String, u64> {
        &self.last_heard
    }

    pub fn recent(&self, limit: usize, mount: Option<&str>) -> Vec<Session> {
        self.sessions
            .iter()
            .rev()
            .filter(|s| mount.is_none_or(|m| s.mount == m))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn begin(&mut self, mount: &str, name: &str, codec: &str, trigger: &str) -> u64 {
        let now = crate::state::now_ms();
        let id = self.sessions.last().map(|s| s.id + 1).unwrap_or(1).max(now);
        let s = Session {
            id,
            mount: mount.into(),
            name: name.into(),
            start_ms: now,
            end_ms: now,
            codec: codec.into(),
            trigger: trigger.into(),
            reason: String::new(),
            reconnects: 0,
            salah: None,
        };
        self.append(&s);
        self.sessions.push(s);
        self.last_heard.insert(mount.into(), now);
        id
    }

    /// Called periodically while audio flows so "last heard" stays current.
    pub fn touch(&mut self, id: u64, reconnects: u32) {
        let now = crate::state::now_ms();
        if let Some(s) = self.sessions.iter_mut().rev().find(|s| s.id == id) {
            s.end_ms = now;
            s.reconnects = reconnects;
            self.last_heard.insert(s.mount.clone(), now);
            if now.saturating_sub(self.last_persist) > 60_000 {
                let s = s.clone();
                self.last_persist = now;
                self.append(&s);
            }
        }
    }

    /// Record which salah a session belongs to (saved with its next write).
    pub fn set_salah(&mut self, id: u64, salah: Option<String>) {
        if let Some(s) = self.sessions.iter_mut().rev().find(|s| s.id == id) {
            s.salah = salah;
        }
    }

    pub fn finish(&mut self, id: u64, reason: &str, reconnects: u32) {
        let now = crate::state::now_ms();
        if let Some(i) = self.sessions.iter().rposition(|s| s.id == id) {
            let s = &mut self.sessions[i];
            s.end_ms = now;
            s.reason = reason.into();
            s.reconnects = reconnects;
            let s = s.clone();
            self.last_heard.insert(s.mount.clone(), now);
            self.append(&s);
        }
        self.compact_if_needed(false);
    }

    fn append(&self, s: &Session) {
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{}", serde_json::to_string(s).unwrap_or_default()));
        if let Err(e) = res {
            tracing::warn!("history write failed: {e}");
        }
    }

    fn compact_if_needed(&mut self, force_check: bool) {
        let len = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let cutoff = crate::state::now_ms().saturating_sub(KEEP_MS);
        let expired = self.sessions.first().is_some_and(|s| s.end_ms < cutoff);
        if expired || self.sessions.len() > KEEP || (force_check && len > 2_000_000) || len > 4_000_000 {
            self.sessions.retain(|s| s.end_ms >= cutoff);
            let skip = self.sessions.len().saturating_sub(KEEP);
            self.sessions.drain(..skip);
            let mut buf = Vec::new();
            for s in &self.sessions {
                let _ = writeln!(buf, "{}", serde_json::to_string(s).unwrap_or_default());
            }
            let _ = crate::config::write_atomic(&self.path, &buf);
        }
    }
}

// ---------------------------------------------------------------- views of the history

/// Listening on one day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DayTotal {
    pub count: u32,
    pub ms: u64,
}

/// Local calendar date ("YYYY-MM-DD") of `ts_ms`, for a zone `offset_s` east of UTC.
pub fn local_date(ts_ms: u64, offset_s: i64) -> String {
    #[allow(clippy::cast_possible_wrap)]
    let secs = (ts_ms / 1000) as i64 + offset_s;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (yoe + era * 400 + i64::from(m <= 2), m as u32, d as u32)
}

/// Does a broadcast's salah label match a filter? "Zuhr" includes Jumu'ah; "other" means no label.
pub fn salah_matches(label: Option<&str>, filter: &str) -> bool {
    match (label, filter) {
        (None, "other") => true,
        (Some(l), "Zuhr") => l == "Zuhr" || l == "Jumu'ah",
        (Some(l), f) => l == f,
        (None, _) => false,
    }
}

/// Count and listening time per local day.
pub fn per_day<'a>(sessions: impl IntoIterator<Item = &'a Session>, offset_s: i64) -> std::collections::BTreeMap<String, DayTotal> {
    let mut days = std::collections::BTreeMap::<String, DayTotal>::new();
    for s in sessions {
        let d = days.entry(local_date(s.start_ms, offset_s)).or_default();
        d.count += 1;
        d.ms += s.end_ms.saturating_sub(s.start_ms);
    }
    days
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(start_ms: u64, mins: u64) -> Session {
        Session {
            id: start_ms,
            mount: "m".into(),
            name: "M".into(),
            start_ms,
            end_ms: start_ms + mins * 60_000,
            codec: "opus".into(),
            trigger: "poll".into(),
            reason: "ended".into(),
            reconnects: 0,
            salah: None,
        }
    }

    #[test]
    fn local_dates() {
        assert_eq!(local_date(0, 0), "1970-01-01");
        // 2026-11-11 22:30 UTC is already the 12th in Johannesburg (UTC+2), still the 11th in UTC.
        let t = 1_794_436_200_000;
        assert_eq!(local_date(t, 0), "2026-11-11");
        assert_eq!(local_date(t, 7200), "2026-11-12");
        assert_eq!(local_date(t, -5 * 3600), "2026-11-11");
        assert_eq!(local_date(951_782_400_000, 0), "2000-02-29", "leap day");
    }

    #[test]
    fn salah_filters() {
        assert!(salah_matches(Some("Zuhr"), "Zuhr"));
        assert!(salah_matches(Some("Jumu'ah"), "Zuhr"), "Jumu'ah counts as Zuhr");
        assert!(!salah_matches(Some("Asr"), "Zuhr"));
        assert!(salah_matches(None, "other"));
        assert!(!salah_matches(Some("Esha"), "other"));
        assert!(!salah_matches(None, "Fajr"));
    }

    #[test]
    fn totals_per_day() {
        let day = 86_400_000;
        let list = [session(10 * day, 20), session(10 * day + 3_600_000, 15), session(11 * day, 5)];
        let days = per_day(&list, 0);
        assert_eq!(days.len(), 2);
        assert_eq!(days["1970-01-11"], DayTotal { count: 2, ms: 35 * 60_000 });
        assert_eq!(days["1970-01-12"], DayTotal { count: 1, ms: 5 * 60_000 });
    }
}
