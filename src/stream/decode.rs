//! Decoders: pull compressed audio from the jitter buffer, push 16-bit stereo PCM to the sink.

use super::buffer::{ByteQueue, QueueReader};
use crate::sink::{RATE, SinkHandle};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// Quran only: playback speed (f32 bits), applied without changing pitch.
    pub speed: Option<Arc<AtomicU32>>,
    /// Which volume this audio follows.
    pub channel: crate::sink::Channel,
}

/// Run a decoder until the buffer is closed and drained, or `stop` is raised.
pub fn run(codec: Codec, ctx: DecodeCtx) {
    let reader = QueueReader::new(ctx.queue.clone(), ctx.stop.clone());
    let mut out = PcmOut::new(ctx);
    let result = match codec {
        Codec::Opus => decode_opus(reader, &mut out),
        Codec::Mp3 => decode_mp3(reader, &mut out),
    };
    if let Err(e) = result
        && !out.ctx.stop.load(Ordering::Relaxed) {
            tracing::warn!("{} decoder stopped: {e}", codec.name());
        }
    out.ctx.buffer_ms.store(0, Ordering::Relaxed);
}

/// Sends PCM to the sink and keeps buffer statistics.
struct PcmOut {
    ctx: DecodeCtx,
    first: Option<Box<dyn FnOnce() + Send>>,
    frames: u64,
    last_stat: Instant,
    stretch: Option<super::stretch::Stretch>,
}

impl PcmOut {
    fn new(mut ctx: DecodeCtx) -> Self {
        let first = Some(std::mem::replace(&mut ctx.on_first_audio, Box::new(|| {})));
        Self { ctx, first, frames: 0, last_stat: Instant::now(), stretch: None }
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
        // Quran: speed (time-stretched, pitch kept). Position stays in
        // recitation time (counted above), whatever the speed.
        let mut stereo = stereo;
        if let Some(sp) = &self.ctx.speed {
            let speed = f32::from_bits(sp.load(Ordering::Relaxed));
            if (speed - 1.0).abs() > 0.01 {
                let st = self.stretch.get_or_insert_with(|| super::stretch::Stretch::new(rate));
                stereo = st.process(&stereo, speed);
            } else {
                self.stretch = None;
            }
        }
        if stereo.is_empty() {
            return true; // the stretcher is still collecting a segment
        }
        self.ctx.sink.send_pcm_on(self.ctx.channel, self.ctx.epoch, rate, stereo)
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
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn stereo_conversion() {
        assert_eq!(super::to_stereo(&[1, 2], 1), vec![1, 1, 2, 2]);
        assert_eq!(super::to_stereo(&[1, 2], 2), vec![1, 2]);
        assert_eq!(super::to_stereo(&[1, 2, 3, 4, 5, 6], 3), vec![1, 2, 4, 5]);
    }

    /// Silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz, stereo).
    pub(crate) fn mp3_frames(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * 417);
        for _ in 0..n {
            let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
            frame.resize(417, 0);
            out.extend(frame);
        }
        out
    }

    fn make_ctx(app: &crate::state::App, audio: Vec<u8>) -> (DecodeCtx, Arc<AtomicU64>) {
        let queue = ByteQueue::new();
        queue.push(audio, &AtomicBool::new(false));
        queue.close();
        let first = Arc::new(AtomicU64::new(0));
        let f = first.clone();
        let ctx = DecodeCtx {
            queue,
            stop: Arc::new(AtomicBool::new(false)),
            sink: app.sink.clone(),
            epoch: 0,
            buffer_ms: Arc::new(AtomicU64::new(0)),
            on_first_audio: Box::new(move || {
                f.fetch_add(1, Ordering::Relaxed);
            }),
            paused: None,
            position_ms: None,
            position_base_ms: 0,
            speed: None,
            channel: crate::sink::Channel::Broadcast,
        };
        (ctx, first)
    }

