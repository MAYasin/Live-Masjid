//! Decoders: pull compressed audio from the jitter buffer, push 16-bit stereo PCM to the sink.

use super::buffer::{ByteQueue, QueueReader};
use crate::sink::{RATE, SinkHandle};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Opus,
    Mp3,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Opus => "opus",
            Codec::Mp3 => "mp3",
        }
    }
}

/// Everything a decoder thread needs.
pub struct DecodeCtx {
    pub queue: Arc<ByteQueue>,
    pub stop: Arc<AtomicBool>,
    pub sink: SinkHandle,
    pub epoch: u64,
    /// Seconds of audio waiting in the jitter buffer, in ms (for the UI).
    pub buffer_ms: Arc<AtomicU64>,
    /// Called once, when the first PCM of this decoder reaches the sink.
    pub on_first_audio: Box<dyn FnOnce() + Send>,
    /// Recordings only: hold output while set.
    pub paused: Option<Arc<AtomicBool>>,
    /// Recordings only: playback position (ms of audio decoded so far).
    pub position_ms: Option<Arc<AtomicU64>>,
    /// Position of the first decoded sample (after a seek), in ms.
    pub position_base_ms: u64,
}

/// Run a decoder until the buffer is closed and drained, or `stop` is raised.
pub fn run(codec: Codec, ctx: DecodeCtx) {
    let reader = QueueReader::new(ctx.queue.clone(), ctx.stop.clone());
    let mut out = PcmOut::new(ctx);
    let result = match codec {
        Codec::Opus => decode_opus(reader, &mut out),
        Codec::Mp3 => decode_mp3(reader, &mut out),
    };
    if let Err(e) = result {
        if !out.ctx.stop.load(Ordering::Relaxed) {
            tracing::warn!("{} decoder stopped: {e}", codec.name());
        }
    }
    out.ctx.buffer_ms.store(0, Ordering::Relaxed);
}

/// Sends PCM to the sink and keeps buffer statistics.
struct PcmOut {
    ctx: DecodeCtx,
    first: Option<Box<dyn FnOnce() + Send>>,
    frames: u64,
    last_stat: Instant,
}

impl PcmOut {
    fn new(mut ctx: DecodeCtx) -> Self {
        let first = Some(std::mem::replace(&mut ctx.on_first_audio, Box::new(|| {})));
        Self { ctx, first, frames: 0, last_stat: Instant::now() }
    }

    fn stopped(&self) -> bool {
        self.ctx.stop.load(Ordering::Relaxed)
    }

    /// Returns false when the sink is gone.
    fn send(&mut self, rate: u32, stereo: Vec<i16>) -> bool {
        if let Some(p) = &self.ctx.paused {
            while p.load(Ordering::Relaxed) && !self.stopped() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.frames += (stereo.len() / 2) as u64;
        if let Some(pos) = &self.ctx.position_ms {
            pos.store(self.ctx.position_base_ms + self.frames * 1000 / u64::from(rate.max(1)), Ordering::Relaxed);
        }
        if let Some(f) = self.first.take() {
            f();
        }
        if self.last_stat.elapsed() > Duration::from_secs(1) {
            self.last_stat = Instant::now();
            // bytes per second of audio, measured from what we've decoded so far
            let secs = self.frames as f64 / rate as f64;
            if secs > 1.0 {
                let bps = self.ctx.queue.consumed() as f64 / secs;
                // Compressed audio in the jitter buffer + decoded audio queued for the sink.
                let ms = self.ctx.queue.buffered() as f64 / bps.max(1.0) * 1000.0;
                self.ctx.buffer_ms.store(ms as u64 + self.ctx.sink.queued_ms(), Ordering::Relaxed);
            }
        }
        self.ctx.sink.send_pcm(self.ctx.epoch, rate, stereo)
    }
}

fn decode_opus(r: impl Read, out: &mut PcmOut) -> Result<(), String> {
    let mut reader = opus_pure::OggOpusReader::new(r).map_err(|e| format!("ogg header: {e}"))?;
    let ch = reader.head().channel_count as usize;
    if !(1..=2).contains(&ch) {
        return Err(format!("unsupported channel count {ch}"));
    }
    let mut dec = reader.head().decoder(RATE as i32).map_err(|e| e.to_string())?;
    let mut pkt = opus_pure::OggPacket::default();
    let mut pcm = vec![0i16; opus_pure::MAX_PACKET_SAMPLES * ch];
    let mut bad = 0u32;
    while !out.stopped() {
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
        if n > 0 && !out.send(RATE, to_stereo(&pcm[..n * ch], ch)) {
            return Err("sink closed".into());
        }
    }
    Ok(())
}

fn decode_mp3(r: impl Read + Send + Sync + 'static, out: &mut PcmOut) -> Result<(), String> {
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
    let params = fmt
        .default_track(TrackType::Audio)
        .and_then(|t| t.codec_params.as_ref())
        .and_then(|c| c.audio())
        .ok_or("no audio track")?
        .clone();
    let mut dec = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| format!("mp3 decoder: {e}"))?;
    let mut buf: Vec<i16> = Vec::new();
    while !out.stopped() {
        let pkt = match fmt.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(SE::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(format!("read: {e}")),
        };
        let ab = match dec.decode(&pkt) {
            Ok(ab) => ab,
            Err(SE::DecodeError(_)) => continue,
            Err(e) => return Err(format!("mp3 decode: {e}")),
        };
        let (rate, ch) = (ab.spec().rate(), ab.spec().channels().count());
        if ab.frames() == 0 || ch == 0 {
            continue;
        }
        ab.copy_to_vec_interleaved(&mut buf);
        let stereo = if ch == 2 { std::mem::take(&mut buf) } else { to_stereo(&buf, ch) };
        if !out.send(rate, stereo) {
            return Err("sink closed".into());
        }
    }
    Ok(())
}

/// Mono → duplicated stereo; more than two channels → first two.
pub fn to_stereo(pcm: &[i16], ch: usize) -> Vec<i16> {
    match ch {
        2 => pcm.to_vec(),
        1 => pcm.iter().flat_map(|&s| [s, s]).collect(),
        _ => pcm.chunks_exact(ch).flat_map(|f| [f[0], f[1]]).collect(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stereo_conversion() {
        assert_eq!(super::to_stereo(&[1, 2], 1), vec![1, 1, 2, 2]);
        assert_eq!(super::to_stereo(&[1, 2, 3, 4, 5, 6], 3), vec![1, 2, 4, 5]);
    }
}
