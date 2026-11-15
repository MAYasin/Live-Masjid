//! Audio output thread.
//!
//! PCM (stereo, s16) is piped into an `aplay` child process ([`device`]). While waiting for a
//! stream the sink keeps the device open and feeds silence ("keep awake"), so when audio
//! arrives it is heard immediately: no ALSA open, no Bluetooth A2DP transport start-up, no
//! speaker auto-standby. Outputs are tried in order (Bluetooth, then wired), with back-off.

mod device;
mod pcm;

pub use pcm::{MIRROR_RATE, chime};

use crate::state::OutputStatus;
use crate::sync::LockExt;
use device::Aplay;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;

/// Real audio within this long keeps the output busy (silence fills the gaps).
const RECENT: Duration = Duration::from_secs(3);
/// Without keep-awake, close the device after this long without audio.
const IDLE_CLOSE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// "bluetooth" | "wired"
    pub kind: &'static str,
    /// ALSA PCM name passed to `aplay -D`.
    pub device: String,
    pub label: String,
}

/// Which volume a piece of audio follows. Broadcasts (live and recordings) and the Quran have
/// their own, independent volumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Broadcast,
    Quran,
}

enum SinkMsg {
    /// Interleaved stereo s16 at `rate`.
    Pcm { epoch: u64, rate: u32, data: Vec<i16>, channel: Channel },
    /// New ordered list of outputs to try (first = preferred).
    Retarget(Vec<Target>),
    /// Force close/reopen (e.g. a Bluetooth speaker reconnected with new transport).
    Reopen,
}

type StatusCb = Box<dyn Fn(OutputStatus) + Send + Sync>;

#[derive(Clone)]
pub struct SinkHandle {
    tx: SyncSender<SinkMsg>,
    keep_awake: Arc<AtomicBool>,
    status_cb: Arc<OnceLock<StatusCb>>,
    pub heartbeat: Arc<AtomicU64>,
    /// Set by the sink while real audio (not silence) is flowing to the device.
    pub audible: Arc<AtomicBool>,
    /// PCM tagged with an older epoch is discarded (instant stop / switch).
    pub epoch: Arc<AtomicU64>,
    targets: Arc<Mutex<Vec<Target>>>,
    /// Copy of exactly what goes to the device (mono, `MIRROR_RATE`, s16le), for browsers that
    /// "listen on this device" in sync with the speaker. Only produced while someone listens.
    pub mirror: tokio::sync::broadcast::Sender<Arc<[u8]>>,
    /// Audio queued for the sink thread but not yet handed to the device, in µs.
    queued_us: Arc<AtomicU64>,
    /// The broadcast volume (0–100).
    volume: Arc<AtomicU8>,
    /// The Quran's volume (0–100), independent of the broadcast volume.
    pub quran_volume: Arc<AtomicU8>,
}

impl SinkHandle {
    /// Start the output thread. `volume` is the broadcast volume (shared with the settings).
    ///
    /// # Panics
    /// If the OS refuses to create the thread.
    pub fn spawn(volume: Arc<AtomicU8>, keep_awake: bool) -> Self {
        let (h, rx) = Self::with_queue(volume, keep_awake);
        let h2 = h.clone();
        crate::sync::spawn_named("sink", move || Sink::new(h2, AplayOpener).run(&rx));
        h
    }

    /// A handle and the queue its output thread reads.
    fn with_queue(volume: Arc<AtomicU8>, keep_awake: bool) -> (Self, Receiver<SinkMsg>) {
        // ~1.3 s of 20 ms chunks: enough to absorb jitter, small enough that "stop" is quick.
        let (tx, rx) = sync_channel(64);
        let h = SinkHandle {
            tx,
            keep_awake: Arc::new(AtomicBool::new(keep_awake)),
            status_cb: Arc::new(OnceLock::new()),
            heartbeat: Arc::new(AtomicU64::new(crate::state::now_ms())),
            audible: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
            targets: Arc::new(Mutex::new(Vec::new())),
            mirror: tokio::sync::broadcast::channel(64).0,
            queued_us: Arc::new(AtomicU64::new(0)),
            volume,
            quran_volume: Arc::new(AtomicU8::new(100)),
        };
        (h, rx)
    }

