//! The reciter catalogue from mp3quran.net (about 300 recitations), cached on disk and refreshed
//! monthly. Each recitation (a reciter's "moshaf") is keyed by its server path.

use crate::sync::LockExt;
use super::{DEFAULT_RECITER, valid, valid_key};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

const CATALOG_URL: &str = "https://www.mp3quran.net/api/v3/reciters?language=eng";
/// Refresh the catalogue when it is older than this.
const MAX_AGE: Duration = Duration::from_secs(30 * 86400);
/// Shown first in the picker: the best-known reciters.
const POPULAR: [&str; 12] = [
    "abdulrahman-sudais-r1", "mishary-alafasy-r1", "saad-ghamdi-r1", "maher-muaiqly-r1", "abdulbasit-abdulsamad-r3",
    "mahmoud-husary-r1", "muhammad-minshawi-r1", "saud-shuraim-r1", "yasser-dosari-r1", "ahmad-ajmi-r1",
    "abubakr-shatri-r1", "nasser-qatami-r1",
];

/// One recitation (a reciter's moshaf) on mp3quran.net.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reciter {
    pub key: String,
    pub name: String,
    /// "Murattal", "Mujawwad", "Warsh, Murattal" … (empty when the reciter has only one).
    pub style: String,
    /// Base URL; surah files are `<server>NNN.mp3`.
    pub server: String,
    /// Surahs this recitation has.
    pub surahs: Vec<u16>,
    #[serde(default)]
    pub popular: bool,
}

impl Reciter {
    pub fn label(&self) -> String {
        if self.style.is_empty() { self.name.clone() } else { format!("{} ({})", self.name, self.style) }
    }

    pub fn has(&self, n: u16) -> bool {
        self.surahs.contains(&n)
    }

    /// Where surah `n` is downloaded from.
    pub fn url(&self, n: u16) -> String {
        format!("{}{n:03}.mp3", self.server)
    }
}

/// The default recitation, usable before the catalogue has been fetched.
pub(super) fn builtin_sudais() -> Reciter {
    Reciter {
        key: DEFAULT_RECITER.into(),
        name: "Abdulrahman Alsudaes".into(),
        style: String::new(),
        server: "https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/".into(),
        surahs: (1..=114).collect(),
        popular: true,
    }
}

/// Key from a server URL: "https://cdn.mp3quran.net/audio/maher-muaiqly/r1/" → "maher-muaiqly-r1".
fn key_of(server: &str) -> Option<String> {
    let rest = server.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if host != "mp3quran.net" && !host.ends_with(".mp3quran.net") {
        return None; // only ever download from mp3quran's servers
    }
    let path = path.strip_prefix("audio/").unwrap_or(path);
    let key: String = path
        .trim_matches('/')
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    valid_key(&key).then_some(key)
}

/// Short style label from a moshaf name ("Rewayat Hafs A'n Assem - Murattal" → "Murattal").
fn style_of(moshaf: &str) -> String {
    let (riwayah, kind) = moshaf.split_once(" - ").unwrap_or(("", moshaf));
    let kind = kind.replace("Almusshaf Al Mojawwad", "Mujawwad").replace("Almusshaf Al Mo'lim", "Teaching");
    let riwayah = riwayah.trim_start_matches("Rewayat ").split(" A'n ").next().unwrap_or("").trim().to_string();
    if riwayah.is_empty() || riwayah == "Hafs" { kind.trim().to_string() } else { format!("{riwayah}, {}", kind.trim()) }
}

/// Parse the mp3quran catalogue into recitations (popular first, then by name).
pub fn parse_catalog(body: &str) -> Option<Vec<Reciter>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut out = Vec::new();
    for r in v.get("reciters")?.as_array()? {
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        let moshafs = r.get("moshaf").and_then(|m| m.as_array()).cloned().unwrap_or_default();
        let several = moshafs.len() > 1;
        for m in moshafs {
            let Some(server) = m.get("server").and_then(|s| s.as_str()) else { continue };
            let Some(key) = key_of(server) else { continue };
            let surahs: Vec<u16> = m
                .get("surah_list")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .filter(|&n| valid(n))
                .collect();
            if name.is_empty() || surahs.is_empty() {
                continue;
            }
            let style = if several { style_of(m.get("name").and_then(|s| s.as_str()).unwrap_or("")) } else { String::new() };
            let server = if server.ends_with('/') { server.to_string() } else { format!("{server}/") };
            out.push(Reciter { popular: POPULAR.contains(&key.as_str()), key, name: name.clone(), style, server, surahs });
        }
    }
    if out.is_empty() {
        return None;
    }
    out.sort_by(|a, b| {
        let rank = |r: &Reciter| POPULAR.iter().position(|p| *p == r.key).unwrap_or(usize::MAX);
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.style.cmp(&b.style))
    });
    out.dedup_by(|a, b| a.key == b.key);
    Some(out)
}

