//! Layer 1: the website's server-sent "online" push for the selected mount.
//!
//! Like the livemasjid.com player (which closes its EventSource once "online" arrives),
//! we only hold the connection while *waiting* for a broadcast, and never while the
//! website is refusing us (see [`crate::site`]).

use super::jitter;
use crate::site::is_refusal;
use crate::state::{App, Phase, Signal, Status, now_ms};
use futures_util::StreamExt;
use std::time::{Duration, Instant};

pub async fn run(app: App) {
    let mut gen_rx = app.mount_gen.subscribe();
    let mut st_rx = app.status.subscribe();
    loop {
        let Some(mount) = app.settings().mount else {
            set_connected(&app, false);
            let _ = gen_rx.changed().await;
            continue;
        };
        let waiting = |s: &Status| s.phase == Phase::Waiting;
        if !waiting(&st_rx.borrow_and_update()) {
            set_connected(&app, false);
            tokio::select! {
                _ = st_rx.wait_for(waiting) => {}
                _ = gen_rx.changed() => {}
            }
            continue;
        }
        tokio::select! {
            _ = listen(&app, &mount) => {}
            _ = gen_rx.changed() => {}
            _ = st_rx.wait_for(|s| s.phase != Phase::Waiting) => {}
        }
    }
}

fn set_connected(app: &App, on: bool) {
    app.status.send_if_modified(|s| std::mem::replace(&mut s.signals.sse_connected, on) != on);
}

/// Never reconnect to the website faster than this.
const MIN_RECONNECT_S: u64 = 15;

/// Keep an SSE connection open, reconnecting politely, until cancelled.
async fn listen(app: &App, mount: &str) {
    let bases = crate::net::www_bases();
    let mut base_idx = 0usize;
    let mut backoff = MIN_RECONNECT_S;
    loop {
        if let Some(until) = app.site.blocked_until() {
            set_connected(app, false);
            let wait = until.saturating_sub(now_ms()) / 1000 + 1;
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        let url = format!("{}/stateQuery.php?mount={}", bases[base_idx % bases.len()], urlencode(mount));
        let started = Instant::now();
        match connect_once(app, &url).await {
            // A long-lived connection that ended normally: reconnect after a polite pause
            // (the status poll still catches a broadcast within ~20 s meanwhile).
            Ok(()) if started.elapsed() > Duration::from_secs(30) => backoff = MIN_RECONNECT_S,
            // Server closed quickly without error: don't spin.
            Ok(()) => backoff = (backoff * 2).min(300),
            Err(e) if is_refusal(&e) => {
                let until = app.site.refused();
                let hours = until.saturating_sub(now_ms()) / 3_600_000;
                app.log("warn", format!("livemasjid.com refused the live push ({e}); not contacting it for {hours} h"));
            }
            Err(e) => {
                tracing::debug!("sse {url}: {e}");
                base_idx += 1;
                backoff = (backoff * 2).min(300);
            }
        }
        set_connected(app, false);
        tokio::time::sleep(Duration::from_secs(jitter(backoff))).await;
    }
}

async fn connect_once(app: &App, url: &str) -> anyhow::Result<()> {
    let resp = app
        .http
        .get(url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .send()
        .await?
        .error_for_status()?;
    app.site.ok();
    set_connected(app, true);
    let mut body = resp.bytes_stream();
    let mut parser = EventParser::default();
    // The endpoint is silent while a mount is offline; recycle the connection
    // periodically so a half-open TCP connection can't leave us deaf.
    let idle = Duration::from_secs(300);
    loop {
        let chunk = match tokio::time::timeout(idle, body.next()).await {
            Err(_) | Ok(None) => return Ok(()),
            Ok(Some(c)) => c?,
        };
        for data in parser.feed(&chunk)? {
            on_event(app, &data);
        }
    }
}

fn on_event(app: &App, data: &str) {
    // Payload is a (multi-line) JSON object: {"msg": "online", "id": "<mount>"}.
    let msg = serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get("msg")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| data.to_string());
    app.status.send_modify(|s| s.signals.sse_last_event_ms = now_ms());
    if msg.to_ascii_lowercase().contains("online") {
        let _ = app.signal_tx.send(Signal::SseOnline);
    }
}

/// Minimal `text/event-stream` parser: joins multi-line `data:` fields per event.
#[derive(Default)]
struct EventParser {
    buf: Vec<u8>,
    data: String,
}

impl EventParser {
    fn feed(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<String>> {
        self.buf.extend_from_slice(chunk);
        anyhow::ensure!(self.buf.len() <= 256 * 1024, "event too large");
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data));
                }
            } else if let Some(v) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        Ok(events)
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_multiline_data() {
        let mut p = EventParser::default();
        let ev = p.feed(b"id: makkah\ndata: {\ndata: \"msg\": \"online\"\ndata: }\n\n").unwrap();
        assert_eq!(ev, vec!["{\n\"msg\": \"online\"\n}".to_string()]);
        assert!(p.feed(b"data: partial").unwrap().is_empty());
        assert_eq!(p.feed(b"\n\n").unwrap(), vec!["partial".to_string()]);
    }

    #[test]
    fn encodes_mounts() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
