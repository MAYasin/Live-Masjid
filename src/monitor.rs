//! Live detection, three independent layers:
//!  1. Server-sent events from livemasjid.com/stateQuery.php (instant "online" push).
//!  2. Icecast status-json poll every few seconds (catches missed/incorrect SSE, and
//!     also feeds live badges for the whole directory in the UI).
//!  3. The stream itself: the player treats actual audio as the final truth and keeps
//!     playing until the mount really goes away, whatever the signals say.

use crate::state::{App, LiveInfo, Phase, Signal, now_ms};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;

/// SSE watcher for the selected mount. Restarts whenever the selection changes.
///
/// Like the livemasjid.com player (which closes its EventSource once "online" arrives),
/// we only hold the SSE connection while *waiting* for a broadcast. While a broadcast is
/// playing, connecting or stopped-by-user, the endpoint would just repeat "online", so we
/// stay disconnected and let the stream itself tell us when it ends.
pub async fn run_sse(app: App) {
    let mut gen_rx = app.mount_gen.subscribe();
    let mut st_rx = app.status.subscribe();
    loop {
        let mount = app.settings().mount.clone();
        let Some(mount) = mount else {
            app.status.send_modify(|s| s.signals.sse_connected = false);
            let _ = gen_rx.changed().await;
            continue;
        };
        // Wait until we're actually waiting for this mount to go live.
        let waiting = |s: &crate::state::Status| s.phase == Phase::Waiting;
        if !waiting(&st_rx.borrow_and_update()) {
            app.status.send_modify(|s| s.signals.sse_connected = false);
            tokio::select! {
                _ = st_rx.wait_for(waiting) => {}
                _ = gen_rx.changed() => {}
            }
            continue;
        }
        tokio::select! {
            _ = sse_loop(&app, &mount) => {}
            _ = gen_rx.changed() => {}
            _ = st_rx.wait_for(|s| s.phase != Phase::Waiting) => {}
        }
    }
}

async fn sse_loop(app: &App, mount: &str) {
    let mut backoff = 2u64;
    let bases = crate::net::www_bases();
    let mut base_idx = 0usize;
    let mut warned = false;
    loop {
        let base = bases[base_idx % bases.len()];
        let url = format!("{base}/stateQuery.php?mount={}", urlencode(mount));
        let started = std::time::Instant::now();
        match sse_once(app, &url).await {
            // A long-lived connection that ended normally: reconnect promptly.
            Ok(()) if started.elapsed() > Duration::from_secs(30) => backoff = 2,
            // Ended quickly without error (server closed): don't spin.
            Ok(()) => backoff = (backoff * 2).min(60),
            Err(e) => {
                let blocked = is_blocked(&e);
                if blocked && !warned {
                    app.log("warn", format!("Live push refused by server ({e}); backing off"));
                    warned = true;
                }
                tracing::debug!("sse {url}: {e}");
                base_idx += 1;
                let cap = if blocked { 600 } else { 60 };
                backoff = (backoff * 2).min(cap);
            }
        }
        if backoff == 2 {
            warned = false;
        }
        app.status.send_modify(|s| s.signals.sse_connected = false);
        tokio::time::sleep(Duration::from_secs(jitter(backoff))).await;
    }
}

/// True for responses that mean "you're making too many requests / not welcome":
/// back off hard instead of retrying.
fn is_blocked(e: &anyhow::Error) -> bool {
    if let Some(re) = e.downcast_ref::<reqwest::Error>() {
        if let Some(s) = re.status() {
            return matches!(s.as_u16(), 403 | 429 | 503);
        }
        return re.is_connect();
    }
    false
}

/// ±20 % randomisation so many devices never synchronise their requests.
fn jitter(secs: u64) -> u64 {
    use rand::Rng;
    let s = secs.max(1) as f64;
    (s * rand::rng().random_range(0.8..1.2)).round().max(1.0) as u64
}

async fn sse_once(app: &App, url: &str) -> anyhow::Result<()> {
    let resp = app
        .http
        .get(url)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .send()
        .await?
        .error_for_status()?;
    app.status.send_modify(|s| s.signals.sse_connected = true);
    let mut body = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut data = String::new();
    // The endpoint is silent while a mount is offline; recycle the connection
    // periodically so half-open TCP connections can't leave us deaf.
    let idle = Duration::from_secs(300);
    loop {
        let chunk = match tokio::time::timeout(idle, body.next()).await {
            Err(_) => return Ok(()),
            Ok(None) => return Ok(()),
            Ok(Some(c)) => c?,
        };
        buf.extend_from_slice(&chunk);
        if buf.len() > 256 * 1024 {
            anyhow::bail!("sse line too long");
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !data.is_empty() {
                    handle_event(app, &data);
                    data.clear();
                }
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
    }
}

fn handle_event(app: &App, data: &str) {
    // Payload is a (multi-line) JSON object: {"msg": "online", "id": "<mount>"}.
    let msg = serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| data.to_string());
    app.status.send_modify(|s| s.signals.sse_last_event_ms = now_ms());
    if msg.to_ascii_lowercase().contains("online") {
        let _ = app.signal_tx.send(Signal::SseOnline);
    }
}

