//! Which salah a broadcast belongs to ("Esha", "Jumu'ah", …), from the masjid's times.
//!
//! A broadcast belongs to a salah when it starts between an hour before its adhān and an hour
//! after its iqāmah (90 minutes after the adhān when there is no iqāmah). On Fridays, Zuhr
//! counts as Jumu'ah. Talks at other times get no label.

use crate::integrations::salah::SalahTimes;

/// Minutes of slack around a salah's adhān/iqāmah.
const BEFORE_MIN: i32 = 60;
const AFTER_MIN: i32 = 60;

fn minutes(hhmm: &str) -> Option<i32> {
    let (h, m) = hhmm.split_once(':')?;
    Some(h.trim().parse::<i32>().ok()? * 60 + m.trim().get(..2).unwrap_or(m).parse::<i32>().ok()?)
}

/// The salah a broadcast starting at `minute` (of the local day) on `weekday` (0 = Sunday)
/// belongs to, if any.
pub fn salah_for(times: &SalahTimes, weekday: u32, minute: i32) -> Option<String> {
    let friday = weekday == 5;
    let mut best: Option<(i32, String)> = None;
    for s in &times.times {
        let (athaan, jamaat) = if s.key == "zuhr" && friday {
            times.jummah.as_ref().map_or((s.athaan.as_deref(), s.jamaat.as_deref()), |j| (j.athaan.as_deref(), j.jamaat.as_deref()))
        } else {
            (s.athaan.as_deref(), s.jamaat.as_deref())
        };
        let Some(start) = athaan.or(jamaat).and_then(minutes) else { continue };
        let end = jamaat.and_then(minutes).unwrap_or(start + 30).max(start);
        if minute < start - BEFORE_MIN || minute > end + AFTER_MIN {
            continue;
        }
        // Closest salah wins when windows overlap (Maghrib and Esha in winter).
        let distance = if minute < start { start - minute } else if minute > end { minute - end } else { 0 };
        let label = if s.key == "zuhr" && friday { "Jumu'ah".to_string() } else { s.label.clone() };
        if best.as_ref().is_none_or(|(d, _)| distance < *d) {
            best = Some((distance, label));
        }
    }
    best.map(|(_, l)| l)
}

/// Local weekday (0 = Sunday) and minute of the day for a UTC timestamp, using the Pi's
/// current UTC offset (South Africa has no daylight saving).
pub fn local_day_minute(ts_ms: u64, offset_s: i64) -> (u32, i32) {
    let local = ts_ms as i64 / 1000 + offset_s;
    let days = local.div_euclid(86_400);
    let minute = (local.rem_euclid(86_400) / 60) as i32;
    // 1970-01-01 was a Thursday (4).
    (((days + 4).rem_euclid(7)) as u32, minute)
}

/// The Pi's UTC offset in seconds (from `date +%z`), cached after the first call.
pub fn utc_offset_s() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        let out = std::process::Command::new("date").arg("+%z").output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        parse_offset(&text).unwrap_or(0)
    })
}

fn parse_offset(z: &str) -> Option<i64> {
    let (sign, rest) = match z.as_bytes().first()? {
        b'+' => (1, &z[1..]),
        b'-' => (-1, &z[1..]),
        _ => return None,
    };
    if rest.len() != 4 {
        return None;
    }
    Some(sign * (rest[..2].parse::<i64>().ok()? * 3600 + rest[2..].parse::<i64>().ok()? * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::salah::{Salah, SalahTimes};

    fn times() -> SalahTimes {
        let s = |k: &str, l: &str, a: &str, j: &str| Salah { key: k.into(), label: l.into(), athaan: Some(a.into()), jamaat: Some(j.into()) };
        SalahTimes {
            times: vec![
                s("fajr", "Fajr", "04:30", "04:45"),
                s("zuhr", "Zuhr", "12:30", "12:45"),
                s("asr", "Asr", "17:15", "17:30"),
                s("maghrib", "Maghrib", "18:32", "18:35"),
                s("esha", "Esha", "20:00", "20:15"),
            ],
            jummah: Some(s("jummah", "Jumu'ah", "12:25", "12:50")),
            ..Default::default()
        }
    }

    #[test]
    fn labels_broadcasts() {
        let t = times();
        let at = |h: i32, m: i32| h * 60 + m;
        assert_eq!(salah_for(&t, 2, at(20, 1)).as_deref(), Some("Esha"));
        assert_eq!(salah_for(&t, 2, at(4, 31)).as_deref(), Some("Fajr"));
        assert_eq!(salah_for(&t, 5, at(12, 20)).as_deref(), Some("Jumu'ah")); // Friday
        assert_eq!(salah_for(&t, 4, at(12, 20)).as_deref(), Some("Zuhr"));
        assert_eq!(salah_for(&t, 2, at(18, 40)).as_deref(), Some("Maghrib")); // closer than Esha
        assert_eq!(salah_for(&t, 2, at(10, 0)), None); // a mid-morning talk
    }

    #[test]
    fn local_time() {
        // 2026-11-08 20:00 SAST (+02:00) = 18:00 UTC, a Sunday.
        let ts = 1_794_160_800_000u64; // 2026-11-08T18:00:00Z
        assert_eq!(local_day_minute(ts, 7200), (0, 20 * 60));
        assert_eq!(parse_offset("+0200"), Some(7200));
        assert_eq!(parse_offset("-0130"), Some(-5400));
        assert_eq!(parse_offset("Z"), None);
    }
}
