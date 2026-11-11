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
mod edge;
mod ogg;
pub mod local;
pub mod recording;
pub mod source;
mod stretch;

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
pub use source::{Events, Source};

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
    pub events: Events<StreamEvent>,
    /// How long to keep retrying before the first audio.
    pub start_grace: Duration,
    /// Buffered audio in ms, published for the UI.
    pub buffer_ms: Arc<AtomicU64>,
}

/// Total compressed audio bytes received from the stream servers (for the System page).
pub static BYTES_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// After a live signal the MP3 mirror / relay can lag by a few seconds.
pub const START_GRACE: Duration = Duration::from_secs(12);
/// How far behind the live edge playback starts and is held, in ms (setting `live_delay_s`).
/// Smaller is closer to live; larger rides out longer network drop-outs without a gap.
pub static LIVE_DELAY_MS: AtomicU64 = AtomicU64::new(20_000);

fn live_delay_ms() -> u64 {
    LIVE_DELAY_MS.load(Ordering::Relaxed)
}

/// How long a dropped broadcast may stay unreachable before we call it finished.
const DROP_GRACE: Duration = Duration::from_secs(30);
/// While no stream server answers at all (our own network is down), keep retrying this long.
const NETWORK_GRACE: Duration = Duration::from_secs(10 * 60);

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

fn run_session(p: SessionParams) {
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
                pipe.fresh = false; // a new encoder: no burst to skip
                carry = Some(rest);
            }
            Pumped::Dropped(why) => {
                let buffered = p.buffer_ms.load(Ordering::Relaxed) / 1000;
                let _ = p.events.send(StreamEvent::Reconnecting(format!("{why}; {buffered} s buffered")));
                match connect(&p, &cands, DROP_GRACE, Some(pipe.codec)) {
                    Ok(c) if codec_of(&c) == pipe.codec => {
                        pipe.filter.reconnected();
                        // Ogg pages resume exactly where we stopped; MP3 can't, so jump to live.
                        pipe.fresh = pipe.codec == Codec::Mp3;
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

impl Source for SessionParams {
    const THREAD: &'static str = "stream";

    fn run(self) {
        run_session(self);
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
    /// The next connection's burst is cut to the live edge (fresh start, or MP3 reconnect).
    fresh: bool,
    /// When we last joined at the live edge.
    started: Instant,
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
            position_base_ms: 0,
            speed: None,
            channel: crate::sink::Channel::Broadcast,
        };
        let decoder = std::thread::Builder::new()
            .name("decoder".into())
            .spawn(move || decode::run(codec, ctx))
            .expect("spawn decoder");
        Self { codec, queue, filter: OggFilter::new(), decoder, fresh: true, started: Instant::now() }
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
    if let Some(bytes) = carry
        && let Some(r) = deliver(&bytes, pipe) {
            return r;
        }
    if std::mem::take(&mut pipe.fresh) {
        let raw = match read_burst(conn, p) {
            Ok(raw) => raw,
            Err(r) => return r,
        };
        let delay = Duration::from_millis(live_delay_ms());
        pipe.started = Instant::now();
        match pipe.codec {
            Codec::Mp3 => {
                let start = edge::mp3_start(&raw, delay);
                tracing::info!("joined near live: kept {} of {} burst bytes", raw.len() - start, raw.len());
                if let Some(r) = deliver(&raw[start..], pipe) {
                    return r;
                }
            }
            Codec::Opus => {
                let mut all = Vec::new();
                if let Feed::NewStream(rest) = pipe.filter.feed(&raw, &mut all) {
                    return Pumped::NewStream(rest);
                }
                let (total, n) = (all.len(), raw.len());
                let kept = edge::opus_pages(all, delay);
                tracing::info!("joined near live: kept {} of {total} pages ({n} burst bytes)", kept.len());
                for page in kept {
                    if !pipe.queue.push(page, &p.stop) {
                        return Pumped::Stopped;
                    }
                }
            }
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

/// Read the connect burst: everything that arrives faster than real time (Icecast sends it
/// in well under a second), stopping at the first read that has to wait for new audio.
fn read_burst(conn: &mut StreamConn, p: &SessionParams) -> Result<Vec<u8>, Pumped> {
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let started = Instant::now();
    loop {
        if p.stop.load(Ordering::Relaxed) {
            return Err(Pumped::Stopped);
        }
        let t = Instant::now();
        match conn.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                BYTES_RECEIVED.fetch_add(n as u64, Ordering::Relaxed);
                let had = !raw.is_empty();
                raw.extend_from_slice(&buf[..n]);
                // Real-time Opus pages arrive ~1/s; a burst may pause briefly on Wi-Fi.
                let waited = t.elapsed() > Duration::from_millis(400);
                if (had && waited) || started.elapsed() > Duration::from_secs(3) || raw.len() > 2 * 1024 * 1024 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if raw.is_empty() => return Err(Pumped::Dropped(format!("no data ({})", e.kind()))),
            Err(_) => break,
        }
    }
    Ok(raw)
}

/// Hold latency near the live-delay target: after a drop-out the buffer refills with the
/// audio we missed, putting us further behind; past target + slack, skip ahead to target.
fn catch_up(pipe: &mut Pipeline, p: &SessionParams) {
    // The buffer estimate needs a few seconds of decoded audio to settle (after joining, and
    // after a skip: acting on the stale figure skipped twice in a row, 16 s then 17 s).
    if pipe.started.elapsed() < Duration::from_secs(8) {
        return;
    }
    let ms = p.buffer_ms.load(Ordering::Relaxed);
    let target = live_delay_ms();
    if ms <= target + (target / 2).max(3_000) {
        return;
    }
    // Only the compressed part can be trimmed; the decoded part is already queued for the sink.
    let decoded = p.sink.queued_ms().min(ms);
    let keep = (pipe.queue.buffered() as u64 * target.saturating_sub(decoded) / (ms - decoded).max(1)) as usize;
    tracing::info!("catch-up: buffer {ms} ms (decoded {decoded} ms, {} bytes queued), keeping {keep} bytes", pipe.queue.buffered());
    if pipe.queue.trim_front(keep) > 0 {
        pipe.started = Instant::now();
        p.buffer_ms.store(target, Ordering::Relaxed);
        let _ = p.events.send(StreamEvent::CaughtUp((ms - target) / 1000));
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
    let mut told_offline = false;
    loop {
        let mut last_err = None;
        // Did any server answer "not live"? If none answered at all, the broadcast may well
        // still be on: it is our network (Wi-Fi, DNS) that is down.
        let mut answered = false;
        for (host, port, path) in &order {
            if p.stop.load(Ordering::Relaxed) {
                return Err(EndReason::Stopped);
            }
            match open_stream(host, *port, path) {
                Ok(c) => return Ok(c),
                Err(OpenError::NotFound) => answered = true,
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if started.elapsed() >= grace {
            // Mid-broadcast and no server reachable: keep retrying (quickly) until the network
            // is back, instead of calling the broadcast over and waiting for a status check.
            let offline = prefer.is_some() && !answered && started.elapsed() < NETWORK_GRACE;
            if offline {
                if !told_offline {
                    told_offline = true;
                    let e = last_err.as_deref().unwrap_or("no response");
                    tracing::warn!("network unreachable ({e}); still retrying the stream");
                }
            } else {
                if let Some(e) = last_err {
                    tracing::warn!("stream connect failed: {e}");
                }
                return Err(EndReason::NotLive);
            }
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
