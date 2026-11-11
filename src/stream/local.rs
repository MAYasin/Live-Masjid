//! Playing a Quran surah from the Pi's storage, with seeking, even while it is still
//! downloading (the reader waits for the file to grow).
//!
//! Seeking: the surahs are constant-bitrate MP3, so a time maps to a byte offset
//! (after the ID3 tag); playback restarts there at the next verified frame boundary.

use super::buffer::ByteQueue;
use super::decode::{self, Codec, DecodeCtx};
use super::edge::mp3_frame;
use super::recording::RecordingEvent;
use crate::quran::Library;
use crate::sink::SinkHandle;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use super::source::{Events, Source};

pub struct LocalParams {
    pub lib: Arc<Library>,
    /// Recitation key (see `quran::Reciter`).
    pub reciter: String,
    pub surah: u16,
    pub start_ms: u64,
    pub sink: SinkHandle,
    pub epoch: u64,
    pub stop: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub position_ms: Arc<AtomicU64>,
    /// Set once the file header has been read.
    pub duration_ms: Arc<AtomicU64>,
    /// Playback speed (f32 bits); can change while playing.
    pub speed: Arc<std::sync::atomic::AtomicU32>,
    pub events: Events<RecordingEvent>,
}

/// Give up when a download makes no progress for this long.
const STALL: Duration = Duration::from_secs(90);

/// A file that may still be growing: reads wait for more data until the download is done.
struct Growing {
    file: File,
    lib: Arc<Library>,
    key: String,
    n: u16,
    stop: Arc<AtomicBool>,
    last_data: Instant,
}

impl Growing {
    fn len(&self) -> u64 {
        self.file.metadata().map(|m| m.len()).unwrap_or(0)
    }

