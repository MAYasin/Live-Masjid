//! Salah (prayer) times from masjidscreen.co.za, for masjids that publish them there.
//! API: `https://masjidscreen.co.za/<mount>/api/prayertimes.php` → JSON with
//! `masjid{name,timezone,…}`, `date`, `salaah_times{fajr,zuhr,asr,maghrib,esha,jummah…}{athaan,jamaat}`.
//! Unknown masjids return 404 `{"error":"Masjid not found"}`.

use super::{Fetched, fetch, valid_mount};
use crate::state::App;
use serde::Serialize;
use std::time::Duration;

const ORDER: [(&str, &str); 5] = [("fajr", "Fajr"), ("zuhr", "Zuhr"), ("asr", "Asr"), ("maghrib", "Maghrib"), ("esha", "Esha")];

#[derive(Clone, Debug, Serialize)]
pub struct Salah {
    pub key: String,
    pub label: String,
    /// "HH:MM"
    pub athaan: Option<String>,
    pub jamaat: Option<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SalahTimes {
    /// "ok" | "not_listed" | "blocked" | "error"
    pub status: String,
    pub message: Option<String>,
    pub masjid: Option<String>,
    pub timezone: Option<String>,
    /// "YYYY-MM-DD" the times apply to.
    pub date: Option<String>,
    pub times: Vec<Salah>,
    pub jummah: Option<Salah>,
    pub source: String,
}

pub async fn get(app: &App, mount: &str) -> SalahTimes {
    let source = format!("https://masjidscreen.co.za/{mount}/");
    if !valid_mount(mount) {
        return SalahTimes { status: "error".into(), message: Some("invalid masjid".into()), source, ..Default::default() };
    }
    let it = &app.integrations;
    if let Some(hit) = it.salah.get(mount) {
        if hit.status != "ok" || hit.date.as_deref() == Some(&today()) {
            return hit;
        }
    }
    let _one = it.masjidscreen_gate.lock().await;
    if let Some(hit) = it.salah.get(mount) {
        if hit.status != "ok" || hit.date.as_deref() == Some(&today()) {
            return hit; // fetched by a concurrent request while we waited
        }
    }
    let url = format!("https://masjidscreen.co.za/{mount}/api/prayertimes.php");
    let (result, ttl) = match fetch(&url, Duration::from_secs(10), &it.masjidscreen).await {
        Fetched::Ok(body) => match parse(&body) {
            Some(mut t) => {
                t.source = source.clone();
                (t, Duration::from_secs(6 * 3600)) // times change daily; re-check a few times a day
            }
            None => (failure("error", "Unexpected answer from masjidscreen.co.za", &source), Duration::from_secs(1800)),
        },
        Fetched::NotFound => (failure("not_listed", "This masjid doesn't publish salah times on masjidscreen.co.za", &source), Duration::from_secs(24 * 3600)),
        Fetched::Blocked { .. } => (failure("blocked", "masjidscreen.co.za is not answering requests from this network right now", &source), Duration::from_secs(3600)),
        Fetched::Failed(e) => {
            tracing::debug!("salah times for {mount}: {e}");
            (failure("error", "Couldn't reach masjidscreen.co.za", &source), Duration::from_secs(900))
        }
    };
    it.salah.put(mount, result.clone(), ttl);
    result
}

fn failure(status: &str, message: &str, source: &str) -> SalahTimes {
    SalahTimes { status: status.into(), message: Some(message.into()), source: source.into(), ..Default::default() }
}

/// Today's date in the device's local time zone, "YYYY-MM-DD".
fn today() -> String {
    // The Pi's clock and zone are what the user lives by; masjidscreen dates are local too.
    let out = std::process::Command::new("date").arg("+%F").output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn hhmm(v: Option<&serde_json::Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    (s.len() >= 5).then(|| s[..5].to_string())
}

pub fn parse(body: &str) -> Option<SalahTimes> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    // Full form nests times under "salaah_times"; the ?fields= form has them at top level.
    let times_obj = v.get("salaah_times").unwrap_or(&v);
    let entry = |key: &str, label: &str| -> Option<Salah> {
        let p = times_obj.get(key)?;
        let (athaan, jamaat) = (hhmm(p.get("athaan")), hhmm(p.get("jamaat")));
        (athaan.is_some() || jamaat.is_some()).then(|| Salah { key: key.into(), label: label.into(), athaan, jamaat })
    };
    let times: Vec<Salah> = ORDER.iter().filter_map(|(k, l)| entry(k, l)).collect();
    if times.is_empty() {
        return None;
    }
    let masjid = v.get("masjid");
    Some(SalahTimes {
        status: "ok".into(),
        message: None,
        masjid: masjid.and_then(|m| m.get("name")).and_then(|n| n.as_str()).map(str::to_owned),
        timezone: masjid.and_then(|m| m.get("timezone")).and_then(|n| n.as_str()).map(str::to_owned),
        date: v.get("date").and_then(|d| d.as_str()).map(str::to_owned),
        jummah: entry("jummah", "Jumu'ah"),
        times,
        source: String::new(),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_full_and_short_forms() {
        let full = r#"{"masjid":{"name":"Lakewood Estate Masjid","timezone":"Africa/Johannesburg"},"date":"2026-11-07",
          "salaah_times":{"fajr":{"athaan":"04:45:00","jamaat":"05:00:00"},"zuhr":{"athaan":"12:30:00","jamaat":"12:45:00"},
          "asr":{"athaan":"17:00:00","jamaat":"17:15:00"},"maghrib":{"athaan":"18:31:00","jamaat":"18:31:00"},
          "esha":{"athaan":"20:10:00","jamaat":"20:25:00"},"jummah":{"athaan":"12:25:00","jamaat":"12:40:00"}}}"#;
        let t = super::parse(full).unwrap();
        assert_eq!(t.times.len(), 5);
        assert_eq!(t.times[0].athaan.as_deref(), Some("04:45"));
        assert_eq!(t.jummah.unwrap().jamaat.as_deref(), Some("12:40"));
        assert_eq!(t.masjid.as_deref(), Some("Lakewood Estate Masjid"));
        let short = r#"{"masjid":"lakewood","date":"2026-11-07","fajr":{"athaan":"04:45:00","jamaat":"05:00:00"}}"#;
        assert_eq!(super::parse(short).unwrap().times.len(), 1);
        assert!(super::parse(r#"{"error":"Masjid not found"}"#).is_none());
    }
}
