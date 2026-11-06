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
}

pub async fn run(app: App, mut cmd_rx: UnboundedReceiver<PlayerCmd>, mut sig_rx: UnboundedReceiver<Signal>) {
    let (ev_tx, mut ev_rx) = unbounded_channel::<(u64, StreamEvent)>();
    let mut active: Option<Active> = None;
    let mut next_token = 0u64;
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
                    if active.is_none() { start = Some("manual"); }
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
                        let ok = s.mount.is_some() && s.autoplay && suppressed_at.is_none()
                            && active.is_none() && Instant::now() >= retry_after;
                        if ok {
                            start = Some(if sig == Signal::SseOnline { "sse" } else { "poll" });
                        }
                    }
                    Signal::PollOffline => {
                        if suppressed_at.take().is_some() && active.is_none() {
                            app.set_phase(if s.mount.is_some() { Phase::Waiting } else { Phase::Idle });
                        }
                    }
                }
            },
            Some((tok, ev)) = ev_rx.recv() => {
                let Some(a) = active.as_mut().filter(|a| a.token == tok) else { continue };
                match ev {
                    StreamEvent::Started { url, codec } => {
                        let name = app.settings().station_name.unwrap_or_else(|| a.mount.clone());
                        if a.hist_id.is_none() {
                            let id = app.history.lock().unwrap().begin(&a.mount, &name, codec, a.trigger);
                            a.hist_id = Some(id);
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
                        if suppressed_at.is_none() {
                            app.set_phase(Phase::Playing);
                        }
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
                            if let Some(m) = &st.mount { st.last_heard_ms = app.history.lock().unwrap().last_heard(m); }
                        });
                        let mount = app.settings().mount;
                        let phase = if suppressed_at.is_some() { Phase::Stopped }
                            else if mount.is_some() { Phase::Waiting } else { Phase::Idle };
                        app.set_phase(phase);
                        // A switch: start watching/playing the new mount right away if live.
                        if a.stop_reason == Some("switched") && mount.as_deref().is_some_and(|m| crate::monitor::is_live(&app.live.read().unwrap(), m)) {
                            start = Some("poll");
                        }
                    }
                }
            },
            _ = tick.tick() => {
                if let Some(a) = &active {
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
            };
            std::thread::Builder::new()
                .name("stream".into())
                .spawn(move || crate::stream::run_session(params))
                .expect("spawn stream thread");
            active = Some(Active { token, stop, mount, trigger, hist_id: None, reconnects: 0, stop_reason: None });
            if trigger != "probe" {
                app.set_phase(Phase::Connecting);
            }
            tracing::info!("starting stream session (trigger: {trigger})");
        }
    }
}
