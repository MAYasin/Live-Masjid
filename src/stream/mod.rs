//! One listening session: connect to the best available mount, buffer, decode, and ride
//! through network drop-outs without an audible gap.
//!
//! ```text
//!  socket ──pump──▶ [OggFilter] ──▶ ByteQueue (jitter buffer) ──▶ decoder thread ──▶ sink
//!     ▲                                    keeps playing while we reconnect
//!     └── reconnect (same codec preferred) ────────────────────────────────────┘
//! ```

mod buffer;
mod decode;
mod ogg;
pub mod recording;

pub use decode::Codec;

use crate::net::{OpenError, STREAM_HOSTS, StreamConn, open_stream};
use crate::sink::SinkHandle;
use buffer::ByteQueue;
use ogg::{Feed, OggFilter};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum StreamEvent {
    /// Audio is flowing (first start, or a reconnect succeeded).
    Started { url: String, codec: &'static str },
    /// Connection lost; buffered audio keeps playing while we reconnect.
    Reconnecting(String),
    /// The buffer grew far past normal (output slower than real time); skipped ahead this many seconds.
    CaughtUp(u64),
    /// Session over.
    Ended(EndReason),
}

#[derive(Debug, Clone)]
pub enum EndReason {
    /// Never got audio: no candidate mount is live.
    NotLive,
    /// Was playing; the mount stayed gone for the whole grace period.
    Finished,
    /// Stop flag raised by the player.
    Stopped,
}

pub struct SessionParams {
    pub mount: String,
    /// "auto" | "opus" | "mp3"
    pub codec: String,
    pub sink: SinkHandle,
    pub epoch: u64,
    pub stop: Arc<AtomicBool>,
    pub events: UnboundedSender<StreamEvent>,
    /// How long to keep retrying before the first audio.
    pub start_grace: Duration,
    /// Buffered audio in ms, published for the UI.
    pub buffer_ms: Arc<AtomicU64>,
}

/// Total compressed audio bytes received from the stream servers (for the System page).
pub static BYTES_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// After a live signal the MP3 mirror / relay can lag by a few seconds.
pub const START_GRACE: Duration = Duration::from_secs(12);
/// Normal buffer is the ~20 s Icecast burst. If it grows past MAX (the output played slower than
/// real time for a while), skip ahead to TARGET so we never drift minutes behind live.
const MAX_BUFFER_MS: u64 = 60_000;
const TARGET_BUFFER_MS: u64 = 20_000;

/// How long a dropped broadcast may stay unreachable before we call it finished.
const DROP_GRACE: Duration = Duration::from_secs(30);

pub fn candidates(mount: &str, codec: &str) -> Vec<(&'static str, u16, String)> {
    let base = mount.trim_start_matches('/');
    let (ogg, mp3) = match base.strip_suffix(".mp3") {
        Some(b) => (b.to_string(), base.to_string()),
        None => (base.to_string(), format!("{base}.mp3")),
    };
    let order = if codec == "mp3" { [&mp3, &ogg] } else { [&ogg, &mp3] };
    STREAM_HOSTS
        .iter()
        .flat_map(|(h, p)| order.iter().map(move |m| (*h, *p, format!("/{m}"))))
        .collect()
}

fn codec_of(c: &StreamConn) -> Codec {
    let ct = &c.content_type;
    if ct.contains("mpeg") || ct.contains("mp3") || (ct.is_empty() && c.url.ends_with(".mp3")) {
        Codec::Mp3
    } else {
        Codec::Opus
    }
}

fn path_codec(path: &str) -> Codec {
    if path.ends_with(".mp3") { Codec::Mp3 } else { Codec::Opus }
}

pub fn run_session(p: SessionParams) {
    let cands = candidates(&p.mount, &p.codec);
    let mut conn = match connect(&p, &cands, p.start_grace, None) {
        Ok(c) => c,
        Err(reason) => return end(&p, reason),
    };
    let mut pipe = Pipeline::start(codec_of(&conn), &p, conn.url.clone());
    let mut carry: Option<Vec<u8>> = None;
    loop {
        match pump(&mut conn, &mut pipe, &p, carry.take()) {
            Pumped::Stopped => {
                pipe.finish();
                return end(&p, EndReason::Stopped);
            }
            Pumped::NewStream(rest) => {
                // Encoder restarted upstream: let the old decoder play out, start fresh.
                tracing::info!("stream restarted upstream; new decoder");
                pipe.finish();
                pipe = Pipeline::start(Codec::Opus, &p, conn.url.clone());
                carry = Some(rest);
            }
            Pumped::Dropped(why) => {
                let buffered = p.buffer_ms.load(Ordering::Relaxed) / 1000;
                let _ = p.events.send(StreamEvent::Reconnecting(format!("{why}; {buffered} s buffered")));
                match connect(&p, &cands, DROP_GRACE, Some(pipe.codec)) {
                    Ok(c) if codec_of(&c) == pipe.codec => {
                        pipe.filter.reconnected();
                        let _ = p.events.send(StreamEvent::Started { url: c.url.clone(), codec: pipe.codec.name() });
                        conn = c;
                    }
                    Ok(c) => {
                        pipe.finish();
                        pipe = Pipeline::start(codec_of(&c), &p, c.url.clone());
                        conn = c;
                    }
                    Err(EndReason::Stopped) => {
                        pipe.finish();
                        return end(&p, EndReason::Stopped);
                    }
                    Err(_) => {
                        pipe.finish(); // play out whatever is still buffered
                        return end(&p, EndReason::Finished);
                    }
                }
            }
        }
    }
}

fn end(p: &SessionParams, reason: EndReason) {
    p.buffer_ms.store(0, Ordering::Relaxed);
    let _ = p.events.send(StreamEvent::Ended(reason));
}

/// Jitter buffer + decoder thread for one logical stream.
struct Pipeline {
    codec: Codec,
    queue: Arc<ByteQueue>,
    filter: OggFilter,
    decoder: JoinHandle<()>,
}

impl Pipeline {
    fn start(codec: Codec, p: &SessionParams, url: String) -> Self {
        let queue = ByteQueue::new();
        let events = p.events.clone();
        let ctx = decode::DecodeCtx {
            queue: queue.clone(),
            stop: p.stop.clone(),
            sink: p.sink.clone(),
            epoch: p.epoch,
            buffer_ms: p.buffer_ms.clone(),
            on_first_audio: Box::new(move || {
                let _ = events.send(StreamEvent::Started { url, codec: codec.name() });
            }),
            paused: None,
            position_ms: None,
        };
        let decoder = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decode::run(codec, ctx))
            .expect("spawn decoder");
        Self { codec, queue, filter: OggFilter::new(), decoder }
    }

    /// Close the buffer and wait for the decoder to play out what's left (or stop).
    fn finish(self) {
        self.queue.close();
        let _ = self.decoder.join();
    }
}

