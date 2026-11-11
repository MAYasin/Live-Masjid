//! The player: turns live signals + user commands into listening sessions.

use crate::state::{App, Phase, PlayerCmd, Signal, now_ms};
use crate::stream::{EndReason, SessionParams, StreamEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

struct Active {
    token: u64,
    stop: Arc<AtomicBool>,
    mount: String,
    trigger: &'static str,
    hist_id: Option<u64>,
    reconnects: u32,
    stop_reason: Option<&'static str>,
    buffer_ms: Arc<std::sync::atomic::AtomicU64>,
}

/// A past recording playing on the speaker.
struct Rec {
    token: u64,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    position_ms: Arc<std::sync::atomic::AtomicU64>,
    mount: String,
    title: String,
    duration_s: Option<u64>,
    /// Was the selected masjid already live when the recording started? Only a broadcast
    /// that starts *during* the recording interrupts it.
    live_at_start: bool,
    /// Quran: the surah playing (a recording otherwise).
    surah: Option<u16>,
    /// Quran: length of the surah, once known.
    duration_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Quran: the playlist this surah belongs to.
    playlist: Option<crate::quran::PlaylistCtx>,
    /// Quran: the library (for download progress).
    lib: Option<Arc<crate::quran::Library>>,
    /// Quran: the recitation.
    reciter: Option<crate::quran::Reciter>,
    /// The output, to tell how much decoded audio hasn't been heard yet.
    sink: crate::sink::SinkHandle,
    /// Quran: playback speed (f32 bits), shared with the decoder.
    speed: Arc<std::sync::atomic::AtomicU32>,
}

fn speed_of(r: &Rec) -> f32 {
    f32::from_bits(r.speed.load(Ordering::Relaxed))
}

/// Audio between the sink queue and the listener's ears: writer queue, pipe, aplay's 250 ms
/// buffer and Bluetooth (about 0.7 s, see the browser-sync notes).
const OUTPUT_LAG_MS: u64 = 700;

/// Position the listener is hearing: decoded audio minus what is still queued for the speaker.
fn heard_ms(r: &Rec) -> u64 {
    let decoded = r.position_ms.load(Ordering::Relaxed);
    if r.paused.load(Ordering::Relaxed) {
        return decoded; // set to the heard position when paused
    }
    // What is queued plays at the current speed, so it covers that much more recitation.
    let queued = ((r.sink.queued_ms() + OUTPUT_LAG_MS) as f32 * speed_of(r)) as u64;
    decoded.saturating_sub(queued)
}

/// The surah before/after (`by` = -1/+1) the playing one: within its playlist, else in
/// Quran order. None at the end (a playlist without repeat, or An-Nās).
/// Surahs the reciter doesn't have are skipped.
fn neighbour(r: &Rec, by: i32) -> Option<(u16, Option<crate::quran::PlaylistCtx>)> {
    let has = |n: u16| r.reciter.as_ref().is_none_or(|x| x.has(n));
    match &r.playlist {
        Some(p) => {
            let mut q = p.step(by)?;
            for _ in 0..p.surahs.len() {
                if has(q.surah()) {
                    return Some((q.surah(), Some(q)));
                }
                let next = q.step(if by < 0 { -1 } else { 1 })?;
                if next.index == q.index {
                    return None;
                }
                q = next;
            }
            None
        }
        None => {
            let mut n = i32::from(r.surah?) + by;
            while (1..=114).contains(&n) {
                if has(n as u16) {
                    return Some((n as u16, None));
                }
                n += by.signum();
            }
            None
        }
    }
}

fn recording_status(r: &Rec) -> crate::state::RecordingStatus {
    let known = r.duration_ms.load(Ordering::Relaxed);
    crate::state::RecordingStatus {
        mount: r.mount.clone(),
        title: r.title.clone(),
        position_s: heard_ms(r) / 1000,
        duration_s: r.duration_s.or((known > 0).then_some(known / 1000)),
        paused: r.paused.load(Ordering::Relaxed),
        kind: if r.surah.is_some() { "quran" } else { "recording" },
        surah: r.surah,
        playlist: r.playlist.clone(),
        loaded_pct: r.surah.and_then(|n| r.lib.as_ref()?.progress(&r.reciter.as_ref()?.key, n)),
        reciter: r.reciter.as_ref().map(|x| x.label()),
        reciter_key: r.reciter.as_ref().map(|x| x.key.clone()),
        speed: speed_of(r),
    }
}

/// Start playing Quran surah `n` from `position_ms` (downloading it if needed).
fn start_quran(
    app: &App,
    speed: &Arc<std::sync::atomic::AtomicU32>,
    reciter: crate::quran::Reciter,
    n: u16,
    playlist: Option<crate::quran::PlaylistCtx>,
    position_ms: u64,
    paused: bool,
    live_at_start: bool,
    token: u64,
    rec_tx: &tokio::sync::mpsc::UnboundedSender<(u64, crate::stream::recording::RecordingEvent)>,
) -> Rec {
    app.quran.request(&reciter.key, n);
    let title = format!("{}. {}", n, crate::quran::name(n));
    let r = Rec {
        token,
        stop: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(paused)),
        position_ms: Arc::new(std::sync::atomic::AtomicU64::new(position_ms)),
        mount: String::new(),
        title,
        duration_s: None,
        live_at_start,
        surah: Some(n),
        duration_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        playlist,
        lib: Some(app.quran.clone()),
        reciter: Some(reciter.clone()),
        sink: app.sink.clone(),
        speed: speed.clone(),
    };
    let (tx, mut rx) = unbounded_channel();
    let fwd = rec_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if fwd.send((token, ev)).is_err() {
                break;
            }
        }
    });
    let params = crate::stream::local::LocalParams {
        lib: app.quran.clone(),
        reciter: reciter.key.clone(),
        surah: n,
        start_ms: position_ms,
        sink: app.sink.clone(),
        epoch: app.sink.new_epoch(),
        stop: r.stop.clone(),
        paused: r.paused.clone(),
        position_ms: r.position_ms.clone(),
        duration_ms: r.duration_ms.clone(),
        speed: speed.clone(),
        events: tx,
    };
    std::thread::Builder::new()
        .name("quran".into())
        .spawn(move || crate::stream::local::run(params))
        .expect("spawn quran thread");
    app.status.send_modify(|st| {
        st.recording = Some(recording_status(&r));
        st.quran_resume = None;
    });
    app.set_phase(Phase::Recording);
    r
}