/// Poll Icecast status JSON. Feeds the selected mount's live state + live badges for all.
pub async fn run_poll(app: App) {
    let urls = crate::net::status_urls();
    let mut idx = 0usize;
    let mut fails = 0u32;
    let mut blocked_rounds = 0u32;
    let mut was_live: Option<bool> = None;
    let mut gen_rx = app.mount_gen.subscribe();
    loop {
        let url = &urls[idx % urls.len()];
        match fetch_status(&app, url).await {
            Ok(map) => {
                fails = 0;
                let mount = app.settings().mount.clone();
                let live = mount.as_deref().is_some_and(|m| is_live(&map, m));
                let count = map.len();
                *app.live.write().unwrap() = map;
                app.status.send_modify(|s| {
                    s.signals.poll_ok = true;
                    s.signals.poll_last_ms = now_ms();
                    s.signals.poll_live = live;
                    s.live_count = count;
                });
                // Send every time while live (cheap, and it re-triggers if a start attempt
                // raced the mount); offline only on edges.
                if live {
                    let _ = app.signal_tx.send(Signal::PollOnline);
                } else if was_live != Some(false) {
                    let _ = app.signal_tx.send(Signal::PollOffline);
                }
                was_live = Some(live);
            }
            Err(e) => {
                fails += 1;
                idx += 1;
                if fails == 3 {
                    app.log("warn", format!("Status check failing: {e}"));
                }
                if is_blocked(&e) {
                    blocked_rounds += 1;
                }
                app.status.send_modify(|s| s.signals.poll_ok = false);
            }
        }
        let secs = app.settings().poll_secs;
        // Failures: try the next source after the normal interval; if every source is
        // refusing us, back off exponentially (up to 10 min) rather than hammering.
        let wait = if fails == 0 {
            blocked_rounds = 0;
            secs
        } else if blocked_rounds as usize >= urls.len() {
            (secs << (blocked_rounds as usize / urls.len()).min(5)).min(600)
        } else {
            secs
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(jitter(wait))) => {}
            _ = gen_rx.changed() => { was_live = None; }
        }
    }
}

pub fn is_live(map: &HashMap<String, Vec<(String, LiveInfo)>>, mount: &str) -> bool {
    let base = mount.strip_suffix(".mp3").unwrap_or(mount);
    map.get(base).is_some_and(|v| !v.is_empty())
}

async fn fetch_status(app: &App, url: &str) -> anyhow::Result<HashMap<String, Vec<(String, LiveInfo)>>> {
    let bytes = app
        .http
        .get(url)
        .timeout(Duration::from_secs(6))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    parse_status(&bytes)
}

pub fn parse_status(bytes: &[u8]) -> anyhow::Result<HashMap<String, Vec<(String, LiveInfo)>>> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let stats = v.get("icestats").ok_or_else(|| anyhow::anyhow!("no icestats"))?;
    let sources = match stats.get("source") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(o @ serde_json::Value::Object(_)) => vec![o.clone()],
        _ => vec![],
    };
    let mut map: HashMap<String, Vec<(String, LiveInfo)>> = HashMap::new();
    for s in sources {
        let Some(listen) = s.get("listenurl").and_then(|x| x.as_str()) else { continue };
        let name = listen.rsplit('/').next().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let stream_start = s.get("stream_start").and_then(|x| x.as_str()).map(str::to_owned);
        if stream_start.is_none() {
            continue; // listed but no source connected
        }
        let base = name.strip_suffix(".mp3").unwrap_or(&name).to_string();
        let info = LiveInfo {
            server_type: s.get("server_type").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            stream_start,
            listeners: s.get("listeners").and_then(|x| x.as_u64()),
            title: s.get("title").and_then(|x| x.as_str()).map(str::to_owned),
        };
        map.entry(base).or_default().push((name, info));
    }
    Ok(map)
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
    #[test]
    fn parses_status() {
        let j = br#"{"icestats":{"source":[
          {"listenurl":"http://livemasjid.com:8000/makkah","server_type":"audio/ogg","stream_start":"06/Nov/2026:19:47:13 +0200"},
          {"listenurl":"http://livemasjid.com:8000/makkah.mp3","server_type":"audio/mpeg","stream_start":"x"},
          {"listenurl":"http://livemasjid.com:8000/idle","server_type":"audio/mpeg"}]}}"#;
        let m = super::parse_status(j).unwrap();
        assert_eq!(m["makkah"].len(), 2);
        assert!(!m.contains_key("idle"));
        assert!(super::is_live(&m, "makkah"));
        assert!(super::is_live(&m, "makkah.mp3"));
        let single = br#"{"icestats":{"source":{"listenurl":"http://h/a","stream_start":"x"}}}"#;
        assert!(super::is_live(&super::parse_status(single).unwrap(), "a"));
    }
}