    #[test]
    fn decodes_mp3_frames_until_the_buffer_ends() {
        let dir = std::env::temp_dir().join(format!("lm-decode-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        let (ctx, _first) = make_ctx(&app, mp3_frames(6));
        run(Codec::Mp3, ctx);

        // An empty buffer ends cleanly, too.
        let (ctx, _first) = make_ctx(&app, Vec::new());
        run(Codec::Mp3, ctx);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_bad_ogg_data_and_stops_on_request() {
        let dir = std::env::temp_dir().join(format!("lm-decode-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);

        let (ctx, _first) = make_ctx(&app, b"not an ogg stream".to_vec());
        run(Codec::Opus, ctx);

        let stopped = make_ctx(&app, mp3_frames(2)).0;
        stopped.stop.store(true, Ordering::Relaxed);
        run(Codec::Mp3, stopped);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pause_position_and_speed_paths_run() {
        let dir = std::env::temp_dir().join(format!("lm-decode-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        let (mut ctx, _first) = make_ctx(&app, mp3_frames(4));
        ctx.paused = Some(Arc::new(AtomicBool::new(false)));
        ctx.position_ms = Some(Arc::new(AtomicU64::new(0)));
        ctx.position_base_ms = 1_000;
        ctx.speed = Some(Arc::new(std::sync::atomic::AtomicU32::new(1.5f32.to_bits())));
        run(Codec::Mp3, ctx);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// One real 20 ms mono Opus packet (a quiet tone), encoded once.
    pub(crate) fn opus_packet() -> Vec<u8> {
        use opus_pure::{Application, OpusEncoder};
        let mut enc = OpusEncoder::new(48_000, 1, Application::Audio).unwrap();
        let input: Vec<f32> = (0..960).map(|i| (i as f32 * 0.05).sin() * 0.1).collect();
        let mut buf = vec![0u8; 1500];
        let mut n = 0;
        for _ in 0..3 {
            n = enc.encode(&input, 960, &mut buf).unwrap();
        }
        buf.truncate(n);
        buf
    }

    /// An Ogg Opus stream (serial `serial`) of `packets` copies of `packet`, `per_page` packets a page.
    pub(crate) fn opus_stream(serial: u32, packet: &[u8], packets: usize, per_page: usize) -> Vec<u8> {
        use opus_pure::{OggOpusWriter, OpusHead, OpusTags};
        let mut w = OggOpusWriter::with_serial(Vec::new(), OpusHead::new(1, 48_000).unwrap(), OpusTags::new(), serial).unwrap();
        w.set_page_target(packet.len() * per_page);
        for _ in 0..packets {
            w.write_packet(packet).unwrap();
        }
        w.finish().unwrap()
    }

    fn temp_app() -> (std::path::PathBuf, crate::state::App) {
        let dir = std::env::temp_dir().join(format!("lm-decode-{}", crate::config::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::state::test_app(&dir);
        (dir, app)
    }

    #[test]
    fn decodes_an_ogg_opus_stream() {
        let (dir, app) = temp_app();
        let (mut ctx, first) = make_ctx(&app, opus_stream(7, &opus_packet(), 25, 5));
        let position = Arc::new(AtomicU64::new(0));
        ctx.position_ms = Some(position.clone());
        run(Codec::Opus, ctx);
        assert_eq!(first.load(Ordering::Relaxed), 1, "first audio reported once");
        assert!(position.load(Ordering::Relaxed) >= 400, "{}", position.load(Ordering::Relaxed));
        assert_eq!(Codec::Opus.name(), "opus");
        assert_eq!(Codec::Mp3.name(), "mp3");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gives_up_on_a_run_of_undecodable_opus_packets() {
        use opus_pure::{OggOpusWriter, OpusHead};
        let (dir, app) = temp_app();
        let mut w = OggOpusWriter::new(Vec::new(), OpusHead::new(1, 48_000).unwrap()).unwrap();
        // A code-3 packet without its frame-count byte is malformed.
        for _ in 0..60 {
            w.write_packet_with_duration(&[0xFF], 960).unwrap();
        }
        let (ctx, first) = make_ctx(&app, w.finish().unwrap());
        run(Codec::Opus, ctx);
        assert_eq!(first.load(Ordering::Relaxed), 0, "nothing decoded");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_opus_with_more_than_two_channels() {
        use opus_pure::{OggOpusWriter, OpusHead, OpusTags};
        let (dir, app) = temp_app();
        let mut head = OpusHead::new(2, 48_000).unwrap();
        head.channel_count = 3;
        head.mapping_family = 1;
        head.stream_count = 2;
        head.coupled_count = 1;
        head.channel_mapping = vec![0, 2, 1];
        let data = OggOpusWriter::with_tags(Vec::new(), head, OpusTags::new()).and_then(|mut w| {
            w.write_packet(&opus_packet())?;
            w.finish()
        });
        if let Ok(data) = data {
            let (ctx, first) = make_ctx(&app, data);
            run(Codec::Opus, ctx);
            assert_eq!(first.load(Ordering::Relaxed), 0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_page_after_the_headers_is_reported() {
        let (dir, app) = temp_app();
        let mut data = opus_stream(3, &opus_packet(), 20, 2);
        // Damage the capture pattern of a page in the middle.
        let pages: Vec<usize> = data.windows(4).enumerate().filter(|(_, w)| w == b"OggS").map(|(i, _)| i).collect();
        let at = pages[pages.len() / 2];
        data[at + 4] = 9; // unsupported stream structure version
        let (ctx, _first) = make_ctx(&app, data);
        run(Codec::Opus, ctx);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz, mono).
    fn mono_mp3_frames(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n * 417);
        for _ in 0..n {
            let mut frame = vec![0xFF, 0xFB, 0x90, 0xC0];
            frame.resize(417, 0);
            out.extend(frame);
        }
        out
    }

    #[test]
    fn mono_mp3_at_normal_speed() {
        let (dir, app) = temp_app();
        let (mut ctx, first) = make_ctx(&app, mono_mp3_frames(8));
        ctx.speed = Some(Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits())));
        run(Codec::Mp3, ctx);
        assert_eq!(first.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn publishes_buffer_statistics_after_a_second() {
        let (dir, app) = temp_app();
        // ~2.9 s of audio: the sink takes it in real time behind a ~1.7 s queue, so decoding
        // lasts over a second.
        let (ctx, _first) = make_ctx(&app, mp3_frames(110));
        let buffer_ms = ctx.buffer_ms.clone();
        let decoder = std::thread::spawn(move || run(Codec::Mp3, ctx));
        let mut seen = 0;
        while !decoder.is_finished() {
            seen = seen.max(buffer_ms.load(Ordering::Relaxed));
            std::thread::sleep(Duration::from_millis(10));
        }
        decoder.join().unwrap();
        assert!(seen > 0, "buffer statistics were published");
        assert_eq!(buffer_ms.load(Ordering::Relaxed), 0, "reset when the decoder ends");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_paused_decoder_wakes_up_when_stopped() {
        let (dir, app) = temp_app();
        let (mut ctx, first) = make_ctx(&app, mp3_frames(8));
        let paused = Arc::new(AtomicBool::new(true));
        ctx.paused = Some(paused);
        let stop = ctx.stop.clone();
        let decoder = std::thread::spawn(move || run(Codec::Mp3, ctx));
        std::thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::Relaxed);
        decoder.join().unwrap();
        assert_eq!(first.load(Ordering::Relaxed), 1, "the paused send still went through once stopped");
        let _ = std::fs::remove_dir_all(dir);
    }
}
