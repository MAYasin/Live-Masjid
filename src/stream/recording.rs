//! Playing a past recording on the Pi speaker.
//!
//! `curl` streams the file (HTTPS via the system's TLS) into the same jitter buffer and
//! decoders the live stream uses. Supports pause/resume and reports the position.

use super::buffer::ByteQueue;
use super::decode::{self, Codec, DecodeCtx};
use crate::sink::SinkHandle;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum RecordingEvent {
    Started,
    /// Finished playing (`None`) or failed (`Some(reason)`).
    Ended(Option<String>),
}

pub struct RecordingParams {
    pub url: String,
    pub sink: SinkHandle,
    pub epoch: u64,
    pub stop: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub position_ms: Arc<AtomicU64>,
    pub events: UnboundedSender<RecordingEvent>,
}

fn codec_for(url: &str) -> Codec {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".ogg") || path.ends_with(".opus") || path.ends_with(".oga") { Codec::Opus } else { Codec::Mp3 }
}

pub fn run(p: RecordingParams) {
    let child = Command::new("curl")
        .args(["-sS", "-L", "--fail", "--max-redirs", "3", "--proto", "=https,http", "--max-time", "21600"])
        .args(["-A", crate::integrations::USER_AGENT, &p.url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = p.events.send(RecordingEvent::Ended(Some(format!("cannot start download: {e}"))));
            return;
        }
    };
    let mut body = child.stdout.take().expect("stdout");
    let queue = ByteQueue::new();

    // Download thread: file → jitter buffer (blocks while the buffer is full = natural pacing).
    let feeder = {
        let (queue, stop) = (queue.clone(), p.stop.clone());
        std::thread::Builder::new()
            .name("rec-download".into())
            .spawn(move || {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match body.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if !queue.push(buf[..n].to_vec(), &stop) {
                                break;
                            }
                        }
                    }
                }
                queue.close();
            })
            .expect("spawn download thread")
    };

    let events = p.events.clone();
    let ctx = DecodeCtx {
        queue: queue.clone(),
        stop: p.stop.clone(),
        sink: p.sink.clone(),
        epoch: p.epoch,
        buffer_ms: Arc::new(AtomicU64::new(0)),
        on_first_audio: Box::new(move || {
            let _ = events.send(RecordingEvent::Started);
        }),
        paused: Some(p.paused.clone()),
        position_ms: Some(p.position_ms.clone()),
        position_base_ms: 0,
    };
    decode::run(codec_for(&p.url), ctx);

    // Done (or stopped): make sure the download ends too.
    queue.close();
    let _ = child.kill();
    let status = child.wait();
    let _ = feeder.join();
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let failed = !p.stop.load(Ordering::Relaxed)
        && p.position_ms.load(Ordering::Relaxed) == 0
        && status.map(|s| !s.success()).unwrap_or(true);
    let reason = failed.then(|| {
        let e = err.trim();
        if e.is_empty() { "could not download the recording".to_string() } else { e.lines().last().unwrap_or(e).to_string() }
    });
    let _ = p.events.send(RecordingEvent::Ended(reason));
}

#[cfg(test)]
mod tests {
    #[test]
    fn codec_from_extension() {
        assert_eq!(super::codec_for("https://x/a.mp3?x=1"), super::Codec::Mp3);
        assert_eq!(super::codec_for("https://x/a.OGG"), super::Codec::Opus);
        assert_eq!(super::codec_for("https://x/download?id=3"), super::Codec::Mp3);
    }
}
