//! Layer 1: Icecast status polling (origin, then relay), politely.
//! Also feeds the live badges for the whole directory in the UI.

use super::jitter;
use crate::state::{App, LiveInfo, Signal, now_ms};
use crate::sync::RwLockExt;
use std::collections::HashMap;
use std::time::Duration;

pub type LiveMap = HashMap<String, Vec<(String, LiveInfo)>>;

pub async fn run(app: App) {
    run_urls(app, crate::net::status_urls()).await;
}

/// Where the poll loop is between rounds.
#[derive(Default)]
struct PollState {
    idx: usize,
    fails: u32,
    was_live: Option<bool>,
}

async fn run_urls(app: App, urls: Vec<String>) {
    let mut st = PollState::default();
    let mut gen_rx = app.mount_gen.subscribe();
    loop {
        let wait = poll_once(&app, &urls, &mut st).await;
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(jitter(wait))) => {}
            _ = gen_rx.changed() => { st.was_live = None; }
        }
    }
}

/// One status check against the current source. Returns the seconds to wait before the next.
async fn poll_once(app: &App, urls: &[String], st: &mut PollState) -> u64 {
    let url = &urls[st.idx % urls.len()];
    match fetch(app, url).await {
        Ok(map) => {
            st.fails = 0;
            let live = app.settings().mount.as_deref().is_some_and(|m| is_live(&map, m));
            let count = map.len();
            *app.live.write_lock() = map;
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
            } else if st.was_live != Some(false) {
                let _ = app.signal_tx.send(Signal::PollOffline);
            }
            st.was_live = Some(live);
        }
        Err(e) => {
            st.fails += 1;
            st.idx += 1; // next source next time
            if st.fails == urls.len() as u32 {
                app.log("warn", format!("Status check failing on every server: {e}"));
            }
            app.status.send_modify(|s| s.signals.poll_ok = false);
        }
    }
    let secs = app.settings().poll_secs;
    // After a full round of failures, back off exponentially (max 10 min).
    let rounds = st.fails / urls.len() as u32;
    if rounds == 0 { secs } else { (secs << rounds.min(5)).min(600) }
}

pub fn is_live(map: &LiveMap, mount: &str) -> bool {
    let base = mount.strip_suffix(".mp3").unwrap_or(mount);
    map.get(base).is_some_and(|v| !v.is_empty())
}

async fn fetch(app: &App, url: &str) -> anyhow::Result<LiveMap> {
    let bytes = app.http.get(url).timeout(Duration::from_secs(8)).send().await?.error_for_status()?.bytes().await?;
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
            listeners: s.get("listeners").and_then(serde_json::Value::as_u64),
            title: str_field("title"),
        };
        map.entry(base).or_default().push((name, info));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use crate::sync::RwLockExt;
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

    #[test]
    fn ignores_mounts_without_a_stream() {
        let j = br#"{"icestats":{"source":[
          {"listenurl":"http://h/offline","server_type":"audio/mpeg"},
          {"listenurl":"http://h/","stream_start":"x"},
          {"server_type":"audio/mpeg","stream_start":"x"}]}}"#;
        let m = super::parse(j).unwrap();
        assert!(m.is_empty(), "no source with both a mount name and stream_start");
        assert!(!super::is_live(&m, "offline"));
        assert!(!super::is_live(&m, "missing"));
    }

    #[test]
    fn rejects_bad_status_json() {
        assert!(super::parse(b"not json").is_err());
        assert!(super::parse(br#"{"other":{}}"#).is_err(), "no icestats");
        assert!(super::parse(br#"{"icestats":{}}"#).unwrap().is_empty(), "no sources at all");
        assert!(super::parse(br#"{"icestats":{"source":[]}}"#).unwrap().is_empty());
    }

    #[test]
    fn mp3_suffix_shares_the_base_mount() {
        let j = br#"{"icestats":{"source":[
          {"listenurl":"http://h/m.mp3","stream_start":"x"},
          {"listenurl":"http://h/m","stream_start":"y"}]}}"#;
        let m = super::parse(j).unwrap();
        assert_eq!(m["m"].len(), 2);
        assert!(super::is_live(&m, "m.mp3") && super::is_live(&m, "m"));
    }

    /// One-shot HTTP server: replies with `status` and `body`.
    async fn serve(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/status-json.xsl")
    }

    #[tokio::test]
    async fn fetches_and_parses_a_status_response() {
        let dir = std::env::temp_dir().join(format!("lm-poll-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        let url = serve("200 OK", r#"{"icestats":{"source":{"listenurl":"http://h/makkah","stream_start":"x"}}}"#).await;
        let map = super::fetch(&app, &url).await.unwrap();
        assert!(super::is_live(&map, "makkah"));

        let bad = serve("503 Service Unavailable", "").await;
        assert!(super::fetch(&app, &bad).await.is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn each_round_updates_the_live_map_and_backs_off_on_failure() {
        use super::{PollState, poll_once};
        let dir = std::env::temp_dir().join(format!("lm-poll1-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        app.update_settings(|s| s.mount = Some("makkah".into())).unwrap();
        let secs = app.settings().poll_secs;
        let (base, _) = crate::integrations::test_http::serve(|path| match path {
            "/live" => (200, r#"{"icestats":{"source":{"listenurl":"http://h/makkah","stream_start":"x"}}}"#.into()),
            "/idle" => (200, r#"{"icestats":{}}"#.into()),
            _ => (500, String::new()),
        })
        .await;

        let mut st = PollState::default();
        assert_eq!(poll_once(&app, &[format!("{base}/live")], &mut st).await, secs);
        assert_eq!(st.was_live, Some(true));
        assert!(app.status.borrow().signals.poll_ok && app.status.borrow().signals.poll_live);
        assert_eq!(app.status.borrow().live_count, 1);
        assert!(super::is_live(&app.live.read_lock(), "makkah"));

        assert_eq!(poll_once(&app, &[format!("{base}/idle")], &mut st).await, secs);
        assert_eq!(st.was_live, Some(false));
        assert_eq!(poll_once(&app, &[format!("{base}/idle")], &mut st).await, secs);
        assert!(!app.status.borrow().signals.poll_live);

        // Two failing sources: the first failure tries the next one; a full round backs off.
        let urls = [format!("{base}/down"), format!("{base}/down2")];
        let mut st = PollState::default();
        assert_eq!(poll_once(&app, &urls, &mut st).await, secs);
        assert_eq!((st.fails, st.idx), (1, 1));
        assert_eq!(poll_once(&app, &urls, &mut st).await, (secs << 1).min(600));
        assert!(!app.status.borrow().signals.poll_ok);
        for _ in 0..12 {
            poll_once(&app, &urls, &mut st).await;
        }
        assert_eq!(poll_once(&app, &urls, &mut st).await, (secs << 5).min(600), "capped");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_loop_polls_until_stopped_and_restarts_on_a_new_mount() {
        let dir = std::env::temp_dir().join(format!("lm-poll2-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let (base, hits) = crate::integrations::test_http::serve(|_| (200, r#"{"icestats":{}}"#.into())).await;
        let task = tokio::spawn(super::run_urls(app.clone(), vec![format!("{base}/status-json.xsl")]));
        for _ in 0..50 {
            if hits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        app.mount_gen.send_modify(|g| *g += 1);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(hits.load(std::sync::atomic::Ordering::SeqCst) >= 2, "a new mount polls at once");
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }
}