/// Remember where the Quran was, to continue after a live broadcast.
fn quran_resume_of(r: &Rec) -> Option<crate::state::QuranResume> {
    Some(crate::state::QuranResume {
        surah: r.surah?,
        title: r.title.clone(),
        position_s: heard_ms(r) / 1000,
        auto: !r.paused.load(Ordering::Relaxed),
        playlist: r.playlist.clone(),
        reciter: r.reciter.as_ref()?.key.clone(),
        speed: speed_of(r),
    })
}

/// The playlist entry to start from: the first one (from `ctx`) the reciter has.
fn first_available(mut ctx: crate::quran::PlaylistCtx, reciter: &crate::quran::Reciter) -> Option<crate::quran::PlaylistCtx> {
    for _ in 0..ctx.surahs.len() {
        if reciter.has(ctx.surah()) {
            return Some(ctx);
        }
        let next = ctx.step(1)?;
        ctx = next;
    }
    None
}

fn is_live_now(app: &App) -> bool {
    app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m))
}

/// Stop a recording (if any) and clear it from the status.
fn stop_recording(app: &App, rec: &mut Option<Rec>) {
    if let Some(r) = rec.take() {
        r.stop.store(true, Ordering::Relaxed);
        r.paused.store(false, Ordering::Relaxed);
        app.sink.new_epoch();
        app.status.send_modify(|st| st.recording = None);
    }
}

