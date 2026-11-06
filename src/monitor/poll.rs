//! Layer 2: Icecast status polling (origin → relay → website), politely.
//! Also feeds the live badges for the whole directory in the UI.

use super::jitter;
use crate::site::{is_refusal, is_website};
use crate::state::{App, LiveInfo, Signal, now_ms};
use std::collections::HashMap;
use std::time::Duration;

pub type LiveMap = HashMap<String, Vec<(String, LiveInfo)>>;

pub async fn run(app: App) {
    let urls = crate::net::status_urls();
    let mut idx = 0usize;
    let mut fails = 0u32;
    let mut was_live: Option<bool> = None;
    let mut gen_rx = app.mount_gen.subscribe();
    loop {
        let url = &urls[idx % urls.len()];
        if is_website(url) && !app.site.allowed() {
            idx += 1; // skip the website while it's refusing us
            continue;
        }
        match fetch(&app, url).await {
            Ok(map) => {
                fails = 0;
                if is_website(url) {
                    app.site.ok();
                }
                let live = app.settings().mount.as_deref().is_some_and(|m| is_live(&map, m));
                let count = map.len();
                *app.live.write().unwrap() = map;
                app.status.send_modify(|s| {
                    s.signals.poll_ok = true;
                    s.signals.poll_last_ms = now_ms();
                    s.signals.poll_live = live;
                    s.live_count = count;
                });
                // Every time while live (re-triggers if a start attempt raced the mount);
                // offline only on edges.
                if live {
                    let _ = app.signal_tx.send(Signal::PollOnline);
                } else if was_live != Some(false) {
                    let _ = app.signal_tx.send(Signal::PollOffline);
                }
                was_live = Some(live);
            }
            Err(e) => {
                fails += 1;
                idx += 1; // next source next time
                if is_website(url) && is_refusal(&e) {
                    app.site.refused();
                }
                if fails == urls.len() as u32 {
                    app.log("warn", format!("Status check failing on every server: {e}"));
                }
                app.status.send_modify(|s| s.signals.poll_ok = false);
            }
        }
        let secs = app.settings().poll_secs;
        // After a full round of failures, back off exponentially (max 10 min).
        let rounds = fails / urls.len() as u32;
        let wait = if rounds == 0 { secs } else { (secs << rounds.min(5)).min(600) };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(jitter(wait))) => {}
            _ = gen_rx.changed() => { was_live = None; }
        }
    }
}

pub fn is_live(map: &LiveMap, mount: &str) -> bool {
    let base = mount.strip_suffix(".mp3").unwrap_or(mount);
    map.get(base).is_some_and(|v| !v.is_empty())
}

async fn fetch(app: &App, url: &str) -> anyhow::Result<LiveMap> {
    let bytes = app
        .http
        .get(url)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    parse(&bytes)
}

pub fn parse(bytes: &[u8]) -> anyhow::Result<LiveMap> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let stats = v.get("icestats").ok_or_else(|| anyhow::anyhow!("no icestats"))?;
    let sources = match stats.get("source") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        Some(o @ serde_json::Value::Object(_)) => vec![o.clone()],
        _ => vec![],
    };
    let mut map = LiveMap::new();
    for s in sources {
        let str_field = |k: &str| s.get(k).and_then(|x| x.as_str()).map(str::to_owned);
        let Some(listen) = str_field("listenurl") else { continue };
        let name = listen.rsplit('/').next().unwrap_or("").to_string();
        // Listed but no source connected → not live.
        let Some(stream_start) = str_field("stream_start") else { continue };
        if name.is_empty() {
            continue;
        }
        let base = name.strip_suffix(".mp3").unwrap_or(&name).to_string();
        let info = LiveInfo {
            server_type: str_field("server_type").unwrap_or_default(),
            stream_start: Some(stream_start),
            listeners: s.get("listeners").and_then(|x| x.as_u64()),
            title: str_field("title"),
        };
        map.entry(base).or_default().push((name, info));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_status() {
        let j = br#"{"icestats":{"source":[
          {"listenurl":"http://livemasjid.com:8000/makkah","server_type":"audio/ogg","stream_start":"06/Nov/2026:19:47:13 +0200"},
          {"listenurl":"http://livemasjid.com:8000/makkah.mp3","server_type":"audio/mpeg","stream_start":"x"},
          {"listenurl":"http://livemasjid.com:8000/idle","server_type":"audio/mpeg"}]}}"#;
        let m = super::parse(j).unwrap();
        assert_eq!(m["makkah"].len(), 2);
        assert!(!m.contains_key("idle"));
        assert!(super::is_live(&m, "makkah"));
        assert!(super::is_live(&m, "makkah.mp3"));
        let single = br#"{"icestats":{"source":{"listenurl":"http://h/a","stream_start":"x"}}}"#;
        assert!(super::is_live(&super::parse(single).unwrap(), "a"));
    }
}
