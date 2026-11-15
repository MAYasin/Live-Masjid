//! Masjid directory, scraped from livemasjid.com's index page (the site has no JSON
//! API for it) and cached on disk so the UI works offline and instantly after boot.

use crate::sync::RwLockExt;
use crate::state::{App, now_ms};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Station {
    pub mount: String,
    pub name: String,
    pub location: String,
    /// "Masjid" | "Institution" | "Aalim"
    pub genre: String,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_ms: u64,
    stations: Vec<Station>,
}

/// Snapshot of the directory taken at build time, so a fresh install has a usable
/// list even if livemasjid.com can't be reached (or refuses us) on first boot.
const SEED: &str = include_str!("catalog_seed.json");

pub fn load_cache(app: &App) {
    if let Ok(bytes) = std::fs::read(&app.paths.catalog)
        && let Ok(c) = serde_json::from_slice::<CacheFile>(&bytes)
            && !c.stations.is_empty() {
                *app.catalog.write_lock() = c.stations;
                app.catalog_fetched_ms.store(c.fetched_ms, Ordering::Relaxed);
                return;
            }
    if let Ok(seed) = serde_json::from_str::<Vec<Station>>(SEED) {
        *app.catalog.write_lock() = seed;
        // fetched_ms stays 0, so the background task refreshes it as soon as it can.
    }
}

/// The directory changes rarely; one fetch a month is plenty (the page is ~480 KB).
/// New masjids can be pulled in any time with the Refresh button.
const MAX_AGE_MS: u64 = 30 * 24 * 3600 * 1000;
/// After a failed refresh: wait 6 h, doubling up to a week.
const RETRY_FIRST_S: u64 = 6 * 3600;
const RETRY_MAX_S: u64 = 7 * 24 * 3600;

/// Refresh now. Returns number of stations.
pub async fn refresh(app: &App) -> anyhow::Result<usize> {
    refresh_from(app, crate::net::www_bases()).await
}

