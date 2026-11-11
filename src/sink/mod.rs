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
        let h2 = h.clone();
        std::thread::Builder::new().name("sink".into()).spawn(move || Sink::new(h2).run(&rx)).expect("spawn sink");
        h
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
        let mut cur = self.targets.lock().unwrap();
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
        match channel {
            Channel::Broadcast => &self.volume,
            Channel::Quran => &self.quran_volume,
        }
        .load(Ordering::Relaxed)
    }
}

struct Sink {
    h: SinkHandle,
    targets: Vec<Target>,
    open: Option<Aplay>,
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

impl Sink {
    fn new(h: SinkHandle) -> Self {
        Self {
            h,
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
        loop {
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
                    cur_rate = rate;
                    self.play(rate, &data, Some(channel));
                }
                Ok(SinkMsg::Retarget(t)) => {
                    self.targets = t;
                    self.restart_outputs();
                }
                Ok(SinkMsg::Reopen) => self.restart_outputs(),
                Err(RecvTimeoutError::Timeout) => self.idle(cur_rate),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.close();
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
        if self.open.as_ref().is_some_and(|o| o.rate != rate) {
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
            let (idx, err) = (o.idx, o.error_text(&e));
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
            match Aplay::spawn(&t.device, rate, idx) {
                Ok(mut o) => {
                    // aplay fails fast if the device is missing/busy: give it a moment.
                    std::thread::sleep(Duration::from_millis(120));
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
        self.retry_at = Instant::now() + Duration::from_secs((2u64 << self.fail_streak.min(4)).min(30));
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
            self.retry_at = Instant::now() + Duration::from_secs((1u64 << self.fail_streak.min(5)).min(30));
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
                let t = &self.targets[o.idx];
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
        if self
            .last_status
            .as_ref()
            .is_some_and(|l| l.device == st.device && l.open == st.open && l.error == st.error && l.label == st.label)
        {
            return;
        }
        self.last_status = Some(st.clone());
        if let Some(cb) = self.h.status_cb.get() {
            cb(st);
        }
    }
}
