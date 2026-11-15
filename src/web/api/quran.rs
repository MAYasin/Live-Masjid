//! Quran stored on the Pi: reciters, library, playlists, playback controls, downloads.
//! Pause and stop use the recording endpoints (`/recordings/pause`, `/recordings/stop`).

use crate::quran::{self, LibraryInfo, Playlist, Reciter};
use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct ReciterQuery {
    reciter: Option<String>,
}

/// The recitation asked for, or the app's default.
fn reciter_key(app: &App, asked: Option<&str>) -> String {
    asked.filter(|k| quran::valid_key(k)).map_or_else(|| app.settings().quran_reciter, str::to_string)
}

/// Surahs of one recitation (default: the app's reciter) with download states.
pub async fn library(State(app): State<App>, Query(q): Query<ReciterQuery>) -> ApiResult<Json<LibraryInfo>> {
    let key = reciter_key(&app, q.reciter.as_deref());
    let lib = app.quran.clone();
    // Reads a few hundred file sizes: off the async threads.
    let info = tokio::task::spawn_blocking(move || lib.info(&key)).await.map_err(ApiError::bad)?;
    Ok(Json(info))
}

/// Every recitation, popular first, plus the default.
pub async fn reciters(State(app): State<App>) -> Json<serde_json::Value> {
    let list: Vec<Reciter> = app.quran.reciters();
    Json(json!({ "default": app.settings().quran_reciter, "reciters": list }))
}

#[derive(Deserialize)]
pub struct SetReciter {
    reciter: String,
}

/// Choose the app's default reciter.
pub async fn set_reciter(State(app): State<App>, Json(r): Json<SetReciter>) -> ApiResult {
    let rc = app.quran.reciter(&r.reciter).ok_or_else(|| ApiError::bad("Unknown reciter"))?;
    app.update_settings(|s| s.quran_reciter.clone_from(&rc.key))?;
    app.log("info", format!("Quran reciter: {}", rc.label()));
    ok()
}

fn surah(n: u16) -> Result<u16, ApiError> {
    if quran::valid(n) { Ok(n) } else { Err(ApiError::bad("no such surah")) }
}

#[derive(Deserialize)]
pub struct PlayReq {
    /// A surah on its own, or…
    surah: Option<u16>,
    /// …a playlist, from entry `index`.
    playlist: Option<String>,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    position_s: u64,
    /// Recitation (default: the playlist's, else the app's).
    reciter: Option<String>,
    /// Speed (default: the playlist's, else the app's).
    speed: Option<f32>,
}

pub async fn play(State(app): State<App>, Json(r): Json<PlayReq>) -> ApiResult {
    let (n, playlist, pinned, pinned_speed) = match (&r.playlist, r.surah) {
        (Some(id), _) => {
            let p = app.quran.playlist(id).ok_or_else(|| ApiError::bad("That playlist no longer exists"))?;
            let ctx = quran::PlaylistCtx::new(&p, r.index);
            (ctx.surah(), Some(ctx), p.reciter, p.speed)
        }
        (None, Some(n)) => (surah(n)?, None, None, None),
        (None, None) => return Err(ApiError::bad("choose a surah or a playlist")),
    };
    let key = reciter_key(&app, r.reciter.as_deref().or(pinned.as_deref()));
    if !app.quran.resolve(Some(&key)).has(n) {
        return Err(ApiError::bad(format!("This reciter doesn't have {}", quran::name(n))));
    }
    let speed = r.speed.or(pinned_speed).unwrap_or_else(|| app.settings().quran_speed).clamp(0.5, 2.0);
    let _ = app.player_tx.send(PlayerCmd::PlayQuran { surah: n, position_ms: r.position_s * 1000, playlist, reciter: key, speed });
    ok()
}

pub async fn playlists(State(app): State<App>) -> Json<Vec<Playlist>> {
    Json(app.quran.playlists())
}

#[derive(Deserialize)]
pub struct SavePlaylist {
    id: Option<String>,
    name: String,
    surahs: Vec<u16>,
    #[serde(default)]
    repeat: bool,
    reciter: Option<String>,
    #[serde(default)]
    days: Vec<u8>,
    time: Option<String>,
    speed: Option<f32>,
}

