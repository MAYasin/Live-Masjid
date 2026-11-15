//! Salah times and recordings. All third-party fetching happens on the Pi, cached
//! (see `crate::integrations`), so browsing the UI never adds load to those sites.

use crate::integrations::{recordings, salah, valid_mount};
use crate::state::{App, PlayerCmd};
use crate::web::{ApiError, ApiResult, ok};
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct MountQuery {
    mount: Option<String>,
}

/// The requested mount, or the selected one.
fn mount_or_selected(app: &App, q: MountQuery) -> Result<String, ApiError> {
    let m = q.mount.or_else(|| app.settings().mount).ok_or_else(|| ApiError::bad("no masjid selected"))?;
    if !valid_mount(&m) {
        return Err(ApiError::bad("invalid masjid"));
    }
    Ok(m)
}

pub async fn salah_times(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult<Json<salah::SalahTimes>> {
    let mount = mount_or_selected(&app, q)?;
    Ok(Json(salah::get(&app, &mount).await))
}

#[derive(Deserialize)]
pub struct BoardChoice {
    mount: String,
    /// A masjidboardlive link or id; "auto" to match automatically; "none" for masjidscreen only.
    board: String,
}

/// Choose where a masjid's salah times come from. A board is checked before it is saved.
pub async fn salah_board(State(app): State<App>, Json(c): Json<BoardChoice>) -> ApiResult<Json<salah::SalahTimes>> {
    use crate::integrations::masjidboard::{self, Lookup};
    if !valid_mount(&c.mount) {
        return Err(ApiError::bad("invalid masjid"));
    }
    let key = c.mount.strip_suffix(".mp3").unwrap_or(&c.mount).to_ascii_lowercase();
    let choice = match c.board.trim() {
        "" | "auto" => None,
        "none" => Some("none".to_string()),
        input => {
            let id = masjidboard::parse_board_ref(input)
                .ok_or_else(|| ApiError::bad("That doesn't look like a masjidboardlive link"))?;
            match masjidboard::lookup(&app, &id).await {
                // Save the board's canonical id (the link may use an alias).
                Lookup::Found(t) => Some(t.board.map(|b| b.id).filter(|b| !b.is_empty()).unwrap_or(id)),
                Lookup::NotFound => return Err(ApiError::bad("No masjidboardlive board has that link")),
                Lookup::Blocked | Lookup::Failed => return Err(ApiError::bad("Couldn't reach masjidboardlive.com; try again later")),
            }
        }
    };
    app.update_settings(|s| match choice {
        Some(v) => {
            s.salah_boards.insert(key.clone(), v);
        }
        None => {
            s.salah_boards.remove(&key);
        }
    })?;
    Ok(Json(salah::get(&app, &c.mount).await))
}

pub async fn recording_list(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult<Json<recordings::RecordingList>> {
    let mount = mount_or_selected(&app, q)?;
    Ok(Json(recordings::list(&app, &mount).await))
}

#[derive(Deserialize)]
pub struct PlayReq {
    mount: String,
    id: String,
}

pub async fn play(State(app): State<App>, Json(r): Json<PlayReq>) -> ApiResult {
    if !valid_mount(&r.mount) {
        return Err(ApiError::bad("invalid masjid"));
    }
    // Only recordings we listed ourselves can be played (no arbitrary URLs).
    let rec = recordings::find(&app, &r.mount, &r.id).await.ok_or_else(|| ApiError::bad("recording not found; refresh the list"))?;
    let _ = app.player_tx.send(PlayerCmd::PlayRecording { mount: r.mount, title: rec.title, url: rec.url, duration_s: rec.duration_s });
    ok()
}

#[derive(Deserialize)]
pub struct PauseReq {
    pause: bool,
}

pub async fn pause(State(app): State<App>, Json(r): Json<PauseReq>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::PauseRecording(r.pause));
    ok()
}

pub async fn stop(State(app): State<App>) -> ApiResult {
    let _ = app.player_tx.send(PlayerCmd::StopRecording);
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ listening in the browser

fn browser_listening_enabled(app: &App) -> Result<(), ApiError> {
    if app.settings().browser_listening {
        Ok(())
    } else {
        Err(ApiError::bad("Listening in the browser is turned off (Settings)"))
    }
}

/// Stream URLs for the visitor's own device. MP3 first (plays everywhere, incl. iOS Safari),
/// relay before origin (keeps load off the origin). The Pi's own output is untouched.
pub async fn listen(State(app): State<App>, Query(q): Query<MountQuery>) -> ApiResult {
    browser_listening_enabled(&app)?;
    let mount = mount_or_selected(&app, q)?;
    let base = mount.strip_suffix(".mp3").unwrap_or(&mount).to_string();
    let live = crate::monitor::is_live(&app.live.read().unwrap(), &base);
    // First: the Pi's own output, in sync with the speaker (and plays recordings too).
    // Fallback: livemasjid's streams directly (not in sync).
    let mut sources = vec![json!({ "url": "/api/listen/live.wav", "type": "audio/wav", "synced": true })];
    for (path, kind) in [(format!("{base}.mp3"), "audio/mpeg"), (base.clone(), "audio/ogg")] {
        for (host, port) in crate::net::STREAM_HOSTS.iter().rev() {
            sources.push(json!({ "url": format!("http://{host}:{port}/{path}"), "type": kind }));
        }
    }
    let playing = matches!(app.status.borrow().phase, crate::state::Phase::Playing | crate::state::Phase::Reconnecting | crate::state::Phase::Recording);
    Ok(Json(json!({ "mount": base, "live": live || playing, "sources": sources })))
}

#[derive(Deserialize)]
pub struct RecordingRef {
    mount: String,
    id: String,
}

/// The file URL of a listed recording, for playing it on the visitor's device.
pub async fn recording_url(State(app): State<App>, Query(r): Query<RecordingRef>) -> ApiResult {
    browser_listening_enabled(&app)?;
    if !valid_mount(&r.mount) {
        return Err(ApiError::bad("invalid masjid"));
    }
    let rec = recordings::find(&app, &r.mount, &r.id).await.ok_or_else(|| ApiError::bad("recording not found; refresh the list"))?;
    Ok(Json(json!({ "url": rec.url, "title": rec.title })))
}

/// Most simultaneous browser listeners (each costs ~48 KB/s of the Pi's Wi-Fi).
const MAX_MIRROR_LISTENERS: usize = 6;

/// WAV header for an endless mono 16-bit stream (sizes set to the maximum).
fn wav_header(rate: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&u32::MAX.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&1u16.to_le_bytes()); // mono
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    h.extend_from_slice(&2u16.to_le_bytes()); // block align
    h.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(u32::MAX - 36).to_le_bytes());
    h
}

