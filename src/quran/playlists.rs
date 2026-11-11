//! Named playlists, shared by everyone using the Pi, optionally pinned to a reciter, given a
//! speed and scheduled on weekdays.

use super::valid;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// At most this many playlists.
const MAX_PLAYLISTS: usize = 50;

/// A user's named selection of surahs, played in order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub surahs: Vec<u16>,
    /// Start again from the first surah after the last.
    pub repeat: bool,
    /// Recitation to use (None = the app's default reciter).
    #[serde(default)]
    pub reciter: Option<String>,
    /// Play automatically on these weekdays (0 = Sunday) at `time` ("HH:MM").
    #[serde(default)]
    pub days: Vec<u8>,
    #[serde(default)]
    pub time: Option<String>,
    /// Playback speed (None = the app's default).
    #[serde(default)]
    pub speed: Option<f32>,
}

impl Playlist {
    pub fn scheduled(&self) -> bool {
        self.time.is_some() && !self.days.is_empty()
    }
}

/// What is playing from a playlist (a snapshot, so editing the playlist doesn't disturb it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlaylistCtx {
    pub id: String,
    pub name: String,
    pub surahs: Vec<u16>,
    pub index: usize,
    pub repeat: bool,
}

impl PlaylistCtx {
    pub fn new(p: &Playlist, index: usize) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            surahs: p.surahs.clone(),
            index: index.min(p.surahs.len().saturating_sub(1)),
            repeat: p.repeat,
        }
    }

    pub fn surah(&self) -> u16 {
        self.surahs[self.index]
    }

    /// The entry `by` steps away (-1/+1); wraps around only with repeat. None past the end.
    pub fn step(&self, by: i32) -> Option<Self> {
        let len = i64::try_from(self.surahs.len()).ok()?;
        let mut i = i64::try_from(self.index).ok()? + i64::from(by);
        if i >= len {
            if !self.repeat {
                return None;
            }
            i = 0;
        }
        if i < 0 {
            i = if self.repeat { len - 1 } else { 0 };
        }
        Some(Self { index: usize::try_from(i).ok()?, ..self.clone() })
    }
}

/// "HH:MM", 24-hour.
fn valid_time(t: &str) -> bool {
    t.len() == 5
        && t.as_bytes()[2] == b':'
        && t[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && t[3..].parse::<u8>().is_ok_and(|m| m < 60)
}

/// Check and tidy a playlist from the UI. `reciter_known` tells whether a recitation key exists.
///
/// # Errors
/// A sentence for the user saying what to fix.
pub fn validate(mut p: Playlist, reciter_known: impl Fn(&str) -> bool) -> Result<Playlist, String> {
    p.name = p.name.trim().to_string();
    if p.name.is_empty() || p.name.chars().count() > 60 {
        return Err("Give the playlist a name (up to 60 characters)".into());
    }
    if p.surahs.is_empty() || p.surahs.len() > 300 || !p.surahs.iter().all(|&n| valid(n)) {
        return Err("Add at least one surah".into());
    }
    if p.reciter.as_deref().is_some_and(|k| !reciter_known(k)) {
        return Err("Unknown reciter".into());
    }
    if p.speed.is_some_and(|v| !(0.5..=2.0).contains(&v)) {
        return Err("Speed must be between 0.5× and 2×".into());
    }
    p.days.retain(|d| *d <= 6);
    p.days.sort_unstable();
    p.days.dedup();
    match &p.time {
        Some(t) => {
            if !valid_time(t) {
                return Err("Pick a time like 11:00".into());
            }
            if p.days.is_empty() {
                return Err("Pick at least one day for the schedule".into());
            }
        }
        None => p.days.clear(),
    }
    Ok(p)
}

/// The playlists, saved in `quran-playlists.json`.
pub(super) struct PlaylistStore {
    path: PathBuf,
    list: Mutex<Vec<Playlist>>,
}

impl PlaylistStore {
    pub fn load(path: PathBuf) -> Self {
        let list = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, list: Mutex::new(list) }
    }

    pub fn all(&self) -> Vec<Playlist> {
        self.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Playlist> {
        self.list.lock().unwrap().iter().find(|p| p.id == id).cloned()
    }

    /// Replace the playlist with the same id, or add it under a new id. `p` is already validated.
    pub fn save(&self, mut p: Playlist) -> Result<Playlist, String> {
        let mut list = self.list.lock().unwrap();
        match list.iter_mut().find(|x| x.id == p.id) {
            Some(x) => *x = p.clone(),
            None => {
                if list.len() >= MAX_PLAYLISTS {
                    return Err(format!("That's the maximum of {MAX_PLAYLISTS} playlists"));
                }
                p.id = crate::config::random_hex(6);
                list.push(p.clone());
            }
        }
        self.persist(&list)?;
        Ok(p)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut list = self.list.lock().unwrap();
        let before = list.len();
        list.retain(|p| p.id != id);
        self.persist(&list)?;
        Ok(list.len() != before)
    }

    fn persist(&self, list: &[Playlist]) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(list).map_err(|e| e.to_string())?;
        crate::config::write_atomic(&self.path, &body).map_err(|e| format!("couldn't save playlists: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn morning() -> Playlist {
        Playlist { id: "a".into(), name: " Morning ".into(), surahs: vec![1, 2, 36], ..Playlist::default() }
    }

    #[test]
    fn playlist_steps() {
        let p = morning();
        let c = PlaylistCtx::new(&p, 0);
        assert_eq!(c.step(1).unwrap().surah(), 2);
        assert_eq!(c.step(-1).unwrap().surah(), 1); // stays at the start
        let last = PlaylistCtx::new(&p, 2);
        assert!(last.step(1).is_none()); // ends without repeat
        let r = PlaylistCtx { repeat: true, ..last };
        assert_eq!(r.step(1).unwrap().surah(), 1); // wraps with repeat
        assert_eq!(r.step(-3).unwrap().surah(), 36); // backwards past the start wraps too
        assert_eq!(PlaylistCtx::new(&p, 9).index, 2);
    }

    #[test]
    fn validation() {
        let known = |k: &str| k == "sudais";
        let ok = validate(morning(), known).unwrap();
        assert_eq!(ok.name, "Morning");
        assert!(ok.days.is_empty() && !ok.scheduled());

        let scheduled = Playlist { time: Some("06:30".into()), days: vec![5, 1, 9, 1], ..morning() };
        let v = validate(scheduled, known).unwrap();
        assert_eq!(v.days, vec![1, 5], "sorted, deduplicated, invalid days dropped");
        assert!(v.scheduled());

        let err = |p: Playlist| validate(p, known).unwrap_err();
        assert!(err(Playlist { name: "  ".into(), ..morning() }).contains("name"));
        assert!(err(Playlist { surahs: vec![], ..morning() }).contains("surah"));
        assert!(err(Playlist { surahs: vec![115], ..morning() }).contains("surah"));
        assert_eq!(err(Playlist { reciter: Some("nobody".into()), ..morning() }), "Unknown reciter");
        assert!(err(Playlist { speed: Some(3.0), ..morning() }).contains("Speed"));
        assert!(err(Playlist { time: Some("24:00".into()), days: vec![1], ..morning() }).contains("time"));
        assert!(err(Playlist { time: Some("6:30".into()), days: vec![1], ..morning() }).contains("time"));
        assert!(err(Playlist { time: Some("06:30".into()), ..morning() }).contains("day"));
        // Days without a time are dropped rather than refused.
        assert!(validate(Playlist { days: vec![1], ..morning() }, known).unwrap().days.is_empty());
    }
}
