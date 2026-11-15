//! Past recordings per masjid, from livemasjid.com's RSS feed
//! (`/api/get_mountrecordings.php?mountname=<m>&feeditems=<n>`, the feed the site links as
//! "RSS Feed"). This is the only page of www.livemasjid.com the app reads besides the monthly
//! directory refresh, so it is fetched sparingly:
//!
//! * only when someone opens a masjid's recordings, never from the Now page or a timer;
//! * a good list is kept for [`FRESH_MS`] in memory **and on disk** (`recordings.json`), so
//!   restarts don't refetch;
//! * never while the site is refusing this network (shared [`crate::site::SiteGuard`]); then,
//!   and on any other failure, the last good list is shown (`stale`) instead of an error.

use super::{Fetched, fetch, valid_mount};
use crate::state::{App, now_ms};
use crate::sync::LockExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const FEED_ITEMS: usize = 40;
/// A good list is reused this long before the feed is asked again.
pub const FRESH_MS: u64 = 6 * 3_600_000;
/// "This masjid has no recordings" is rechecked once a day.
const NOT_FOUND_TTL: Duration = Duration::from_secs(24 * 3600);
/// After a failure (site unreachable, odd answer), wait this long before trying again.
const FAILED_TTL: Duration = Duration::from_secs(15 * 60);
/// Masjids whose list is kept on disk; the oldest are dropped beyond this.
const KEEP_MOUNTS: usize = 50;

#[derive(Clone, Debug, Serialize, PartialEq)]
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

/// A [`Recording`] as saved on disk (with its URL, which the API never shows).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct SavedItem {
    title: String,
    url: String,
    published: Option<String>,
    size_bytes: Option<u64>,
    duration_s: Option<u64>,
}

impl From<&Recording> for SavedItem {
    fn from(r: &Recording) -> Self {
        Self {
            title: r.title.clone(),
            url: r.url.clone(),
            published: r.published.clone(),
            size_bytes: r.size_bytes,
            duration_s: r.duration_s,
        }
    }
}

