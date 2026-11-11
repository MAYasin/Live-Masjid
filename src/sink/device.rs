//! One open output: an `aplay` child process. aplay is part of alsa-utils (always present on
//! Pi OS), speaks to any ALSA device including bluez-alsa Bluetooth speakers, and keeps our
//! binary free of C dependencies.
//!
//! Writes go through a small queue to a writer thread, so the sink thread never blocks on the
//! pipe: a blocked sink thread starves the systemd watchdog and gets the whole service killed.

use std::io::{ErrorKind, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// If the device accepts no audio for this long, it is stalled (e.g. a Bluetooth link that
/// stopped moving data without closing). We drop it and reopen rather than block.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// What the user sees when the output stalls.
pub(super) const STALLED: &str = "audio device stopped accepting data";

pub(super) struct Aplay {
    child: Child,
    tx: SyncSender<Vec<u8>>,
    /// Set by the writer thread when the pipe breaks.
    broken: Arc<AtomicBool>,
    pub rate: u32,
    /// Which target this is.
    pub idx: usize,
    /// aplay's last error line.
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

impl Aplay {
    /// Start aplay on ALSA `device` for interleaved stereo s16 at `rate`.
    pub fn spawn(device: &str, rate: u32, idx: usize) -> std::io::Result<Self> {
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
        let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("aplay has no stdin"))?;
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
        Ok(Self { child, tx, broken, rate, idx, stderr })
    }

    /// aplay fails fast when the device is missing or busy: its error, if it has already exited.
    pub fn exited(&mut self) -> Option<String> {
        matches!(self.child.try_wait(), Ok(Some(_))).then(|| self.stderr.lock().unwrap().trim().to_string())
    }

    /// Queue `chunk` for the device, waiting (at real-time pace) while it is full.
    /// Keeps `heartbeat` fresh while waiting.
    ///
    /// # Errors
    /// `TimedOut` when the device accepted nothing for [`STALL_TIMEOUT`]; `BrokenPipe` when aplay
    /// has gone.
    pub fn write(&mut self, mut chunk: Vec<u8>, heartbeat: &AtomicU64) -> std::io::Result<()> {
        let started = Instant::now();
        loop {
            if self.broken.load(Ordering::Relaxed) {
                return Err(ErrorKind::BrokenPipe.into());
            }
            match self.tx.try_send(chunk) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(ErrorKind::BrokenPipe.into()),
                Err(TrySendError::Full(c)) => {
                    if started.elapsed() > STALL_TIMEOUT {
                        return Err(std::io::Error::new(ErrorKind::TimedOut, "output stalled"));
                    }
                    chunk = c;
                    // The device drains ~1 chunk per 20 ms. Stay alive meanwhile.
                    heartbeat.store(crate::state::now_ms(), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    /// What went wrong after a failed write: aplay's own message if it has one.
    pub fn error_text(&mut self, e: &std::io::Error) -> String {
        if e.kind() == ErrorKind::TimedOut {
            return STALLED.to_string();
        }
        // Never wait unbounded: aplay may be stuck in the driver.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = self.stderr.lock().unwrap().trim().to_string();
        if s.is_empty() { e.to_string() } else { s }
    }

    /// Let aplay finish what it has, then make sure it is gone.
    pub fn close(self) {
        let Self { mut child, tx, .. } = self;
        drop(tx); // the writer thread finishes and closes aplay's stdin
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Killing aplay also unblocks a writer thread stuck in write().
        let _ = child.kill();
        let _ = child.wait();
    }
}
