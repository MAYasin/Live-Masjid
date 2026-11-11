//! The player: turns live signals and user commands into what the speaker plays.
//!
//! One task owns all playback state ([`Player`]) and reacts to five inputs: user commands, live
//! signals, events from the live session, events from on-demand playback (recordings, Quran),
//! and a 5 s tick. Live has priority: a broadcast that starts interrupts a recording or the
//! Quran, and the Quran continues once the broadcast has closed.

mod live;
mod media;

use crate::config::Settings;
use crate::quran::{PlaylistCtx, Reciter};
use crate::state::{App, Phase, PlayerCmd, QuranResume, Signal, now_ms};
use crate::stream::local::LocalParams;
use crate::stream::recording::{RecordingEvent, RecordingParams};
use crate::stream::{EndReason, Events, SessionParams, StreamEvent};
use live::{LiveSession, StopReason, Trigger, end_label};
use media::{Controls, Media, Surah, first_available};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How often the tick runs (status refresh, resume and probe checks).
const TICK: Duration = Duration::from_secs(5);
/// The Quran continues once the interrupting broadcast has looked closed this long.
const RESUME_AFTER_CLOSED: Duration = Duration::from_secs(15);
/// How long a scheduled playlist waits for a live broadcast to end.
const SCHEDULE_WAIT: Duration = Duration::from_secs(2 * 3600);
/// If "offline" is never observed after the user pressed Stop, live auto-play re-arms after this.
const SUPPRESS_MAX: Duration = Duration::from_secs(3 * 3600);
/// Both live signals down this long: try the stream directly, at most this often.
const BLIND_MS: u64 = 120_000;
const PROBE_EVERY: Duration = Duration::from_secs(120);
/// After "live" but no stream yet, wait this long before trying again.
const NOT_LIVE_RETRY: Duration = Duration::from_secs(30);

/// A scheduled playlist waiting for the live broadcast to end.
#[derive(Clone)]
struct Pending {
    ctx: PlaylistCtx,
    reciter: Reciter,
    until: Instant,
    speed: f32,
}

/// Where to start a surah.
struct QuranStart {
    reciter: Reciter,
    surah: u16,
    playlist: Option<PlaylistCtx>,
    position_ms: u64,
    paused: bool,
    live_at_start: bool,
}

struct Player {
    app: App,
    live_tx: UnboundedSender<(u64, StreamEvent)>,
    media_tx: UnboundedSender<(u64, RecordingEvent)>,
    live: Option<LiveSession>,
    media: Option<Media>,
    /// Identifies sessions, so late events from a replaced one are ignored.
    next_token: u64,
    /// The Quran, paused for a live broadcast; continues once the broadcast has closed.
    quran_resume: Option<QuranResume>,
    /// Since when the broadcast has looked closed (resume waits a little, not on a reconnect).
    closed_since: Option<Instant>,
    pending: Option<Pending>,
    /// The Quran's playback speed, shared by every surah it plays (seek/next keep it).
    qspeed: Arc<AtomicU32>,
    /// The user pressed Stop during this broadcast: don't auto-restart until it goes offline.
    suppressed_at: Option<Instant>,
    retry_after: Instant,
    last_probe: Instant,
}

pub async fn run(app: App, mut cmd_rx: UnboundedReceiver<PlayerCmd>, mut sig_rx: UnboundedReceiver<Signal>) {
    let (live_tx, mut live_rx) = unbounded_channel();
    let (media_tx, mut media_rx) = unbounded_channel();
    let mut player = Player::new(app, live_tx, media_tx);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        player.app.hb_player.store(now_ms(), Ordering::Relaxed);
        let start = tokio::select! {
            Some(cmd) = cmd_rx.recv() => player.on_command(cmd),
            Some(sig) = sig_rx.recv() => player.on_signal(sig),
            Some((token, ev)) = live_rx.recv() => player.on_live_event(token, ev),
            Some((token, ev)) = media_rx.recv() => player.on_media_event(token, ev),
            _ = tick.tick() => player.on_tick(),
        };
        if let Some(trigger) = start {
            player.start_live(trigger);
        }
    }
}

/// Waiting for a broadcast if a masjid is selected, else idle.
fn idle_phase(s: &Settings) -> Phase {
    if s.mount.is_some() { Phase::Waiting } else { Phase::Idle }
}