impl From<SavedItem> for Recording {
    fn from(s: SavedItem) -> Self {
        Self {
            id: short_id(&s.url),
            title: s.title,
            url: s.url,
            published: s.published,
            size_bytes: s.size_bytes,
            duration_s: s.duration_s,
        }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct RecordingList {
    /// "ok" | "empty" | "blocked" | "error"
    pub status: String,
    pub message: Option<String>,
    pub mount: String,
    pub items: Vec<Recording>,
    /// When this list was fetched from livemasjid.com.
    pub fetched_ms: Option<u64>,
    /// The site couldn't be asked just now, so this is the last good list.
    pub stale: bool,
}

/// One masjid's last good list.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub fetched_ms: u64,
    pub items: Vec<Recording>,
}

impl Saved {
    fn age_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.fetched_ms)
    }

    fn to_list(&self, mount: &str, stale: bool) -> RecordingList {
        RecordingList {
            status: "ok".into(),
            message: None,
            mount: mount.to_string(),
            items: self.items.clone(),
            fetched_ms: Some(self.fetched_ms),
            stale,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SavedFile {
    fetched_ms: u64,
    items: Vec<SavedItem>,
}

/// Last good list per masjid, kept on disk so restarts and refusals never empty the page.
#[derive(Default)]
pub struct Store {
    path: Option<PathBuf>,
    map: Mutex<HashMap<String, Saved>>,
}

impl Store {
    /// Restore from `path` (a missing or unreadable file starts empty) and save there.
    pub fn load(path: PathBuf) -> Self {
        let file: HashMap<String, SavedFile> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let map = file
            .into_iter()
            .map(|(k, f)| (k, Saved { fetched_ms: f.fetched_ms, items: f.items.into_iter().map(Recording::from).collect() }))
            .collect();
        Self { path: Some(path), map: Mutex::new(map) }
    }

    pub fn get(&self, mount: &str) -> Option<Saved> {
        self.map.locked().get(mount).cloned()
    }

    pub fn put(&self, mount: &str, saved: Saved) {
        let mut map = self.map.locked();
        map.insert(mount.to_string(), saved);
        if map.len() > KEEP_MOUNTS {
            let mut by_age: Vec<(u64, String)> = map.iter().map(|(k, v)| (v.fetched_ms, k.clone())).collect();
            by_age.sort_unstable();
            let extra = map.len() - KEEP_MOUNTS;
            for (_, k) in by_age.into_iter().take(extra) {
                map.remove(&k);
            }
        }
        let Some(path) = &self.path else { return };
        let file: HashMap<&str, SavedFile> = map
            .iter()
            .map(|(k, v)| (k.as_str(), SavedFile { fetched_ms: v.fetched_ms, items: v.items.iter().map(SavedItem::from).collect() }))
            .collect();
        if let Ok(bytes) = serde_json::to_vec(&file)
            && let Err(e) = crate::config::write_atomic(path, &bytes)
        {
            tracing::warn!("saving {}: {e}", path.display());
        }
    }
}

/// Feed URL. `LIVEMASJID_RECORDINGS_FEED` (with `{mount}` and `{n}`) overrides it for testing.
fn feed_url(mount: &str) -> String {
    let template = std::env::var("LIVEMASJID_RECORDINGS_FEED")
        .unwrap_or_else(|_| "https://www.livemasjid.com/api/get_mountrecordings.php?mountname={mount}&feeditems={n}".into());
    template.replace("{mount}", mount).replace("{n}", &FEED_ITEMS.to_string())
}

/// What to show after asking the feed, and how long to keep it in memory.
/// Returns the list to save when the feed gave one. Pure, so every case is unit-tested.
pub(crate) fn outcome(mount: &str, fetched: Fetched, saved: Option<&Saved>, now: u64) -> (RecordingList, Duration, Option<Saved>) {
    let base = RecordingList { mount: mount.to_string(), ..Default::default() };
    // When the feed can't be asked, the last good list beats an error.
    let fallback = |status: &str, message: &str, ttl: Duration| match saved {
        Some(s) => (s.to_list(mount, true), ttl, None),
        None => (RecordingList { status: status.into(), message: Some(message.into()), ..base.clone() }, ttl, None),
    };
    match fetched {
        Fetched::Ok(xml) => {
            let items = parse(&xml);
            if items.is_empty() {
                let list =
                    RecordingList { status: "empty".into(), message: Some("No recordings yet".into()), fetched_ms: Some(now), ..base };
                (list, Duration::from_millis(FRESH_MS), None)
            } else {
                let save = Saved { fetched_ms: now, items };
                (save.to_list(mount, false), Duration::from_millis(FRESH_MS), Some(save))
            }
        }
        Fetched::NotFound => {
            let list = RecordingList { status: "empty".into(), message: Some("No recordings for this masjid".into()), ..base };
            (list, NOT_FOUND_TTL, None)
        }
        // Stay away until the site's back-off ends.
        Fetched::Blocked { until_ms } => fallback(
            "blocked",
            "Recordings can't be loaded from livemasjid.com right now. Live broadcasts are not affected.",
            Duration::from_millis(until_ms.saturating_sub(now)).max(FAILED_TTL),
        ),
        Fetched::Failed(e) => {
            tracing::debug!("recordings for {mount}: {e}");
            fallback("error", "Couldn't reach livemasjid.com. Try again later.", FAILED_TTL)
        }
    }
}

pub async fn list(app: &App, mount: &str) -> RecordingList {
    list_from(app, mount, feed_url).await
}

/// [`list`] with the feed at `url(mount)` (the real feed, or a local server in tests).
async fn list_from(app: &App, mount: &str, url: impl FnOnce(&str) -> String) -> RecordingList {
    if !valid_mount(mount) {
        return RecordingList {
            status: "error".into(),
            message: Some("invalid masjid".into()),
            mount: mount.to_string(),
            ..Default::default()
        };
    }
    let it = &app.integrations;
    if let Some(hit) = it.recordings.get(mount) {
        return hit;
    }
    let _one = it.livemasjid_gate.lock().await;
    if let Some(hit) = it.recordings.get(mount) {
        return hit;
    }
    // A list saved by an earlier run that is still fresh: no request at all.
    let now = now_ms();
    let saved = it.recordings_saved.get(mount);
    if let Some(s) = saved.as_ref().filter(|s| s.age_ms(now) < FRESH_MS) {
        let list = s.to_list(mount, false);
        it.recordings.put(mount, list.clone(), Duration::from_millis(FRESH_MS - s.age_ms(now)));
        return list;
    }
    let fetched = fetch(&url(mount), Duration::from_secs(15), &app.site).await;
    let (result, ttl, save) = outcome(mount, fetched, saved.as_ref(), now);
    if let Some(save) = save {
        it.recordings_saved.put(mount, save);
    }
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
            let title = tag(item, "title")
                .map(unescape)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| url.rsplit('/').next().unwrap_or("Recording").to_string());
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

    #[test]
    fn supports_single_quotes_and_skips_bad_items() {
        let xml = r#"<item><title>T</title><enclosure url='https://x/a.mp3' length='0'/></item>
          <item><link>ftp://x/b.mp3</link></item>
          <item><title>No url</title></item>"#;
        let r = super::parse(xml);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://x/a.mp3");
        assert_eq!(r[0].size_bytes, None, "zero length means unknown");
        assert!(super::parse("<item></item>").is_empty());
    }

    #[test]
    fn durations() {
        assert_eq!(super::duration("32:15"), Some(32 * 60 + 15));
        assert_eq!(super::duration("1:02:03"), Some(3723));
        assert_eq!(super::duration(" 123 "), Some(123));
        assert_eq!(super::duration("12:xx"), None);
        assert_eq!(super::duration(""), None);
    }

    #[test]
    fn unescapes_cdata_and_entities() {
        assert_eq!(super::unescape("<![CDATA[ Fajr Talk ]]>"), " Fajr Talk ");
        assert_eq!(super::unescape(" A &amp; B &lt;x&gt; &quot;q&quot; &#39;s&#39; &apos;t&apos; "), "A & B <x> \"q\" 's' 't'");
    }

    #[test]
    fn ids_are_stable_and_url_dependent() {
        assert_eq!(super::short_id("https://x/a.mp3"), super::short_id("https://x/a.mp3"));
        assert_ne!(super::short_id("https://x/a.mp3"), super::short_id("https://x/b.mp3"));
        assert_eq!(super::short_id("https://x/a.mp3").len(), 16);
    }

    use super::{FAILED_TTL, FRESH_MS, KEEP_MOUNTS, NOT_FOUND_TTL, Saved, Store, outcome};
    use crate::integrations::Fetched;
    use std::time::Duration;

    const NOW: u64 = 1_800_000_000_000;
    const FEED: &str = r#"<item><title>Fajr</title><enclosure url="https://cdn.x/1.mp3"/></item>"#;

    fn saved(at: u64) -> Saved {
        Saved { fetched_ms: at, items: super::parse(FEED) }
    }

    #[test]
    fn a_good_feed_is_kept_for_six_hours_and_saved() {
        let (list, ttl, save) = outcome("m", Fetched::Ok(FEED.into()), None, NOW);
        assert_eq!((list.status.as_str(), list.items.len(), list.stale), ("ok", 1, false));
        assert_eq!(list.fetched_ms, Some(NOW));
        assert_eq!(ttl, Duration::from_millis(FRESH_MS));
        assert_eq!(save, Some(saved(NOW)));
    }

    #[test]
    fn an_empty_feed_or_unknown_masjid_is_not_an_error() {
        let (list, ttl, save) = outcome("m", Fetched::Ok("<rss></rss>".into()), None, NOW);
        assert_eq!(list.status, "empty");
        assert_eq!(ttl, Duration::from_millis(FRESH_MS));
        assert!(save.is_none());
        let (list, ttl, _) = outcome("m", Fetched::NotFound, None, NOW);
        assert_eq!(list.status, "empty");
        assert_eq!(ttl, NOT_FOUND_TTL);
    }

    #[test]
    fn a_refusal_shows_the_last_good_list_until_the_back_off_ends() {
        let old = saved(NOW - 2 * FRESH_MS);
        let until = NOW + 6 * 3_600_000;
        let (list, ttl, save) = outcome("m", Fetched::Blocked { until_ms: until }, Some(&old), NOW);
        assert_eq!((list.status.as_str(), list.stale, list.items.len()), ("ok", true, 1));
        assert_eq!(list.fetched_ms, Some(old.fetched_ms));
        assert_eq!(ttl, Duration::from_millis(until - NOW));
        assert!(save.is_none(), "a stale list is not saved again");
    }

    #[test]
    fn a_refusal_without_a_saved_list_explains_itself() {
        let (list, ttl, _) = outcome("m", Fetched::Blocked { until_ms: NOW + 1 }, None, NOW);
        assert_eq!(list.status, "blocked");
        assert!(list.message.unwrap().contains("Live broadcasts are not affected"));
        assert_eq!(ttl, FAILED_TTL, "never retried sooner than a failure");
    }

    #[test]
    fn a_failure_falls_back_too() {
        let (list, ttl, _) = outcome("m", Fetched::Failed("timeout".into()), Some(&saved(NOW - FRESH_MS)), NOW);
        assert!(list.stale);
        assert_eq!(ttl, FAILED_TTL);
        let (list, _, _) = outcome("m", Fetched::Failed("timeout".into()), None, NOW);
        assert_eq!(list.status, "error");
    }

    #[test]
    fn the_store_survives_a_restart_without_exposing_urls() {
        let path = std::env::temp_dir().join(format!("lm-rec-{}.json", crate::config::random_hex(6)));
        Store::load(path.clone()).put("lakewood", saved(NOW));
        let back = Store::load(path.clone()).get("lakewood").unwrap();
        assert_eq!(back, saved(NOW), "ids are rebuilt from the saved URLs");
        let api = serde_json::to_string(&back.to_list("lakewood", false)).unwrap();
        assert!(!api.contains("cdn.x"), "the API never shows feed URLs");
        assert!(Store::load(path.clone()).get("other").is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_store_keeps_only_the_newest_masjids() {
        let s = Store::default();
        for i in 0..=KEEP_MOUNTS as u64 {
            s.put(&format!("m{i}"), saved(NOW + i));
        }
        assert!(s.get("m0").is_none(), "the oldest is dropped");
        assert!(s.get(&format!("m{KEEP_MOUNTS}")).is_some());
    }

    #[test]
    fn an_unreadable_store_starts_empty() {
        let path = std::env::temp_dir().join(format!("lm-rec-bad-{}.json", crate::config::random_hex(6)));
        std::fs::write(&path, b"{not json").unwrap();
        assert!(Store::load(path.clone()).get("x").is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_feed_url_names_the_mount() {
        if std::env::var_os("LIVEMASJID_RECORDINGS_FEED").is_none() {
            assert_eq!(
                super::feed_url("lakewood"),
                "https://www.livemasjid.com/api/get_mountrecordings.php?mountname=lakewood&feeditems=40"
            );
        }
    }

    #[test]
    fn self_closing_tags_have_no_text() {
        assert_eq!(super::tag("<title/>", "title"), None);
        assert_eq!(super::tag("<title>x</title>", "title"), Some("x"));
    }

    #[test]
    fn a_store_that_cannot_be_written_still_works_in_memory() {
        let path = std::env::temp_dir().join(format!("lm-rec-missing-{}", crate::config::random_hex(6))).join("r.json");
        let s = Store::load(path.clone());
        s.put("m", saved(NOW));
        assert_eq!(s.get("m"), Some(saved(NOW)));
        assert!(!path.exists());
    }

    fn app() -> (std::path::PathBuf, crate::state::App) {
        let dir = std::env::temp_dir().join(format!("lm-recl-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    #[tokio::test]
    async fn a_fresh_saved_list_needs_no_request() {
        let (dir, app) = app();
        let now = crate::state::now_ms();
        app.integrations.recordings_saved.put("lakewood", saved(now - 1000));
        let id = super::parse(FEED)[0].id.clone();
        let r = super::find(&app, "lakewood", &id).await.expect("listed");
        assert_eq!(r.url, "https://cdn.x/1.mp3");
        assert!(super::find(&app, "lakewood", "nope").await.is_none());
        let bad = super::list(&app, "a/b").await;
        assert_eq!(bad.status, "error");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn lists_are_fetched_cached_and_saved() {
        use std::sync::atomic::Ordering;
        let (dir, app) = app();
        let (base, hits) = crate::integrations::test_http::serve(|path| match path {
            "/feed/good" => (200, FEED.to_string()),
            "/feed/none" => (404, String::new()),
            _ => (500, String::new()),
        })
        .await;
        let url = |m: &str| format!("{base}/feed/{m}");

        let l = super::list_from(&app, "good", url).await;
        assert_eq!((l.status.as_str(), l.items.len(), l.stale), ("ok", 1, false));
        assert!(app.integrations.recordings_saved.get("good").is_some(), "saved for restarts");
        let n = hits.load(Ordering::SeqCst);
        assert_eq!(super::list_from(&app, "good", url).await.items.len(), 1);
        assert_eq!(hits.load(Ordering::SeqCst), n, "served from memory");

        assert_eq!(super::list_from(&app, "none", url).await.status, "empty");
        assert!(app.integrations.recordings_saved.get("none").is_none());

        // An old saved list is refreshed; when that fails, it's shown as stale.
        app.integrations.recordings_saved.put("broken", saved(crate::state::now_ms() - 2 * FRESH_MS));
        let l = super::list_from(&app, "broken", url).await;
        assert!(l.stale && l.status == "ok");
        let _ = std::fs::remove_dir_all(dir);
    }
}