pub async fn run(app: App, mut cmd_rx: UnboundedReceiver<PlayerCmd>, mut sig_rx: UnboundedReceiver<Signal>) {
    let (ev_tx, mut ev_rx) = unbounded_channel::<(u64, StreamEvent)>();
    let (rec_tx, mut rec_rx) = unbounded_channel::<(u64, crate::stream::recording::RecordingEvent)>();
    let mut rec: Option<Rec> = None;
    let mut active: Option<Active> = None;
    let mut next_token = 0u64;
    // The Quran, paused for a live broadcast; continues once the broadcast has closed.
    let mut quran_resume: Option<crate::state::QuranResume> = None;
    // Since when the broadcast has looked closed (resume waits a little, not on a reconnect).
    let mut closed_since: Option<Instant> = None;
    // A scheduled playlist waiting for the live broadcast to end (and until when it may wait).
    let mut pending: Option<(crate::quran::PlaylistCtx, crate::quran::Reciter, Instant, f32)> = None;
    // The Quran's playback speed, shared by every surah it plays (seek/next keep it).
    let qspeed = Arc::new(std::sync::atomic::AtomicU32::new(app.settings().quran_speed.to_bits()));
    // User pressed stop during this broadcast: don't auto-restart until it goes offline.
    let mut suppressed_at: Option<Instant> = None;
    let mut retry_after = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut last_probe = Instant::now();
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let initial = if app.settings().mount.is_some() { Phase::Waiting } else { Phase::Idle };
    app.set_phase(initial);

    loop {
        app.hb_player.store(now_ms(), Ordering::Relaxed);
        let mut start: Option<&'static str> = None;
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => match cmd {
                PlayerCmd::PlayNow => {
                    suppressed_at = None;
                    // Live now; the Quran (if playing) continues after the broadcast.
                    if let Some(res) = rec.as_ref().and_then(quran_resume_of) {
                        quran_resume = Some(res.clone());
                        app.status.send_modify(|st| st.quran_resume = Some(res));
                    }
                    stop_recording(&app, &mut rec);
                    if active.is_none() { start = Some("manual"); }
                }
                PlayerCmd::PlayRecording { mount, title, url, duration_s } => {
                    stop_recording(&app, &mut rec);
                    if let Some(a) = active.as_mut() {
                        a.stop_reason = Some("recording");
                        a.stop.store(true, Ordering::Relaxed);
                    }
                    next_token += 1;
                    let token = next_token;
                    let r = Rec {
                        token,
                        stop: Arc::new(AtomicBool::new(false)),
                        paused: Arc::new(AtomicBool::new(false)),
                        position_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                        mount,
                        title: title.clone(),
                        duration_s,
                        live_at_start: is_live_now(&app),
                        surah: None,
                        playlist: None,
                        lib: None,
                        reciter: None,
                        sink: app.sink.clone(),
                        speed: Arc::new(std::sync::atomic::AtomicU32::new(1f32.to_bits())),
                        duration_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    };
                    quran_resume = None;
                    app.status.send_modify(|st| st.quran_resume = None);
                    let (tx, mut rx) = unbounded_channel();
                    let fwd = rec_tx.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = rx.recv().await {
                            if fwd.send((token, ev)).is_err() { break; }
                        }
                    });
                    let params = crate::stream::recording::RecordingParams {
                        url,
                        sink: app.sink.clone(),
                        epoch: app.sink.new_epoch(),
                        stop: r.stop.clone(),
                        paused: r.paused.clone(),
                        position_ms: r.position_ms.clone(),
                        events: tx,
                    };
                    std::thread::Builder::new()
                        .name("recording".into())
                        .spawn(move || crate::stream::recording::run(params))
                        .expect("spawn recording thread");
                    app.status.send_modify(|st| st.recording = Some(recording_status(&r)));
                    app.set_phase(Phase::Recording);
                    app.log("info", format!("▶ Playing recording: {title}"));
                    rec = Some(r);
                }
                PlayerCmd::PauseRecording(pause) => {
                    let quran_paused = rec.as_ref().is_some_and(|r| r.surah.is_some() && r.paused.load(Ordering::Relaxed));
                    if !pause && quran_paused {
                        // Resume exactly where the listener stopped hearing it (the queued audio
                        // was dropped on pause): a fresh start from that position, from the file.
                        if let Some((n, pl, rc, live, pos)) = rec.as_ref().and_then(|r| Some((r.surah?, r.playlist.clone(), r.reciter.clone()?, r.live_at_start, heard_ms(r)))) {
                            stop_recording(&app, &mut rec);
                            next_token += 1;
                            rec = Some(start_quran(&app, &qspeed, rc, n, pl, pos, false, live, next_token, &rec_tx));
                        }
                    } else if let Some(r) = &rec {
                        if pause && r.surah.is_some() && !r.paused.load(Ordering::Relaxed) {
                            // Silence now rather than after the ~2 s already queued for the speaker.
                            let heard = heard_ms(r);
                            r.paused.store(true, Ordering::Relaxed);
                            r.position_ms.store(heard, Ordering::Relaxed);
                            app.sink.new_epoch();
                        } else {
                            r.paused.store(pause, Ordering::Relaxed);
                        }
                        app.status.send_modify(|st| st.recording = Some(recording_status(r)));
                    }
                }
                PlayerCmd::QuranSpeed(v) => {
                    qspeed.store(v.clamp(0.5, 2.0).to_bits(), Ordering::Relaxed);
                    if let Some(r) = &rec {
                        app.status.send_modify(|st| st.recording = Some(recording_status(r)));
                    }
                }
                PlayerCmd::PlayQuran { surah, position_ms, playlist, reciter, speed } => {
                    qspeed.store(speed.clamp(0.5, 2.0).to_bits(), Ordering::Relaxed);
                    stop_recording(&app, &mut rec);
                    if let Some(a) = active.as_mut() {
                        a.stop_reason = Some("recording");
                        a.stop.store(true, Ordering::Relaxed);
                    }
                    quran_resume = None;
                    pending = None;
                    let reciter = app.quran.resolve(Some(&reciter));
                    next_token += 1;
                    let r = start_quran(&app, &qspeed, reciter.clone(), surah, playlist, position_ms, false, is_live_now(&app), next_token, &rec_tx);
                    app.log("info", format!("▶ Quran: {} ({})", r.title, reciter.label()));
                    rec = Some(r);
                }
                PlayerCmd::ScheduledPlaylist(id) => {
                    if let Some(p) = app.quran.playlist(&id) {
                        let s = app.settings();
                        let reciter = app.quran.resolve(Some(p.reciter.as_deref().unwrap_or(&s.quran_reciter)));
                        if let Some(ctx) = first_available(crate::quran::PlaylistCtx::new(&p, 0), &reciter) {
                            // Live keeps priority: wait (up to 2 h) for the broadcast to end.
                            let live = active.is_some() || (is_live_now(&app) && s.autoplay && suppressed_at.is_none());
                            if live {
                                app.log("info", format!("\"{}\" will play when the live broadcast ends", p.name));
                                app.status.send_modify(|st| st.quran_pending = Some(p.name.clone()));
                                pending = Some((ctx, reciter, Instant::now() + Duration::from_secs(2 * 3600), p.speed.unwrap_or(s.quran_speed)));
                            } else {
                                stop_recording(&app, &mut rec);
                                quran_resume = None;
                                app.status.send_modify(|st| st.quran_resume = None);
                                qspeed.store(p.speed.unwrap_or(s.quran_speed).to_bits(), Ordering::Relaxed);
                                next_token += 1;
                                let r = start_quran(&app, &qspeed, reciter, ctx.surah(), Some(ctx), 0, false, false, next_token, &rec_tx);
                                app.log("info", format!("▶ Scheduled: {}", r.title));
                                rec = Some(r);
                            }
                        }
                    }
                }
                PlayerCmd::QuranSeek(ms) => {
                    if let Some((n, paused, live, pl, rc)) = rec.as_ref().and_then(|r| Some((r.surah?, r.paused.load(Ordering::Relaxed), r.live_at_start, r.playlist.clone(), r.reciter.clone()?))) {
                        stop_recording(&app, &mut rec);
                        next_token += 1;
                        rec = Some(start_quran(&app, &qspeed, rc, n, pl, ms, paused, live, next_token, &rec_tx));
                    }
                }
                PlayerCmd::QuranStep(by) => {
                    if let Some(((next, pl), live, rc)) = rec.as_ref().and_then(|r| Some((neighbour(r, by)?, r.live_at_start, r.reciter.clone()?))) {
                        stop_recording(&app, &mut rec);
                        next_token += 1;
                        let r = start_quran(&app, &qspeed, rc, next, pl, 0, false, live, next_token, &rec_tx);
                        app.log("info", format!("▶ Quran: {}", r.title));
                        rec = Some(r);
                    }
                }
                PlayerCmd::StopRecording => {
                    if quran_resume.take().is_some() {
                        app.status.send_modify(|st| st.quran_resume = None);
                    }
                    if rec.is_some() {
                        stop_recording(&app, &mut rec);
                        app.log("info", "Recording stopped");
                        let s = app.settings();
                        app.set_phase(if s.mount.is_some() { Phase::Waiting } else { Phase::Idle });
                        // Back to the radio: if the masjid is on air, resume live.
                        if s.autoplay && suppressed_at.is_none() && active.is_none()
                            && s.mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m)) {
                            start = Some("poll");
                        }
                    }
                }
                PlayerCmd::Stop => {
                    if let Some(a) = active.as_mut() {
                        a.stop_reason = Some("stopped");
                        a.stop.store(true, Ordering::Relaxed);
                        app.sink.new_epoch();
                        suppressed_at = Some(Instant::now());
                        app.set_phase(Phase::Stopped);
                        app.log("info", "Stopped by user (will resume next time the masjid goes live)");
                    }
                }
                PlayerCmd::TestTone => {
                    let sink = app.sink.clone();
                    let epoch = sink.epoch.load(Ordering::SeqCst);
                    tokio::task::spawn_blocking(move || {
                        sink.send_pcm(epoch, crate::sink::RATE, crate::sink::chime());
                    });
                }
                PlayerCmd::Reconfigure => {
                    let mount = app.settings().mount.clone();
                    if let Some(a) = active.as_mut() {
                        if Some(&a.mount) != mount.as_ref() {
                            a.stop_reason = Some("switched");
                            a.stop.store(true, Ordering::Relaxed);
                            app.sink.new_epoch();
                        }
                    }
                    suppressed_at = None;
                    retry_after = Instant::now();
                    if active.is_none() {
                        app.set_phase(if mount.is_some() { Phase::Waiting } else { Phase::Idle });
                    }
                }
            },
            Some(sig) = sig_rx.recv() => {
                let s = app.settings();
                match sig {
                    Signal::SseOnline | Signal::PollOnline => {
                        // A broadcast that starts while a recording plays takes priority.
                        if rec.as_ref().is_some_and(|r| !r.live_at_start) && s.autoplay && suppressed_at.is_none() {
                            if let Some(res) = rec.as_ref().and_then(quran_resume_of) {
                                app.log("info", format!("Live broadcast started; Quran paused at {}:{:02} (continues when the broadcast ends)",
                                    res.position_s / 60, res.position_s % 60));
                                quran_resume = Some(res.clone());
                                app.status.send_modify(|st| st.quran_resume = Some(res));
                            } else {
                                app.log("info", "Live broadcast started; stopping the recording");
                            }
                            stop_recording(&app, &mut rec);
                        }
                        let ok = s.mount.is_some() && s.autoplay && suppressed_at.is_none()
                            && active.is_none() && rec.is_none() && Instant::now() >= retry_after;
                        if ok {
                            start = Some(if sig == Signal::SseOnline { "sse" } else { "poll" });
                        }
                    }
                    Signal::PollOffline => {
                        if let Some(r) = rec.as_mut() { r.live_at_start = false; }
                        if suppressed_at.take().is_some() && active.is_none() {
                            app.set_phase(if s.mount.is_some() { Phase::Waiting } else { Phase::Idle });
                        }
                    }
                }
            },
            Some((tok, ev)) = ev_rx.recv() => {
                let Some(a) = active.as_mut().filter(|a| a.token == tok) else { continue };
                match ev {
                    // A session we've asked to stop can still report a late start; ignore it.
                    StreamEvent::Started { .. } | StreamEvent::Reconnecting(_) if a.stop_reason.is_some() => {}
                    StreamEvent::Started { url, codec } => {
                        let name = app.settings().station_name.unwrap_or_else(|| a.mount.clone());
                        if a.hist_id.is_none() {
                            let id = app.history.lock().unwrap().begin(&a.mount, &name, codec, a.trigger);
                            a.hist_id = Some(id);
                            // Which salah this broadcast is for, from today's times (cached; off the loop).
                            let (app2, mount) = (app.clone(), a.mount.clone());
                            tokio::spawn(async move {
                                let times = crate::integrations::salah::get(&app2, &mount).await;
                                if times.status == "ok" {
                                    let (weekday, minute) = crate::salah_match::local_day_minute(now_ms(), crate::salah_match::utc_offset_s());
                                    let label = crate::salah_match::salah_for(&times, weekday, minute);
                                    app2.history.lock().unwrap().set_salah(id, label);
                                }
                            });
                            app.log("info", format!("▶ Playing {name} ({codec}, via {})", a.trigger));
                        } else {
                            a.reconnects += 1;
                            app.log("info", format!("Stream recovered ({codec})"));
                        }
                        app.status.send_modify(|st| {
                            st.codec = Some(codec.to_string());
                            st.stream_url = Some(url);
                            st.trigger = Some(a.trigger.to_string());
                            st.last_heard_ms = Some(now_ms());
                        });
                        if suppressed_at.is_none() && rec.is_none() {
                            app.set_phase(Phase::Playing);
                        }
                    }
                    StreamEvent::CaughtUp(secs) => {
                        app.log("info", format!("Skipped {secs} s ahead to stay near live (after a network drop-out or slow output)"));
                    }
                    StreamEvent::Reconnecting(msg) => {
                        app.log("warn", format!("Stream interrupted ({msg}); reconnecting"));
                        app.set_phase(Phase::Reconnecting);
                    }
                    StreamEvent::Ended(reason) => {
                        let a = active.take().unwrap();
                        let why = match (&reason, a.stop_reason) {
                            (_, Some(r)) => r,
                            (EndReason::Finished, None) => "ended",
                            (EndReason::Stopped, None) => "stopped",
                            (EndReason::NotLive, None) => "notlive",
                        };
                        if let Some(id) = a.hist_id {
                            app.history.lock().unwrap().finish(id, why, a.reconnects);
                            app.log("info", format!("■ Broadcast {why}"));
                        } else if matches!(reason, EndReason::NotLive) {
                            tracing::info!("live signal but no stream available yet");
                            retry_after = Instant::now() + Duration::from_secs(30);
                        }
                        app.status.send_modify(|st| {
                            st.codec = None;
                            st.stream_url = None;
                            st.buffer_ms = 0;
                            if let Some(m) = &st.mount { st.last_heard_ms = app.history.lock().unwrap().last_heard(m); }
                        });
                        let mount = app.settings().mount;
                        let phase = if rec.is_some() { Phase::Recording }
                            else if suppressed_at.is_some() { Phase::Stopped }
                            else if mount.is_some() { Phase::Waiting } else { Phase::Idle };
                        app.set_phase(phase);
                        // A switch: start watching/playing the new mount right away if live.
                        if a.stop_reason == Some("switched") && mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m)) {
                            start = Some("poll");
                        }
                    }
                }
            },
            Some((tok, ev)) = rec_rx.recv() => {
                if rec.as_ref().is_some_and(|r| r.token == tok) {
                    use crate::stream::recording::RecordingEvent;
                    match ev {
                        RecordingEvent::Started => {}
                        // Quran surah finished: continue with the next one (the playlist).
                        // (Next in the playlist; past its end only with repeat.)
                        RecordingEvent::Ended(None) if rec.as_ref().and_then(|r| neighbour(r, 1)).is_some() => {
                            let r = rec.take().unwrap();
                            let (next, pl) = neighbour(&r, 1).unwrap();
                            let rc = r.reciter.clone().unwrap_or_else(|| app.quran.resolve(None));
                            next_token += 1;
                            let r = start_quran(&app, &qspeed, rc, next, pl, 0, false, r.live_at_start, next_token, &rec_tx);
                            app.log("info", format!("▶ Quran: {}", r.title));
                            rec = Some(r);
                        }
                        RecordingEvent::Ended(err) => {
                            match err {
                                Some(e) => app.log("warn", format!("Recording could not be played: {e}")),
                                None => app.log("info", "Recording finished"),
                            }
                            rec = None;
                            app.status.send_modify(|st| st.recording = None);
                            let s = app.settings();
                            app.set_phase(if s.mount.is_some() { Phase::Waiting } else { Phase::Idle });
                            if s.autoplay && suppressed_at.is_none() && active.is_none()
                                && s.mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m)) {
                                start = Some("poll");
                            }
                        }
                    }
                }
            },
            _ = tick.tick() => {
                if let Some(r) = &rec {
                    let st = recording_status(r);
                    app.status.send_modify(|s| s.recording = Some(st));
                }
                // A scheduled playlist that waited for the live broadcast: start once it's over.
                if let Some((ctx, rc, until, speed)) = pending.clone() {
                    let s = app.settings();
                    let live = active.is_some() || (is_live_now(&app) && s.autoplay && suppressed_at.is_none());
                    if Instant::now() > until {
                        app.log("info", format!("Skipped \"{}\": the live broadcast lasted more than 2 hours", ctx.name));
                        pending = None;
                        app.status.send_modify(|st| st.quran_pending = None);
                    } else if !live {
                        pending = None;
                        stop_recording(&app, &mut rec);
                        quran_resume = None;
                        app.status.send_modify(|st| { st.quran_pending = None; st.quran_resume = None; });
                        qspeed.store(speed.to_bits(), Ordering::Relaxed);
                        next_token += 1;
                        let r = start_quran(&app, &qspeed, rc, ctx.surah(), Some(ctx), 0, false, false, next_token, &rec_tx);
                        app.log("info", format!("▶ Scheduled (after the broadcast): {}", r.title));
                        rec = Some(r);
                    }
                }
                // Continue the Quran once the broadcast that interrupted it has *closed*: no
                // session (a reconnect keeps its session) and not live (or stopped by the
                // user), for 15 s in a row.
                let closed = active.is_none() && rec.is_none() && (suppressed_at.is_some() || !is_live_now(&app));
                if quran_resume.as_ref().is_some_and(|q| q.auto) && closed {
                    let since = *closed_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(15) {
                        let q = quran_resume.take().unwrap();
                        next_token += 1;
                        let rc = app.quran.resolve(Some(&q.reciter));
                        qspeed.store(q.speed.to_bits(), Ordering::Relaxed);
                        let r = start_quran(&app, &qspeed, rc, q.surah, q.playlist, q.position_s * 1000, false, false, next_token, &rec_tx);
                        app.log("info", format!("Broadcast over; continuing the Quran: {}", r.title));
                        rec = Some(r);
                        closed_since = None;
                    }
                } else {
                    closed_since = None;
                }
                if let Some(a) = &active {
                    let b = a.buffer_ms.load(Ordering::Relaxed);
                    app.status.send_if_modified(|st| { let changed = st.buffer_ms / 1000 != b / 1000; st.buffer_ms = b; changed });
                    if let Some(id) = a.hist_id {
                        app.history.lock().unwrap().touch(id, a.reconnects);
                        app.status.send_modify(|st| st.last_heard_ms = Some(now_ms()));
                    }
                }
                // Suppression safety valve: if offline was never observed, re-arm after 3 h.
                if suppressed_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3 * 3600)) && active.is_none() {
                    suppressed_at = None;
                    app.set_phase(Phase::Waiting);
                }
                // Layer 3: both signal sources down for 2 min -> probe the stream directly every 2 min.
                let s = app.settings();
                let sig = app.status.borrow().signals.clone();
                let blind = !sig.poll_ok && now_ms().saturating_sub(sig.poll_last_ms) > 120_000
                    && now_ms().saturating_sub(app.status.borrow().started_ms) > 120_000;
                if blind && s.autoplay && s.mount.is_some() && active.is_none() && suppressed_at.is_none()
                    && last_probe.elapsed() > Duration::from_secs(120) {
                    last_probe = Instant::now();
                    start = Some("probe");
                }
            }
        }

        if let Some(trigger) = start {
            let s = app.settings();
            let Some(mount) = s.mount.clone() else { continue };
            next_token += 1;
            let token = next_token;
            let stop = Arc::new(AtomicBool::new(false));
            let buffer_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let epoch = app.sink.new_epoch();
            let (tx, mut rx) = unbounded_channel();
            let fwd = ev_tx.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if fwd.send((token, ev)).is_err() {
                        break;
                    }
                }
            });
            let params = SessionParams {
                mount: mount.clone(),
                codec: s.codec.clone(),
                sink: app.sink.clone(),
                epoch,
                stop: stop.clone(),
                events: tx,
                start_grace: if trigger == "probe" { Duration::ZERO } else { crate::stream::START_GRACE },
                buffer_ms: buffer_ms.clone(),
            };
            std::thread::Builder::new()
                .name("stream".into())
                .spawn(move || crate::stream::run_session(params))
                .expect("spawn stream thread");
            active = Some(Active { token, stop, mount, trigger, hist_id: None, reconnects: 0, stop_reason: None, buffer_ms });
            if trigger != "probe" {
                app.set_phase(Phase::Connecting);
            }
            tracing::info!("starting stream session (trigger: {trigger})");
        }
    }
}