/// The cached catalogue.
pub(super) struct Catalog {
    path: PathBuf,
    list: Mutex<Vec<Reciter>>,
}

impl Catalog {
    pub fn load(path: PathBuf) -> Self {
        let list: Vec<Reciter> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, list: Mutex::new(list) }
    }

    /// All recitations (the built-in default if the catalogue hasn't been fetched yet).
    pub fn all(&self) -> Vec<Reciter> {
        let c = self.list.locked();
        if c.is_empty() { vec![builtin_sudais()] } else { c.clone() }
    }

    pub fn get(&self, key: &str) -> Option<Reciter> {
        let found = self.list.locked().iter().find(|r| r.key == key).cloned();
        found.or_else(|| (key == DEFAULT_RECITER).then(builtin_sudais))
    }

    /// A usable recitation for `key` (falls back to the default).
    pub fn resolve(&self, key: Option<&str>) -> Reciter {
        key.and_then(|k| self.get(k)).unwrap_or_else(|| self.get(DEFAULT_RECITER).unwrap_or_else(builtin_sudais))
    }

    /// Fetch the catalogue when missing or too old. Runs for the life of the app.
    pub async fn keep_fresh(&self) {
        loop {
            let wait = self.refresh_if_due(CATALOG_URL, "=https").await;
            tokio::time::sleep(wait).await;
        }
    }

    /// One check of [`Self::keep_fresh`] against `url` (only over the protocols in `proto`).
    /// Returns how long to wait before the next.
    async fn refresh_if_due(&self, url: &str, proto: &str) -> Duration {
        let age = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        let due = self.list.locked().is_empty() || age.is_none_or(|a| a > MAX_AGE);
        if !due {
            return Duration::from_secs(86400);
        }
        let out = Command::new("curl")
            .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", proto, "--max-time", "40"])
            .args(["-A", crate::integrations::USER_AGENT, url])
            .kill_on_drop(true)
            .output()
            .await;
        match out.ok().filter(|o| o.status.success()).and_then(|o| parse_catalog(&String::from_utf8_lossy(&o.stdout))) {
            Some(list) => {
                tracing::info!("Quran: {} recitations available", list.len());
                if let Ok(body) = serde_json::to_vec(&list) {
                    let _ = crate::config::write_atomic(&self.path, &body);
                }
                *self.list.locked() = list;
                Duration::from_secs(86400)
            }
            None => {
                tracing::warn!("Quran: couldn't fetch the reciter list; retrying in an hour");
                Duration::from_secs(3600)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parsing() {
        let body = r#"{"reciters":[
          {"id":102,"name":"Maher Al Meaqli","moshaf":[
            {"id":133,"name":"Rewayat Hafs A'n Assem - Almusshaf Al Mojawwad","server":"https://cdn.mp3quran.net/audio/maher-muaiqly/r3/","surah_list":"1,2"},
            {"id":102,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://cdn.mp3quran.net/audio/maher-muaiqly/r1/","surah_list":"1,2"}]},
          {"id":54,"name":"Abdulrahman  Alsudaes","moshaf":[{"id":54,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/","surah_list":"1,2,3,114"}]},
          {"id":9,"name":"Elsewhere","moshaf":[{"id":9,"name":"x","server":"https://evil.example/audio/x/","surah_list":"1"}]},
          {"id":10,"name":"Two Riwayat","moshaf":[{"id":1,"name":"Rewayat Warsh A'n Nafi' - Murattal","server":"https://server15.mp3quran.net/warsh1/","surah_list":"1"},{"id":2,"name":"Rewayat Hafs A'n Assem - Murattal","server":"https://server15.mp3quran.net/hafs1","surah_list":"1"}]}
        ]}"#;
        let list = parse_catalog(body).unwrap();
        assert_eq!(list[0].key, "abdulrahman-sudais-r1"); // popular first
        assert_eq!(list[0].name, "Abdulrahman Alsudaes"); // whitespace tidied
        assert!(list[0].has(114) && !list[0].has(5));
        assert!(!list.iter().any(|r| r.name == "Elsewhere")); // only mp3quran servers
        assert_eq!(list.iter().find(|r| r.key == "maher-muaiqly-r3").unwrap().style, "Mujawwad");
        assert_eq!(list.iter().find(|r| r.key == "warsh1").unwrap().style, "Warsh, Murattal");
        assert_eq!(list.iter().find(|r| r.key == "hafs1").unwrap().server, "https://server15.mp3quran.net/hafs1/");
        assert_eq!(list[0].url(2), "https://cdn.mp3quran.net/audio/abdulrahman-sudais/r1/002.mp3");
        assert!(valid_key("maher-muaiqly-r1") && !valid_key("../x") && !valid_key(""));
    }

    #[test]
    fn falls_back_to_the_builtin_default() {
        let c = Catalog::load(PathBuf::from("/nonexistent/reciters.json"));
        assert_eq!(c.all().len(), 1);
        assert_eq!(c.resolve(Some("unknown")).key, DEFAULT_RECITER);
        assert_eq!(c.resolve(None).key, DEFAULT_RECITER);
        assert!(c.get("unknown").is_none());
    }

    #[test]
    fn unusable_entries_are_skipped() {
        let body = r#"{"reciters":[
          {"name":"","moshaf":[{"server":"https://cdn.mp3quran.net/a/","surah_list":"1"}]},
          {"name":"No surahs","moshaf":[{"server":"https://cdn.mp3quran.net/b/","surah_list":"0,999"}]},
          {"name":"No server","moshaf":[{"surah_list":"1"}]},
          {"name":"No moshaf"}]}"#;
        assert!(parse_catalog(body).is_none(), "nothing usable");
        assert!(parse_catalog(r#"{"other":1}"#).is_none());
        assert!(parse_catalog("not json").is_none());
        assert_eq!(key_of("http://cdn.mp3quran.net/a/"), None, "https only");
        assert_eq!(key_of("https://cdn.mp3quran.net"), None, "no path");
        assert_eq!(style_of("Murattal"), "Murattal");
        assert_eq!(style_of("Rewayat Hafs A'n Assem - Almusshaf Al Mo'lim"), "Teaching");
        let r = builtin_sudais();
        assert_eq!(r.label(), "Abdulrahman Alsudaes");
        assert_eq!(Reciter { style: "Mujawwad".into(), ..r }.label(), "Abdulrahman Alsudaes (Mujawwad)");
    }

    const BODY: &str = r#"{"reciters":[{"name":"Maher","moshaf":[{"name":"x","server":"https://cdn.mp3quran.net/audio/maher-muaiqly/r1/","surah_list":"1,2"}]}]}"#;

    #[tokio::test]
    async fn refreshes_when_missing_and_waits_when_fresh_or_failing() {
        let dir = std::env::temp_dir().join(format!("lm-reciters-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, hits) = crate::integrations::test_http::serve(|path| match path {
            "/reciters" => (200, BODY.to_string()),
            _ => (500, String::new()),
        })
        .await;
        let c = Catalog::load(dir.join("reciters.json"));

        // Failure: retry in an hour, still the built-in default.
        assert_eq!(c.refresh_if_due(&format!("{base}/broken"), "=http").await, Duration::from_secs(3600));
        assert_eq!(c.all().len(), 1);

        assert_eq!(c.refresh_if_due(&format!("{base}/reciters"), "=http").await, Duration::from_secs(86400));
        assert_eq!(c.get("maher-muaiqly-r1").unwrap().name, "Maher");
        assert!(c.get(DEFAULT_RECITER).is_some(), "the default stays available");
        // Saved: a restart reads it back without asking.
        let again = Catalog::load(dir.join("reciters.json"));
        assert_eq!(again.all().len(), 1);
        let n = hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(again.refresh_if_due(&format!("{base}/reciters"), "=http").await, Duration::from_secs(86400));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), n, "fresh: no request");
        let _ = std::fs::remove_dir_all(dir);
    }
}