impl Player {
    fn new(app: App, live_tx: UnboundedSender<(u64, StreamEvent)>, media_tx: UnboundedSender<(u64, RecordingEvent)>) -> Self {
        let s = app.settings();
        app.set_phase(idle_phase(&s));
        Self {
            live_tx,
            media_tx,
            live: None,
            media: None,
            next_token: 0,
            quran_resume: None,
            closed_since: None,
            pending: None,
            qspeed: Arc::new(AtomicU32::new(s.quran_speed.to_bits())),
            suppressed_at: None,
            retry_after: Instant::now(),
            last_probe: Instant::now(),
            app,
        }
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    /// Is the selected masjid on air?
    fn live_now(&self) -> bool {
        self.app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&self.app.live.read().unwrap(), m))
    }

    /// Live is playing, or will start by itself.
    fn live_has_priority(&self, s: &Settings) -> bool {
        self.live.is_some() || (self.live_now() && s.autoplay && self.suppressed_at.is_none())
    }

    /// After on-demand playback ends: go back to the radio if the masjid is on air.
    fn should_resume_live(&self, s: &Settings) -> bool {
        s.autoplay && self.suppressed_at.is_none() && self.live.is_none() && self.live_now()
    }

    fn set_quran_speed(&self, speed: f32) {
        self.qspeed.store(speed.to_bits(), Ordering::Relaxed);
    }

    fn publish_media(&self) {
        if let Some(m) = &self.media {
            let st = m.status();
            self.app.status.send_modify(|s| s.recording = Some(st));
        }
    }

    /// Remember where the playing Quran is, to continue after live.
    fn keep_resume_point(&mut self) -> Option<QuranResume> {
        let res = self.media.as_ref()?.resume_point()?;
        self.quran_resume = Some(res.clone());
        let shown = res.clone();
        self.app.status.send_modify(|st| st.quran_resume = Some(shown));
        Some(res)
    }

    fn forget_resume_point(&mut self) {
        self.quran_resume = None;
        self.app.status.send_modify(|st| st.quran_resume = None);
    }

    fn stop_live_for_media(&mut self) {
        if let Some(a) = self.live.as_mut() {
            a.request_stop(StopReason::Recording);
        }
    }

    /// Stop on-demand playback (if any) and clear it from the status.
    fn stop_media(&mut self) {
        if let Some(m) = self.media.take() {
            m.stop();
            self.app.sink.new_epoch();
            self.app.status.send_modify(|st| st.recording = None);
        }
    }

    /// The playing surah, restarted from where it is heard.
    fn restart_point(&self) -> Option<QuranStart> {
        let m = self.media.as_ref()?;
        let s = m.surah.as_ref()?;
        Some(QuranStart {
            reciter: s.reciter.clone(),
            surah: s.number,
            playlist: s.playlist.clone(),
            position_ms: m.heard_ms(),
            paused: false,
            live_at_start: m.live_at_start,
        })
    }

    /// Start a surah (downloading it if needed); returns its title.
    fn start_quran(&mut self, q: QuranStart) -> String {
        let app = self.app.clone();
        app.quran.request(&q.reciter.key, q.surah);
        let token = self.token();
        let controls = Controls::new(q.position_ms, q.paused);
        let m = Media {
            token,
            controls: controls.clone(),
            mount: String::new(),
            title: format!("{}. {}", q.surah, crate::quran::name(q.surah)),
            duration_s: None,
            live_at_start: q.live_at_start,
            surah: Some(Surah { number: q.surah, playlist: q.playlist, reciter: q.reciter.clone(), lib: app.quran.clone() }),
            speed: self.qspeed.clone(),
            sink: app.sink.clone(),
        };
        crate::stream::source::spawn(LocalParams {
            lib: app.quran.clone(),
            reciter: q.reciter.key,
            surah: q.surah,
            start_ms: q.position_ms,
            sink: app.sink.clone(),
            epoch: app.sink.new_epoch(),
            stop: controls.stop,
            paused: controls.paused,
            position_ms: controls.position_ms,
            duration_ms: controls.duration_ms,
            speed: self.qspeed.clone(),
            events: Events::new(token, &self.media_tx),
        });
        let st = m.status();
        app.status.send_modify(|s| {
            s.recording = Some(st);
            s.quran_resume = None;
        });
        app.set_phase(Phase::Recording);
        let title = m.title.clone();
        self.media = Some(m);
        title
    }

    // ---- user commands ----

    fn on_command(&mut self, cmd: PlayerCmd) -> Option<Trigger> {
        match cmd {
            PlayerCmd::PlayNow => {
                self.suppressed_at = None;
                // Live now; the Quran (if playing) continues after the broadcast.
                self.keep_resume_point();
                self.stop_media();
                if self.live.is_none() {
                    return Some(Trigger::Manual);
                }
            }
            PlayerCmd::PlayRecording { mount, title, url, duration_s } => self.play_recording(mount, title, url, duration_s),
            PlayerCmd::PauseRecording(pause) => self.pause_media(pause),
            PlayerCmd::QuranSpeed(v) => {
                self.set_quran_speed(v.clamp(0.5, 2.0));
                self.publish_media();
            }
            PlayerCmd::PlayQuran { surah, position_ms, playlist, reciter, speed } => {
                self.set_quran_speed(speed.clamp(0.5, 2.0));
                self.stop_media();
                self.stop_live_for_media();
                self.quran_resume = None;
                self.pending = None;
                let reciter = self.app.quran.resolve(Some(&reciter));
                let label = reciter.label();
                let live_at_start = self.live_now();
                let title = self.start_quran(QuranStart { reciter, surah, playlist, position_ms, paused: false, live_at_start });
                self.app.log("info", format!("▶ Quran: {title} ({label})"));
            }
            PlayerCmd::ScheduledPlaylist(id) => self.scheduled_playlist(&id),
            PlayerCmd::QuranSeek(ms) => {
                if let Some(mut q) = self.restart_point() {
                    q.position_ms = ms;
                    q.paused = self.media.as_ref().is_some_and(|m| m.controls.is_paused());
                    self.stop_media();
                    self.start_quran(q);
                }
            }
            PlayerCmd::QuranStep(by) => {
                let next = self.media.as_ref().and_then(|m| m.neighbour(by));
                if let (Some((surah, playlist)), Some(mut q)) = (next, self.restart_point()) {
                    q.surah = surah;
                    q.playlist = playlist;
                    q.position_ms = 0;
                    self.stop_media();
                    let title = self.start_quran(q);
                    self.app.log("info", format!("▶ Quran: {title}"));
                }
            }
            PlayerCmd::StopRecording => {
                if self.quran_resume.is_some() {
                    self.forget_resume_point();
                }
                if self.media.is_some() {
                    self.stop_media();
                    self.app.log("info", "Recording stopped");
                    let s = self.app.settings();
                    self.app.set_phase(idle_phase(&s));
                    // Back to the radio: if the masjid is on air, resume live.
                    if self.should_resume_live(&s) {
                        return Some(Trigger::Poll);
                    }
                }
            }
            PlayerCmd::Stop => {
                if let Some(a) = self.live.as_mut() {
                    a.request_stop(StopReason::Stopped);
                    self.app.sink.new_epoch();
                    self.suppressed_at = Some(Instant::now());
                    self.app.set_phase(Phase::Stopped);
                    self.app.log("info", "Stopped by user (will resume next time the masjid goes live)");
                }
            }
            PlayerCmd::TestTone => {
                let sink = self.app.sink.clone();
                let epoch = sink.epoch.load(Ordering::SeqCst);
                tokio::task::spawn_blocking(move || {
                    sink.send_pcm(epoch, crate::sink::RATE, crate::sink::chime());
                });
            }
            PlayerCmd::Reconfigure => {
                let s = self.app.settings();
                if let Some(a) = self.live.as_mut()
                    && Some(&a.mount) != s.mount.as_ref() {
                        a.request_stop(StopReason::Switched);
                        self.app.sink.new_epoch();
                    }
                self.suppressed_at = None;
                self.retry_after = Instant::now();
                if self.live.is_none() {
                    self.app.set_phase(idle_phase(&s));
                }
            }
        }
        None
    }

    fn play_recording(&mut self, mount: String, title: String, url: String, duration_s: Option<u64>) {
        self.stop_media();
        self.stop_live_for_media();
        let token = self.token();
        let controls = Controls::new(0, false);
        let m = Media {
            token,
            controls: controls.clone(),
            mount,
            title: title.clone(),
            duration_s,
            live_at_start: self.live_now(),
            surah: None,
            speed: Arc::new(AtomicU32::new(1f32.to_bits())),
            sink: self.app.sink.clone(),
        };
        self.forget_resume_point();
        crate::stream::source::spawn(RecordingParams {
            url,
            sink: self.app.sink.clone(),
            epoch: self.app.sink.new_epoch(),
            stop: controls.stop,
            paused: controls.paused,
            position_ms: controls.position_ms,
            events: Events::new(token, &self.media_tx),
        });
        let st = m.status();
        self.app.status.send_modify(|s| s.recording = Some(st));
        self.app.set_phase(Phase::Recording);
        self.app.log("info", format!("▶ Playing recording: {title}"));
        self.media = Some(m);
    }

    fn pause_media(&mut self, pause: bool) {
        let quran_paused = self.media.as_ref().is_some_and(|m| m.is_quran() && m.controls.is_paused());
        if !pause && quran_paused {
            // Resume exactly where the listener stopped hearing it (the queued audio was
            // dropped on pause): a fresh start from that position, from the file.
            if let Some(q) = self.restart_point() {
                self.stop_media();
                self.start_quran(q);
            }
        } else if let Some(m) = &self.media {
            if pause && m.is_quran() && !m.controls.is_paused() {
                // Silence now rather than after the ~2 s already queued for the speaker.
                m.freeze();
                self.app.sink.new_epoch();
            } else {
                m.controls.paused.store(pause, Ordering::Relaxed);
            }
            self.publish_media();
        }
    }

    fn scheduled_playlist(&mut self, id: &str) {
        let Some(p) = self.app.quran.playlist(id) else { return };
        let s = self.app.settings();
        let reciter = self.app.quran.resolve(Some(p.reciter.as_deref().unwrap_or(&s.quran_reciter)));
        let Some(ctx) = first_available(PlaylistCtx::new(&p, 0), |n| reciter.has(n)) else { return };
        let speed = p.speed.unwrap_or(s.quran_speed);
        if self.live_has_priority(&s) {
            // Live keeps priority: wait for the broadcast to end.
            self.app.log("info", format!("\"{}\" will play when the live broadcast ends", p.name));
            self.app.status.send_modify(|st| st.quran_pending = Some(p.name.clone()));
            self.pending = Some(Pending { ctx, reciter, until: Instant::now() + SCHEDULE_WAIT, speed });
        } else {
            self.stop_media();
            self.forget_resume_point();
            self.set_quran_speed(speed);
            let surah = ctx.surah();
            let title = self.start_quran(QuranStart { reciter, surah, playlist: Some(ctx), position_ms: 0, paused: false, live_at_start: false });
            self.app.log("info", format!("▶ Scheduled: {title}"));
        }
    }

    // ---- live signals ----

    fn on_signal(&mut self, sig: Signal) -> Option<Trigger> {
        let s = self.app.settings();
        match sig {
            Signal::SseOnline | Signal::PollOnline => {
                // A broadcast that starts while a recording plays takes priority.
                if self.media.as_ref().is_some_and(|m| !m.live_at_start) && s.autoplay && self.suppressed_at.is_none() {
                    if let Some(res) = self.keep_resume_point() {
                        self.app.log("info", format!(
                            "Live broadcast started; Quran paused at {}:{:02} (continues when the broadcast ends)",
                            res.position_s / 60,
                            res.position_s % 60
                        ));
                    } else {
                        self.app.log("info", "Live broadcast started; stopping the recording");
                    }
                    self.stop_media();
                }
                let ok = s.mount.is_some()
                    && s.autoplay
                    && self.suppressed_at.is_none()
                    && self.live.is_none()
                    && self.media.is_none()
                    && Instant::now() >= self.retry_after;
                if ok {
                    return Some(if sig == Signal::SseOnline { Trigger::Sse } else { Trigger::Poll });
                }
            }
            Signal::PollOffline => {
                if let Some(m) = self.media.as_mut() {
                    m.live_at_start = false;
                }
                if self.suppressed_at.take().is_some() && self.live.is_none() {
                    self.app.set_phase(idle_phase(&s));
                }
            }
        }
        None
    }

    // ---- live session ----

    fn start_live(&mut self, trigger: Trigger) {
        let s = self.app.settings();
        let Some(mount) = s.mount.clone() else { return };
        let token = self.token();
        let stop = Arc::new(AtomicBool::new(false));
        let buffer_ms = Arc::new(AtomicU64::new(0));
        crate::stream::source::spawn(SessionParams {
            mount: mount.clone(),
            codec: s.codec.clone(),
            sink: self.app.sink.clone(),
            epoch: self.app.sink.new_epoch(),
            stop: stop.clone(),
            events: Events::new(token, &self.live_tx),
            start_grace: if trigger == Trigger::Probe { Duration::ZERO } else { crate::stream::START_GRACE },
            buffer_ms: buffer_ms.clone(),
        });
        self.live = Some(LiveSession { token, stop, mount, trigger, hist_id: None, reconnects: 0, stop_reason: None, buffer_ms });
        if trigger != Trigger::Probe {
            self.app.set_phase(Phase::Connecting);
        }
        tracing::info!("starting stream session (trigger: {})", trigger.as_str());
    }

    fn on_live_event(&mut self, token: u64, ev: StreamEvent) -> Option<Trigger> {
        let app = &self.app;
        let a = self.live.as_mut().filter(|a| a.token == token)?;
        match ev {
            // A session we've asked to stop can still report a late start; ignore it.
            StreamEvent::Started { .. } | StreamEvent::Reconnecting(_) if a.stop_reason.is_some() => {}
            StreamEvent::Started { url, codec } => {
                let name = app.settings().station_name.unwrap_or_else(|| a.mount.clone());
                if a.hist_id.is_none() {
                    let id = app.history.lock().unwrap().begin(&a.mount, &name, codec, a.trigger.as_str());
                    a.hist_id = Some(id);
                    label_salah(app.clone(), a.mount.clone(), id);
                    app.log("info", format!("▶ Playing {name} ({codec}, via {})", a.trigger.as_str()));
                } else {
                    a.reconnects += 1;
                    app.log("info", format!("Stream recovered ({codec})"));
                }
                let trigger = a.trigger.as_str();
                app.status.send_modify(|st| {
                    st.codec = Some(codec.to_string());
                    st.stream_url = Some(url);
                    st.trigger = Some(trigger.to_string());
                    st.last_heard_ms = Some(now_ms());
                });
                if self.suppressed_at.is_none() && self.media.is_none() {
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
            StreamEvent::Ended(reason) => return self.live_ended(&reason),
        }
        None
    }

    fn live_ended(&mut self, reason: &EndReason) -> Option<Trigger> {
        let a = self.live.take()?;
        let app = &self.app;
        let why = end_label(reason, a.stop_reason);
        if let Some(id) = a.hist_id {
            app.history.lock().unwrap().finish(id, why, a.reconnects);
            app.log("info", format!("■ Broadcast {why}"));
        } else if matches!(reason, EndReason::NotLive) {
            tracing::info!("live signal but no stream available yet");
            self.retry_after = Instant::now() + NOT_LIVE_RETRY;
        }
        app.status.send_modify(|st| {
            st.codec = None;
            st.stream_url = None;
            st.buffer_ms = 0;
            if let Some(m) = &st.mount {
                st.last_heard_ms = app.history.lock().unwrap().last_heard(m);
            }
        });
        let s = app.settings();
        let phase = if self.media.is_some() {
            Phase::Recording
        } else if self.suppressed_at.is_some() {
            Phase::Stopped
        } else {
            idle_phase(&s)
        };
        app.set_phase(phase);
        // A switch: start watching/playing the new mount right away if live.
        (a.stop_reason == Some(StopReason::Switched) && self.live_now()).then_some(Trigger::Poll)
    }

    // ---- on-demand playback ----

    fn on_media_event(&mut self, token: u64, ev: RecordingEvent) -> Option<Trigger> {
        let m = self.media.as_ref().filter(|m| m.token == token)?;
        match ev {
            RecordingEvent::Started => None,
            // A surah finished: continue with the next one (in the playlist; past its end only
            // with repeat).
            RecordingEvent::Ended(None) if m.neighbour(1).is_some() => {
                let m = self.media.take()?;
                let (surah, playlist) = m.neighbour(1)?;
                let reciter = m.surah?.reciter;
                let title = self.start_quran(QuranStart { reciter, surah, playlist, position_ms: 0, paused: false, live_at_start: m.live_at_start });
                self.app.log("info", format!("▶ Quran: {title}"));
                None
            }
            RecordingEvent::Ended(err) => {
                match err {
                    Some(e) => self.app.log("warn", format!("Recording could not be played: {e}")),
                    None => self.app.log("info", "Recording finished"),
                }
                self.media = None;
                self.app.status.send_modify(|st| st.recording = None);
                let s = self.app.settings();
                self.app.set_phase(idle_phase(&s));
                self.should_resume_live(&s).then_some(Trigger::Poll)
            }
        }
    }

    // ---- periodic checks ----

    fn on_tick(&mut self) -> Option<Trigger> {
        self.publish_media();
        self.run_pending_playlist();
        self.resume_quran_when_closed();
        if let Some(a) = &self.live {
            let b = a.buffer_ms.load(Ordering::Relaxed);
            self.app.status.send_if_modified(|st| {
                let changed = st.buffer_ms / 1000 != b / 1000;
                st.buffer_ms = b;
                changed
            });
            if let Some(id) = a.hist_id {
                self.app.history.lock().unwrap().touch(id, a.reconnects);
                self.app.status.send_modify(|st| st.last_heard_ms = Some(now_ms()));
            }
        }
        // Suppression safety valve: if offline was never observed, re-arm.
        if self.suppressed_at.is_some_and(|t| t.elapsed() > SUPPRESS_MAX) && self.live.is_none() {
            self.suppressed_at = None;
            self.app.set_phase(Phase::Waiting);
        }
        self.probe_due().then(|| {
            self.last_probe = Instant::now();
            Trigger::Probe
        })
    }

    /// A scheduled playlist that waited for the live broadcast: start once it's over.
    fn run_pending_playlist(&mut self) {
        let Some(p) = self.pending.clone() else { return };
        let s = self.app.settings();
        if Instant::now() > p.until {
            self.app.log("info", format!("Skipped \"{}\": the live broadcast lasted more than 2 hours", p.ctx.name));
            self.pending = None;
            self.app.status.send_modify(|st| st.quran_pending = None);
        } else if !self.live_has_priority(&s) {
            self.pending = None;
            self.stop_media();
            self.quran_resume = None;
            self.app.status.send_modify(|st| {
                st.quran_pending = None;
                st.quran_resume = None;
            });
            self.set_quran_speed(p.speed);
            let surah = p.ctx.surah();
            let q = QuranStart { reciter: p.reciter, surah, playlist: Some(p.ctx), position_ms: 0, paused: false, live_at_start: false };
            let title = self.start_quran(q);
            self.app.log("info", format!("▶ Scheduled (after the broadcast): {title}"));
        }
    }

    /// Continue the Quran once the broadcast that interrupted it has *closed*: no session (a
    /// reconnect keeps its session) and not live (or stopped by the user), for a while.
    fn resume_quran_when_closed(&mut self) {
        let closed = self.live.is_none() && self.media.is_none() && (self.suppressed_at.is_some() || !self.live_now());
        if !(closed && self.quran_resume.as_ref().is_some_and(|q| q.auto)) {
            self.closed_since = None;
            return;
        }
        let since = *self.closed_since.get_or_insert_with(Instant::now);
        if since.elapsed() < RESUME_AFTER_CLOSED {
            return;
        }
        let Some(q) = self.quran_resume.take() else { return };
        let reciter = self.app.quran.resolve(Some(&q.reciter));
        self.set_quran_speed(q.speed);
        let start = QuranStart { reciter, surah: q.surah, playlist: q.playlist, position_ms: q.position_s * 1000, paused: false, live_at_start: false };
        let title = self.start_quran(start);
        self.app.log("info", format!("Broadcast over; continuing the Quran: {title}"));
        self.closed_since = None;
    }

    /// Layer 3: both signal sources down for a while -> probe the stream directly, now and then.
    fn probe_due(&self) -> bool {
        let s = self.app.settings();
        let (blind, up_long_enough) = {
            let st = self.app.status.borrow();
            (
                !st.signals.poll_ok && now_ms().saturating_sub(st.signals.poll_last_ms) > BLIND_MS,
                now_ms().saturating_sub(st.started_ms) > BLIND_MS,
            )
        };
        blind
            && up_long_enough
            && s.autoplay
            && s.mount.is_some()
            && self.live.is_none()
            && self.suppressed_at.is_none()
            && self.last_probe.elapsed() > PROBE_EVERY
    }
}

/// Label a new history entry with the salah it is for, from today's times (cached; off the loop).
fn label_salah(app: App, mount: String, id: u64) {
    tokio::spawn(async move {
        let times = crate::integrations::salah::get(&app, &mount).await;
        if times.status == "ok" {
            let (weekday, minute) = crate::salah_match::local_day_minute(now_ms(), crate::salah_match::utc_offset_s());
            let label = crate::salah_match::salah_for(&times, weekday, minute);
            app.history.lock().unwrap().set_salah(id, label);
        }
    });
}