    pub fn on_status(&self, cb: impl Fn(OutputStatus) + Send + Sync + 'static) {
        let _ = self.status_cb.set(Box::new(cb));
    }

    pub fn set_keep_awake(&self, on: bool) {
        self.keep_awake.store(on, Ordering::Relaxed);
    }

    /// Blocking send of broadcast PCM (applies back-pressure to the decoder). Returns false if
    /// the sink is gone.
    pub fn send_pcm(&self, epoch: u64, rate: u32, data: Vec<i16>) -> bool {
        self.send_pcm_on(Channel::Broadcast, epoch, rate, data)
    }

    /// Like `send_pcm`, for audio that follows `channel`'s volume.
    pub fn send_pcm_on(&self, channel: Channel, epoch: u64, rate: u32, data: Vec<i16>) -> bool {
        self.queued_us.fetch_add(pcm::chunk_us(rate, &data), Ordering::Relaxed);
        self.tx.send(SinkMsg::Pcm { epoch, rate, data, channel }).is_ok()
    }

    fn dequeued(&self, rate: u32, data: &[i16]) {
        let us = pcm::chunk_us(rate, data);
        // A saturating decrement (a plain loop: `try_update` is newer than the MSRV).
        let mut cur = self.queued_us.load(Ordering::Relaxed);
        while let Err(seen) = self.queued_us.compare_exchange_weak(cur, cur.saturating_sub(us), Ordering::Relaxed, Ordering::Relaxed) {
            cur = seen;
        }
    }

    /// Decoded audio waiting for the sink thread, in ms.
    pub fn queued_ms(&self) -> u64 {
        self.queued_us.load(Ordering::Relaxed) / 1000
    }

    /// Start a new epoch: anything queued from before is dropped. Returns the new epoch.
    pub fn new_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn retarget(&self, targets: Vec<Target>) {
        let mut cur = self.targets.locked();
        if *cur != targets {
            cur.clone_from(&targets);
            self.control(SinkMsg::Retarget(targets));
        }
    }

    pub fn reopen(&self) {
        self.control(SinkMsg::Reopen);
    }

    fn control(&self, msg: SinkMsg) {
        // Control messages must not block async callers; if the queue is full of PCM,
        // deliver from a helper thread.
        if let Err(TrySendError::Full(msg)) = self.tx.try_send(msg) {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(msg);
            });
        }
    }

    fn volume_of(&self, channel: Channel) -> u8 {
        volume_for(channel, self.volume.load(Ordering::Relaxed), self.quran_volume.load(Ordering::Relaxed))
    }
}

/// Which volume a piece of audio follows.
fn volume_for(channel: Channel, broadcast: u8, quran: u8) -> u8 {
    match channel {
        Channel::Broadcast => broadcast,
        Channel::Quran => quran,
    }
}

/// Retry delay after `streak` consecutive failures: `base << streak`, capped at 30 s.
fn backoff_secs(streak: u32, base: u64, max_shift: u32) -> u64 {
    (base << streak.min(max_shift)).min(30)
}

/// Whether two reports describe the same output state (so a status push can be skipped).
fn same_output(a: &OutputStatus, b: &OutputStatus) -> bool {
    a.device == b.device && a.open == b.open && a.error == b.error && a.label == b.label
}

/// One open output device: an `aplay` process ([`Aplay`]), or a fake in tests.
trait Output {
    /// The sample rate it was opened for.
    fn rate(&self) -> u32;
    /// Which target it is.
    fn idx(&self) -> usize;
    /// Its error, if it has already exited.
    fn exited(&mut self) -> Option<String>;
    /// Queue `chunk` for the device without blocking for long (keeps `heartbeat` fresh).
    fn write(&mut self, chunk: Vec<u8>, heartbeat: &AtomicU64) -> std::io::Result<()>;
    /// What went wrong after a failed write.
    fn error_text(&mut self, e: &std::io::Error) -> String;
    fn close(self);
}

/// Opens outputs.
trait Opener {
    type Out: Output;
    /// How long a fresh output gets to fail fast before it counts as open.
    const SETTLE: Duration;
    fn open(&mut self, device: &str, rate: u32, idx: usize) -> std::io::Result<Self::Out>;
}

