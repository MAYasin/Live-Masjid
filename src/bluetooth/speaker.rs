//! Keeping the preferred speaker connected — without flapping.
//!
//! "Connected" means *audio-ready*: bluez-alsa exposes an A2DP PCM for the speaker. A bare
//! radio (ACL) link is not enough: a speaker that has forgotten our pairing key accepts the
//! link and then rejects the audio channel (`avdtp_connect_cb … Invalid exchange (52)`, i.e.
//! "key missing"). Retrying that forever is what made the speaker flap every ~10 s.
//!
//! States: Ready → (audio lost) → Reconnecting with back-off (speaker off / out of range is
//! normal and quiet) → after repeated *refusals* NeedsRepair: stop retrying, tell the user,
//! and watch for the speaker in pairing mode to re-pair it automatically.

use super::ctl;
use crate::state::App;
use std::time::{Duration, Instant};

/// Link came up but audio was refused this many times in a row → the bond is broken.
const REFUSALS_BEFORE_REPAIR: u32 = 3;
const BACKOFF_MIN: u64 = 5;
const BACKOFF_MAX: u64 = 60;
/// While a re-pair is needed, look for the speaker in pairing mode this often.
const REPAIR_SCAN_EVERY: Duration = Duration::from_secs(120);

pub struct Speaker {
    next_attempt: Instant,
    backoff: u64,
    refusals: u32,
    pub needs_repair: bool,
    last_repair_scan: Option<Instant>,
    ready: bool,
    last_reopen: Instant,
}

impl Speaker {
    pub fn new() -> Self {
        Self {
            next_attempt: Instant::now(),
            backoff: BACKOFF_MIN,
            refusals: 0,
            needs_repair: false,
            last_repair_scan: None,
            ready: false,
            last_reopen: Instant::now(),
        }
    }

    /// Forget all failure history (new speaker chosen, or a successful re-pair).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn failed(&mut self, refused: bool) {
        if refused {
            self.refusals += 1;
        }
        self.next_attempt = Instant::now() + Duration::from_secs(self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    fn succeeded(&mut self) {
        self.refusals = 0;
        self.backoff = BACKOFF_MIN;
        self.needs_repair = false;
    }
}

/// Is the speaker's A2DP audio channel available in bluez-alsa?
pub async fn audio_ready(mac: &str) -> bool {
    let fut = tokio::process::Command::new("bluealsa-aplay").arg("-L").kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).contains(&format!("DEV={mac},PROFILE=a2dp")),
        _ => false,
    }
}

/// One supervision step for the preferred speaker.
pub async fn supervise(app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    let ready = audio_ready(mac).await;
    if ready {
        if !sp.ready {
            app.log("info", format!("Bluetooth speaker ready: {name}"));
        }
        sp.ready = true;
        sp.succeeded();
        publish(app, sp, true).await;
        ensure_routed(app, sp);
        return;
    }
    if sp.ready {
        app.log("warn", format!("Bluetooth speaker disconnected: {name}"));
        sp.ready = false;
        sp.next_attempt = Instant::now() + Duration::from_secs(2);
    }

    if sp.needs_repair {
        try_auto_repair(app, sp, mac, name).await;
    } else if Instant::now() >= sp.next_attempt {
        connect_once(app, sp, mac, name).await;
    }
    publish(app, sp, audio_ready(mac).await).await;
}

async fn connect_once(app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    tracing::debug!("connecting to Bluetooth speaker {mac}");
    let _ = ctl::quick(&["trust", mac]).await;
    let _ = ctl::btctl(&["connect", mac], Duration::from_secs(15)).await;
    // Give bluez-alsa a moment to set up the A2DP transport.
    for _ in 0..6 {
        if audio_ready(mac).await {
            sp.succeeded();
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Link up but no audio = the speaker refused A2DP (typically a lost pairing key).
    let link_up = ctl::device_info(mac).await.is_ok_and(|i| i.connected);
    sp.failed(link_up);
    if link_up {
        // Don't leave a half-open link: it confuses the speaker and our status.
        let _ = ctl::btctl(&["disconnect", mac], Duration::from_secs(8)).await;
    }
    if sp.refusals >= REFUSALS_BEFORE_REPAIR && !sp.needs_repair {
        sp.needs_repair = true;
        app.log("warn", format!("{name} keeps refusing audio: it has probably forgotten this Pi. Put it in pairing mode to re-pair."));
    }
}

/// While a re-pair is needed, periodically look for the speaker in pairing mode and
/// re-pair automatically when it shows up.
async fn try_auto_repair(app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    if sp.last_repair_scan.is_some_and(|t| t.elapsed() < REPAIR_SCAN_EVERY) {
        return;
    }
    sp.last_repair_scan = Some(Instant::now());
    if !discoverable(mac).await {
        return;
    }
    app.log("info", format!("{name} is in pairing mode; re-pairing"));
    match super::devices::repair(app, mac).await {
        Ok(_) => {
            sp.reset();
            app.log("info", format!("Re-paired {name}"));
        }
        Err(e) => app.log("warn", format!("Automatic re-pair failed: {e}")),
    }
}

/// Short discovery; true if the speaker answered (i.e. it's in pairing/discoverable mode).
async fn discoverable(mac: &str) -> bool {
    let _ = ctl::btctl(&["--timeout", "8", "scan", "on"], Duration::from_secs(12)).await;
    ctl::quick(&["info", mac]).await.is_ok_and(|s| s.contains("RSSI:"))
}

async fn publish(app: &App, sp: &Speaker, ready: bool) {
    let needs_repair = sp.needs_repair;
    let changed = app.status.send_if_modified(|st| {
        let before = (st.bt.connected, st.bt.needs_repair);
        st.bt.connected = ready;
        st.bt.needs_repair = needs_repair && !ready;
        before != (st.bt.connected, st.bt.needs_repair)
    });
    if changed {
        app.sink.retarget(super::compute_targets(app));
    }
}

/// Audio-ready but the sink isn't on it yet (e.g. transport came up after the sink gave up).
fn ensure_routed(app: &App, sp: &mut Speaker) {
    let on_bt = app.status.borrow().output.kind == "bluetooth";
    if !on_bt && sp.last_reopen.elapsed() > Duration::from_secs(12) {
        sp.last_reopen = Instant::now();
        app.sink.reopen();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_lead_to_repair_and_backoff_grows() {
        let mut sp = Speaker::new();
        sp.failed(false); // speaker off: not a refusal
        assert_eq!(sp.refusals, 0);
        assert_eq!(sp.backoff, 10);
        for _ in 0..REFUSALS_BEFORE_REPAIR {
            sp.failed(true);
        }
        assert_eq!(sp.refusals, REFUSALS_BEFORE_REPAIR);
        assert_eq!(sp.backoff, BACKOFF_MAX);
        sp.succeeded();
        assert_eq!((sp.refusals, sp.backoff), (0, BACKOFF_MIN));
    }
}
