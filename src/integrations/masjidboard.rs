//! Salah times from masjidboardlive.com (the most detailed and accurate source we found).
//!
//! API: `https://masjidboardlive.com/boards/api/board.php?id=<board>` → `{"ok":true,"data":{…}}`
//! with every time of the day, Jumu'ah, upcoming time changes, the Hijri adjustment and a
//! `meta` block (names, city, `web_url` = canonical board id). Unknown ids give
//! `{"ok":false,"error":"not_found"}`.
//!
//! Board ids differ from livemasjid mounts ("lakewood" → "lakewood-kauthar"), and the API
//! matches ids loosely ("medina" finds an unrelated masjid). But each board names its
//! livemasjid stream in `liveStreamURL`, so a board is used automatically **only** when that
//! equals the selected mount; anything else is offered as a suggestion to confirm.

use super::salah::{Board, Change, Moment, Salah, SalahTimes};
use super::{Fetched, fetch};
use crate::state::App;
use serde_json::Value;
use std::time::Duration;

const API: &str = "https://masjidboardlive.com/boards/api/board.php?id=";
pub const SITE: &str = "https://masjidboardlive.com/boards/?id=";

/// Result of looking a board id up (cached per id).
#[derive(Clone, Debug)]
pub enum Lookup {
    Found(Box<SalahTimes>),
    NotFound,
    Blocked,
    Failed,
}

/// Board ids are used in URLs: letters, digits, '-' and '_' only.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Accepts a board id or any masjidboardlive link (`boards/?id=x`, `boards/?x`, `v2?mid=x`).
pub fn parse_board_ref(input: &str) -> Option<String> {
    let s = input.trim();
    let id = if let Some((_, q)) = s.split_once('?') {
        let q = q.split('#').next().unwrap_or(q);
        q.split('&')
            .find_map(|kv| kv.strip_prefix("id=").or_else(|| kv.strip_prefix("mid=")))
            .unwrap_or_else(|| if q.contains('=') { "" } else { q })
    } else {
        s.trim_end_matches('/').rsplit('/').next().unwrap_or(s)
    };
    let id = id.trim().to_ascii_lowercase();
    valid_id(&id).then_some(id)
}

/// Fetch (cached) and parse a board.
pub async fn lookup(app: &App, id: &str) -> Lookup {
    let it = &app.integrations;
    let fresh = |l: &Lookup| match l {
        Lookup::Found(t) => t.date.as_deref() == Some(&super::salah::today()),
        _ => true,
    };
    if let Some(hit) = it.boards.get(id).filter(fresh) {
        return hit;
    }
    let _one = it.masjidboard_gate.lock().await;
    if let Some(hit) = it.boards.get(id).filter(fresh) {
        return hit; // fetched by a concurrent request while we waited
    }
    let (result, ttl) = match fetch(&format!("{API}{id}"), Duration::from_secs(10), &it.masjidboard).await {
        Fetched::Ok(body) => match parse(&body) {
            Some(mut t) => {
                t.date = Some(super::salah::today());
                // Times change rarely (a few minutes every week or two); hourly is plenty.
                (Lookup::Found(Box::new(t)), Duration::from_secs(3600))
            }
            None if body.contains("not_found") => (Lookup::NotFound, Duration::from_secs(24 * 3600)),
            None => (Lookup::Failed, Duration::from_secs(900)),
        },
        Fetched::NotFound => (Lookup::NotFound, Duration::from_secs(24 * 3600)),
        Fetched::Blocked { .. } => (Lookup::Blocked, Duration::from_secs(3600)),
        Fetched::Failed(e) => {
            tracing::debug!("masjidboardlive {id}: {e}");
            (Lookup::Failed, Duration::from_secs(900))
        }
    };
    it.boards.put(id, result.clone(), ttl);
    result
}

/// "HH:MM" or None for the board's placeholders ("~~~~", "#N/A", "00:00", "&nbsp;", …).
fn time(d: &Value, key: &str) -> Option<String> {
    let s = d.get(key)?.as_str()?.trim();
    let ok = s.len() >= 4 && s != "00:00" && s.as_bytes()[0].is_ascii_digit() && s.contains(':');
    ok.then(|| {
        let (h, m) = s.split_once(':').unwrap();
        format!("{:0>2}:{}", h.trim(), &m.trim()[..m.trim().len().min(2)])
    })
}