/// The real outputs: `aplay` processes.
struct AplayOpener;

impl Opener for AplayOpener {
    type Out = Aplay;
    const SETTLE: Duration = Duration::from_millis(120);

    fn open(&mut self, device: &str, rate: u32, idx: usize) -> std::io::Result<Aplay> {
        Aplay::spawn(device, rate, idx)
    }
}

impl Output for Aplay {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn idx(&self) -> usize {
        self.idx
    }

    fn exited(&mut self) -> Option<String> {
        Aplay::exited(self)
    }

    fn write(&mut self, chunk: Vec<u8>, heartbeat: &AtomicU64) -> std::io::Result<()> {
        Aplay::write(self, chunk, heartbeat)
    }

    fn error_text(&mut self, e: &std::io::Error) -> String {
        Aplay::error_text(self, e)
    }

    fn close(self) {
        Aplay::close(self);
    }
}

struct Sink<P: Opener = AplayOpener> {
    h: SinkHandle,
    opener: P,
    targets: Vec<Target>,
    open: Option<P::Out>,
    /// Index of target to try next.
    next_idx: usize,
    retry_at: Instant,
    last_pcm: Option<Instant>,
    last_status: Option<OutputStatus>,
    bytes: Vec<u8>,
    silence: Vec<i16>,
    /// When there is no usable output we pace audio by sleeping so decoders don't race.
    none_mode: bool,
    /// Consecutive "all outputs failed" rounds, for retry back-off.
    fail_streak: u32,
}

impl<P: Opener> Sink<P> {
    fn new(h: SinkHandle, opener: P) -> Self {
        Self {
            h,
            opener,
            targets: Vec::new(),
            open: None,
            next_idx: 0,
            retry_at: Instant::now(),
            last_pcm: None,
            last_status: None,
            bytes: Vec::with_capacity(16 * 1024),
            silence: vec![0; (RATE as usize / 50) * 2],
            none_mode: true,
            fail_streak: 0,
        }
    }

    fn recent(&self) -> bool {
        self.last_pcm.is_some_and(|t| t.elapsed() < RECENT)
    }

    fn run(mut self, rx: &Receiver<SinkMsg>) {
        let mut cur_rate = RATE;
        while self.step(rx, &mut cur_rate) {}
        self.close();
    }

    /// Handle one message (or a timeout). False once every sender has gone.
    fn step(&mut self, rx: &Receiver<SinkMsg>, cur_rate: &mut u32) -> bool {
        self.h.heartbeat.store(crate::state::now_ms(), Ordering::Relaxed);
        // While feeding silence, don't wait: write it back-to-back and let the device pace
        // us (play() blocks while the device buffer is full). Waiting 25 ms before each
        // 20 ms chunk delivered only ~80% of real time, so idle speakers kept under-running.
        let feeding = self.open.is_some() && (self.h.keep_awake.load(Ordering::Relaxed) || self.recent());
        let wait = if feeding { Duration::from_millis(1) } else { Duration::from_millis(25) };
        match rx.recv_timeout(wait) {
            Ok(SinkMsg::Pcm { epoch, rate, data, .. }) if epoch != self.h.epoch.load(Ordering::SeqCst) => {
                self.h.dequeued(rate, &data);
            }
            Ok(SinkMsg::Pcm { rate, data, channel, .. }) => {
                self.h.dequeued(rate, &data);
                self.last_pcm = Some(Instant::now());
                self.h.audible.store(true, Ordering::Relaxed);
                *cur_rate = rate;
                self.play(rate, &data, Some(channel));
            }
            Ok(SinkMsg::Retarget(t)) => {
                self.targets = t;
                self.restart_outputs();
            }
            Ok(SinkMsg::Reopen) => self.restart_outputs(),
            Err(RecvTimeoutError::Timeout) => self.idle(*cur_rate),
            Err(RecvTimeoutError::Disconnected) => return false,
        }
        true
    }

    /// Close the output and try from the preferred target again, now.
    fn restart_outputs(&mut self) {
        self.fail_streak = 0;
        self.close();
        self.next_idx = 0;
        self.retry_at = Instant::now();
    }