    /// Wait until the file holds at least `want` bytes (or is complete). False if it never will.
    fn wait_for(&mut self, want: u64) -> Result<(), String> {
        let mut last_len = self.len();
        let mut since = Instant::now();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err("stopped".into());
            }
            let len = self.len();
            if len >= want || self.lib.is_ready(&self.key, self.n) {
                return Ok(());
            }
            if len != last_len {
                (last_len, since) = (len, Instant::now());
            } else if since.elapsed() > STALL {
                return Err("the download stopped; check the Pi's internet connection".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let k = self.file.read(buf)?;
            if k > 0 {
                self.last_data = Instant::now();
                return Ok(k);
            }
            if self.lib.is_ready(&self.key, self.n) {
                // Complete (renamed): read anything written just before, then EOF.
                return self.file.read(buf);
            }
            if self.last_data.elapsed() > STALL {
                return Err(std::io::Error::other("the download stopped; check the Pi's internet connection"));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Size of a leading ID3v2 tag (0 if none).
fn id3_len(h: &[u8]) -> u64 {
    if h.len() < 10 || &h[..3] != b"ID3" {
        return 0;
    }
    let size = h[6..10].iter().fold(0u64, |acc, &b| (acc << 7) | u64::from(b & 0x7f));
    10 + size + if h[5] & 0x10 != 0 { 10 } else { 0 }
}

/// First frame at or after the start of `buf` that is followed by another frame.
fn find_frame(buf: &[u8]) -> Option<(usize, usize, u64)> {
    (0..buf.len().saturating_sub(4)).find_map(|i| {
        let (len, us) = mp3_frame(&buf[i..])?;
        mp3_frame(buf.get(i + len..)?)?;
        Some((i, len, us))
    })
}

fn read_at(f: &mut File, at: u64, n: usize) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(at))?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match f.read(&mut buf[got..])? {
            0 => break,
            k => got += k,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// Open the surah file, find where to start for `start_ms`. Returns the reader positioned
/// there and the position (ms) it corresponds to.
fn open(p: &LocalParams) -> Result<(Growing, u64), String> {
    let (lib, n, key) = (&p.lib, p.surah, p.reciter.as_str());
    // Wait for the download to create the file.
    let t0 = Instant::now();
    let file = loop {
        if p.stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let path = if lib.is_ready(key, n) { lib.final_path(key, n) } else { lib.part_path(key, n) };
        if let Ok(f) = File::open(&path) {
            break f;
        }
        if t0.elapsed() > STALL {
            return Err("couldn't download the surah; check the Pi's internet connection".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut g = Growing { file, lib: lib.clone(), key: key.to_string(), n, stop: p.stop.clone(), last_data: Instant::now() };
    let io = |e: std::io::Error| e.to_string();

    g.wait_for(10)?;
    let start = id3_len(&read_at(&mut g.file, 0, 10).map_err(io)?);
    g.wait_for(start + 8192)?;
    let head = read_at(&mut g.file, start, 8192).map_err(io)?;
    let (off, len, us) = find_frame(&head).ok_or("not an MP3 file")?;
    let first = start + off as u64;
    let bytes_per_ms = len as f64 / (us as f64 / 1000.0);
    if let Some(total) = lib.total(key, n) {
        p.duration_ms.store((total.saturating_sub(first) as f64 / bytes_per_ms) as u64, Ordering::Relaxed);
    }

    let mut at = first;
    let mut pos = 0;
    if p.start_ms > 0 {
        let mut target = first + (p.start_ms as f64 * bytes_per_ms) as u64;
        if let Some(total) = lib.total(key, n) {
            target = target.min(total.saturating_sub(16 * 1024).max(first));
        }
        g.wait_for(target + 16 * 1024)?;
        let win = read_at(&mut g.file, target, 16 * 1024).map_err(io)?;
        if let Some((o, _, _)) = find_frame(&win) {
            at = target + o as u64;
            pos = ((at - first) as f64 / bytes_per_ms) as u64;
        }
    }
    g.file.seek(SeekFrom::Start(at)).map_err(io)?;
    Ok((g, pos))
}

impl Source for LocalParams {
    const THREAD: &'static str = "quran";

    fn run(self) {
        run(self);
    }
}

fn run(p: LocalParams) {
    let (mut reader, base) = match open(&p) {
        Ok(v) => v,
        Err(e) => {
            let stopped = p.stop.load(Ordering::Relaxed);
            let _ = p.events.send(RecordingEvent::Ended((!stopped).then_some(e)));
            return;
        }
    };
    p.position_ms.store(base, Ordering::Relaxed);
    let queue = ByteQueue::new();
    let failure: Arc<Mutex<Option<String>>> = Arc::default();

    // File → jitter buffer (blocks while full: paced by playback).
    let feeder = {
        let (queue, stop, failure) = (queue.clone(), p.stop.clone(), failure.clone());
        std::thread::Builder::new()
            .name("quran-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => {
                            if !queue.push(buf[..k].to_vec(), &stop) {
                                break;
                            }
                        }
                        Err(e) => {
                            *failure.lock().unwrap() = Some(e.to_string());
                            break;
                        }
                    }
                }
                queue.close();
            })
            .expect("spawn quran reader")
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
        position_base_ms: base,
        speed: Some(p.speed.clone()),
        channel: crate::sink::Channel::Quran,
    };
    decode::run(Codec::Mp3, ctx);
    queue.close();
    let _ = feeder.join();
    let err = failure.lock().unwrap().take().filter(|_| !p.stop.load(Ordering::Relaxed));
    let _ = p.events.send(RecordingEvent::Ended(err));
}

#[cfg(test)]
mod tests {
    #[test]
    fn id3_and_frames() {
        // ID3v2 header with a 0x0101 syncsafe size = 129 bytes.
        let h = [b'I', b'D', b'3', 4, 0, 0, 0, 0, 1, 1];
        assert_eq!(super::id3_len(&h), 10 + 129);
        assert_eq!(super::id3_len(b"\xff\xfb\x90\x00abcdef"), 0);
        // 128 kbps / 44.1 kHz MPEG1 L3 frames (417 bytes), after 3 junk bytes.
        let mut buf = vec![0u8, 1, 2];
        for _ in 0..3 {
            let mut f = vec![0xFF, 0xFB, 0x90, 0x00];
            f.resize(417, 0);
            buf.extend(f);
        }
        let (off, len, _) = super::find_frame(&buf).unwrap();
        assert_eq!((off, len), (3, 417));
    }
}
