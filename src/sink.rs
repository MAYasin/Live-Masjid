//! Audio output thread.
//!
//! PCM (48 kHz, stereo, s16) is piped into an `aplay` child process. aplay is part of
//! alsa-utils (always present on Pi OS), speaks to any ALSA device including
//! bluez-alsa Bluetooth speakers, and keeps our binary free of C dependencies.
//!
//! While waiting for a stream the sink keeps the device open and feeds silence
//! ("keep awake"), so when audio arrives it is heard immediately – no ALSA open,
//! no Bluetooth A2DP transport start-up, no speaker auto-standby.

use crate::state::OutputStatus;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// "bluetooth" | "wired"
    pub kind: &'static str,
    /// ALSA PCM name passed to `aplay -D`.
    pub device: String,
    pub label: String,
}

pub enum SinkMsg {
    /// Interleaved stereo s16 at `rate`.
    Pcm { epoch: u64, rate: u32, data: Vec<i16> },
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
}

impl SinkHandle {
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
        };
        let h2 = h.clone();
        std::thread::Builder::new()
            .name("sink".into())
            .spawn(move || Sink::new(h2, volume).run(rx))
            .expect("spawn sink");
        h
    }

    pub fn on_status(&self, cb: impl Fn(OutputStatus) + Send + Sync + 'static) {
        let _ = self.status_cb.set(Box::new(cb));
    }

    pub fn set_keep_awake(&self, on: bool) {
        self.keep_awake.store(on, Ordering::Relaxed);
    }

    /// Blocking send of PCM (applies back-pressure to the decoder). Returns false if the sink is gone.
    pub fn send_pcm(&self, epoch: u64, rate: u32, data: Vec<i16>) -> bool {
        self.tx.send(SinkMsg::Pcm { epoch, rate, data }).is_ok()
    }

    /// Start a new epoch: anything queued from before is dropped. Returns the new epoch.
    pub fn new_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn retarget(&self, targets: Vec<Target>) {
        let mut cur = self.targets.lock().unwrap();
        if *cur != targets {
            *cur = targets.clone();
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
}

/// If the device accepts no audio for this long, it is stalled (e.g. a Bluetooth link that
/// stopped moving data without closing). We drop it and reopen rather than block: a blocked
/// sink thread starves the systemd watchdog and gets the whole service killed.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// An open `aplay`. Writes go through a small queue to a writer thread, so the sink thread
/// itself never blocks on the pipe.
struct Open {
    child: Child,
    tx: SyncSender<Vec<u8>>,
    /// Set by the writer thread when the pipe breaks.
    broken: Arc<AtomicBool>,
    rate: u32,
    idx: usize,
    stderr: Arc<Mutex<String>>,
}

fn spawn_writer(mut stdin: ChildStdin, broken: Arc<AtomicBool>) -> SyncSender<Vec<u8>> {
    // ~4 chunks (~80 ms) in flight on top of the 64 KB pipe buffer.
    let (tx, rx) = sync_channel::<Vec<u8>>(4);
    std::thread::Builder::new()
        .name("aplay-writer".into())
        .spawn(move || {
            for buf in rx {
                if stdin.write_all(&buf).is_err() {
                    broken.store(true, Ordering::Relaxed);
                    return;
                }
            }
        })
        .expect("spawn writer");
    tx
}

struct Sink {
    h: SinkHandle,
    volume: Arc<AtomicU8>,
    targets: Vec<Target>,
    open: Option<Open>,
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
    fn new(h: SinkHandle, volume: Arc<AtomicU8>) -> Self {
        Self {
            h,
            volume,
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

    fn run(mut self, rx: Receiver<SinkMsg>) {
        let mut cur_rate = RATE;
        loop {
            self.h.heartbeat.store(crate::state::now_ms(), Ordering::Relaxed);
            // While feeding silence, don't wait: write it back-to-back and let the device pace
            // us (play() blocks while the device buffer is full). Waiting 25 ms before each
            // 20 ms chunk delivered only ~80% of real time, so idle speakers kept under-running.
            let feeding = self.open.is_some()
                && (self.h.keep_awake.load(Ordering::Relaxed)
                    || self.last_pcm.is_some_and(|t| t.elapsed() < Duration::from_secs(3)));
            let wait = if feeding { Duration::from_millis(1) } else { Duration::from_millis(25) };
            match rx.recv_timeout(wait) {
                Ok(SinkMsg::Pcm { epoch, .. }) if epoch != self.h.epoch.load(Ordering::SeqCst) => {}
                Ok(SinkMsg::Pcm { rate, data, .. }) => {
                    self.last_pcm = Some(Instant::now());
                    self.h.audible.store(true, Ordering::Relaxed);
                    cur_rate = rate;
                    self.play(rate, &data, true);
                }
                Ok(SinkMsg::Retarget(t)) => {
                    self.targets = t;
                    self.fail_streak = 0;
                    self.close();
                    self.next_idx = 0;
                    self.retry_at = Instant::now();
                }
                Ok(SinkMsg::Reopen) => {
                    self.fail_streak = 0;
                    self.close();
                    self.next_idx = 0;
                    self.retry_at = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {
                    let recent = self.last_pcm.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
                    if !recent {
                        self.h.audible.store(false, Ordering::Relaxed);
                    }
                    if self.h.keep_awake.load(Ordering::Relaxed) || recent {
                        // Feed silence: keeps device + Bluetooth transport warm, avoids underrun clicks.
                        let s = std::mem::take(&mut self.silence);
                        self.play(cur_rate, &s, false);
                        self.silence = s;
                    } else if self.open.is_some()
                        && self.last_pcm.is_none_or(|t| t.elapsed() > Duration::from_secs(10))
                    {
                        self.close();
                        self.report(None);
                    } else if self.open.is_none() && !self.none_mode {
                        self.report(None);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.close();
    }

    fn play(&mut self, rate: u32, data: &[i16], real: bool) {
        if self.open.as_ref().is_some_and(|o| o.rate != rate) {
            self.close();
        }
        if self.open.is_none() {
            self.try_open(rate);
        }
        let Some(o) = self.open.as_mut() else {
            // No output: pace real audio in real time; drop silence.
            if real {
                std::thread::sleep(Duration::from_micros(data.len() as u64 * 500_000 / rate as u64));
            }
            return;
        };
        // Perceptual volume curve (squared), Q16 fixed point.
        let v = self.volume.load(Ordering::Relaxed).min(100) as u32;
        let gain = (v * v * 65536 / 10_000) as i32;
        self.bytes.clear();
        if gain == 65536 || !real {
            for s in data {
                self.bytes.extend_from_slice(&s.to_le_bytes());
            }
        } else {
            for s in data {
                let x = ((*s as i32 * gain) >> 16) as i16;
                self.bytes.extend_from_slice(&x.to_le_bytes());
            }
        }
        let idx = o.idx;
        let mut chunk = std::mem::take(&mut self.bytes);
        let started = Instant::now();
        let outcome = loop {
            if o.broken.load(Ordering::Relaxed) {
                break Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            match o.tx.try_send(chunk) {
                Ok(()) => break Ok(()),
                Err(TrySendError::Disconnected(_)) => break Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
                Err(TrySendError::Full(c)) => {
                    if started.elapsed() > STALL_TIMEOUT {
                        break Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "output stalled"));
                    }
                    chunk = c;
                    // Real-time pacing: the device drains ~1 chunk per 20 ms. Stay alive meanwhile.
                    self.h.heartbeat.store(crate::state::now_ms(), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        self.bytes = Vec::with_capacity(16 * 1024);
        if let Err(e) = outcome {
            let stalled = e.kind() == std::io::ErrorKind::TimedOut;
            let err = if stalled { "audio device stopped accepting data".to_string() } else { self.child_error(e) };
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
            match spawn_aplay(&t.device, rate) {
                Ok(mut o) => {
                    // aplay fails fast if the device is missing/busy: give it a moment.
                    std::thread::sleep(Duration::from_millis(120));
                    if let Ok(Some(_)) = o.child.try_wait() {
                        let msg = o.stderr.lock().unwrap().trim().to_string();
                        if self.fail_streak == 0 {
                            tracing::warn!("output {} failed: {msg}", t.device);
                        }
                        continue;
                    }
                    o.idx = idx;
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

    fn child_error(&mut self, e: std::io::Error) -> String {
        if let Some(o) = self.open.as_mut() {
            // Never wait unbounded: aplay may be stuck in the driver.
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline && matches!(o.child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let s = o.stderr.lock().unwrap().trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
        e.to_string()
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
        if let Some(mut o) = self.open.take() {
            drop(o.tx); // writer thread finishes and closes aplay's stdin
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = o.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // Killing aplay also unblocks a writer thread stuck in write().
            let _ = o.child.kill();
            let _ = o.child.wait();
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
        if self.last_status.as_ref().is_some_and(|l| {
            l.device == st.device && l.open == st.open && l.error == st.error && l.label == st.label
        }) {
            return;
        }
        self.last_status = Some(st.clone());
        if let Some(cb) = self.h.status_cb.get() {
            cb(st);
        }
    }
}

fn spawn_aplay(device: &str, rate: u32) -> std::io::Result<Open> {
    let mut child = Command::new("aplay")
        .args(["-q", "-t", "raw", "-f", "S16_LE", "-c", "2"])
        .arg("-r")
        .arg(rate.to_string())
        .arg("-D")
        .arg(device)
        // 250 ms device buffer: low latency but robust on a busy Pi Zero.
        .args(["--buffer-time=250000", "--period-time=50000", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("stdin");
    let broken = Arc::new(AtomicBool::new(false));
    let tx = spawn_writer(stdin, broken.clone());
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = child.stderr.take() {
        let buf = stderr.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            *buf.lock().unwrap() = s.lines().last().unwrap_or("").to_string();
        });
    }
    Ok(Open { child, tx, broken, rate, idx: 0, stderr })
}

/// A short two-tone chime used by "Test speaker".
pub fn chime() -> Vec<i16> {
    let mut v = Vec::with_capacity(RATE as usize * 2);
    for (freq, dur) in [(660.0f32, 0.25f32), (880.0, 0.45)] {
        let n = (RATE as f32 * dur) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = (1.0 - i as f32 / n as f32).powf(1.5) * (i as f32 / 400.0).min(1.0);
            let s = ((t * freq * std::f32::consts::TAU).sin() * env * 9000.0) as i16;
            v.push(s);
            v.push(s);
        }
    }
    v
}