    /// No audio arrived: feed silence, or close an output nobody needs.
    fn idle(&mut self, rate: u32) {
        let recent = self.recent();
        if !recent {
            self.h.audible.store(false, Ordering::Relaxed);
        }
        if self.h.keep_awake.load(Ordering::Relaxed) || recent {
            // Feed silence: keeps device + Bluetooth transport warm, avoids underrun clicks.
            let s = std::mem::take(&mut self.silence);
            self.play(rate, &s, None);
            self.silence = s;
        } else if self.open.is_some() && self.last_pcm.is_none_or(|t| t.elapsed() > IDLE_CLOSE) {
            self.close();
            self.report(None);
        } else if self.open.is_none() && !self.none_mode {
            self.report(None);
        }
    }

    /// Hand audio to the device: `channel` is None for keep-awake silence.
    fn play(&mut self, rate: u32, data: &[i16], channel: Option<Channel>) {
        if self.h.mirror.receiver_count() > 0 && rate > 0 {
            let _ = self.h.mirror.send(pcm::mirror_mono(rate, data).into());
        }
        if self.open.as_ref().is_some_and(|o| o.rate() != rate) {
            self.close();
        }
        if self.open.is_none() {
            self.try_open(rate);
        }
        let Some(o) = self.open.as_mut() else {
            // No output: pace real audio in real time; drop silence.
            if channel.is_some() {
                std::thread::sleep(Duration::from_micros(data.len() as u64 * 500_000 / u64::from(rate.max(1))));
            }
            return;
        };
        let gain = channel.map_or(pcm::UNITY, |c| pcm::gain_q16(self.h.volume_of(c)));
        self.bytes.clear();
        pcm::encode(data, gain, &mut self.bytes);
        let chunk = std::mem::replace(&mut self.bytes, Vec::with_capacity(16 * 1024));
        if let Err(e) = o.write(chunk, &self.h.heartbeat) {
            let (idx, err) = (o.idx(), o.error_text(&e));
            self.fail(idx, err);
        }
    }

    fn try_open(&mut self, rate: u32) {
        if Instant::now() < self.retry_at {
            return;
        }
        if self.targets.is_empty() {
            self.none_mode = true;
            self.retry_at = Instant::now() + Duration::from_secs(2);
            self.report(Some("No audio output available".into()));
            return;
        }
        let start = self.next_idx.min(self.targets.len() - 1);
        for idx in start..self.targets.len() {
            let t = self.targets[idx].clone();
            match self.opener.open(&t.device, rate, idx) {
                Ok(mut o) => {
                    // aplay fails fast if the device is missing/busy: give it a moment.
                    std::thread::sleep(P::SETTLE);
                    if let Some(msg) = o.exited() {
                        if self.fail_streak == 0 {
                            tracing::warn!("output {} failed: {msg}", t.device);
                        }
                        continue;
                    }
                    tracing::info!("audio output opened: {} ({})", t.label, t.device);
                    self.open = Some(o);
                    self.none_mode = false;
                    self.fail_streak = 0;
                    self.report(None);
                    return;
                }
                Err(e) => tracing::warn!("cannot start aplay: {e}"),
            }
        }
        self.none_mode = true;
        self.next_idx = 0;
        self.fail_streak += 1;
        self.retry_at = Instant::now() + Duration::from_secs(backoff_secs(self.fail_streak, 2, 4));
        self.report(Some("All audio outputs failed; retrying".into()));
    }

    fn fail(&mut self, idx: usize, err: String) {
        tracing::warn!("audio output error: {err}");
        self.close();
        // Try the next target right away (e.g. Bluetooth dropped -> wired fallback),
        // and come back to the preferred one later.
        self.next_idx = idx + 1;
        if self.next_idx >= self.targets.len() {
            self.next_idx = 0;
            self.fail_streak += 1;
            self.retry_at = Instant::now() + Duration::from_secs(backoff_secs(self.fail_streak, 1, 5));
        }
        self.report(Some(err));
    }

    fn close(&mut self) {
        if let Some(o) = self.open.take() {
            o.close();
        }
    }