enum Pumped {
    Stopped,
    Dropped(String),
    NewStream(Vec<u8>),
}

/// Move bytes from the socket into the jitter buffer until the connection breaks.
fn pump(conn: &mut StreamConn, pipe: &mut Pipeline, p: &SessionParams, carry: Option<Vec<u8>>) -> Pumped {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pages = Vec::new();
    let mut deliver = |data: &[u8], pipe: &mut Pipeline| -> Option<Pumped> {
        match pipe.codec {
            Codec::Mp3 => (!pipe.queue.push(data.to_vec(), &p.stop)).then_some(Pumped::Stopped),
            Codec::Opus => {
                if let Feed::NewStream(rest) = pipe.filter.feed(data, &mut pages) {
                    return Some(Pumped::NewStream(rest));
                }
                for page in pages.drain(..) {
                    if !pipe.queue.push(page, &p.stop) {
                        return Some(Pumped::Stopped);
                    }
                }
                None
            }
        }
    };
    if let Some(bytes) = carry {
        if let Some(r) = deliver(&bytes, pipe) {
            return r;
        }
    }
    loop {
        if p.stop.load(Ordering::Relaxed) {
            return Pumped::Stopped;
        }
        match conn.reader.read(&mut buf) {
            Ok(0) => return Pumped::Dropped("stream closed by server".into()),
            Ok(n) => {
                BYTES_RECEIVED.fetch_add(n as u64, Ordering::Relaxed);
                if let Some(r) = deliver(&buf[..n], pipe) {
                    return r;
                }
                catch_up(pipe, p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Pumped::Dropped(format!("no data ({})", e.kind())),
        }
    }
}

/// Bound latency behind live (see MAX_BUFFER_MS).
fn catch_up(pipe: &Pipeline, p: &SessionParams) {
    let ms = p.buffer_ms.load(Ordering::Relaxed);
    if ms <= MAX_BUFFER_MS {
        return;
    }
    let keep = (pipe.queue.buffered() as u64 * TARGET_BUFFER_MS / ms) as usize;
    if pipe.queue.trim_front(keep) > 0 {
        p.buffer_ms.store(TARGET_BUFFER_MS, Ordering::Relaxed);
        let _ = p.events.send(StreamEvent::CaughtUp((ms - TARGET_BUFFER_MS) / 1000));
    }
}

/// Connect to the first live candidate, retrying for up to `grace`.
fn connect(
    p: &SessionParams,
    cands: &[(&'static str, u16, String)],
    grace: Duration,
    prefer: Option<Codec>,
) -> Result<StreamConn, EndReason> {
    let mut order: Vec<_> = cands.iter().collect();
    if let Some(c) = prefer {
        order.sort_by_key(|(_, _, path)| path_codec(path) != c);
    }
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        let mut last_err = None;
        for (host, port, path) in &order {
            if p.stop.load(Ordering::Relaxed) {
                return Err(EndReason::Stopped);
            }
            match open_stream(host, *port, path) {
                Ok(c) => return Ok(c),
                Err(OpenError::NotFound) => {}
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if started.elapsed() >= grace {
            if let Some(e) = last_err {
                tracing::warn!("stream connect failed: {e}");
            }
            return Err(EndReason::NotLive);
        }
        attempt += 1;
        // Quick first retries (a fresh mount appears within seconds), then ease off:
        // each round can be up to four requests.
        let wait = match attempt {
            0..=2 => 1000,
            3..=5 => 2000,
            _ => 4000,
        };
        let until = Instant::now() + Duration::from_millis(wait);
        while Instant::now() < until {
            if p.stop.load(Ordering::Relaxed) {
                return Err(EndReason::Stopped);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn candidate_order() {
        let c = super::candidates("lakewood", "auto");
        assert_eq!(c[0], ("livemasjid.com", 8000, "/lakewood".to_string()));
        assert_eq!(c[1].2, "/lakewood.mp3");
        assert_eq!(c[2].0, "relay2.livemasjid.com");
        assert_eq!(super::candidates("lakewood", "mp3")[0].2, "/lakewood.mp3");
        assert_eq!(super::candidates("annoor.mp3", "auto")[0].2, "/annoor");
    }
}