pub async fn save_playlist(State(app): State<App>, Json(r): Json<SavePlaylist>) -> ApiResult<Json<Playlist>> {
    let p = Playlist {
        id: r.id.unwrap_or_default(),
        name: r.name,
        surahs: r.surahs,
        repeat: r.repeat,
        reciter: r.reciter.filter(|k| !k.is_empty()),
        days: r.days,
        time: r.time.filter(|t| !t.is_empty()),
        speed: r.speed,
    };
    app.quran.save_playlist(p).map(Json).map_err(ApiError::bad)
}

#[derive(Deserialize)]
pub struct PlaylistRef {
    id: String,
}

pub async fn delete_playlist(State(app): State<App>, Json(r): Json<PlaylistRef>) -> ApiResult {
    app.quran.delete_playlist(&r.id).map_err(ApiError::bad)?;
    ok()
}

#[derive(Deserialize)]
pub struct SeekReq {
    position_s: u64,
}

pub async fn seek(State(app): State<App>, Json(r): Json<SeekReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::QuranSeek(r.position_s * 1000));
    ok()
}

#[derive(Deserialize)]
pub struct StepReq {
    by: i32,
}

pub async fn step(State(app): State<App>, Json(r): Json<StepReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::QuranStep(r.by.clamp(-1, 1)));
    ok()
}

#[derive(Deserialize)]
pub struct DownloadReq {
    /// Download one surah, or start/stop downloading all of the recitation.
    surah: Option<u16>,
    all: Option<bool>,
    reciter: Option<String>,
}

pub async fn download(State(app): State<App>, Json(r): Json<DownloadReq>) -> ApiResult {
    let key = reciter_key(&app, r.reciter.as_deref());
    if let Some(n) = r.surah {
        app.quran.request(&key, surah(n)?);
    }
    if let Some(on) = r.all {
        app.quran.download_all(on.then_some(key.as_str()));
        app.log("info", if on { "Quran: downloading all surahs in the background" } else { "Quran: stopped downloading all" });
    }
    ok()
}

#[derive(Deserialize)]
pub struct DeleteReq {
    surah: Option<u16>,
    reciter: Option<String>,
}

