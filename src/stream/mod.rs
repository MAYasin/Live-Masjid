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
pub mod local;
mod ogg;
pub mod recording;
pub mod source;
mod stretch;

pub use decode::Codec;

use crate::net::{OpenError, STREAM_HOSTS, StreamConn, open_stream};
use crate::sink::SinkHandle;
use buffer::ByteQueue;
use ogg::{Feed, OggFilter};
pub use source::{Events, Source};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    STREAM_HOSTS.iter().flat_map(|(h, p)| order.iter().map(move |m| (*h, *p, format!("/{m}")))).collect()
}

fn codec_of(c: &StreamConn) -> Codec {
    codec_for(&c.content_type, &c.url)
}

/// Which decoder to use for a response: its content type, or the path when it is missing.
fn codec_for(content_type: &str, url: &str) -> Codec {
    if content_type.contains("mpeg") || content_type.contains("mp3") || (content_type.is_empty() && url.ends_with(".mp3")) {
        Codec::Mp3
    } else {
        Codec::Opus
    }
}

fn path_codec(path: &str) -> Codec {
    if path.ends_with(".mp3") { Codec::Mp3 } else { Codec::Opus }
}

fn run_session(p: &SessionParams) {
    let cands = candidates(&p.mount, &p.codec);
    run_session_on(p, &cands, DROP_GRACE);
}

