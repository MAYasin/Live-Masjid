//! One listening session: connect to the best available mount, decode, feed the sink,
//! and transparently reconnect through short drop-outs. Runs on a blocking thread.

use crate::net::{OpenError, STREAM_HOSTS, StreamConn, open_stream};
use crate::sink::SinkHandle;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
pub enum StreamEvent {
    /// Connected and the first audio has been decoded.
    Started { url: String, codec: &'static str },
    /// Connection lost mid-session; trying to get it back.
    Reconnecting(String),
    /// Session over.
    Ended(EndReason),
}

#[derive(Debug, Clone)]
pub enum EndReason {
    /// Never got audio: no candidate mount is live.
    NotLive,
    /// Was playing, the broadcast ended (mount gone for the whole grace period).
    Finished,
    /// Stop flag set by the player.
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
    /// How long to keep retrying before the first audio (see START_GRACE).
    pub start_grace: Duration,
}

/// Grace period for a mid-session drop before we call the broadcast finished.
const DROP_GRACE: Duration = Duration::from_secs(25);
/// When first connecting after a live signal, keep trying this long (MP3 mirror and
/// relay can lag the live signal by several seconds).
pub const START_GRACE: Duration = Duration::from_secs(12);

pub fn candidates(mount: &str, codec: &str) -> Vec<(&'static str, u16, String)> {
    let base = mount.trim_start_matches('/');
    let (ogg, mp3) = if let Some(b) = base.strip_suffix(".mp3") {
        (b.to_string(), base.to_string())
    } else {
        (base.to_string(), format!("{base}.mp3"))
    };
    let order: [&String; 2] = if codec == "mp3" { [&mp3, &ogg] } else { [&ogg, &mp3] };
    let mut v = Vec::new();
    for (h, p) in STREAM_HOSTS {
        for m in order {
            v.push((*h, *p, format!("/{m}")));
        }
    }
    v
}

pub fn run_session(p: SessionParams) {
    let cands = candidates(&p.mount, &p.codec);
    let mut ever_played = false;
    let mut grace_start = Instant::now();
    let mut attempt = 0u32;
    loop {
        if p.stop.load(Ordering::Relaxed) {
            let _ = p.events.send(StreamEvent::Ended(EndReason::Stopped));
            return;
        }
        let mut all_404 = true;
        let mut last_err = String::new();
        let mut conn = None;
        for (h, port, path) in &cands {
            if p.stop.load(Ordering::Relaxed) {
                break;
            }
            match open_stream(h, *port, path) {
                Ok(c) => {
                    conn = Some(c);
                    break;
                }
                Err(OpenError::NotFound) => {}
                Err(e) => {
                    all_404 = false;
                    last_err = e.to_string();
                }
            }
        }
        match conn {
            Some(c) => {
                let url = c.url.clone();
                let result = decode(c, &p, &mut ever_played, &url);
                if p.stop.load(Ordering::Relaxed) {
                    let _ = p.events.send(StreamEvent::Ended(EndReason::Stopped));
                    return;
                }
                let msg = match result {
                    Ok(()) => "stream ended".to_string(),
                    Err(e) => e,
                };
                if ever_played {
                    let _ = p.events.send(StreamEvent::Reconnecting(msg));
                }
                grace_start = Instant::now();
                attempt = 0;
            }
            None => {
                let grace = if ever_played { DROP_GRACE } else { p.start_grace };
                if grace_start.elapsed() > grace {
                    let reason = if ever_played { EndReason::Finished } else { EndReason::NotLive };
                    if !all_404 {
                        tracing::warn!("stream connect failed: {last_err}");
                    }
                    let _ = p.events.send(StreamEvent::Ended(reason));
                    return;
                }
                attempt += 1;
                // Quick first retries (a fresh mount appears within a second or two), then
                // ease off: each round can be up to four requests.
                let wait = match attempt {
                    0..=2 => 1000,
                    3..=5 => 2000,
                    _ => 4000,
                };
                sleep_unless_stopped(&p.stop, Duration::from_millis(wait));
            }
        }
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn is_ogg(c: &StreamConn) -> bool {
    c.content_type.contains("ogg") || c.content_type.contains("opus") || (!c.url.ends_with(".mp3") && !c.content_type.contains("mpeg"))
}

/// Decode until the connection ends. Ok(()) on clean EOF, Err(msg) on error.
fn decode(c: StreamConn, p: &SessionParams, ever_played: &mut bool, url: &str) -> Result<(), String> {
    if is_ogg(&c) {
        decode_opus(c.reader, p, ever_played, url)
    } else {
        decode_mp3(c.reader, p, ever_played, url)
    }
}

fn started(p: &SessionParams, ever_played: &mut bool, first: &mut bool, url: &str, codec: &'static str) {
    if *first {
        *first = false;
        *ever_played = true;
        let _ = p.events.send(StreamEvent::Started { url: url.to_string(), codec });
    }
}

fn decode_opus(r: impl Read, p: &SessionParams, ever_played: &mut bool, url: &str) -> Result<(), String> {
    let mut reader = opus_pure::OggOpusReader::new(r).map_err(|e| format!("ogg header: {e}"))?;
    let ch = reader.head().channel_count as usize;
    if ch == 0 || ch > 2 {
        return Err(format!("unsupported channel count {ch}"));
    }
    let mut dec = reader.head().decoder(crate::sink::RATE as i32).map_err(|e| e.to_string())?;
    let mut pkt = opus_pure::OggPacket::default();
    let mut pcm = vec![0i16; opus_pure::MAX_PACKET_SAMPLES * ch];
    let mut first = true;
    let mut bad = 0u32;
    loop {
        if p.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match reader.read_packet_into(&mut pkt) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => return Err(format!("read: {e}")),
        }
        let n = match dec.decode_s16(&pkt.data, opus_pure::MAX_PACKET_SAMPLES, &mut pcm) {
            Ok(n) => {
                bad = 0;
                n
            }
            Err(e) => {
                bad += 1;
                if bad > 50 {
                    return Err(format!("opus decode: {e}"));
                }
                continue;
            }
        };
        if n == 0 {
            continue;
        }
        let out = to_stereo(&pcm[..n * ch], ch);
        started(p, ever_played, &mut first, url, "opus");
        if !p.sink.send_pcm(p.epoch, crate::sink::RATE, out) {
            return Err("sink closed".into());
        }
    }
}

fn decode_mp3(r: impl Read + Send + Sync + 'static, p: &SessionParams, ever_played: &mut bool, url: &str) -> Result<(), String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error as SE;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use symphonia::core::meta::MetadataOptions;

    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(r)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut fmt = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("mp3 probe: {e}"))?;
    let track = fmt.default_track(TrackType::Audio).ok_or("no audio track")?.clone();
    let params = track
        .codec_params
        .as_ref()
        .and_then(|c| c.audio())
        .ok_or("no audio params")?
        .clone();
    let mut dec = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| format!("mp3 decoder: {e}"))?;
    let mut buf: Vec<i16> = Vec::new();
    let mut first = true;
    loop {
        if p.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let pkt = match fmt.next_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => return Ok(()),
            Err(SE::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(format!("read: {e}")),
        };
        let ab = match dec.decode(&pkt) {
            Ok(ab) => ab,
            Err(SE::DecodeError(_)) => continue,
            Err(e) => return Err(format!("mp3 decode: {e}")),
        };
        let rate = ab.spec().rate();
        let ch = ab.spec().channels().count();
        if ab.frames() == 0 || ch == 0 {
            continue;
        }
        ab.copy_to_vec_interleaved(&mut buf);
        let out = if ch == 2 {
            std::mem::take(&mut buf)
        } else {
            to_stereo(&buf, ch)
        };
        started(p, ever_played, &mut first, url, "mp3");
        if !p.sink.send_pcm(p.epoch, rate, out) {
            return Err("sink closed".into());
        }
    }
}

/// Mono -> stereo duplicate; >2 channels -> first two.
fn to_stereo(pcm: &[i16], ch: usize) -> Vec<i16> {
    match ch {
        2 => pcm.to_vec(),
        1 => {
            let mut v = Vec::with_capacity(pcm.len() * 2);
            for s in pcm {
                v.push(*s);
                v.push(*s);
            }
            v
        }
        _ => pcm.chunks_exact(ch).flat_map(|f| [f[0], f[1]]).collect(),
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
        let c = super::candidates("lakewood", "mp3");
        assert_eq!(c[0].2, "/lakewood.mp3");
        let c = super::candidates("annoor.mp3", "auto");
        assert_eq!(c[0].2, "/annoor");
    }
}
