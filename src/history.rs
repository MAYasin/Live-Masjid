//! Play history: one JSON line per finished session, appended to history.jsonl.
//! An in-progress session is also recorded on start (so "last heard" survives a crash)
//! and rewritten when it ends.

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
}

const KEEP: usize = 2000;

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
        if self.sessions.len() > KEEP || (force_check && len > 2_000_000) || len > 4_000_000 {
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
