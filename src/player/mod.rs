//! The player: turns live signals and user commands into what the speaker plays.
//!
//! One task owns all playback state ([`Player`]) and reacts to five inputs: user commands, live
//! signals, events from the live session, events from on-demand playback (recordings, Quran),
//! and a 5 s tick. Live has priority: a broadcast that starts interrupts a recording or the
//! Quran, and the Quran continues once the broadcast has closed.

mod live;
mod media;

use crate::sync::{LockExt, RwLockExt};
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
    /// Starts a live session's thread (tests replace it, so they never reach the network).
    spawn_live: fn(SessionParams),
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
            spawn_live: crate::stream::source::spawn::<SessionParams>,
            app,
        }
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    /// Is the selected masjid on air?
    fn live_now(&self) -> bool {
        self.app.settings().mount.as_deref().is_some_and(|m| crate::monitor::is_live(&self.app.live.read_lock(), m))
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
            title,
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
        self.app.log("info", format!("▶ Playing recording: {}", m.title));
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
            Signal::PollOnline => {
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
                    return Some(Trigger::Poll);
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
        (self.spawn_live)(SessionParams {
            mount: mount.clone(),
            codec: s.codec,
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
                    let id = app.history.locked().begin(&a.mount, &name, codec, a.trigger.as_str());
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
            app.history.locked().finish(id, why, a.reconnects);
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
                st.last_heard_ms = app.history.locked().last_heard(m);
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
                self.app.history.locked().touch(id, a.reconnects);
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
            app.history.locked().set_salah(id, label);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::live::{LiveSession, StopReason};
    use super::*;
    use crate::state::{LiveInfo, test_app};

    /// A temp data folder, removed when the test ends.
    struct Dir(std::path::PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fx() -> (App, Dir) {
        let dir = std::env::temp_dir().join(format!("livemasjid-player-{}", crate::config::random_hex(6)));
        (test_app(&dir), Dir(dir))
    }

    /// A player whose live sessions never start a thread (no network in tests).
    fn player(app: &App) -> Player {
        let (ltx, _lr) = unbounded_channel();
        let (mtx, _mr) = unbounded_channel();
        let mut p = Player::new(app.clone(), ltx, mtx);
        p.spawn_live = |_| {};
        p
    }

    fn logged(app: &App, needle: &str) -> bool {
        app.logs.locked().iter().any(|l| l.msg.contains(needle))
    }

    fn phase(app: &App) -> Phase {
        app.status.borrow().phase
    }

    fn not_live(app: &App) {
        app.live.write_lock().clear();
    }

    fn play_surah(p: &mut Player, surah: u16) {
        p.on_command(PlayerCmd::PlayQuran { surah, position_ms: 0, playlist: None, reciter: String::new(), speed: 1.0 });
    }

    /// A recording from a local port nothing listens on (fails at once, never leaves the host).
    fn play_recording(p: &mut Player) {
        p.on_command(PlayerCmd::PlayRecording {
            mount: "m".into(),
            title: "Talk".into(),
            url: "http://127.0.0.1:9/talk.mp3".into(),
            duration_s: Some(60),
        });
    }

    fn surah_of(p: &Player) -> Option<u16> {
        p.media.as_ref().and_then(|m| m.surah.as_ref()).map(|s| s.number)
    }

    fn resume(app: &App, surah: u16, auto: bool) -> QuranResume {
        QuranResume {
            surah,
            title: "t".into(),
            position_s: 30,
            auto,
            playlist: None,
            reciter: app.quran.resolve(None).key,
            speed: 1.5,
        }
    }

    fn save_playlist(app: &App, surahs: &[u16]) -> String {
        let p = crate::quran::Playlist {
            id: "morning".into(),
            name: "Morning".into(),
            surahs: surahs.to_vec(),
            speed: Some(1.25),
            ..crate::quran::Playlist::default()
        };
        app.quran.save_playlist(p).expect("valid playlist").id
    }

    fn session(token: u64, stop_reason: Option<StopReason>) -> LiveSession {
        LiveSession {
            token,
            stop: Arc::new(AtomicBool::new(false)),
            mount: "m".into(),
            trigger: Trigger::Poll,
            hist_id: None,
            reconnects: 0,
            stop_reason,
            buffer_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn select(app: &App, autoplay: bool) {
        app.update_settings(|s| {
            s.mount = Some("m".into());
            s.autoplay = autoplay;
        })
        .unwrap();
        let info = LiveInfo { server_type: "audio/ogg".into(), stream_start: Some("x".into()), listeners: None, title: None };
        app.live.write_lock().insert("m".into(), vec![("m".into(), info)]);
    }

    #[test]
    fn idle_phase_depends_on_a_selected_masjid() {
        assert_eq!(idle_phase(&Settings::default()), Phase::Idle);
        let s = Settings { mount: Some("lakewood".into()), ..Settings::default() };
        assert_eq!(idle_phase(&s), Phase::Waiting);
    }

    #[test]
    fn tokens_increase_and_quran_speed_is_clamped() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        assert_eq!((p.token(), p.token()), (1, 2));
        p.on_command(PlayerCmd::QuranSpeed(9.0));
        assert_eq!(f32::from_bits(p.qspeed.load(Ordering::Relaxed)), 2.0);
        p.on_command(PlayerCmd::QuranSpeed(0.1));
        assert_eq!(f32::from_bits(p.qspeed.load(Ordering::Relaxed)), 0.5);
    }

    #[test]
    fn stop_without_a_session_is_harmless() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        assert!(p.on_command(PlayerCmd::Stop).is_none());
        assert!(p.live.is_none() && p.suppressed_at.is_none());
        assert!(p.live_ended(&EndReason::Finished).is_none(), "nothing to end");
        p.start_live(Trigger::Manual);
        assert!(p.live.is_none(), "no masjid selected");
    }

    #[test]
    fn offline_signal_re_arms_after_stop() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.suppressed_at = Some(Instant::now());
        app.set_phase(Phase::Stopped);
        assert!(p.on_signal(Signal::PollOffline).is_none());
        assert!(p.suppressed_at.is_none());
        assert_eq!(app.status.borrow().phase, Phase::Waiting);
    }

    #[test]
    fn online_signals_trigger_when_armed() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        assert_eq!(p.on_signal(Signal::PollOnline), Some(Trigger::Poll));
        p.suppressed_at = Some(Instant::now());
        assert!(p.on_signal(Signal::PollOnline).is_none(), "stopped by the user");
    }

    #[test]
    fn online_signal_needs_autoplay() {
        let (app, _dir) = fx();
        select(&app, false);
        let mut p = player(&app);
        assert!(p.on_signal(Signal::PollOnline).is_none());
    }

    #[test]
    fn live_state_comes_from_the_poll_map() {
        let (app, _dir) = fx();
        select(&app, true);
        let p = player(&app);
        let s = app.settings();
        assert!(p.live_now());
        assert!(p.live_has_priority(&s));
        assert!(p.should_resume_live(&s));
        app.live.write_lock().clear();
        assert!(!p.live_now());
        assert!(!p.live_has_priority(&s));
        assert!(!p.should_resume_live(&s));
    }

    #[test]
    fn suppression_blocks_priority_and_resume() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.suppressed_at = Some(Instant::now());
        let s = app.settings();
        assert!(!p.live_has_priority(&s));
        assert!(!p.should_resume_live(&s));
    }

    #[test]
    fn probe_runs_only_while_blind_and_due() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        app.status.send_modify(|s| {
            s.signals.poll_ok = false;
            s.signals.poll_last_ms = 0;
            s.started_ms = 0;
        });
        p.last_probe = Instant::now().checked_sub(PROBE_EVERY + Duration::from_secs(1)).unwrap();
        assert_eq!(p.on_tick(), Some(Trigger::Probe));
        assert!(p.on_tick().is_none(), "not due again at once");
        app.status.send_modify(|s| s.signals.poll_ok = true);
        p.last_probe = Instant::now().checked_sub(PROBE_EVERY + Duration::from_secs(1)).unwrap();
        assert!(p.on_tick().is_none(), "signals are fine");
    }

    #[test]
    fn suppression_expires_on_the_tick() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.suppressed_at = Some(Instant::now().checked_sub(SUPPRESS_MAX + Duration::from_secs(1)).unwrap());
        app.set_phase(Phase::Stopped);
        p.on_tick();
        assert!(p.suppressed_at.is_none());
        assert_eq!(app.status.borrow().phase, Phase::Waiting);
    }

    #[test]
    fn recording_commands_are_noops_without_media() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        assert!(p.on_command(PlayerCmd::StopRecording).is_none());
        assert!(p.on_command(PlayerCmd::QuranStep(1)).is_none());
        p.pause_media(true);
        assert!(p.keep_resume_point().is_none());
        assert!(p.restart_point().is_none());
        assert!(p.on_media_event(5, RecordingEvent::Started).is_none());
        p.scheduled_playlist("missing");
        assert!(p.pending.is_none());
    }

    #[test]
    fn reconfigure_without_a_session_returns_to_waiting() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        app.set_phase(Phase::Playing);
        p.on_command(PlayerCmd::Reconfigure);
        assert!(p.live.is_none());
        assert_eq!(app.status.borrow().phase, Phase::Waiting);
    }

    #[test]
    fn play_now_without_live_asks_for_a_manual_start() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        assert_eq!(p.on_command(PlayerCmd::PlayNow), Some(Trigger::Manual));
    }

    #[test]
    fn stale_live_events_are_ignored() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        assert!(p.on_live_event(2, StreamEvent::Reconnecting("x".into())).is_none());
        assert_eq!(app.status.borrow().phase, Phase::Waiting, "the stale event changed nothing");
    }

    #[test]
    fn a_stopped_session_ignores_a_late_start() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, Some(StopReason::Stopped)));
        assert!(p.on_live_event(1, StreamEvent::Started { url: "u".into(), codec: "opus" }).is_none());
        assert!(p.live.is_some(), "still the same session");
        assert!(app.history.locked().last_heard("m").is_none(), "no history entry");
    }

    #[test]
    fn reconnect_and_end_update_the_player() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        p.on_live_event(1, StreamEvent::CaughtUp(4));
        p.on_live_event(1, StreamEvent::Reconnecting("drop".into()));
        assert_eq!(app.status.borrow().phase, Phase::Reconnecting);
        assert!(p.on_live_event(1, StreamEvent::Ended(EndReason::Finished)).is_none());
        assert!(p.live.is_none());
        assert_eq!(app.status.borrow().phase, Phase::Waiting);
        assert!(app.status.borrow().codec.is_none());
    }

    #[test]
    fn switching_mount_resumes_when_the_new_one_is_live() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, Some(StopReason::Switched)));
        assert_eq!(p.live_ended(&EndReason::Stopped), Some(Trigger::Poll));
    }

    #[test]
    fn a_recording_reports_its_status_and_stops() {
        let (app, _dir) = fx();
        let controls = Controls::new(5_000, false);
        let m = Media {
            token: 1,
            controls,
            mount: "m".into(),
            title: "R".into(),
            duration_s: Some(120),
            live_at_start: false,
            surah: None,
            speed: Arc::new(AtomicU32::new(1f32.to_bits())),
            sink: app.sink.clone(),
        };
        assert!(!m.is_quran());
        assert_eq!(m.neighbour(1), None);
        assert!(m.resume_point().is_none());
        let st = m.status();
        assert_eq!((st.kind, st.duration_s, st.position_s), ("recording", Some(120), 4));
        m.stop();
        assert!(m.controls.stop.load(Ordering::Relaxed));
    }

    #[test]
    fn a_surah_reports_its_status_and_freezes_at_the_heard_position() {
        let (app, _dir) = fx();
        let controls = Controls::new(10_000, false);
        let reciter = app.quran.resolve(None);
        let surah = Surah { number: 36, playlist: None, reciter: reciter.clone(), lib: app.quran.clone() };
        let m = Media {
            token: 1,
            controls,
            mount: String::new(),
            title: "36. Ya-Sin".into(),
            duration_s: None,
            live_at_start: false,
            surah: Some(surah),
            speed: Arc::new(AtomicU32::new(1f32.to_bits())),
            sink: app.sink.clone(),
        };
        assert!(m.is_quran());
        assert!(m.neighbour(1).is_some());
        let st = m.status();
        assert_eq!((st.kind, st.surah, st.position_s), ("quran", Some(36), 9));
        assert!(st.reciter.is_some() && st.reciter_key.as_deref() == Some(&reciter.key));
        let r = m.resume_point().expect("quran resumes");
        assert!(r.auto);
        assert_eq!(r.surah, 36);
        m.freeze();
        assert!(m.controls.is_paused());
        assert_eq!(m.controls.position_ms.load(Ordering::Relaxed), 9_300);
    }

    #[test]
    fn play_quran_takes_the_speaker_from_live() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        play_surah(&mut p, 36);
        let a = p.live.as_ref().unwrap();
        assert_eq!(a.stop_reason, Some(StopReason::Recording));
        assert!(a.stop.load(Ordering::Relaxed));
        assert_eq!(surah_of(&p), Some(36));
        assert!(p.media.as_ref().unwrap().live_at_start);
        {
            let st = app.status.borrow();
            let r = st.recording.as_ref().unwrap();
            assert_eq!((r.kind, r.surah), ("quran", Some(36)));
        }
        assert_eq!(phase(&app), Phase::Recording);
        assert!(logged(&app, "▶ Quran: 36."));
        p.stop_media();
        assert!(app.status.borrow().recording.is_none());
    }

    #[test]
    fn play_now_keeps_the_quran_for_after_the_broadcast() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        play_surah(&mut p, 2);
        assert_eq!(p.on_command(PlayerCmd::PlayNow), Some(Trigger::Manual));
        assert!(p.media.is_none());
        let r = p.quran_resume.clone().unwrap();
        assert_eq!(r.surah, 2);
        assert!(r.auto);
        assert!(app.status.borrow().quran_resume.is_some());
        assert!(app.status.borrow().recording.is_none());
        p.live = Some(session(9, None));
        assert!(p.on_command(PlayerCmd::PlayNow).is_none(), "already playing live");
    }

    #[test]
    fn seek_pause_resume_and_step_restart_the_surah() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        p.on_command(PlayerCmd::QuranSeek(5_000));
        assert!(p.media.is_none(), "nothing to seek");
        play_surah(&mut p, 2);
        let first = p.media.as_ref().unwrap().token;

        p.on_command(PlayerCmd::PauseRecording(true));
        assert!(p.media.as_ref().unwrap().controls.is_paused());
        assert!(app.status.borrow().recording.as_ref().unwrap().paused);

        p.on_command(PlayerCmd::QuranSeek(60_000));
        let m = p.media.as_ref().unwrap();
        assert!(m.token > first);
        assert!(m.controls.is_paused(), "a paused surah stays paused after a seek");
        assert_eq!(m.controls.position_ms.load(Ordering::Relaxed), 60_000);

        let paused_token = m.token;
        p.on_command(PlayerCmd::PauseRecording(false));
        let m = p.media.as_ref().unwrap();
        assert!(m.token > paused_token, "resume restarts from the file");
        assert!(!m.controls.is_paused());
        assert_eq!(m.controls.position_ms.load(Ordering::Relaxed), 60_000);

        p.on_command(PlayerCmd::QuranStep(1));
        assert_eq!(surah_of(&p), Some(3));
        assert_eq!(p.media.as_ref().unwrap().controls.position_ms.load(Ordering::Relaxed), 0);
        assert!(logged(&app, "▶ Quran: 3."));
        p.on_command(PlayerCmd::QuranStep(-1));
        assert_eq!(surah_of(&p), Some(2));
        p.on_command(PlayerCmd::QuranSpeed(1.5));
        assert!((app.status.borrow().recording.as_ref().unwrap().speed - 1.5).abs() < f32::EPSILON);
        p.stop_media();
    }

    #[test]
    fn a_recording_pauses_and_stops_back_to_live() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        p.quran_resume = Some(resume(&app, 36, true));
        play_recording(&mut p);
        assert_eq!(p.live.as_ref().unwrap().stop_reason, Some(StopReason::Recording));
        assert!(p.quran_resume.is_none(), "a recording replaces the Quran's resume point");
        assert_eq!(phase(&app), Phase::Recording);
        assert!(logged(&app, "▶ Playing recording: Talk"));
        {
            let st = app.status.borrow();
            let r = st.recording.as_ref().unwrap();
            assert_eq!((r.kind, r.duration_s, r.title.as_str()), ("recording", Some(60), "Talk"));
        }
        p.on_command(PlayerCmd::PauseRecording(true));
        assert!(p.media.as_ref().unwrap().controls.is_paused());
        p.on_command(PlayerCmd::PauseRecording(false));
        assert!(!p.media.as_ref().unwrap().controls.is_paused());

        p.live = None;
        p.quran_resume = Some(resume(&app, 36, true));
        assert_eq!(p.on_command(PlayerCmd::StopRecording), Some(Trigger::Poll), "the masjid is on air");
        assert!(p.media.is_none() && p.quran_resume.is_none());
        assert_eq!(phase(&app), Phase::Waiting);
        assert!(logged(&app, "Recording stopped"));

        not_live(&app);
        play_recording(&mut p);
        assert!(p.on_command(PlayerCmd::StopRecording).is_none(), "off air: nothing to resume");
    }

    #[test]
    fn stop_suppresses_live_until_it_ends() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        assert!(p.on_command(PlayerCmd::Stop).is_none());
        let a = p.live.as_ref().unwrap();
        assert_eq!(a.stop_reason, Some(StopReason::Stopped));
        assert!(a.stop.load(Ordering::Relaxed));
        assert!(p.suppressed_at.is_some());
        assert_eq!(phase(&app), Phase::Stopped);
        assert!(p.live_ended(&EndReason::Stopped).is_none());
        assert_eq!(phase(&app), Phase::Stopped, "stays stopped until the masjid goes offline");
    }

    #[test]
    fn reconfigure_switches_only_for_another_mount() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        app.set_phase(Phase::Playing);
        p.on_command(PlayerCmd::Reconfigure);
        assert!(p.live.as_ref().unwrap().stop_reason.is_none(), "same masjid");
        assert_eq!(phase(&app), Phase::Playing);
        app.update_settings(|s| s.mount = Some("other".into())).unwrap();
        p.on_command(PlayerCmd::Reconfigure);
        assert_eq!(p.live.as_ref().unwrap().stop_reason, Some(StopReason::Switched));
    }

    #[tokio::test]
    async fn test_tone_goes_to_the_sink() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        assert!(p.on_command(PlayerCmd::TestTone).is_none());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[test]
    fn a_scheduled_playlist_starts_at_once_when_off_air() {
        let (app, _dir) = fx();
        select(&app, false);
        let mut p = player(&app);
        let id = save_playlist(&app, &[36, 67]);
        p.on_command(PlayerCmd::ScheduledPlaylist(id));
        assert_eq!(surah_of(&p), Some(36));
        let m = p.media.as_ref().unwrap();
        assert_eq!(m.surah.as_ref().unwrap().playlist.as_ref().map(|c| c.name.as_str()), Some("Morning"));
        assert!((f32::from_bits(p.qspeed.load(Ordering::Relaxed)) - 1.25).abs() < f32::EPSILON);
        assert!(logged(&app, "▶ Scheduled: 36."));
        p.stop_media();
    }

    #[test]
    fn a_scheduled_playlist_waits_for_the_broadcast() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        let id = save_playlist(&app, &[67]);
        p.on_command(PlayerCmd::ScheduledPlaylist(id.clone()));
        assert!(p.media.is_none());
        assert!(p.pending.is_some());
        assert_eq!(app.status.borrow().quran_pending.as_deref(), Some("Morning"));
        assert!(logged(&app, "will play when the live broadcast ends"));

        p.on_tick();
        assert!(p.pending.is_some(), "still live");

        not_live(&app);
        p.quran_resume = Some(resume(&app, 2, false));
        p.on_tick();
        assert!(p.pending.is_none() && p.quran_resume.is_none());
        assert_eq!(surah_of(&p), Some(67));
        assert!(app.status.borrow().quran_pending.is_none());
        assert!(logged(&app, "▶ Scheduled (after the broadcast): 67."));
        p.stop_media();

        // A broadcast that lasts too long: the playlist is skipped.
        select(&app, true);
        p.on_command(PlayerCmd::ScheduledPlaylist(id));
        p.pending.as_mut().unwrap().until = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        p.on_tick();
        assert!(p.pending.is_none() && p.media.is_none());
        assert!(app.status.borrow().quran_pending.is_none());
        assert!(logged(&app, "Skipped \"Morning\""));
    }

    #[test]
    fn a_broadcast_interrupts_the_quran_and_recordings() {
        let (app, _dir) = fx();
        select(&app, true);
        not_live(&app);
        let mut p = player(&app);
        play_surah(&mut p, 36);
        select(&app, true);
        assert_eq!(p.on_signal(Signal::PollOnline), Some(Trigger::Poll));
        assert!(p.media.is_none());
        assert_eq!(p.quran_resume.as_ref().map(|r| r.surah), Some(36));
        assert!(logged(&app, "Quran paused at"));

        not_live(&app);
        play_recording(&mut p);
        select(&app, true);
        assert_eq!(p.on_signal(Signal::PollOnline), Some(Trigger::Poll));
        assert!(p.media.is_none());
        assert!(logged(&app, "stopping the recording"));

        // Already live when it started: the recording keeps playing until offline is seen.
        play_recording(&mut p);
        assert!(p.on_signal(Signal::PollOnline).is_none());
        assert!(p.media.is_some());
        p.on_signal(Signal::PollOffline);
        assert!(!p.media.as_ref().unwrap().live_at_start);
        p.stop_media();
    }

    #[test]
    fn start_live_spawns_a_session() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.start_live(Trigger::Probe);
        assert_eq!(p.live.as_ref().map(|a| a.trigger), Some(Trigger::Probe));
        assert_eq!(phase(&app), Phase::Waiting, "a probe is silent until it plays");
        p.live = None;
        p.start_live(Trigger::Poll);
        assert_eq!(p.live.as_ref().map(|a| (a.token, a.mount.as_str())), Some((2, "m")));
        assert_eq!(phase(&app), Phase::Connecting);
        assert!(p.on_tick().is_none(), "no audio yet: nothing to record");
        assert!(app.status.borrow().last_heard_ms.is_none());
    }

    #[tokio::test]
    async fn a_broadcast_is_recorded_in_history() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        // An invalid mount name keeps the salah lookup off the network.
        p.live = Some(LiveSession { mount: "bad mount".into(), ..session(1, None) });
        p.on_live_event(1, StreamEvent::Started { url: "u".into(), codec: "opus" });
        assert!(p.live.as_ref().unwrap().hist_id.is_some());
        assert_eq!(phase(&app), Phase::Playing);
        {
            let st = app.status.borrow();
            assert_eq!((st.codec.as_deref(), st.stream_url.as_deref(), st.trigger.as_deref()), (Some("opus"), Some("u"), Some("poll")));
        }
        assert!(logged(&app, "▶ Playing bad mount (opus, via poll)"));
        p.on_live_event(1, StreamEvent::Started { url: "u".into(), codec: "opus" });
        assert_eq!(p.live.as_ref().unwrap().reconnects, 1);
        assert!(logged(&app, "Stream recovered"));

        p.live.as_ref().unwrap().buffer_ms.store(2_500, Ordering::Relaxed);
        app.status.send_modify(|s| s.mount = Some("bad mount".into()));
        p.on_tick();
        assert_eq!(app.status.borrow().buffer_ms, 2_500);
        assert!(app.status.borrow().last_heard_ms.is_some());

        assert!(p.on_live_event(1, StreamEvent::Ended(EndReason::Finished)).is_none());
        let s = app.history.locked().recent(1, None);
        assert_eq!((s[0].reason.as_str(), s[0].reconnects, s[0].mount.as_str()), ("ended", 1, "bad mount"));
        assert!(logged(&app, "■ Broadcast ended"));
        assert_eq!(app.status.borrow().buffer_ms, 0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn a_start_during_on_demand_playback_keeps_its_phase() {
        let (app, _dir) = fx();
        select(&app, true);
        app.update_settings(|s| s.station_name = Some("Masjid".into())).unwrap();
        app.status.send_modify(|s| s.mount = Some("m".into()));
        let mut p = player(&app);
        play_recording(&mut p);
        p.live = Some(LiveSession { mount: "bad mount".into(), ..session(1, None) });
        p.on_live_event(1, StreamEvent::Started { url: "u".into(), codec: "mp3" });
        assert_eq!(phase(&app), Phase::Recording);
        assert!(logged(&app, "▶ Playing Masjid"));
        p.live_ended(&EndReason::Finished);
        assert_eq!(phase(&app), Phase::Recording, "the recording still plays");
        p.stop_media();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[test]
    fn no_stream_yet_waits_before_retrying() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        p.live = Some(session(1, None));
        assert!(p.live_ended(&EndReason::NotLive).is_none());
        assert!(p.retry_after > Instant::now());
        assert!(p.on_signal(Signal::PollOnline).is_none(), "too soon to retry");
    }

    #[test]
    fn a_finished_surah_moves_to_the_next() {
        let (app, _dir) = fx();
        select(&app, true);
        let mut p = player(&app);
        play_surah(&mut p, 113);
        let t = p.media.as_ref().unwrap().token;
        assert!(p.on_media_event(t, RecordingEvent::Started).is_none());
        assert!(p.on_media_event(t, RecordingEvent::Ended(None)).is_none());
        assert_eq!(surah_of(&p), Some(114));
        assert!(logged(&app, "▶ Quran: 114."));
        let t = p.media.as_ref().unwrap().token;
        assert!(p.on_media_event(t + 100, RecordingEvent::Ended(None)).is_none(), "stale event");
        assert_eq!(surah_of(&p), Some(114));
        assert_eq!(p.on_media_event(t, RecordingEvent::Ended(None)), Some(Trigger::Poll), "the end: back to live");
        assert!(p.media.is_none());
        assert!(app.status.borrow().recording.is_none());
        assert_eq!(phase(&app), Phase::Waiting);
        assert!(logged(&app, "Recording finished"));
    }

    #[test]
    fn a_failed_recording_is_reported() {
        let (app, _dir) = fx();
        let mut p = player(&app);
        play_recording(&mut p);
        let t = p.media.as_ref().unwrap().token;
        assert!(p.on_media_event(t, RecordingEvent::Ended(Some("boom".into()))).is_none());
        assert!(logged(&app, "Recording could not be played: boom"));
        assert_eq!(phase(&app), Phase::Idle);
    }

    #[test]
    fn the_quran_continues_once_the_broadcast_has_closed() {
        let (app, _dir) = fx();
        select(&app, true);
        not_live(&app);
        let mut p = player(&app);
        p.quran_resume = Some(resume(&app, 36, false));
        p.on_tick();
        assert!(p.closed_since.is_none(), "paused by the user: no automatic resume");

        p.quran_resume = Some(resume(&app, 36, true));
        p.on_tick();
        assert!(p.closed_since.is_some() && p.media.is_none(), "waits a little first");
        p.closed_since = Some(Instant::now().checked_sub(RESUME_AFTER_CLOSED + Duration::from_secs(1)).unwrap());
        p.on_tick();
        assert_eq!(surah_of(&p), Some(36));
        assert_eq!(p.media.as_ref().unwrap().controls.position_ms.load(Ordering::Relaxed), 30_000);
        assert!((f32::from_bits(p.qspeed.load(Ordering::Relaxed)) - 1.5).abs() < f32::EPSILON);
        assert!(p.quran_resume.is_none() && p.closed_since.is_none());
        assert!(logged(&app, "Broadcast over; continuing the Quran"));
        p.on_tick();
        assert!(app.status.borrow().recording.is_some());
        p.stop_media();
    }

    #[tokio::test]
    async fn the_loop_feeds_every_input_to_the_player() {
        let (app, _dir) = fx();
        let (ctx, crx) = unbounded_channel();
        let (stx, srx) = unbounded_channel();
        let task = tokio::spawn(run(app.clone(), crx, srx));
        stx.send(Signal::PollOffline).unwrap();
        // No masjid selected: the manual start finds nothing to play.
        ctx.send(PlayerCmd::PlayNow).unwrap();
        ctx.send(PlayerCmd::PlayRecording {
            mount: "m".into(),
            title: "Talk".into(),
            url: "http://127.0.0.1:9/talk.mp3".into(),
            duration_s: None,
        })
        .unwrap();
        let mut ended = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if logged(&app, "Recording could not be played") || logged(&app, "Recording finished") {
                ended = true;
                break;
            }
        }
        task.abort();
        assert!(ended, "the recording's end came back through the loop");
        assert!(app.status.borrow().recording.is_none());
    }
}
