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

use super::ctl::{self, Bt, System};
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
    audio_ready_with(&System, mac).await
}

pub(super) async fn audio_ready_with<B: Bt>(bt: &B, mac: &str) -> bool {
    has_a2dp(&bt.pcms().await, mac)
}

/// Does a `bluealsa-aplay -L` listing have an A2DP PCM for `mac`?
fn has_a2dp(pcms: &str, mac: &str) -> bool {
    pcms.contains(&format!("DEV={mac},PROFILE=a2dp"))
}

/// One supervision step for the preferred speaker.
pub async fn supervise(app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    supervise_with(&System, app, sp, mac, name).await;
}

async fn supervise_with<B: Bt>(bt: &B, app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    let ready = audio_ready_with(bt, mac).await;
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
        try_auto_repair(bt, app, sp, mac, name).await;
    } else if Instant::now() >= sp.next_attempt {
        connect_once(bt, app, sp, mac, name).await;
    }
    publish(app, sp, audio_ready_with(bt, mac).await).await;
}

async fn connect_once<B: Bt>(bt: &B, app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    tracing::debug!("connecting to Bluetooth speaker {mac}");
    let _ = ctl::quick_with(bt, &["trust", mac]).await;
    let _ = bt.ctl(&["connect", mac], Duration::from_secs(15)).await;
    // Give bluez-alsa a moment to set up the A2DP transport.
    for _ in 0..6 {
        if audio_ready_with(bt, mac).await {
            sp.succeeded();
            return;
        }
        bt.sleep(Duration::from_secs(1)).await;
    }
    // Link up but no audio = the speaker refused A2DP (typically a lost pairing key).
    let link_up = ctl::device_info_with(bt, mac).await.is_ok_and(|i| i.connected);
    sp.failed(link_up);
    if link_up {
        // Don't leave a half-open link: it confuses the speaker and our status.
        let _ = bt.ctl(&["disconnect", mac], Duration::from_secs(8)).await;
    }
    if sp.refusals >= REFUSALS_BEFORE_REPAIR && !sp.needs_repair {
        sp.needs_repair = true;
        app.log("warn", format!("{name} keeps refusing audio: it has probably forgotten this Pi. Put it in pairing mode to re-pair."));
    }
}

/// While a re-pair is needed, periodically look for the speaker in pairing mode and
/// re-pair automatically when it shows up.
async fn try_auto_repair<B: Bt>(bt: &B, app: &App, sp: &mut Speaker, mac: &str, name: &str) {
    if sp.last_repair_scan.is_some_and(|t| t.elapsed() < REPAIR_SCAN_EVERY) {
        return;
    }
    sp.last_repair_scan = Some(Instant::now());
    if !discoverable(bt, mac).await {
        return;
    }
    app.log("info", format!("{name} is in pairing mode; re-pairing"));
    match super::devices::repair_with(bt, app, mac).await {
        Ok(_) => {
            sp.reset();
            app.log("info", format!("Re-paired {name}"));
        }
        Err(e) => app.log("warn", format!("Automatic re-pair failed: {e}")),
    }
}