/// A session over the given candidate servers (tests point these at a local fake Icecast).
fn run_session_on(p: &SessionParams, cands: &[(&'static str, u16, String)], drop_grace: Duration) {
    let mut conn = match connect(p, cands, p.start_grace, None) {
        Ok(c) => c,
        Err(reason) => return end(p, reason),
    };
    let mut pipe = Pipeline::start(codec_of(&conn), p, conn.url.clone());
    let mut carry: Option<Vec<u8>> = None;
    loop {
        match pump(&mut conn, &mut pipe, p, carry.take()) {
            Pumped::Stopped => {
                pipe.finish();
                return end(p, EndReason::Stopped);
            }
            Pumped::NewStream(rest) => {
                // Encoder restarted upstream: let the old decoder play out, start fresh.
                tracing::info!("stream restarted upstream; new decoder");
                pipe.finish();
                pipe = Pipeline::start(Codec::Opus, p, conn.url.clone());
                pipe.fresh = false; // a new encoder: no burst to skip
                carry = Some(rest);
            }
            Pumped::Dropped(why) => {
                let buffered = p.buffer_ms.load(Ordering::Relaxed) / 1000;
                let _ = p.events.send(StreamEvent::Reconnecting(format!("{why}; {buffered} s buffered")));
                match connect(p, cands, drop_grace, Some(pipe.codec)) {
                    Ok(c) if codec_of(&c) == pipe.codec => {
                        pipe.filter.reconnected();
                        // Ogg pages resume exactly where we stopped; MP3 can't, so jump to live.
                        pipe.fresh = pipe.codec == Codec::Mp3;
                        let _ = p.events.send(StreamEvent::Started { url: c.url.clone(), codec: pipe.codec.name() });
                        conn = c;
                    }
                    Ok(c) => {
                        pipe.finish();
                        pipe = Pipeline::start(codec_of(&c), p, c.url.clone());
                        conn = c;
                    }
                    Err(EndReason::Stopped) => {
                        pipe.finish();
                        return end(p, EndReason::Stopped);
                    }
                    Err(_) => {
                        pipe.finish(); // play out whatever is still buffered
                        return end(p, EndReason::Finished);
                    }
                }
            }
        }
    }
}

impl Source for SessionParams {
    const THREAD: &'static str = "stream";

    fn run(self) {
        run_session(&self);
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
        let decoder = crate::sync::spawn_named("decoder", move || decode::run(codec, ctx));
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
        && let Some(r) = deliver(&bytes, pipe)
    {
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
    // Only the compressed part can be trimmed; the decoded part is already queued for the sink.
    let decoded = p.sink.queued_ms().min(ms);
    let Some(keep) = catch_up_keep(pipe.queue.buffered(), ms, decoded, target) else { return };
    tracing::info!("catch-up: buffer {ms} ms (decoded {decoded} ms, {} bytes queued), keeping {keep} bytes", pipe.queue.buffered());
    if pipe.queue.trim_front(keep) > 0 {
        pipe.started = Instant::now();
        p.buffer_ms.store(target, Ordering::Relaxed);
        let _ = p.events.send(StreamEvent::CaughtUp((ms - target) / 1000));
    }
}

/// How many compressed bytes to keep when the buffer is past target + slack (None: it isn't).
fn catch_up_keep(buffered: usize, buffer_ms: u64, decoded_ms: u64, target_ms: u64) -> Option<usize> {
    if buffer_ms <= target_ms + (target_ms / 2).max(3_000) {
        return None;
    }
    Some((buffered as u64 * target_ms.saturating_sub(decoded_ms) / (buffer_ms - decoded_ms).max(1)) as usize)
}

/// Delay before the next connect round: quick first retries, then ease off.
fn retry_wait_ms(attempt: u32) -> u64 {
    match attempt {
        0..=2 => 1000,
        3..=5 => 2000,
        _ => 4000,
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
        let until = Instant::now() + Duration::from_millis(retry_wait_ms(attempt));
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

    #[test]
    fn codecs_come_from_the_content_type_or_the_path() {
        assert_eq!(super::codec_for("audio/mpeg", "x"), super::Codec::Mp3);
        assert_eq!(super::codec_for("audio/mp3", ""), super::Codec::Mp3);
        assert_eq!(super::codec_for("", "/m.mp3"), super::Codec::Mp3, "no content type: use the path");
        assert_eq!(super::codec_for("", "/m"), super::Codec::Opus);
        assert_eq!(super::codec_for("application/ogg", "/m"), super::Codec::Opus);
        assert_eq!(super::path_codec("/m.mp3"), super::Codec::Mp3);
        assert_eq!(super::path_codec("/m"), super::Codec::Opus);
    }

    #[test]
    fn catch_up_waits_for_slack_then_keeps_the_target() {
        // 20 s target: nothing to do up to 30 s (target + max(10 s, 3 s)).
        assert_eq!(super::catch_up_keep(100_000, 20_000, 0, 20_000), None);
        assert_eq!(super::catch_up_keep(100_000, 30_000, 0, 20_000), None);
        // 40 s buffered, nothing decoded: keep half the bytes to land on 20 s.
        assert_eq!(super::catch_up_keep(100_000, 40_000, 0, 20_000), Some(50_000));
        // Decoded audio is already queued: only the compressed part can be trimmed.
        assert_eq!(super::catch_up_keep(100_000, 40_000, 10_000, 20_000), Some(33_333));
        // More decoded than the target leaves nothing to keep.
        assert_eq!(super::catch_up_keep(100_000, 40_000, 25_000, 20_000), Some(0));
    }

    #[test]
    fn retries_start_quick_then_ease_off() {
        assert_eq!([1, 2, 3, 4, 5, 6, 9].map(super::retry_wait_ms), [1000, 1000, 2000, 2000, 2000, 4000, 4000]);
    }

    use super::decode::tests::{mp3_frames, opus_packet, opus_stream};
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// What the fake Icecast does on one connection.
    enum Step {
        Send(Vec<u8>),
        Wait(u64),
        /// Close with the request unread: the kernel answers with a reset.
        Reset,
    }

    fn ok(ctype: &str) -> Step {
        Step::Send(format!("HTTP/1.0 200 OK\r\nContent-Type: {ctype}\r\n\r\n").into_bytes())
    }

    /// A fake Icecast on 127.0.0.1: one script per connection, then 404 for any later one.
    /// Reports each accepted connection on the returned channel.
    fn icecast(replies: Vec<Vec<Step>>) -> (u16, std::sync::mpsc::Receiver<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            for (i, sock) in listener.incoming().enumerate() {
                let Ok(mut sock) = sock else { continue };
                let steps = replies.next().unwrap_or_else(|| vec![Step::Send(b"HTTP/1.0 404 Not Found\r\n\r\n".to_vec())]);
                let reset = steps.iter().any(|s| matches!(s, Step::Reset));
                let mut req = Vec::new();
                let mut b = [0u8; 256];
                while let Ok(n) = sock.read(if reset { &mut b[..4] } else { &mut b }) {
                    req.extend_from_slice(&b[..n]);
                    if n == 0 || reset || req.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = tx.send(i);
                for s in steps {
                    match s {
                        Step::Send(bytes) => {
                            let _ = sock.write_all(&bytes);
                        }
                        Step::Wait(ms) => std::thread::sleep(Duration::from_millis(ms)),
                        Step::Reset => break,
                    }
                }
            }
        });
        (port, rx)
    }

    fn local(port: u16, path: &str) -> Vec<(&'static str, u16, String)> {
        vec![("127.0.0.1", port, path.to_string())]
    }

    fn temp_app() -> (std::path::PathBuf, crate::state::App) {
        let dir = std::env::temp_dir().join(format!("lm-stream-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    fn params(app: &crate::state::App) -> (SessionParams, UnboundedReceiver<(u64, StreamEvent)>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let p = SessionParams {
            mount: "m".into(),
            codec: "auto".into(),
            sink: app.sink.clone(),
            epoch: 0,
            stop: Arc::default(),
            events: Events::new(1, &tx),
            start_grace: Duration::ZERO,
            buffer_ms: Arc::default(),
        };
        (p, rx)
    }

    fn drain(rx: &mut UnboundedReceiver<(u64, StreamEvent)>) -> Vec<StreamEvent> {
        std::iter::from_fn(|| rx.try_recv().ok().map(|(_, e)| e)).collect()
    }

    /// Wait (up to 5 s) for an event matching `want`.
    fn wait_for(rx: &mut UnboundedReceiver<(u64, StreamEvent)>, want: impl Fn(&StreamEvent) -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            match rx.try_recv() {
                Ok((_, e)) if want(&e) => return,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("event never arrived");
    }

    fn started(events: &[StreamEvent], codec: &str) -> usize {
        events.iter().filter(|e| matches!(e, StreamEvent::Started { codec: c, .. } if *c == codec)).count()
    }

    #[test]
    fn rides_reconnects_and_a_codec_change_until_the_mount_is_gone() {
        let (dir, app) = temp_app();
        let opus = opus_stream(1, &opus_packet(), 40, 2);
        let (head, tail) = opus.split_at(opus.len() * 2 / 3);
        let (port, _accepted) = icecast(vec![
            // Burst, a pause (the live edge), then real-time audio, then the server closes.
            vec![
                ok("audio/mpeg"),
                Step::Send(mp3_frames(40)),
                Step::Wait(600),
                Step::Send(mp3_frames(10)),
                Step::Wait(100),
                Step::Send(mp3_frames(10)),
            ],
            // Same codec again: a reconnect.
            vec![ok("audio/mpeg"), Step::Send(mp3_frames(20))],
            // Now only Opus answers: a new pipeline.
            vec![ok("application/ogg"), Step::Send(head.to_vec()), Step::Wait(600), Step::Send(tail.to_vec())],
        ]);
        let (p, mut rx) = params(&app);
        run_session_on(&p, &local(port, "/m.mp3"), Duration::ZERO);

        let events = drain(&mut rx);
        assert!(started(&events, "mp3") >= 2, "{events:?}");
        assert!(started(&events, "opus") >= 1, "{events:?}");
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Reconnecting(why) if why.contains("closed"))), "{events:?}");
        assert!(matches!(events.last(), Some(StreamEvent::Ended(EndReason::Finished))), "{events:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_upstream_encoder_restart_gets_a_fresh_decoder() {
        let (dir, app) = temp_app();
        let pkt = opus_packet();
        let a = opus_stream(1, &pkt, 20, 2);
        let b = opus_stream(2, &pkt, 20, 2);
        let c = opus_stream(3, &pkt, 20, 2);
        let (a1, a2) = a.split_at(a.len() / 2);
        let (port, _accepted) = icecast(vec![
            // A new serial arrives after the burst.
            vec![
                ok("application/ogg"),
                Step::Send(a1.to_vec()),
                Step::Wait(600),
                Step::Send(a2.to_vec()),
                Step::Wait(100),
                Step::Send(b.clone()),
            ],
            // Reconnect: the same stream is replayed, then a restart inside the burst... of the next session.
            vec![ok("application/ogg"), Step::Send(b.clone())],
        ]);
        let (p, mut rx) = params(&app);
        run_session_on(&p, &local(port, "/m"), Duration::ZERO);
        let events = drain(&mut rx);
        assert!(started(&events, "opus") >= 2, "{events:?}");
        assert!(matches!(events.last(), Some(StreamEvent::Ended(EndReason::Finished))), "{events:?}");

        // A restart inside the connect burst itself.
        let mut both = b;
        both.extend_from_slice(&c);
        let (port, _accepted) = icecast(vec![vec![ok("application/ogg"), Step::Send(both)]]);
        let (p, mut rx) = params(&app);
        run_session_on(&p, &local(port, "/m"), Duration::ZERO);
        let events = drain(&mut rx);
        assert!(started(&events, "opus") >= 1, "{events:?}");
        assert!(matches!(events.last(), Some(StreamEvent::Ended(EndReason::Finished))), "{events:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn connection_resets_are_drop_outs() {
        let (dir, app) = temp_app();
        let (port, _accepted) = icecast(vec![
            // Reset before any audio.
            vec![ok("audio/mpeg"), Step::Wait(200), Step::Reset],
            // Reset in the middle of the burst.
            vec![ok("audio/mpeg"), Step::Send(mp3_frames(10)), Step::Wait(200), Step::Reset],
            // Reset after the burst, while streaming.
            vec![ok("audio/mpeg"), Step::Send(mp3_frames(10)), Step::Wait(600), Step::Send(mp3_frames(2)), Step::Wait(200), Step::Reset],
        ]);
        let (p, mut rx) = params(&app);
        run_session_on(&p, &local(port, "/m.mp3"), Duration::ZERO);
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Reconnecting(why) if why.contains("no data"))), "{events:?}");
        assert!(matches!(events.last(), Some(StreamEvent::Ended(EndReason::Finished))), "{events:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn trickle(n: usize) -> Vec<Step> {
        let mut steps = vec![ok("audio/mpeg")];
        for _ in 0..n {
            steps.push(Step::Send(mp3_frames(1)));
            steps.push(Step::Wait(100));
        }
        steps
    }

    #[test]
    fn stops_while_playing() {
        let (dir, app) = temp_app();
        let mut steps = vec![ok("audio/mpeg"), Step::Send(mp3_frames(20)), Step::Wait(600)];
        steps.extend(trickle(40).into_iter().skip(1));
        let (port, _accepted) = icecast(vec![steps]);
        let (p, mut rx) = params(&app);
        let stop = p.stop.clone();
        let session = std::thread::spawn(move || run_session_on(&p, &local(port, "/m.mp3"), Duration::ZERO));
        wait_for(&mut rx, |e| matches!(e, StreamEvent::Started { .. }));
        stop.store(true, Ordering::Relaxed);
        session.join().unwrap();
        assert!(matches!(drain(&mut rx).last(), Some(StreamEvent::Ended(EndReason::Stopped))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stops_while_reading_the_burst() {
        let (dir, app) = temp_app();
        let (port, accepted) = icecast(vec![trickle(40)]);
        let (p, mut rx) = params(&app);
        let stop = p.stop.clone();
        let session = std::thread::spawn(move || run_session_on(&p, &local(port, "/m.mp3"), Duration::ZERO));
        accepted.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        session.join().unwrap();
        assert!(matches!(drain(&mut rx).last(), Some(StreamEvent::Ended(EndReason::Stopped))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stops_while_reconnecting() {
        let (dir, app) = temp_app();
        let (port, _accepted) = icecast(vec![vec![ok("audio/mpeg"), Step::Send(mp3_frames(10))]]);
        let (p, mut rx) = params(&app);
        let stop = p.stop.clone();
        // A long drop grace: the 404s keep it retrying until stopped.
        let session = std::thread::spawn(move || run_session_on(&p, &local(port, "/m.mp3"), Duration::from_secs(30)));
        wait_for(&mut rx, |e| matches!(e, StreamEvent::Reconnecting(_)));
        stop.store(true, Ordering::Relaxed);
        session.join().unwrap();
        assert!(matches!(drain(&mut rx).last(), Some(StreamEvent::Ended(EndReason::Stopped))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mount_that_is_not_live_ends_at_once() {
        let (dir, app) = temp_app();
        let (port, _accepted) = icecast(Vec::new());
        let (p, mut rx) = params(&app);
        let buffer = p.buffer_ms.clone();
        buffer.store(5, Ordering::Relaxed);
        run_session_on(&p, &local(port, "/m"), Duration::ZERO);
        assert!(matches!(drain(&mut rx).as_slice(), [StreamEvent::Ended(EndReason::NotLive)]));
        assert_eq!(buffer.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stopped_session_never_touches_the_network() {
        let (dir, app) = temp_app();
        let (p, mut rx) = params(&app);
        p.stop.store(true, Ordering::Relaxed);
        super::source::spawn(p);
        wait_for(&mut rx, |e| matches!(e, StreamEvent::Ended(EndReason::Stopped)));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn refused_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn connect_keeps_retrying_while_the_network_is_down() {
        let (dir, app) = temp_app();
        let (p, _rx) = params(&app);
        let port = refused_port();
        let cands = vec![("127.0.0.1", port, "/m".to_string()), ("127.0.0.1", port, "/m.mp3".to_string())];

        // Before audio: nothing reachable means not live.
        assert!(matches!(connect(&p, &cands, Duration::ZERO, None), Err(EndReason::NotLive)));

        // Mid-broadcast: no answer at all is our network, so keep trying until stopped.
        let stop = p.stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::Relaxed);
        });
        assert!(matches!(connect(&p, &cands, Duration::ZERO, Some(Codec::Mp3)), Err(EndReason::Stopped)));
        t.join().unwrap();

        // Already stopped: no attempt at all.
        assert!(matches!(connect(&p, &cands, Duration::ZERO, None), Err(EndReason::Stopped)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn catch_up_trims_a_buffer_that_fell_far_behind() {
        let (dir, app) = temp_app();
        let (p, mut rx) = params(&app);
        let decoder_stop = Arc::new(AtomicBool::new(true));
        let p = SessionParams { stop: decoder_stop, ..p };
        let mut pipe = Pipeline::start(Codec::Mp3, &p, "http://x/m.mp3".into());
        let never = AtomicBool::new(false);
        for _ in 0..10 {
            pipe.queue.push(vec![0u8; 1000], &never);
        }
        let target = live_delay_ms();
        p.buffer_ms.store(target * 10 + 10_000, Ordering::Relaxed);

        // Just joined: the estimate hasn't settled, so nothing happens.
        catch_up(&mut pipe, &p);
        assert_eq!(pipe.queue.buffered(), 10_000);

        // Settled and far behind: skip ahead to the target.
        pipe.started = Instant::now().checked_sub(Duration::from_secs(9)).unwrap();
        catch_up(&mut pipe, &p);
        assert!(pipe.queue.buffered() < 10_000);
        assert_eq!(p.buffer_ms.load(Ordering::Relaxed), target);
        assert!(drain(&mut rx).iter().any(|e| matches!(e, StreamEvent::CaughtUp(_))));

        // Near the target: nothing to trim.
        pipe.started = Instant::now().checked_sub(Duration::from_secs(9)).unwrap();
        let before = pipe.queue.buffered();
        catch_up(&mut pipe, &p);
        assert_eq!(pipe.queue.buffered(), before);
        pipe.finish();
        let _ = std::fs::remove_dir_all(dir);
    }
}