/// [`refresh`] from the first of `bases` that answers (the website, or a local server in tests).
async fn refresh_from(app: &App, bases: &[&str]) -> anyhow::Result<usize> {
    if let Some(until) = app.site.blocked_until() {
        let hours = until.saturating_sub(now_ms()).div_ceil(3_600_000);
        anyhow::bail!("livemasjid.com is refusing this network; not contacting it for another {hours} h");
    }
    let mut last_err = None;
    for base in bases {
        let url = format!("{base}/index.php");
        match fetch_text(app, &url).await {
            Err(e) if crate::site::is_refusal(&e) => {
                app.site.refused();
                return Err(e);
            }
            Ok(html) => {
                app.site.ok();
                let stations = parse(&html);
                if stations.len() < 10 {
                    anyhow::bail!("catalog page parsed to only {} stations", stations.len());
                }
                let n = stations.len();
                let now = now_ms();
                let file = CacheFile { fetched_ms: now, stations: stations.clone() };
                crate::config::write_atomic(&app.paths.catalog, &serde_json::to_vec(&file)?)?;
                *app.catalog.write_lock() = stations;
                app.catalog_fetched_ms.store(now, Ordering::Relaxed);
                return Ok(n);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no base url")))
}

async fn fetch_text(app: &App, url: &str) -> anyhow::Result<String> {
    let resp = app
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.text().await?)
}

/// Background task: refresh monthly; on failure retry after 6 h, 12 h, … up to a week.
pub async fn run(app: App) {
    let mut delay = 30u64; // let the network settle after boot
    let mut fail_delay = RETRY_FIRST_S;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        delay = round(&app, crate::net::www_bases(), &mut fail_delay).await;
    }
}

/// One wake-up of [`run`]: refresh if due and allowed. Returns the seconds until the next.
async fn round(app: &App, bases: &[&str], fail_delay: &mut u64) -> u64 {
    let age = now_ms().saturating_sub(app.catalog_fetched_ms.load(Ordering::Relaxed));
    if age < MAX_AGE_MS {
        return ((MAX_AGE_MS - age) / 1000).max(60);
    }
    if let Some(until) = app.site.blocked_until() {
        return until.saturating_sub(now_ms()) / 1000 + 60;
    }
    match refresh_from(app, bases).await {
        Ok(n) => {
            app.log("info", format!("Masjid directory updated ({n} streams)"));
            let _ = app.update_settings(|_| {}); // station location may now be known
            *fail_delay = RETRY_FIRST_S;
            MAX_AGE_MS / 1000
        }
        Err(e) => {
            app.log("warn", format!("Masjid directory refresh failed: {e}"));
            let delay = *fail_delay;
            *fail_delay = (*fail_delay * 2).min(RETRY_MAX_S);
            delay
        }
    }
}

/// Parse the server-rendered card list. Each card looks like:
/// `<div class="bs-component"><p hidden class="location d-none">LOC</p> ...
///  <img class="genre" ... alt="Masjid"> ... <a class="text-muted masjidname" href="/MOUNT">NAME</a>`
pub fn parse(html: &str) -> Vec<Station> {
    let mut out: Vec<Station> = Vec::new();
    for chunk in html.split("class=\"bs-component\"").skip(1) {
        let location = between(chunk, "class=\"location d-none\">", "</p>").unwrap_or("");
        let genre = chunk
            .find("class=\"genre\"")
            .and_then(|i| between(&chunk[i..], "alt=\"", "\""))
            .unwrap_or("Masjid");
        let Some(i) = chunk.find("masjidname\" href=\"/") else { continue };
        let rest = &chunk[i + "masjidname\" href=\"/".len()..];
        let Some(end) = rest.find('"') else { continue };
        let mount = rest[..end].trim().to_string();
        let name = between(&rest[end..], ">", "</a>").map_or_else(|| mount.clone(), clean);
        if mount.is_empty() || mount.contains(['/', '?', ' ']) || out.iter().any(|s| s.mount == mount) {
            continue;
        }
        let genre = match genre.trim_end_matches('s') {
            "Institution" => "Institution",
            "Aalim" => "Aalim",
            _ => "Masjid",
        };
        out.push(Station {
            mount,
            name,
            location: clean(location),
            genre: genre.into(),
        });
    }
    out
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

fn clean(s: &str) -> String {
    let s = unescape(s);
    let s: String = strip_tags(&s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest[..rest.len().min(10)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match rep {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cards() {
        let html = r#"
  <div class="col-lg-6">
    <div class="bs-component">
      <p hidden class="location d-none">Mecca 24231, Saudi Arabia</p>
      <div class="card border-primary mb-6"><a class="stretched-link" href="/makkah"></a>
              <img class="genre" src="./images/MasjidLogo.png" alt="Masjid" width="40px">
              <p class="text-capitalize"><b><a class="text-muted masjidname" href="/makkah">  Masjid al-Haram, Makkah &amp; Co</a></b></p>
    </div></div>
    <div class="bs-component">
      <p hidden class="location d-none"></p>
              <img class="genre" src="./images/InstitutionLogo.png" alt="Institution" width="40px">
              <p><b><a class="text-muted masjidname" href="/activestream">   Quraan Recitation</a></b></p>
    </div>"#;
        let s = parse(html);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].mount, "makkah");
        assert_eq!(s[0].name, "Masjid al-Haram, Makkah & Co");
        assert_eq!(s[0].location, "Mecca 24231, Saudi Arabia");
        assert_eq!(s[0].genre, "Masjid");
        assert_eq!(s[1].genre, "Institution");
        assert_eq!(s[1].location, "");
    }

    #[test]
    fn seed_parses() {
        let s: Vec<Station> = serde_json::from_str(SEED).unwrap();
        assert!(s.len() > 400);
    }

    #[test]
    fn unescapes() {
        assert_eq!(unescape("a &#039;b&#x27; &amp; &bogus c"), "a 'b' & &bogus c");
    }

    #[test]
    fn skips_bad_and_duplicate_mounts() {
        let html = r#"
  <div class="bs-component"><p hidden class="location d-none">X</p>
    <a class="text-muted masjidname" href="/same">Same</a></div>
  <div class="bs-component"><a class="text-muted masjidname" href="/same">Same again</a></div>
  <div class="bs-component"><a class="text-muted masjidname" href="/bad/mount">Slash</a></div>
  <div class="bs-component"><a class="text-muted masjidname" href="/bad mount">Space</a></div>
  <div class="bs-component"><a class="text-muted masjidname" href="/bad?x=1">Query</a></div>
  <div class="bs-component"><a class="text-muted masjidname" href="/">Empty</a></div>"#;
        let s = parse(html);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].mount, "same");
    }

    #[test]
    fn genre_and_name_fallbacks() {
        let html = r#"
  <div class="bs-component"><img class="genre" alt="Institutions">
    <a class="masjidname" href="/inst">An Institution</a></div>
  <div class="bs-component"><img class="genre" alt="Aalims">
    <a class="masjidname" href="/aalim">A Scholar</a></div>
  <div class="bs-component"><img class="genre" alt="Something else">
    <a class="masjidname" href="/unknown">Unknown genre</a></div>
  <div class="bs-component"><a class="masjidname" href="/bare"></a></div>"#;
        let s = parse(html);
        assert_eq!(s[0].genre, "Institution");
        assert_eq!(s[1].genre, "Aalim");
        assert_eq!(s[2].genre, "Masjid");
        assert_eq!(s[3].name, "", "an empty link text stays empty");
    }

    #[test]
    fn cleans_markup_and_entities() {
        assert_eq!(clean("  <b>Al&nbsp;Noor</b>\n  Masjid  "), "Al Noor Masjid");
        assert_eq!(clean("&#x41;&#66;"), "AB");
        assert_eq!(strip_tags("<p>a</p>b"), "ab");
        assert_eq!(between("x[abc]y", "[", "]"), Some("abc"));
        assert_eq!(between("xabc", "[", "]"), None);
    }

    #[test]
    fn loads_a_saved_cache_and_falls_back_to_the_seed() {
        let dir = std::env::temp_dir().join(format!("lm-catalog-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);

        let file = serde_json::json!({
            "fetched_ms": 1234,
            "stations": [{ "mount": "saved", "name": "Saved Masjid", "location": "Here", "genre": "Masjid" }]
        });
        std::fs::write(&app.paths.catalog, serde_json::to_vec(&file).unwrap()).unwrap();
        load_cache(&app);
        assert_eq!(app.catalog.read_lock().len(), 1);
        assert_eq!(app.catalog.read_lock()[0].mount, "saved");
        assert_eq!(app.catalog_fetched_ms.load(Ordering::Relaxed), 1234);

        // A broken cache file must not leave the app without a directory.
        std::fs::write(&app.paths.catalog, b"not json").unwrap();
        app.catalog.write_lock().clear();
        app.catalog_fetched_ms.store(0, Ordering::Relaxed);
        load_cache(&app);
        assert!(app.catalog.read_lock().len() > 400, "the embedded seed is used");
        assert_eq!(app.catalog_fetched_ms.load(Ordering::Relaxed), 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refresh_refuses_while_the_website_is_blocked() {
        let dir = std::env::temp_dir().join(format!("lm-catalog-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        app.site.refused();
        let err = refresh(&app).await.unwrap_err().to_string();
        assert!(err.contains("refusing"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unescapes_named_entities_and_stray_ampersands() {
        assert_eq!(unescape("&lt;b&gt; &quot;x&quot; &apos;y&apos;"), "<b> \"x\" 'y'");
        assert_eq!(unescape("a &unknown; b"), "a &unknown; b");
        assert_eq!(unescape("fish & chips & more text here"), "fish & chips & more text here");
        assert_eq!(unescape("&#xZZ;"), "&#xZZ;");
    }

    /// A directory page with `n` cards.
    fn page(n: usize) -> String {
        (0..n).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            let _ = write!(s, r#"<div class="bs-component"><p hidden class="location d-none">Town {i}</p><a class="text-muted masjidname" href="/m{i}">Masjid {i}</a></div>"#);
            s
        })
    }

    #[tokio::test]
    async fn refresh_saves_a_good_page_and_rejects_a_bad_one() {
        let dir = std::env::temp_dir().join(format!("lm-catalog-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let (good, _) = crate::integrations::test_http::serve(|_| (200, page(12))).await;
        let (short, _) = crate::integrations::test_http::serve(|_| (200, page(3))).await;
        let (broken, _) = crate::integrations::test_http::serve(|_| (500, String::new())).await;

        // The first base fails; the next one answers.
        assert_eq!(refresh_from(&app, &[broken.as_str(), good.as_str()]).await.unwrap(), 12);
        assert_eq!(app.catalog.read_lock().len(), 12);
        assert!(app.catalog_fetched_ms.load(Ordering::Relaxed) > 0);
        app.catalog.write_lock().clear();
        load_cache(&app);
        assert_eq!(app.catalog.read_lock()[0].location, "Town 0", "saved to disk");

        let err = refresh_from(&app, &[short.as_str()]).await.unwrap_err().to_string();
        assert!(err.contains("only 3 stations"), "{err}");
        assert!(refresh_from(&app, &[broken.as_str()]).await.is_err());
        let err = refresh_from(&app, &[]).await.unwrap_err().to_string();
        assert!(err.contains("no base url"), "{err}");
        assert!(app.site.blocked_until().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_refusal_stops_at_once_and_pauses_the_site() {
        let dir = std::env::temp_dir().join(format!("lm-catalog-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let (refusing, _) = crate::integrations::test_http::serve(|_| (403, String::new())).await;
        let (good, hits) = crate::integrations::test_http::serve(|_| (200, page(12))).await;
        assert!(refresh_from(&app, &[refusing.as_str(), good.as_str()]).await.is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "no other base is tried");
        assert!(app.site.blocked_until().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_background_round_waits_refreshes_and_backs_off() {
        let dir = std::env::temp_dir().join(format!("lm-catalog-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let (good, _) = crate::integrations::test_http::serve(|_| (200, page(12))).await;
        let (broken, _) = crate::integrations::test_http::serve(|_| (500, String::new())).await;
        let mut fail = RETRY_FIRST_S;

        // Never fetched: due now. A failure waits 6 h, then 12 h, … capped at a week.
        assert_eq!(round(&app, &[broken.as_str()], &mut fail).await, RETRY_FIRST_S);
        assert_eq!(round(&app, &[broken.as_str()], &mut fail).await, 2 * RETRY_FIRST_S);
        for _ in 0..10 {
            round(&app, &[broken.as_str()], &mut fail).await;
        }
        assert_eq!(fail, RETRY_MAX_S);
        // Success: next month, and the back-off resets.
        assert_eq!(round(&app, &[good.as_str()], &mut fail).await, MAX_AGE_MS / 1000);
        assert_eq!(fail, RETRY_FIRST_S);
        // Fresh: sleep until it's a month old.
        let wait = round(&app, &[good.as_str()], &mut fail).await;
        assert!(wait > MAX_AGE_MS / 1000 - 60 && wait <= MAX_AGE_MS / 1000, "{wait}");
        // Due but the site is refusing us: wait out the back-off.
        app.catalog_fetched_ms.store(0, Ordering::Relaxed);
        app.site.refused();
        let wait = round(&app, &[good.as_str()], &mut fail).await;
        assert!(wait > 5 * 3600 && wait <= 6 * 3600 + 60, "{wait}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
