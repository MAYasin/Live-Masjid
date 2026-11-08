//! Scheduled Quran playlists ("every Friday at 11:00"). Checks the Pi's local clock a few
//! times a minute and tells the player when a playlist is due; the player decides how
//! (live broadcasts keep priority: it waits for the broadcast to end, up to 2 hours).

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

pub async fn run(app: App) {
    // Playlist id → date it last fired, so each slot fires once.
    let mut fired: HashMap<String, String> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Some((weekday, hhmm, date)) = local_now().await else { continue };
        for p in app.quran.playlists().into_iter().filter(|p| p.scheduled()) {
            let due = p.days.contains(&weekday) && p.time.as_deref() == Some(hhmm.as_str());
            if due && fired.get(&p.id) != Some(&date) {
                fired.insert(p.id.clone(), date.clone());
                app.log("info", format!("Scheduled playlist \"{}\" ({hhmm})", p.name));
                let _ = app.player_tx.send(PlayerCmd::ScheduledPlaylist(p.id));
            }
        }
    }
}
