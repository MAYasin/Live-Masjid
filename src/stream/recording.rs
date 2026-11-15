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
use super::source::{Events, Source};

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
    pub events: Events<RecordingEvent>,
}

fn codec_for(url: &str) -> Codec {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".ogg") || path.ends_with(".opus") || path.ends_with(".oga") { Codec::Opus } else { Codec::Mp3 }
}

impl Source for RecordingParams {
    const THREAD: &'static str = "recording";

    fn run(self) {
        run(self);
    }
}

fn run(p: RecordingParams) {
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
        speed: None,
        channel: crate::sink::Channel::Broadcast,
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

    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// A one-shot HTTP server on 127.0.0.1 answering with `response`. Returns its port.
    fn serve(response: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            let mut b = [0u8; 1024];
            while let Ok(n) = sock.read(&mut b) {
                req.extend_from_slice(&b[..n]);
                if n == 0 || req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock.write_all(&response);
        });
        port
    }

    fn params(app: &crate::state::App, url: String) -> (RecordingParams, tokio::sync::mpsc::UnboundedReceiver<(u64, RecordingEvent)>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let p = RecordingParams {
            url,
            sink: app.sink.clone(),
            epoch: 0,
            stop: Arc::default(),
            paused: Arc::default(),
            position_ms: Arc::default(),
            events: Events::new(1, &tx),
        };
        (p, rx)
    }

    fn events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, RecordingEvent)>) -> Vec<RecordingEvent> {
        std::iter::from_fn(|| rx.try_recv().ok().map(|(_, e)| e)).collect()
    }

    fn temp_app() -> (std::path::PathBuf, crate::state::App) {
        let dir = std::env::temp_dir().join(format!("lm-rec-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    #[test]
    fn plays_a_downloaded_recording_to_the_end() {
        let (dir, app) = temp_app();
        let mut response = b"HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\n\r\n".to_vec();
        response.extend(super::super::decode::tests::mp3_frames(40));
        let port = serve(response);
        let (p, mut rx) = params(&app, format!("http://127.0.0.1:{port}/rec.mp3"));
        let position = p.position_ms.clone();
        Source::run(p);
        let ev = events(&mut rx);
        assert!(matches!(ev.as_slice(), [RecordingEvent::Started, RecordingEvent::Ended(None)]), "{ev:?}");
        assert!(position.load(Ordering::Relaxed) > 900);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_download_reports_curls_reason() {
        let (dir, app) = temp_app();
        let port = serve(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec());
        let (p, mut rx) = params(&app, format!("http://127.0.0.1:{port}/gone.mp3"));
        run(p);
        match events(&mut rx).as_slice() {
            [RecordingEvent::Ended(Some(why))] => assert!(why.contains("404"), "{why}"),
            other => panic!("unexpected events: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_a_recording_is_not_a_failure() {
        let (dir, app) = temp_app();
        // A server that never answers: curl waits, the stop ends the session.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (p, mut rx) = params(&app, format!("http://127.0.0.1:{port}/slow.ogg"));
        let stop = p.stop.clone();
        let t0 = Instant::now();
        let player = std::thread::spawn(move || run(p));
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        player.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(3));
        assert!(matches!(events(&mut rx).as_slice(), [RecordingEvent::Ended(None)]));
        drop(listener);
        let _ = std::fs::remove_dir_all(dir);
    }
}