/// Remove downloads (one surah of a recitation, or everything) to free space.
/// The surah playing is kept.
pub async fn delete(State(app): State<App>, Json(r): Json<DeleteReq>) -> ApiResult {
    let what = match r.surah {
        Some(n) => Some((reciter_key(&app, r.reciter.as_deref()), surah(n)?)),
        None => None,
    };
    let playing = app.status.borrow().recording.as_ref().and_then(|x| Some((x.reciter_key.clone()?, x.surah?)));
    let lib = app.quran.clone();
    let removed = tokio::task::spawn_blocking(move || lib.delete(what, playing.as_ref())).await.map_err(ApiError::bad)?;
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

#[derive(Deserialize)]
pub struct SpeedReq {
    speed: f32,
}

/// Finite and inside the range the player supports.
fn valid_speed(speed: f32) -> bool {
    speed.is_finite() && (0.5..=2.0).contains(&speed)
}

/// Change the playing Quran's speed. It also becomes the default speed, unless the playing
/// playlist has its own.
pub async fn speed(State(app): State<App>, Json(r): Json<SpeedReq>) -> ApiResult {
    if !valid_speed(r.speed) {
        return Err(ApiError::bad("Speed must be between 0.5× and 2×"));
    }
    let playlist = app.status.borrow().recording.as_ref().and_then(|x| x.playlist.as_ref().map(|p| p.id.clone()));
    let pinned = playlist.and_then(|id| app.quran.playlist(&id)).is_some_and(|p| p.speed.is_some());
    if !pinned {
        app.update_settings(|s| s.quran_speed = r.speed)?;
    }
    let _ = app.player_tx.send(PlayerCmd::QuranSpeed(r.speed));
    ok()
}

#[cfg(test)]
mod tests {
    use crate::sync::LockExt;
    use super::*;
    use crate::state::test_app;

    fn app() -> App {
        let dir = std::env::temp_dir().join(format!("livemasjid-quran-api-{}", crate::config::random_hex(6)));
        test_app(&dir)
    }

    /// An app whose reciter catalogue has one partial recitation (`partial-r1`: Al-Fātiḥah only),
    /// and whose player commands can be read back.
    fn app_with_catalogue() -> (App, std::path::PathBuf, tokio::sync::mpsc::UnboundedReceiver<PlayerCmd>) {
        let dir = std::env::temp_dir().join(format!("livemasjid-quran-api-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(dir.join("quran")).unwrap();
        let catalogue = json!([{ "key": "partial-r1", "name": "Partial", "style": "", "server": "https://server.mp3quran.net/partial/", "surahs": [1] }]);
        std::fs::write(dir.join("quran/reciters.json"), catalogue.to_string()).unwrap();
        let mut app = test_app(&dir);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::sync::Arc::get_mut(&mut app).expect("sole owner").player_tx = tx;
        (app, dir, rx)
    }

    fn playlist(name: &str, surahs: Vec<u16>, reciter: Option<&str>, speed: Option<f32>) -> SavePlaylist {
        SavePlaylist { id: None, name: name.into(), surahs, repeat: true, reciter: reciter.map(str::to_string), days: vec![], time: None, speed }
    }

    #[tokio::test]
    async fn the_default_reciter_can_be_changed() {
        let (app, dir, _) = app_with_catalogue();
        assert!(super::set_reciter(State(app.clone()), Json(SetReciter { reciter: "nobody-r9".into() })).await.is_err());
        assert!(super::set_reciter(State(app.clone()), Json(SetReciter { reciter: "partial-r1".into() })).await.is_ok());
        assert_eq!(app.settings().quran_reciter, "partial-r1");
        let Json(v) = super::reciters(State(app.clone())).await;
        assert_eq!(v["default"], "partial-r1");
        assert_eq!(v["reciters"][0]["key"], "partial-r1");
        let Json(info) = super::library(State(app.clone()), Query(ReciterQuery { reciter: None })).await.ok().expect("library");
        assert!(serde_json::to_value(info).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn playlists_play_with_their_own_reciter_and_speed() {
        let (app, dir, mut rx) = app_with_catalogue();
        let p = super::save_playlist(State(app.clone()), Json(playlist("Night", vec![1], Some("partial-r1"), Some(1.5)))).await.ok().expect("saved").0;
        let req = PlayReq { surah: None, playlist: Some(p.id.clone()), index: 0, position_s: 3, reciter: None, speed: None };
        assert!(super::play(State(app.clone()), Json(req)).await.is_ok());
        match rx.try_recv() {
            Ok(PlayerCmd::PlayQuran { surah, position_ms, playlist, reciter, speed }) => {
                assert_eq!((surah, position_ms, reciter.as_str()), (1, 3000, "partial-r1"));
                assert!((speed - 1.5).abs() < f32::EPSILON);
                assert_eq!(playlist.expect("playlist").id, p.id);
            }
            other => panic!("unexpected {other:?}"),
        }
        // The pinned recitation lacks Yā-Sīn.
        let p = super::save_playlist(State(app.clone()), Json(playlist("Yasin", vec![36], Some("partial-r1"), None))).await.ok().expect("saved").0;
        let req = PlayReq { surah: None, playlist: Some(p.id), index: 0, position_s: 0, reciter: None, speed: None };
        let Err(e) = super::play(State(app.clone()), Json(req)).await else { panic!("missing surah played") };
        assert!(e.1.contains("doesn't have"), "{}", e.1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn speed_of_a_playlist_with_its_own_speed_is_not_the_default() {
        let (app, dir, _) = app_with_catalogue();
        let p = super::save_playlist(State(app.clone()), Json(playlist("Fast", vec![1], None, Some(1.5)))).await.ok().expect("saved").0;
        let ctx = quran::PlaylistCtx::new(&p, 0);
        app.status.send_modify(|s| {
            s.recording = Some(crate::state::RecordingStatus {
                mount: String::new(),
                title: "Al-Fatihah".into(),
                position_s: 0,
                duration_s: None,
                paused: false,
                kind: "quran",
                surah: Some(1),
                playlist: Some(ctx),
                loaded_pct: None,
                reciter: None,
                reciter_key: Some("partial-r1".into()),
                speed: 1.5,
            });
        });
        let before = app.settings().quran_speed;
        assert!(super::speed(State(app.clone()), Json(SpeedReq { speed: 0.75 })).await.is_ok());
        assert!((app.settings().quran_speed - before).abs() < f32::EPSILON, "the playlist keeps its own speed");

        // Deleting everything keeps the surah that is playing.
        let removed = super::delete(State(app.clone()), Json(DeleteReq { surah: Some(1), reciter: Some("partial-r1".into()) })).await.ok().expect("deleted");
        assert_eq!(removed.0["removed"], 0);
        assert!(super::delete(State(app.clone()), Json(DeleteReq { surah: Some(200), reciter: None })).await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn downloads_can_be_requested_and_stopped() {
        let (app, dir, _) = app_with_catalogue();
        let req = |surah, all| DownloadReq { surah, all, reciter: Some("partial-r1".into()) };
        assert!(super::download(State(app.clone()), Json(req(Some(1), None))).await.is_ok());
        assert!(super::download(State(app.clone()), Json(req(None, Some(true)))).await.is_ok());
        assert!(super::download(State(app.clone()), Json(req(None, Some(false)))).await.is_ok());
        let logs: Vec<String> = app.logs.locked().iter().map(|l| l.msg.clone()).collect();
        assert!(logs.iter().any(|m| m.contains("downloading all")));
        assert!(logs.iter().any(|m| m.contains("stopped downloading all")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn surah_numbers_are_bounded() {
        for n in [1, 2, 36, 114] {
            assert!(surah(n).is_ok(), "surah {n} should be valid");
        }
        for n in [0, 115, 999] {
            assert!(surah(n).is_err(), "surah {n} should be rejected");
        }
    }

    #[test]
    fn speeds_are_finite_and_bounded() {
        for s in [0.5, 0.75, 1.0, 1.5, 2.0] {
            assert!(valid_speed(s), "{s} should be valid");
        }
        for s in [0.0, 0.49, 2.01, 10.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!valid_speed(s), "{s} should be rejected");
        }
    }

    #[test]
    fn reciter_keys_fall_back_to_the_default() {
        let app = app();
        let default = app.settings().quran_reciter;
        assert_eq!(super::reciter_key(&app, Some("not a key")), default);
        assert_eq!(super::reciter_key(&app, None), default);
        assert_eq!(super::reciter_key(&app, Some("abdulrahman-sudais-r1")), "abdulrahman-sudais-r1");
    }

    #[tokio::test]
    async fn playlists_round_trip_on_disk() {
        let app = app();
        assert!(super::playlists(State(app.clone())).await.0.is_empty());
        let req = SavePlaylist {
            id: None,
            name: "Morning".into(),
            surahs: vec![36, 67],
            repeat: false,
            reciter: None,
            days: vec![],
            time: None,
            speed: None,
        };
        let saved = super::save_playlist(State(app.clone()), Json(req)).await.ok().expect("saved").0;
        assert_eq!(saved.name, "Morning");
        assert_eq!(super::playlists(State(app.clone())).await.0.len(), 1);
        let _ = super::delete_playlist(State(app.clone()), Json(PlaylistRef { id: saved.id.clone() })).await.ok().expect("deleted");
        assert!(super::playlists(State(app.clone())).await.0.is_empty());
    }

    #[tokio::test]
    async fn play_requires_a_valid_surah_or_playlist() {
        let app = app();
        let empty = PlayReq { surah: None, playlist: None, index: 0, position_s: 0, reciter: None, speed: None };
        assert!(super::play(State(app.clone()), Json(empty)).await.is_err());
        let bad = PlayReq { surah: Some(115), playlist: None, index: 0, position_s: 0, reciter: None, speed: None };
        assert!(super::play(State(app.clone()), Json(bad)).await.is_err());
        let missing = PlayReq { surah: None, playlist: Some("nope".into()), index: 0, position_s: 0, reciter: None, speed: None };
        assert!(super::play(State(app.clone()), Json(missing)).await.is_err());
        let good = PlayReq { surah: Some(36), playlist: None, index: 0, position_s: 5, reciter: None, speed: None };
        assert!(super::play(State(app.clone()), Json(good)).await.is_ok());
    }

    #[tokio::test]
    async fn speed_is_validated_and_saved() {
        let app = app();
        assert!(super::speed(State(app.clone()), Json(SpeedReq { speed: 3.0 })).await.is_err());
        assert!(super::speed(State(app.clone()), Json(SpeedReq { speed: 1.25 })).await.is_ok());
        assert!((app.settings().quran_speed - 1.25).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn seek_step_and_download_accept_valid_input() {
        let app = app();
        assert!(super::seek(State(app.clone()), Json(SeekReq { position_s: 5 })).await.is_ok());
        assert!(super::step(State(app.clone()), Json(StepReq { by: 9 })).await.is_ok());
        assert!(super::download(State(app.clone()), Json(DownloadReq { surah: None, all: None, reciter: None })).await.is_ok());
        assert!(super::download(State(app.clone()), Json(DownloadReq { surah: Some(0), all: None, reciter: None })).await.is_err());
        let removed = super::delete(State(app.clone()), Json(DeleteReq { surah: None, reciter: None })).await.ok().expect("deleted");
        assert_eq!(removed.0["ok"], true);
    }
}