/// Short discovery; true if the speaker answered (i.e. it's in pairing/discoverable mode).
async fn discoverable<B: Bt>(bt: &B, mac: &str) -> bool {
    let _ = bt.ctl(&["--timeout", "8", "scan", "on"], Duration::from_secs(12)).await;
    ctl::quick_with(bt, &["info", mac]).await.is_ok_and(|s| s.contains("RSSI:"))
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

    use crate::bluetooth::ctl::fake::{FakeBt, a2dp};

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    fn app(name: &str) -> (std::path::PathBuf, App) {
        let dir = std::env::temp_dir().join(format!("lm-speaker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    fn logged(app: &App, needle: &str) -> bool {
        app.logs.lock().unwrap().iter().any(|l| l.msg.contains(needle))
    }

    fn ago(secs: u64) -> Instant {
        Instant::now().checked_sub(Duration::from_secs(secs)).unwrap()
    }

    #[test]
    fn reads_the_a2dp_listing() {
        assert!(has_a2dp(&a2dp(MAC), MAC));
        assert!(!has_a2dp(&a2dp("11:22:33:44:55:66"), MAC));
        assert!(!has_a2dp("", MAC));
        assert!(!has_a2dp(&format!("bluealsa:DEV={MAC},PROFILE=sco"), MAC));
    }

    #[test]
    fn reset_forgets_failures() {
        let mut sp = Speaker::new();
        sp.failed(true);
        sp.needs_repair = true;
        sp.reset();
        assert_eq!((sp.refusals, sp.backoff, sp.needs_repair), (0, BACKOFF_MIN, false));
    }

    #[tokio::test]
    async fn a_ready_speaker_is_published_and_routed() {
        let (dir, app) = app("ready");
        let bt = FakeBt::default();
        bt.ready(MAC, &[true]);
        let mut sp = Speaker::new();
        sp.refusals = 2;
        sp.last_reopen = ago(13);
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert!(sp.ready);
        assert_eq!(sp.refusals, 0);
        assert!(app.status.borrow().bt.connected);
        assert!(logged(&app, "Bluetooth speaker ready: VSW07"));
        assert!(sp.last_reopen.elapsed() < Duration::from_secs(5), "the sink was reopened on the speaker");
        assert!(bt.calls().is_empty(), "nothing to do while audio-ready");

        // Still ready: no second "ready" line, no second reopen within 12 s.
        let reopened = sp.last_reopen;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(app.logs.lock().unwrap().iter().filter(|l| l.msg.contains("ready")).count(), 1);
        assert_eq!(sp.last_reopen, reopened);

        // Already routed to Bluetooth: never reopen.
        app.status.send_modify(|s| s.output.kind = "bluetooth".into());
        sp.last_reopen = ago(60);
        let before = sp.last_reopen;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(sp.last_reopen, before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn losing_audio_waits_briefly_before_reconnecting() {
        let (dir, app) = app("lost");
        let bt = FakeBt::default();
        bt.ready(MAC, &[true, false]);
        let mut sp = Speaker::new();
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert!(!sp.ready);
        assert!(logged(&app, "Bluetooth speaker disconnected: VSW07"));
        assert!(sp.next_attempt > Instant::now(), "the next attempt is a moment away");
        assert!(bt.calls().is_empty(), "no connect attempt yet");
        assert!(!app.status.borrow().bt.connected);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_connect_that_brings_audio_up_succeeds() {
        let (dir, app) = app("connect");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false, false, true]);
        let mut sp = Speaker::new();
        sp.backoff = 40;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(bt.calls(), [format!("trust {MAC}"), format!("connect {MAC}")]);
        assert_eq!(sp.backoff, BACKOFF_MIN);
        assert!(app.status.borrow().bt.connected, "published as audio-ready");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_speaker_that_is_off_backs_off_quietly() {
        let (dir, app) = app("off");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false]).reply("info", Ok("\tConnected: no\n"));
        let mut sp = Speaker::new();
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(sp.refusals, 0);
        assert_eq!(sp.backoff, BACKOFF_MIN * 2);
        assert!(sp.next_attempt > Instant::now());
        assert!(!bt.called(&format!("disconnect {MAC}")));

        // Not due yet: no attempt.
        let n = bt.calls().len();
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(bt.calls().len(), n);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn repeated_refusals_ask_for_a_re_pair() {
        let (dir, app) = app("refuse");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false]).reply("info", Ok("\tConnected: yes\n"));
        let mut sp = Speaker::new();
        for _ in 0..REFUSALS_BEFORE_REPAIR {
            sp.next_attempt = Instant::now();
            supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        }
        assert!(sp.needs_repair);
        assert!(bt.called(&format!("disconnect {MAC}")), "the half-open link is dropped");
        assert!(logged(&app, "VSW07 keeps refusing audio"));
        assert!(app.status.borrow().bt.needs_repair);

        // Once a re-pair is needed, no more connect attempts; a recent look for pairing mode isn't repeated.
        let n = bt.calls().len();
        sp.next_attempt = Instant::now();
        sp.last_repair_scan = Some(Instant::now());
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert_eq!(bt.calls().len(), n);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn re_pairs_when_the_speaker_is_in_pairing_mode() {
        let (dir, app) = app("repair");
        let bt = FakeBt::default();
        // Not ready, then the re-pair's connect finds audio.
        bt.ready(MAC, &[false, true])
            .reply("info", Ok("\tName: VSW07\n\tRSSI: -60\n"))
            .reply("devices Paired", Ok(&format!("Device {MAC} VSW07")));
        let mut sp = Speaker::new();
        sp.needs_repair = true;
        sp.refusals = 3;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert!(bt.called("--timeout 8 scan on"));
        assert!(bt.called(&format!("remove {MAC}")));
        assert!(logged(&app, "VSW07 is in pairing mode; re-pairing"));
        assert!(logged(&app, "Re-paired VSW07"));
        assert!(!sp.needs_repair);
        assert_eq!(sp.refusals, 0);
        assert_eq!(app.settings().bt_device.as_deref(), Some(MAC));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_unseen_speaker_is_looked_for_again_later() {
        let (dir, app) = app("unseen");
        let bt = FakeBt::default();
        bt.ready(MAC, &[false]).reply("info", Ok("\tName: VSW07\n"));
        let mut sp = Speaker::new();
        sp.needs_repair = true;
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert!(sp.last_repair_scan.is_some());
        assert!(!bt.called(&format!("remove {MAC}")), "not in pairing mode: nothing removed");
        assert!(sp.needs_repair);

        // A failing re-pair is logged and the speaker still needs one.
        bt.reply("info", Ok("\tRSSI: -70\n")).reply("devices Paired", Ok(&format!("Device {MAC} VSW07")));
        sp.last_repair_scan = Some(ago(REPAIR_SCAN_EVERY.as_secs() + 1));
        supervise_with(&bt, &app, &mut sp, MAC, "VSW07").await;
        assert!(logged(&app, "Automatic re-pair failed"));
        assert!(sp.needs_repair);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_real_check_returns() {
        // bluez-alsa isn't installed where the tests run: only that it answers.
        let _ = audio_ready(MAC).await;
        let (dir, app) = app("real");
        let mut sp = Speaker::new();
        sp.next_attempt = Instant::now() + Duration::from_secs(60);
        supervise(&app, &mut sp, MAC, "VSW07").await;
        assert!(!sp.ready);
        let _ = std::fs::remove_dir_all(dir);
    }
}
