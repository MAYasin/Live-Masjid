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
    #[test]
    fn wav_header_is_well_formed() {
        let h = super::wav_header(24_000);
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 24_000);
        assert_eq!(&h[36..40], b"data");
    }
}
