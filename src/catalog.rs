//! Masjid directory, scraped from livemasjid.com's index page (the site has no JSON
//! API for it) and cached on disk so the UI works offline and instantly after boot.

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
    if let Ok(bytes) = std::fs::read(&app.paths.catalog) {
        if let Ok(c) = serde_json::from_slice::<CacheFile>(&bytes) {
            if !c.stations.is_empty() {
                *app.catalog.write().unwrap() = c.stations;
                app.catalog_fetched_ms.store(c.fetched_ms, Ordering::Relaxed);
                return;
            }
        }
    }
    if let Ok(seed) = serde_json::from_str::<Vec<Station>>(SEED) {
        *app.catalog.write().unwrap() = seed;
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
    if let Some(until) = app.site.blocked_until() {
        let hours = until.saturating_sub(now_ms()).div_ceil(3_600_000);
        anyhow::bail!("livemasjid.com is refusing this network; not contacting it for another {hours} h");
    }
    let mut last_err = None;
    for base in crate::net::www_bases() {
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
                *app.catalog.write().unwrap() = stations;
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
        let age = now_ms().saturating_sub(app.catalog_fetched_ms.load(Ordering::Relaxed));
        if age < MAX_AGE_MS {
            delay = ((MAX_AGE_MS - age) / 1000).max(60);
            continue;
        }
        if let Some(until) = app.site.blocked_until() {
            delay = until.saturating_sub(now_ms()) / 1000 + 60;
            continue;
        }
        match refresh(&app).await {
            Ok(n) => {
                app.log("info", format!("Masjid directory updated ({n} streams)"));
                let _ = app.update_settings(|_| {}); // station location may now be known
                fail_delay = RETRY_FIRST_S;
                delay = MAX_AGE_MS / 1000;
            }
            Err(e) => {
                app.log("warn", format!("Masjid directory refresh failed: {e}"));
                delay = fail_delay;
                fail_delay = (fail_delay * 2).min(RETRY_MAX_S);
            }
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
        let name = between(&rest[end..], ">", "</a>").map(clean).unwrap_or_else(|| mount.clone());
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
}
