//! On-demand playback: a past recording or a Quran surah on the speaker. Unlike live broadcasts,
//! these can pause, seek and move to the next item. The pure helpers here hold the rules (heard
//! position, next surah) so they can be tested without a speaker.

use crate::quran::{Library, PlaylistCtx, Reciter};
use crate::sink::SinkHandle;
use crate::state::{QuranResume, RecordingStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Audio between the sink queue and the listener's ears: writer queue, pipe, aplay's 250 ms
/// buffer and Bluetooth (about 0.7 s, see the browser-sync notes).
pub(super) const OUTPUT_LAG_MS: u64 = 700;

/// Flags shared with the playback thread.
#[derive(Clone)]
pub(super) struct Controls {
    pub stop: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    /// Decoded position (the heard position while paused).
    pub position_ms: Arc<AtomicU64>,
    /// Length, once the file header has been read (Quran).
    pub duration_ms: Arc<AtomicU64>,
}

impl Controls {
    pub fn new(position_ms: u64, paused: bool) -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(paused)),
            position_ms: Arc::new(AtomicU64::new(position_ms)),
            duration_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// The Quran part of an on-demand item.
pub(super) struct Surah {
    pub number: u16,
    /// The playlist this surah belongs to.
    pub playlist: Option<PlaylistCtx>,
    pub reciter: Reciter,
    /// For download progress.
    pub lib: Arc<Library>,
}

/// A recording or a surah playing on the speaker.
pub(super) struct Media {
    pub token: u64,
    pub controls: Controls,
    /// The masjid a recording is from (empty for the Quran).
    pub mount: String,
    pub title: String,
    /// Recording length from its feed, if known.
    pub duration_s: Option<u64>,
    /// Was the selected masjid already live when this started? Only a broadcast that starts
    /// *during* playback interrupts it.
    pub live_at_start: bool,
    pub surah: Option<Surah>,
    /// Playback speed (f32 bits), shared with the decoder.
    pub speed: Arc<AtomicU32>,
    /// The output, to tell how much decoded audio hasn't been heard yet.
    pub sink: SinkHandle,
}

impl Media {
    pub fn speed(&self) -> f32 {
        f32::from_bits(self.speed.load(Ordering::Relaxed))
    }

    pub fn is_quran(&self) -> bool {
        self.surah.is_some()
    }

    /// Position the listener is hearing.
    pub fn heard_ms(&self) -> u64 {
        heard_ms(
            self.controls.position_ms.load(Ordering::Relaxed),
            self.controls.is_paused(),
            self.sink.queued_ms(),
            self.speed(),
        )
    }

    /// The surah before/after (`by` = -1/+1) this one.
    pub fn neighbour(&self, by: i32) -> Option<(u16, Option<PlaylistCtx>)> {
        let s = self.surah.as_ref()?;
        neighbour(s.number, s.playlist.as_ref(), |n| s.reciter.has(n), by)
    }

    pub fn status(&self) -> RecordingStatus {
        let known = self.controls.duration_ms.load(Ordering::Relaxed);
        let s = self.surah.as_ref();
        RecordingStatus {
            mount: self.mount.clone(),
            title: self.title.clone(),
            position_s: self.heard_ms() / 1000,
            duration_s: self.duration_s.or((known > 0).then_some(known / 1000)),
            paused: self.controls.is_paused(),
            kind: if s.is_some() { "quran" } else { "recording" },
            surah: s.map(|s| s.number),
            playlist: s.and_then(|s| s.playlist.clone()),
            loaded_pct: s.and_then(|s| s.lib.progress(&s.reciter.key, s.number)),
            reciter: s.map(|s| s.reciter.label()),
            reciter_key: s.map(|s| s.reciter.key.clone()),
            speed: self.speed(),
        }
    }

    /// Where the Quran was, to continue after a live broadcast (None for a recording).
    pub fn resume_point(&self) -> Option<QuranResume> {
        let s = self.surah.as_ref()?;
        Some(QuranResume {
            surah: s.number,
            title: self.title.clone(),
            position_s: self.heard_ms() / 1000,
            auto: !self.controls.is_paused(),
            playlist: s.playlist.clone(),
            reciter: s.reciter.key.clone(),
            speed: self.speed(),
        })
    }

    /// Pause at once: freeze the heard position (the caller drops the queued audio).
    pub fn freeze(&self) {
        let heard = self.heard_ms();
        self.controls.paused.store(true, Ordering::Relaxed);
        self.controls.position_ms.store(heard, Ordering::Relaxed);
    }

    /// Ask the playback thread to stop.
    pub fn stop(&self) {
        self.controls.stop.store(true, Ordering::Relaxed);
        self.controls.paused.store(false, Ordering::Relaxed);
    }
}

/// Position heard: `decoded` minus what is still queued for the speaker (which plays at `speed`,
/// so it covers that much more recitation). While paused, `decoded` already is the heard position.
pub(super) fn heard_ms(decoded: u64, paused: bool, queued_ms: u64, speed: f32) -> u64 {
    if paused {
        return decoded;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let queued = ((queued_ms + OUTPUT_LAG_MS) as f32 * speed) as u64;
    decoded.saturating_sub(queued)
}

/// The surah before/after (`by` = -1/+1) `surah`: within its playlist, else in Quran order.
/// Surahs the reciter doesn't have (`has`) are skipped. None at the end (a playlist without
/// repeat, or An-Nās).
pub(super) fn neighbour(
    surah: u16,
    playlist: Option<&PlaylistCtx>,
    has: impl Fn(u16) -> bool,
    by: i32,
) -> Option<(u16, Option<PlaylistCtx>)> {
    match playlist {
        Some(p) => {
            let mut q = p.step(by)?;
            for _ in 0..p.surahs.len() {
                if has(q.surah()) {
                    return Some((q.surah(), Some(q)));
                }
                let next = q.step(if by < 0 { -1 } else { 1 })?;
                if next.index == q.index {
                    return None;
                }
                q = next;
            }
            None
        }
        None => {
            let mut n = i32::from(surah) + by;
            while (1..=114).contains(&n) {
                let n16 = u16::try_from(n).ok()?;
                if has(n16) {
                    return Some((n16, None));
                }
                n += by.signum();
            }
            None
        }
    }
}

/// The playlist entry to start from: the first one (from `ctx`) the reciter has.
pub(super) fn first_available(mut ctx: PlaylistCtx, has: impl Fn(u16) -> bool) -> Option<PlaylistCtx> {
    for _ in 0..ctx.surahs.len() {
        if has(ctx.surah()) {
            return Some(ctx);
        }
        ctx = ctx.step(1)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quran::Playlist;

    fn playlist(surahs: &[u16], repeat: bool) -> Playlist {
        Playlist { id: "p".into(), name: "P".into(), surahs: surahs.to_vec(), repeat, ..Playlist::default() }
    }

    #[test]
    fn heard_position_subtracts_queued_audio_at_speed() {
        assert_eq!(heard_ms(10_000, false, 1_300, 1.0), 8_000);
        assert_eq!(heard_ms(10_000, false, 1_300, 2.0), 6_000);
        assert_eq!(heard_ms(10_000, true, 1_300, 2.0), 10_000, "paused: already the heard position");
        assert_eq!(heard_ms(500, false, 1_300, 1.0), 0, "never negative");
    }

    #[test]
    fn next_surah_in_quran_order() {
        let all = |_| true;
        assert_eq!(neighbour(1, None, all, 1), Some((2, None)));
        assert_eq!(neighbour(2, None, all, -1), Some((1, None)));
        assert_eq!(neighbour(114, None, all, 1), None);
        assert_eq!(neighbour(1, None, all, -1), None);
        // Missing surahs are skipped.
        assert_eq!(neighbour(1, None, |n| n != 2 && n != 3, 1), Some((4, None)));
        assert_eq!(neighbour(113, None, |n| n != 114, 1), None);
    }

    #[test]
    fn next_surah_in_a_playlist() {
        let all = |_| true;
        let p = PlaylistCtx::new(&playlist(&[36, 67, 18], false), 0);
        let (n, ctx) = neighbour(36, Some(&p), all, 1).unwrap();
        assert_eq!((n, ctx.as_ref().map(|c| c.index)), (67, Some(1)));
        let last = PlaylistCtx::new(&playlist(&[36, 67, 18], false), 2);
        assert_eq!(neighbour(18, Some(&last), all, 1), None, "no repeat: stops at the end");
        let looped = PlaylistCtx::new(&playlist(&[36, 67, 18], true), 2);
        assert_eq!(neighbour(18, Some(&looped), all, 1).map(|x| x.0), Some(36), "repeat wraps");
        // A surah the reciter lacks is skipped; none available ends the playlist.
        assert_eq!(neighbour(36, Some(&p), |n| n != 67, 1).map(|x| x.0), Some(18));
        assert_eq!(neighbour(36, Some(&looped), |_| false, 1), None);
    }

    #[test]
    fn playlist_starts_at_the_first_available_surah() {
        let p = PlaylistCtx::new(&playlist(&[36, 67, 18], false), 0);
        assert_eq!(first_available(p.clone(), |_| true).map(|c| c.index), Some(0));
        assert_eq!(first_available(p.clone(), |n| n == 18).map(|c| c.index), Some(2));
        assert!(first_available(p, |_| false).is_none());
    }
}