fn text(d: &Value, key: &str) -> Option<String> {
    let s = d.get(key)?.as_str()?.replace("<br>", " ").replace("&nbsp;", " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

/// Labels for `jumuahHeadings` indices, as on the board.
const JUMUAH_HEADINGS: [&str; 11] =
    ["Adhān", "Lecture", "Khutbah 1", "Sunan", "Khutbah 2", "Khutbah 3", "Khutbah", "Bayān", "Session 1", "Session 2", "Session 3"];

pub fn parse(body: &str) -> Option<SalahTimes> {
    let v: Value = serde_json::from_str(body).ok()?;
    if v.get("ok")?.as_bool() != Some(true) {
        return None;
    }
    let d = v.get("data")?;
    let meta = d.get("meta").cloned().unwrap_or(Value::Null);

    let sunday = super::salah::weekday() == 0;
    let zuhr_athaan = time(d, "dhuhrAthan");
    let zuhr_jamaat = if sunday { time(d, "zuhrSundays").or_else(|| time(d, "dhuhrJamaah")) } else { time(d, "dhuhrJamaah") };
    let salah = |key: &str, label: &str, a: Option<String>, j: Option<String>| {
        (a.is_some() || j.is_some()).then(|| Salah { key: key.into(), label: label.into(), athaan: a, jamaat: j })
    };
    let times: Vec<Salah> = [
        salah("fajr", "Fajr", time(d, "fajrAthan"), time(d, "fajrJamaah")),
        salah("zuhr", "Zuhr", zuhr_athaan, zuhr_jamaat),
        salah("asr", "Asr", time(d, "asrAthan"), time(d, "asrJamaah")),
        salah("maghrib", "Maghrib", time(d, "maghribAthan"), time(d, "maghribJamaah")),
        salah("esha", "Esha", time(d, "eshaAthan"), time(d, "eshaJamaah")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if times.is_empty() {
        return None;
    }

    // Jumu'ah: up to three times with headings like "0,3,6" → Adhān, Sunan, Khutbah.
    let headings: Vec<usize> = d.get("jumuahHeadings").and_then(Value::as_str).unwrap_or("0,3,6")
        .split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let jumuah_times: Vec<Moment> = ["jumuahTime1", "jumuahTime2", "jumuahTime3"].iter().enumerate()
        .filter_map(|(i, k)| {
            let t = time(d, k)?;
            let label = headings.get(i).and_then(|&h| JUMUAH_HEADINGS.get(h)).copied().unwrap_or("Jumu'ah");
            Some(Moment { key: (*k).into(), label: label.into(), time: t })
        })
        .collect();
    let jummah = (!jumuah_times.is_empty()).then(|| Salah {
        key: "jummah".into(),
        label: "Jumu'ah".into(),
        athaan: jumuah_times.first().map(|m| m.time.clone()),
        jamaat: jumuah_times.last().map(|m| m.time.clone()),
    });

    let day_keys = [
        ("sehriEnds", "Suhūr ends"),
        ("fajrStarts", "Fajr starts"),
        ("sunrise", "Sunrise"),
        ("ishraaq", "Ishrāq"),
        ("duha", "Duhā"),
        ("istiwaCaution", "Zawāl starts"),
        ("zawaalEnd", "Zawāl ends"),
        ("asrShafi", "Asr (Shāfi'ī)"),
        ("asrHanafi", "Asr (Hanafī)"),
        ("sunset", "Sunset"),
        ("eshaStarts", "Esha starts"),
    ];
    let day = day_keys.iter()
        .filter_map(|(k, l)| time(d, k).map(|t| Moment { key: (*k).into(), label: (*l).into(), time: t }))
        .collect();

    let changes = [("fajr", "Fajr"), ("asr", "Asr"), ("esha", "Esha")].iter()
        .filter_map(|(k, l)| {
            let date = text(d, &format!("{k}NextDate"))?;
            let time = time(d, &format!("{k}NextTime"))?;
            Some(Change { label: (*l).into(), date, time })
        })
        .collect();

    let id = text(&meta, "web_url").filter(|s| valid_id(s)).unwrap_or_default();
    let name = text(&meta, "masjid_name_2").or_else(|| text(&meta, "masjid")).or_else(|| text(&meta, "masjid_name_1"));
    let board = Board {
        url: format!("{SITE}{id}"),
        id,
        name: name.clone().unwrap_or_else(|| "Masjid".into()),
        city: text(&meta, "city"),
        stream: text(d, "liveStreamURL").map(|s| s.to_ascii_lowercase()),
    };
    Some(SalahTimes {
        status: "ok".into(),
        provider: "masjidboardlive".into(),
        masjid: name,
        source: board.url.clone(),
        times,
        jummah,
        jumuah_times,
        khateeb: text(d, "jumuah_khateeb").filter(|s| s != "-"),
        day,
        changes,
        hijri_adjust: text(d, "islamicDateAdjust").and_then(|s| s.parse().ok()).unwrap_or(0),
        updated: text(d, "last_updated"),
        board: Some(board),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"ok":true,"data":{"islamicDateAdjust":"-1","sehriEnds":"03:50","fajrStarts":"03:50","fajrAthan":"04:30","fajrJamaah":"04:45","sunrise":"05:14","ishraaq":"05:29","duha":"08:33","istiwaCaution":"11:47","istiwa":"11:52","zawaalEnd":"11:57","dhuhrAthan":"12:30","dhuhrJamaah":"12:45","jumuahTime1":"12:25","jumuahTime2":"12:30","jumuahTime3":"12:50","jumuah_khateeb":"Ml Talhah Tarag","asrShafi":"15:21","asrHanafi":"16:32","asrAthan":"17:15","asrJamaah":"17:30","sunset":"18:29","maghribAthan":"18:32","maghribJamaah":"18:35","eshaStarts":"19:53","eshaAthan":"20:00","eshaJamaah":"~~~~","last_updated":"Mon, 26 Oct 2026, 19:03:25","jumuahHeadings":"0,3,6","liveStreamURL":"lakewood","zuhrSundays":"12:45","fajrNextDate":"Sun 15 Nov","asrNextDate":"&nbsp;","eshaNextDate":"&nbsp;","fajrNextTime":"04:20","asrNextTime":"&nbsp;","eshaNextTime":"&nbsp;",
      "meta":{"masjid_name_1":"Lakewood","masjid_name_2":"Masjid Al Kauthar","masjid":"Masjid Kauthar<br>(Lakewood Estate)","city":"Ormonde","web_url":"lakewood-kauthar"}}}"#;

    #[test]
    fn parses_a_board() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t.times.len(), 5);
        assert_eq!(t.times[0].athaan.as_deref(), Some("04:30"));
        assert_eq!(t.times[4].jamaat, None); // "~~~~" placeholder
        let labels: Vec<_> = t.jumuah_times.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Adhān", "Sunan", "Khutbah"]);
        assert_eq!(t.khateeb.as_deref(), Some("Ml Talhah Tarag"));
        assert_eq!(t.day.len(), 11);
        assert_eq!(t.changes.len(), 1);
        assert_eq!(t.changes[0].time, "04:20");
        assert_eq!(t.hijri_adjust, -1);
        let b = t.board.unwrap();
        assert_eq!((b.id.as_str(), b.stream.as_deref(), b.city.as_deref()), ("lakewood-kauthar", Some("lakewood"), Some("Ormonde")));
        assert!(parse(r#"{"ok":false,"error":"not_found"}"#).is_none());
    }

    #[test]
    fn board_refs() {
        assert_eq!(parse_board_ref("https://masjidboardlive.com/boards/?id=lakewood-kauthar").as_deref(), Some("lakewood-kauthar"));
        assert_eq!(parse_board_ref("https://masjidboardlive.com/boards/?lakewood-kauthar").as_deref(), Some("lakewood-kauthar"));
        assert_eq!(parse_board_ref("https://premium.masjidboardlive.com/v2?mid=Lakewood-Kauthar").as_deref(), Some("lakewood-kauthar"));
        assert_eq!(parse_board_ref(" lakewood-kauthar ").as_deref(), Some("lakewood-kauthar"));
        assert_eq!(parse_board_ref("https://evil.example/x?id=../../etc"), None);
        assert_eq!(parse_board_ref(""), None);
    }
}