    fn report(&mut self, error: Option<String>) {
        let st = match &self.open {
            Some(o) => {
                let t = &self.targets[o.idx()];
                OutputStatus { kind: t.kind.into(), device: t.device.clone(), label: t.label.clone(), open: true, error }
            }
            None => OutputStatus {
                kind: "none".into(),
                device: String::new(),
                label: if self.targets.is_empty() { "No output".into() } else { "Idle".into() },
                open: false,
                error,
            },
        };
        if self.last_status.as_ref().is_some_and(|l| same_output(l, &st)) {
            return;
        }
        self.last_status = Some(st.clone());
        if let Some(cb) = self.h.status_cb.get() {
            cb(st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// Outputs that record what they're given. Devices in `refuse` can't be opened, those in
    /// `exit` exit at once, and writes fail while `fail_writes` is set.
    #[derive(Default)]
    struct FakeState {
        refuse: Vec<String>,
        exit: Vec<String>,
        fail_writes: bool,
        opened: Vec<String>,
        closed: usize,
        writes: Vec<(usize, Vec<u8>)>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);

    impl Fake {
        fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
            f(&mut self.0.locked())
        }
    }

    struct FakeOut {
        fake: Fake,
        device: String,
        rate: u32,
        idx: usize,
    }

    impl Output for FakeOut {
        fn rate(&self) -> u32 {
            self.rate
        }

        fn idx(&self) -> usize {
            self.idx
        }

        fn exited(&mut self) -> Option<String> {
            let d = self.device.clone();
            self.fake.with(|s| s.exit.contains(&d)).then(|| format!("{d}: no such device"))
        }

        fn write(&mut self, chunk: Vec<u8>, _heartbeat: &AtomicU64) -> std::io::Result<()> {
            let idx = self.idx;
            self.fake.with(|s| {
                if s.fail_writes {
                    return Err(std::io::Error::new(ErrorKind::BrokenPipe, "gone"));
                }
                s.writes.push((idx, chunk));
                Ok(())
            })
        }

        fn error_text(&mut self, e: &std::io::Error) -> String {
            format!("{} failed: {e}", self.device)
        }

        fn close(self) {
            self.fake.with(|s| s.closed += 1);
        }
    }

    impl Opener for Fake {
        type Out = FakeOut;
        const SETTLE: Duration = Duration::ZERO;

        fn open(&mut self, device: &str, rate: u32, idx: usize) -> std::io::Result<FakeOut> {
            self.with(|s| {
                if s.refuse.iter().any(|d| d == device) {
                    return Err(std::io::Error::other("cannot start"));
                }
                s.opened.push(device.to_string());
                Ok(())
            })?;
            Ok(FakeOut { fake: self.clone(), device: device.to_string(), rate, idx })
        }
    }

    type Seen = Arc<Mutex<Vec<OutputStatus>>>;

    fn sink(keep_awake: bool) -> (SinkHandle, Receiver<SinkMsg>, Sink<Fake>, Fake, Seen) {
        let (h, rx) = SinkHandle::with_queue(Arc::new(AtomicU8::new(100)), keep_awake);
        let seen: Seen = Arc::default();
        let s2 = seen.clone();
        h.on_status(move |st| s2.locked().push(st));
        let fake = Fake::default();
        let sink = Sink::new(h.clone(), fake.clone());
        (h, rx, sink, fake, seen)
    }

    fn target(kind: &'static str, device: &str) -> Target {
        Target { kind, device: device.into(), label: format!("{device} label") }
    }

    /// 20 ms of stereo at 48 kHz.
    fn chunk(v: i16) -> Vec<i16> {
        vec![v; 1920]
    }

    fn encoded(data: &[i16], volume: u8) -> Vec<u8> {
        let mut out = Vec::new();
        pcm::encode(data, pcm::gain_q16(volume), &mut out);
        out
    }

    fn last(seen: &Seen) -> OutputStatus {
        seen.locked().last().cloned().expect("a status")
    }

    #[test]
    fn the_handle_counts_queued_audio_and_epochs() {
        let (h, rx, ..) = sink(false);
        assert!(h.send_pcm(0, RATE, chunk(1)));
        assert!(h.send_pcm_on(Channel::Quran, 0, RATE, chunk(1)));
        assert_eq!(h.queued_ms(), 40);
        h.dequeued(RATE, &chunk(1));
        h.dequeued(RATE, &chunk(1));
        h.dequeued(RATE, &chunk(1));
        assert_eq!(h.queued_ms(), 0, "never below zero");
        assert!(matches!(rx.try_recv(), Ok(SinkMsg::Pcm { channel: Channel::Broadcast, .. })));
        assert!(matches!(rx.try_recv(), Ok(SinkMsg::Pcm { channel: Channel::Quran, .. })));
        assert_eq!(h.new_epoch(), 1);
        assert_eq!(h.new_epoch(), 2);
        h.set_keep_awake(true);
        assert!(h.keep_awake.load(Ordering::Relaxed));
    }

    #[test]
    fn retargets_only_on_a_change() {
        let (h, rx, ..) = sink(false);
        h.retarget(vec![target("wired", "hw:0")]);
        h.retarget(vec![target("wired", "hw:0")]);
        h.reopen();
        assert!(matches!(rx.try_recv(), Ok(SinkMsg::Retarget(t)) if t.len() == 1));
        assert!(matches!(rx.try_recv(), Ok(SinkMsg::Reopen)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn control_messages_never_block_on_a_full_queue() {
        let (h, rx, ..) = sink(false);
        for _ in 0..64 {
            assert!(h.send_pcm(0, RATE, chunk(0)));
        }
        h.reopen(); // queue full: delivered from a helper thread
        for _ in 0..64 {
            assert!(matches!(rx.recv_timeout(Duration::from_secs(1)), Ok(SinkMsg::Pcm { .. })));
        }
        assert!(matches!(rx.recv_timeout(Duration::from_secs(1)), Ok(SinkMsg::Reopen)));
    }

    #[test]
    fn plays_on_the_preferred_output_at_each_channels_volume() {
        let (h, rx, mut s, fake, seen) = sink(false);
        let mut rate = RATE;
        h.retarget(vec![target("bluetooth", "bt"), target("wired", "hw:0")]);
        assert!(s.step(&rx, &mut rate));
        h.volume.store(50, Ordering::Relaxed);
        h.quran_volume.store(0, Ordering::Relaxed);
        h.send_pcm(0, RATE, chunk(1000));
        h.send_pcm_on(Channel::Quran, 0, RATE, chunk(1000));
        assert!(s.step(&rx, &mut rate));
        assert!(s.step(&rx, &mut rate));
        assert!(h.audible.load(Ordering::Relaxed));
        assert_eq!(h.queued_ms(), 0);
        fake.with(|f| {
            assert_eq!(f.opened, ["bt"]);
            assert_eq!(f.writes.len(), 2);
            assert_eq!(f.writes[0], (0, encoded(&chunk(1000), 50)));
            assert_eq!(f.writes[1], (0, encoded(&chunk(1000), 0)));
        });
        let st = last(&seen);
        assert_eq!((st.kind.as_str(), st.device.as_str(), st.label.as_str(), st.open), ("bluetooth", "bt", "bt label", true));
    }

    #[test]
    fn audio_from_an_old_epoch_is_dropped() {
        let (h, rx, mut s, fake, _) = sink(false);
        let mut rate = RATE;
        s.targets = vec![target("wired", "hw:0")];
        h.send_pcm(0, RATE, chunk(1));
        h.new_epoch();
        assert!(s.step(&rx, &mut rate));
        assert_eq!(h.queued_ms(), 0);
        assert!(fake.with(|f| f.writes.is_empty() && f.opened.is_empty()));
        assert!(!h.audible.load(Ordering::Relaxed));
    }

    #[test]
    fn a_new_rate_reopens_the_output() {
        let (_h, _rx, mut s, fake, _) = sink(false);
        s.targets = vec![target("wired", "hw:0")];
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        s.play(24_000, &chunk(1), Some(Channel::Broadcast));
        fake.with(|f| {
            assert_eq!(f.opened.len(), 2);
            assert_eq!(f.closed, 1);
        });
        assert_eq!(s.open.as_ref().map(Output::rate), Some(24_000));
    }

    #[test]
    fn falls_back_when_the_preferred_output_cannot_open() {
        for refuse in [true, false] {
            let (_h, _rx, mut s, fake, seen) = sink(false);
            s.targets = vec![target("bluetooth", "bt"), target("wired", "hw:0")];
            fake.with(|f| if refuse { f.refuse.push("bt".into()) } else { f.exit.push("bt".into()) });
            s.play(RATE, &chunk(1), Some(Channel::Broadcast));
            assert_eq!(s.open.as_ref().map(Output::idx), Some(1), "refuse={refuse}");
            assert_eq!(last(&seen).kind, "wired");
        }
    }

    #[test]
    fn all_outputs_failing_backs_off() {
        let (_h, _rx, mut s, fake, seen) = sink(false);
        s.targets = vec![target("wired", "hw:0")];
        fake.with(|f| f.refuse.push("hw:0".into()));
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        assert!(s.none_mode);
        assert_eq!(s.fail_streak, 1);
        assert!(s.retry_at > Instant::now());
        assert_eq!(last(&seen).error.as_deref(), Some("All audio outputs failed; retrying"));

        // Before the retry time: no new attempt, and real audio is paced in real time.
        fake.with(|f| f.refuse.clear());
        let t = Instant::now();
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        assert!(t.elapsed() >= Duration::from_millis(19));
        assert!(fake.with(|f| f.opened.is_empty()));
        // Silence isn't paced.
        let t = Instant::now();
        s.play(RATE, &chunk(0), None);
        assert!(t.elapsed() < Duration::from_millis(15));

        // A retarget resets the back-off and opens at once.
        s.restart_outputs();
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        assert_eq!(fake.with(|f| f.opened.clone()), ["hw:0"]);
        assert_eq!(s.fail_streak, 0);
    }

    #[test]
    fn no_outputs_at_all() {
        let (_h, _rx, mut s, _, seen) = sink(false);
        s.play(RATE, &chunk(0), None);
        let st = last(&seen);
        assert_eq!((st.kind.as_str(), st.label.as_str()), ("none", "No output"));
        assert_eq!(st.error.as_deref(), Some("No audio output available"));
    }

    #[test]
    fn a_failing_output_moves_to_the_next_then_backs_off() {
        let (_h, _rx, mut s, fake, seen) = sink(false);
        s.targets = vec![target("bluetooth", "bt"), target("wired", "hw:0")];
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        fake.with(|f| f.fail_writes = true);
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        assert!(s.open.is_none());
        assert_eq!(s.next_idx, 1);
        assert_eq!(last(&seen).error.as_deref(), Some("bt failed: gone"));

        s.play(RATE, &chunk(1), Some(Channel::Broadcast)); // opens hw:0, and its write fails too
        assert_eq!(s.next_idx, 0);
        assert_eq!(s.fail_streak, 1);
        assert!(s.retry_at > Instant::now());
        assert_eq!(last(&seen).error.as_deref(), Some("hw:0 failed: gone"));
        assert_eq!(fake.with(|f| f.closed), 2);
    }

    #[test]
    fn keep_awake_feeds_silence() {
        let (h, rx, mut s, fake, _) = sink(true);
        let mut rate = RATE;
        s.targets = vec![target("wired", "hw:0")];
        let mut mirror = h.mirror.subscribe();
        assert!(s.step(&rx, &mut rate)); // timeout → silence opens the output
        assert!(s.step(&rx, &mut rate)); // and keeps feeding it, without waiting
        fake.with(|f| {
            assert_eq!(f.writes.len(), 2);
            assert!(f.writes.iter().all(|(_, b)| b.len() == 1920 * 2 && b.iter().all(|&x| x == 0)));
        });
        assert!(mirror.try_recv().is_ok(), "listeners get a copy");
        assert!(!h.audible.load(Ordering::Relaxed));
    }

    #[test]
    fn recent_audio_keeps_the_output_fed_then_it_closes() {
        let (h, _rx, mut s, fake, seen) = sink(false);
        s.targets = vec![target("wired", "hw:0")];
        s.last_pcm = Some(Instant::now());
        h.audible.store(true, Ordering::Relaxed);
        s.idle(RATE);
        assert!(h.audible.load(Ordering::Relaxed), "still recent");
        assert_eq!(fake.with(|f| f.writes.len()), 1, "a gap is filled with silence");

        // Not recent, but within the idle time: stays open, nothing written.
        s.last_pcm = Instant::now().checked_sub(Duration::from_secs(5));
        s.idle(RATE);
        assert!(s.open.is_some());
        assert!(!h.audible.load(Ordering::Relaxed));

        s.last_pcm = Instant::now().checked_sub(Duration::from_secs(11));
        s.idle(RATE);
        assert!(s.open.is_none());
        let st = last(&seen);
        assert_eq!((st.kind.as_str(), st.label.as_str(), st.open), ("none", "Idle", false));
        let n = seen.locked().len();
        s.idle(RATE);
        assert_eq!(seen.locked().len(), n, "the same status isn't pushed twice");
    }

    #[test]
    fn reopen_and_retarget_messages_restart_outputs() {
        let (h, rx, mut s, fake, _) = sink(false);
        let mut rate = RATE;
        s.targets = vec![target("wired", "hw:0")];
        s.play(RATE, &chunk(1), Some(Channel::Broadcast));
        s.next_idx = 1;
        h.reopen();
        assert!(s.step(&rx, &mut rate));
        assert!(s.open.is_none());
        assert_eq!(s.next_idx, 0);
        h.retarget(vec![target("wired", "hw:1")]);
        assert!(s.step(&rx, &mut rate));
        assert_eq!(s.targets[0].device, "hw:1");
        assert_eq!(fake.with(|f| f.closed), 1);
    }

    #[test]
    fn stops_when_every_sender_has_gone() {
        let (h, rx) = SinkHandle::with_queue(Arc::new(AtomicU8::new(100)), false);
        let (other, _rx2) = SinkHandle::with_queue(Arc::new(AtomicU8::new(100)), false);
        let mut s = Sink::new(other, Fake::default());
        drop(h);
        let mut rate = RATE;
        assert!(!s.step(&rx, &mut rate));
        s.run(&rx);
    }

    #[test]
    fn the_real_output_is_aplay() {
        // aplay's "null" device, where alsa-utils is installed; otherwise only that it fails cleanly.
        let hb = AtomicU64::new(0);
        if let Ok(mut o) = AplayOpener.open("null", RATE, 3) {
            assert_eq!((Output::rate(&o), Output::idx(&o)), (RATE, 3));
            let _ = Output::write(&mut o, vec![0; 3840], &hb);
            let _ = Output::exited(&mut o);
            assert_eq!(Output::error_text(&mut o, &ErrorKind::TimedOut.into()), device::STALLED);
            Output::close(o);
        }
        if let Ok(mut o) = AplayOpener.open("no-such-device-livemasjid", RATE, 0) {
            std::thread::sleep(Duration::from_millis(200));
            let _ = Output::exited(&mut o);
            let _ = Output::error_text(&mut o, &ErrorKind::BrokenPipe.into());
            Output::close(o);
        }
    }

    #[test]
    fn channels_have_their_own_volume() {
        assert_eq!(volume_for(Channel::Broadcast, 40, 90), 40);
        assert_eq!(volume_for(Channel::Quran, 40, 90), 90);
    }

    #[test]
    fn retry_backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1, 2, 4), 4);
        assert_eq!(backoff_secs(2, 2, 4), 8);
        assert_eq!(backoff_secs(4, 2, 4), 30, "2 << 4 = 32, capped to 30");
        assert_eq!(backoff_secs(9, 2, 4), 30);
        assert_eq!(backoff_secs(1, 1, 5), 2);
        assert_eq!(backoff_secs(5, 1, 5), 30, "1 << 5 = 32, capped");
    }

    #[test]
    fn output_status_dedupe() {
        let a = OutputStatus { kind: "bluetooth".into(), device: "d".into(), label: "L".into(), open: true, error: None };
        let mut b = a.clone();
        assert!(same_output(&a, &b));
        b.error = Some("x".into());
        assert!(!same_output(&a, &b));
        b.error = None;
        b.open = false;
        assert!(!same_output(&a, &b));
        b.open = true;
        b.device = "other".into();
        assert!(!same_output(&a, &b));
    }
}
