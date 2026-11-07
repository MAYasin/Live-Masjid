//! Past recordings per masjid, from livemasjid.com's RSS feed
//! (`/api/get_mountrecordings.php?mountname=<m>&feeditems=<n>`, the feed the site links as
//! "RSS Feed"). Only the Pi fetches it, cached for 15 minutes, and never while the site is
//! refusing this network (shared [`crate::site::SiteGuard`]).

use super::{Fetched, fetch, valid_mount};
use crate::state::App;
use serde::Serialize;
use std::time::Duration;

const FEED_ITEMS: usize = 40;

#[derive(Clone, Debug, Serialize)]
pub struct Recording {
    /// Stable id within the list (used to ask the Pi to play it — never a raw URL).
    pub id: String,
    pub title: String,
    #[serde(skip_serializing)]
    pub url: String,
    pub published: Option<String>,
    pub size_bytes: Option<u64>,
    pub duration_s: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct RecordingList {
    /// "ok" | "empty" | "blocked" | "error"
    pub status: String,
    pub message: Option<String>,
    pub mount: String,
    pub items: Vec<Recording>,
}

/// Feed URL. `LIVEMASJID_RECORDINGS_FEED` (with `{mount}` and `{n}`) overrides it for testing.
fn feed_url(mount: &str) -> String {
    let template = std::env::var("LIVEMASJID_RECORDINGS_FEED")
        .unwrap_or_else(|_| "https://www.livemasjid.com/api/get_mountrecordings.php?mountname={mount}&feeditems={n}".into());
    template.replace("{mount}", mount).replace("{n}", &FEED_ITEMS.to_string())
}

pub async fn list(app: &App, mount: &str) -> RecordingList {
    let base = RecordingList { mount: mount.to_string(), ..Default::default() };
    if !valid_mount(mount) {
        return RecordingList { status: "error".into(), message: Some("invalid masjid".into()), ..base };
    }
    let it = &app.integrations;
    if let Some(hit) = it.recordings.get(mount) {
        return hit;
    }
    let _one = it.livemasjid_gate.lock().await;
    if let Some(hit) = it.recordings.get(mount) {
        return hit;
    }
    let (result, ttl) = match fetch(&feed_url(mount), Duration::from_secs(15), &app.site).await {
        Fetched::Ok(xml) => {
            let items = parse(&xml);
            if items.is_empty() {
                (RecordingList { status: "empty".into(), message: Some("No recordings yet".into()), ..base }, Duration::from_secs(900))
            } else {
                (RecordingList { status: "ok".into(), items, ..base }, Duration::from_secs(900))
            }
        }
        Fetched::NotFound => (RecordingList { status: "empty".into(), message: Some("No recordings for this masjid".into()), ..base }, Duration::from_secs(6 * 3600)),
        Fetched::Blocked { until_ms } => {
            let msg = format!(
                "livemasjid.com is refusing requests from this network, so recordings can't be loaded right now (retrying after {}).",
                crate::state::fmt_local_time(until_ms)
            );
            (RecordingList { status: "blocked".into(), message: Some(msg), ..base }, Duration::from_secs(600))
        }
        Fetched::Failed(e) => {
            tracing::debug!("recordings for {mount}: {e}");
            (RecordingList { status: "error".into(), message: Some("Couldn't reach livemasjid.com".into()), ..base }, Duration::from_secs(300))
        }
    };
    it.recordings.put(mount, result.clone(), ttl);
    result
}

/// Find a recording previously listed for `mount` (only listed items can be played).
pub async fn find(app: &App, mount: &str, id: &str) -> Option<Recording> {
    list(app, mount).await.items.into_iter().find(|r| r.id == id)
}

// ------------------------------------------------------------------------------- RSS parsing

fn unescape(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(s);
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'")
}

fn tag<'a>(item: &'a str, name: &str) -> Option<&'a str> {
    let open = item.find(&format!("<{name}"))?;
    let start = open + item[open..].find('>')? + 1;
    if item[open..start].ends_with("/>") {
        return None;
    }
    let end = start + item[start..].find(&format!("</{name}>"))?;
    Some(&item[start..end])
}

fn attr<'a>(item: &'a str, tag_name: &str, attr_name: &str) -> Option<&'a str> {
    let open = item.find(&format!("<{tag_name}"))?;
    let close = open + item[open..].find('>')?;
    let el = &item[open..close];
    let key = format!("{attr_name}=");
    let at = el.find(&key)? + key.len();
    let quote = el[at..].chars().next()?;
    let rest = &el[at + 1..];
    Some(&rest[..rest.find(quote)?])
}

/// "1:02:03", "02:03" or "123" seconds.
fn duration(s: &str) -> Option<u64> {
    s.trim().split(':').try_fold(0u64, |acc, part| Some(acc * 60 + part.trim().parse::<u64>().ok()?))
}

fn short_id(url: &str) -> String {
    // FNV-1a: stable, short, no extra dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn parse(xml: &str) -> Vec<Recording> {
    xml.split("<item")
        .skip(1)
        .filter_map(|chunk| {
            let item = &chunk[..chunk.find("</item>").unwrap_or(chunk.len())];
            let url = attr(item, "enclosure", "url").map(unescape).or_else(|| tag(item, "link").map(unescape))?;
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return None;
            }
            let title = tag(item, "title").map(unescape).filter(|t| !t.is_empty()).unwrap_or_else(|| url.rsplit('/').next().unwrap_or("Recording").to_string());
            Some(Recording {
                id: short_id(&url),
                title,
                published: tag(item, "pubDate").map(unescape),
                size_bytes: attr(item, "enclosure", "length").and_then(|v| v.parse().ok()).filter(|n| *n > 0),
                duration_s: tag(item, "itunes:duration").and_then(|d| duration(&unescape(d))),
                url,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_rss_items() {
        let xml = r#"<?xml version="1.0"?><rss><channel><title>lakewood</title>
          <item><title><![CDATA[Lakewood - Fajr Talk]]></title><link>https://example.org/r/1.mp3</link>
            <pubDate>Sat, 07 Nov 2026 05:10:00 +0200</pubDate>
            <enclosure url="https://example.org/r/1.mp3?a=1&amp;b=2" length="7340032" type="audio/mpeg"/>
            <itunes:duration>32:15</itunes:duration></item>
          <item><title>No audio</title><link>not-a-url</link></item>
          <item><link>https://example.org/r/2.mp3</link></item>
        </channel></rss>"#;
        let r = super::parse(xml);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Lakewood - Fajr Talk");
        assert_eq!(r[0].url, "https://example.org/r/1.mp3?a=1&b=2");
        assert_eq!(r[0].size_bytes, Some(7_340_032));
        assert_eq!(r[0].duration_s, Some(32 * 60 + 15));
        assert_eq!(r[1].title, "2.mp3");
        assert_ne!(r[0].id, r[1].id);
    }
}
