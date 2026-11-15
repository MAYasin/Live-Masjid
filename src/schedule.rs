//! Scheduled Quran playlists ("every Friday at 11:00"). Checks the Pi's local clock a few
//! times a minute and tells the player when a playlist is due; the player decides how
//! (live broadcasts keep priority: it waits for the broadcast to end, up to 2 hours).

use crate::quran::Playlist;
use crate::state::{App, PlayerCmd};
use std::collections::HashMap;
use std::time::Duration;

/// Local weekday (0 = Sunday), "HH:MM" and date, from the system clock and time zone.
async fn local_now() -> Option<(u8, String, String)> {
    let out = tokio::process::Command::new("date").arg("+%w %H:%M %F").output().await.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.to_string(), it.next()?.to_string()))
}

/// Is this playlist scheduled for `weekday` at exactly `hhmm`?
fn due(p: &Playlist, weekday: u8, hhmm: &str) -> bool {
    p.days.contains(&weekday) && p.time.as_deref() == Some(hhmm)
}

/// Playlists that should fire now, marking each one as fired for `date` so a slot fires
/// only once. Cloned so the caller can log names without holding the playlist lock.
fn take_due(playlists: &[Playlist], weekday: u8, hhmm: &str, date: &str, fired: &mut HashMap<String, String>) -> Vec<Playlist> {
    let mut out = Vec::new();
    for p in playlists.iter().filter(|p| p.scheduled()) {
        if due(p, weekday, hhmm) && fired.get(&p.id).map(String::as_str) != Some(date) {
            fired.insert(p.id.clone(), date.to_string());
            out.push(p.clone());
        }
    }
    out
}

pub async fn run(app: App) {
    // Playlist id → date it last fired, so each slot fires once.
    let mut fired: HashMap<String, String> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Some((weekday, hhmm, date)) = local_now().await else { continue };
        fire_due(&app, weekday, &hhmm, &date, &mut fired);
    }
}

/// Tell the player about every playlist due now.
fn fire_due(app: &App, weekday: u8, hhmm: &str, date: &str, fired: &mut HashMap<String, String>) {
    for p in take_due(&app.quran.playlists(), weekday, hhmm, date, fired) {
        app.log("info", format!("Scheduled playlist \"{}\" ({hhmm})", p.name));
        let _ = app.player_tx.send(PlayerCmd::ScheduledPlaylist(p.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(id: &str, days: &[u8], time: &str) -> Playlist {
        Playlist { id: id.into(), name: id.into(), surahs: vec![1], days: days.to_vec(), time: Some(time.into()), ..Playlist::default() }
    }

    #[test]
    fn fires_on_matching_day_and_minute() {
        let p = playlist("a", &[5], "11:00");
        assert!(due(&p, 5, "11:00"));
        assert!(!due(&p, 4, "11:00"), "wrong day");
        assert!(!due(&p, 5, "11:01"), "wrong minute");
    }

    #[test]
    fn fires_only_once_per_date() {
        let list = vec![playlist("a", &[5], "11:00"), playlist("b", &[5], "11:00")];
        let mut fired = HashMap::new();
        let first: Vec<_> = take_due(&list, 5, "11:00", "2026-11-13", &mut fired).into_iter().map(|p| p.id).collect();
        assert_eq!(first, vec!["a".to_string(), "b".to_string()]);
        assert!(take_due(&list, 5, "11:00", "2026-11-13", &mut fired).is_empty());
        // The same slot fires again the following week.
        assert_eq!(take_due(&list, 5, "11:00", "2026-11-20", &mut fired).len(), 2);
    }

    #[test]
    fn ignores_unscheduled_and_other_days() {
        let mut unscheduled = playlist("u", &[5], "11:00");
        unscheduled.time = None;
        let list = vec![unscheduled, playlist("m", &[1], "11:00"), playlist("n", &[5], "10:00")];
        let mut fired = HashMap::new();
        assert!(take_due(&list, 5, "11:00", "2026-11-13", &mut fired).is_empty());
    }

    #[test]
    fn one_playlist_firing_does_not_block_another() {
        let list = vec![playlist("a", &[5], "11:00")];
        let mut fired = HashMap::new();
        fired.insert("b".into(), "2026-11-13".into());
        assert_eq!(take_due(&list, 5, "11:00", "2026-11-13", &mut fired).len(), 1);
    }

    #[tokio::test]
    async fn reads_the_local_clock() {
        let (weekday, hhmm, date) = local_now().await.expect("date is available");
        assert!(weekday <= 6);
        assert_eq!((hhmm.len(), &hhmm[2..3]), (5, ":"));
        assert_eq!(date.len(), 10);
    }

    #[test]
    fn due_playlists_are_sent_to_the_player_once() {
        let dir = std::env::temp_dir().join(format!("lm-schedule-{}", crate::config::random_hex(6)));
        let app = crate::state::test_app(&dir);
        let saved = app.quran.save_playlist(playlist("x", &[5], "11:00")).unwrap();
        let mut fired = HashMap::new();
        fire_due(&app, 5, "11:00", "2026-11-13", &mut fired);
        assert_eq!(fired.get(&saved.id).map(String::as_str), Some("2026-11-13"));
        fire_due(&app, 5, "11:01", "2026-11-13", &mut fired);
        assert_eq!(fired.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