/// Exactly what the Pi's speaker is playing, as an endless WAV stream, paced by the Pi's
/// audio device — so the visitor hears the same audio at (nearly) the same moment.
pub async fn listen_live(State(app): State<App>) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;
    use tokio::sync::broadcast::error::RecvError;
    browser_listening_enabled(&app)?;
    if app.sink.mirror.receiver_count() >= MAX_MIRROR_LISTENERS {
        return Err(ApiError::bad("Too many devices are listening right now"));
    }
    let rx = app.sink.mirror.subscribe();
    let header = bytes::Bytes::from(wav_header(crate::sink::MIRROR_RATE));
    let stream = futures_util::stream::unfold((rx, Some(header)), |(mut rx, header)| async move {
        if let Some(h) = header {
            return Some((Ok::<_, std::convert::Infallible>(h), (rx, None)));
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => return Some((Ok(bytes::Bytes::copy_from_slice(&chunk)), (rx, None))),
                // A slow listener fell behind: skip ahead rather than drift.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Ok((
        [
            (header::CONTENT_TYPE, "audio/wav"),
            (header::CACHE_CONTROL, "no-store"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_app;

    fn app() -> App {
        let dir = std::env::temp_dir().join(format!("livemasjid-extras-api-{}", crate::config::random_hex(6)));
        test_app(&dir)
    }

    #[test]
    fn wav_header_is_well_formed() {
        let h = super::wav_header(24_000);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 24_000);
        assert_eq!(&h[36..40], b"data");
    }

    #[test]
    fn mount_selection_rules() {
        let app = app();
        assert!(super::mount_or_selected(&app, MountQuery { mount: None }).is_err(), "none selected");
        assert_eq!(super::mount_or_selected(&app, MountQuery { mount: Some("m".into()) }).ok(), Some("m".into()));
        assert!(super::mount_or_selected(&app, MountQuery { mount: Some("a/b".into()) }).is_err());
        app.update_settings(|s| s.mount = Some("selected".into())).unwrap();
        assert_eq!(super::mount_or_selected(&app, MountQuery { mount: None }).ok(), Some("selected".into()));
    }

    #[tokio::test]
    async fn browser_listening_switch_and_simple_commands() {
        let app = app();
        assert!(super::browser_listening_enabled(&app).is_ok());
        assert!(super::pause(State(app.clone()), Json(PauseReq { pause: true })).await.is_ok());
        assert!(super::stop(State(app.clone())).await.is_ok());
        app.update_settings(|s| s.browser_listening = false).unwrap();
        assert!(super::browser_listening_enabled(&app).is_err());
        let url = super::recording_url(State(app.clone()), Query(RecordingRef { mount: "m".into(), id: "x".into() })).await;
        assert!(url.is_err(), "listening is off");
    }

    #[tokio::test]
    async fn listen_lists_synced_then_direct_sources() {
        let app = app();
        app.update_settings(|s| s.mount = Some("m".into())).unwrap();
        let Json(v) = super::listen(State(app.clone()), Query(MountQuery { mount: None })).await.ok().expect("listen");
        assert_eq!(v["mount"], "m");
        assert_eq!(v["live"], false);
        let sources = v["sources"].as_array().expect("sources");
        assert_eq!(sources[0]["url"], "/api/listen/live.wav");
        assert_eq!(sources[0]["synced"], true);
        assert!(sources.iter().any(|s| s["type"] == "audio/mpeg"));
        assert!(sources.iter().any(|s| s["type"] == "audio/ogg"));
        app.set_phase(crate::state::Phase::Playing);
        let Json(v) = super::listen(State(app.clone()), Query(MountQuery { mount: Some("m".into()) })).await.ok().expect("listen");
        assert_eq!(v["live"], true, "a playing phase counts as live");
    }

    // ------------------------------------------------------------- cached third-party data
    // Everything below is served from pre-filled caches: nothing reaches the internet.

    use crate::integrations::masjidboard::Lookup;
    use crate::integrations::recordings::{Recording, RecordingList};
    use std::time::Duration;

    const HOUR: Duration = Duration::from_secs(3600);

    fn cache_recordings(app: &App, mount: &str) {
        let rec = Recording {
            id: "r1".into(),
            title: "Esha".into(),
            url: "https://cdn.livemasjid.com/m/esha.mp3".into(),
            published: None,
            size_bytes: None,
            duration_s: Some(600),
        };
        let list = RecordingList { status: "ok".into(), mount: mount.into(), items: vec![rec], ..Default::default() };
        app.integrations.recordings.put(mount, list, HOUR);
    }

    /// masjidscreen "not listed" for `mount`, and no automatic board.
    fn cache_masjidscreen(app: &App, mount: &str) {
        let t = salah::SalahTimes { status: "not_listed".into(), provider: "masjidscreen".into(), ..Default::default() };
        app.integrations.salah.put(mount, t, HOUR);
        app.integrations.boards.put(mount, Lookup::NotFound, HOUR);
    }

    fn board(id: &str) -> Lookup {
        let b = salah::Board { id: id.into(), name: "Board".into(), city: None, url: String::new(), stream: None };
        let t = salah::SalahTimes {
            status: "ok".into(),
            provider: "masjidboardlive".into(),
            date: Some(salah::today()),
            board: Some(b),
            ..Default::default()
        };
        Lookup::Found(Box::new(t))
    }

    #[tokio::test]
    async fn salah_times_for_the_selected_masjid() {
        let app = app();
        assert!(super::salah_times(State(app.clone()), Query(MountQuery { mount: None })).await.is_err(), "none selected");
        app.update_settings(|s| s.mount = Some("m".into())).unwrap();
        cache_masjidscreen(&app, "m");
        let Json(t) = super::salah_times(State(app.clone()), Query(MountQuery { mount: None })).await.ok().expect("times");
        assert_eq!(t.status, "not_listed");
        assert!(!t.manual);
    }

    #[tokio::test]
    async fn salah_board_choices() {
        let app = app();
        cache_masjidscreen(&app, "m");
        let it = &app.integrations;
        it.boards.put("alias", board("canonical"), HOUR);
        it.boards.put("canonical", board("canonical"), HOUR);
        it.boards.put("gone", Lookup::NotFound, HOUR);
        it.boards.put("down", Lookup::Failed, HOUR);
        it.boards.put("refused", Lookup::Blocked, HOUR);
        let choose = |mount: &str, board: &str| {
            super::salah_board(State(app.clone()), Json(BoardChoice { mount: mount.into(), board: board.into() }))
        };

        assert!(choose("a/b", "auto").await.is_err(), "invalid masjid");
        assert!(choose("m", "not a link!").await.is_err());
        for missing in ["gone", "down", "refused"] {
            assert!(choose("m", missing).await.is_err(), "{missing}");
        }
        assert!(app.settings().salah_boards.is_empty(), "nothing saved after a failure");

        // A link to an alias saves the board's canonical id.
        let Json(t) = choose("M.mp3", "https://masjidboardlive.com/boards/?id=alias").await.ok().expect("board");
        assert_eq!(app.settings().salah_boards.get("m").map(String::as_str), Some("canonical"));
        assert_eq!(t.provider, "masjidboardlive");
        assert!(t.manual);

        let Json(t) = choose("m", "none").await.ok().expect("masjidscreen only");
        assert_eq!(app.settings().salah_boards.get("m").map(String::as_str), Some("none"));
        assert_eq!(t.provider, "masjidscreen");
        assert!(t.manual);

        let Json(t) = choose("m", " auto ").await.ok().expect("automatic");
        assert!(app.settings().salah_boards.is_empty());
        assert!(!t.manual);
    }

    #[tokio::test]
    async fn recordings_play_only_listed_items() {
        let app = app();
        cache_recordings(&app, "m");
        let Json(list) = super::recording_list(State(app.clone()), Query(MountQuery { mount: Some("m".into()) })).await.ok().expect("list");
        assert_eq!(list.items.len(), 1);

        let play = |mount: &str, id: &str| super::play(State(app.clone()), Json(PlayReq { mount: mount.into(), id: id.into() }));
        assert!(play("a/b", "r1").await.is_err());
        assert!(play("m", "unknown").await.is_err(), "only listed recordings");
        assert!(play("m", "r1").await.is_ok());

        let url = |mount: &str, id: &str| super::recording_url(State(app.clone()), Query(RecordingRef { mount: mount.into(), id: id.into() }));
        assert!(url("a/b", "r1").await.is_err());
        assert!(url("m", "unknown").await.is_err());
        let Json(v) = url("m", "r1").await.ok().expect("url");
        assert_eq!(v["url"], "https://cdn.livemasjid.com/m/esha.mp3");
        assert_eq!(v["title"], "Esha");
    }

    #[tokio::test]
    async fn play_sends_the_recording_to_the_player() {
        let dir = std::env::temp_dir().join(format!("livemasjid-extras-api-{}", crate::config::random_hex(6)));
        let mut app = test_app(&dir);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        std::sync::Arc::get_mut(&mut app).expect("sole owner").player_tx = tx;
        cache_recordings(&app, "m");
        assert!(super::play(State(app.clone()), Json(PlayReq { mount: "m".into(), id: "r1".into() })).await.is_ok());
        match rx.try_recv() {
            Ok(PlayerCmd::PlayRecording { mount, title, url, duration_s }) => {
                assert_eq!((mount.as_str(), title.as_str(), duration_s), ("m", "Esha", Some(600)));
                assert!(url.ends_with("esha.mp3"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn live_wav_streams_the_mirror() {
        use futures_util::StreamExt;
        let app = app();
        let resp = super::listen_live(State(app.clone())).await.ok().expect("stream");
        assert_eq!(resp.headers()[axum::http::header::CONTENT_TYPE], "audio/wav");
        assert_eq!(resp.headers()[axum::http::header::CACHE_CONTROL], "no-store");
        let mut body = resp.into_body().into_data_stream();
        let header = body.next().await.expect("header").expect("bytes");
        assert_eq!(&header[..4], b"RIFF");

        let _ = app.sink.mirror.send(std::sync::Arc::from(vec![1u8, 2, 3, 4]));
        let chunk = body.next().await.expect("chunk").expect("bytes");
        assert_eq!(&chunk[..], &[1, 2, 3, 4]);

        // A listener that falls behind skips ahead to what is current.
        for i in 0..100u8 {
            let _ = app.sink.mirror.send(std::sync::Arc::from(vec![i]));
        }
        let chunk = body.next().await.expect("chunk").expect("bytes");
        assert!(chunk[0] >= 36, "skipped ahead to the newest chunks, got {}", chunk[0]);
    }

    #[tokio::test]
    async fn live_wav_has_a_listener_limit_and_a_switch() {
        let app = app();
        let held: Vec<_> = (0..MAX_MIRROR_LISTENERS).map(|_| app.sink.mirror.subscribe()).collect();
        let Err(e) = super::listen_live(State(app.clone())).await else { panic!("over the limit") };
        assert!(e.1.contains("Too many"));
        drop(held);
        app.update_settings(|s| s.browser_listening = false).unwrap();
        assert!(super::listen_live(State(app.clone())).await.is_err(), "listening is off");
    }
}
