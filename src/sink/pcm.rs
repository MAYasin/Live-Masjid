//! Sample maths for the output: volume, byte encoding, the browser mirror, chunk timing and the
//! test chime. Pure functions, so they are tested without a device.

use super::RATE;

/// Unity gain in Q16 fixed point.
pub(super) const UNITY: i32 = 65536;

/// Sample rate of the browser mirror (mono). 24 kHz is plenty for speech and adhan.
pub const MIRROR_RATE: u32 = 24_000;

/// Gain for a volume of 0–100 on a perceptual (squared) curve, Q16 fixed point.
pub(super) fn gain_q16(volume: u8) -> i32 {
    let v = i32::from(volume.min(100));
    v * v * UNITY / 10_000
}

/// Interleaved s16 samples as little-endian bytes, scaled by `gain` (Q16), appended to `out`.
pub(super) fn encode(data: &[i16], gain: i32, out: &mut Vec<u8>) {
    out.reserve(data.len() * 2);
    if gain == UNITY {
        for s in data {
            out.extend_from_slice(&s.to_le_bytes());
        }
    } else {
        for s in data {
            // |s × gain| ≤ 32768 × 65536 fits in i32 after the shift.
            #[allow(clippy::cast_possible_truncation)]
            let x = ((i32::from(*s) * gain) >> 16) as i16;
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
}

/// Duration of an interleaved stereo chunk, in µs.
pub(super) fn chunk_us(rate: u32, stereo: &[i16]) -> u64 {
    (stereo.len() / 2) as u64 * 1_000_000 / u64::from(rate.max(1))
}

/// Down-mix to mono and resample to [`MIRROR_RATE`] (nearest sample: cheap and fine for
/// speech), as little-endian bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub(super) fn mirror_mono(rate: u32, stereo: &[i16]) -> Vec<u8> {
    let frames = stereo.len() / 2;
    let step = f64::from(rate) / f64::from(MIRROR_RATE);
    let mut out = Vec::with_capacity((frames as f64 / step) as usize * 2 + 2);
    let mut pos = 0.0f64;
    while (pos as usize) < frames {
        let i = pos as usize * 2;
        // The mean of two i16 values is always an i16.
        let mono = ((i32::from(stereo[i]) + i32::from(stereo[i + 1])) / 2) as i16;
        out.extend_from_slice(&mono.to_le_bytes());
        pos += step;
    }
    out
}

/// A short two-tone chime used by "Test speaker".
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
pub fn chime() -> Vec<i16> {
    let mut v = Vec::with_capacity(RATE as usize * 2);
    for (freq, dur) in [(660.0f32, 0.25f32), (880.0, 0.45)] {
        let n = (RATE as f32 * dur) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = (1.0 - i as f32 / n as f32).powf(1.5) * (i as f32 / 400.0).min(1.0);
            let s = ((t * freq * std::f32::consts::TAU).sin() * env * 9000.0) as i16;
            v.push(s);
            v.push(s);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_curve() {
        assert_eq!(gain_q16(100), UNITY);
        assert_eq!(gain_q16(200), UNITY, "clamped");
        assert_eq!(gain_q16(50), UNITY / 4, "squared: half volume is a quarter of the amplitude");
        assert_eq!(gain_q16(0), 0);
    }

    #[test]
    fn encoding_applies_gain() {
        let mut out = Vec::new();
        encode(&[1000, -1000, i16::MAX, i16::MIN], UNITY, &mut out);
        assert_eq!(out, [1000i16, -1000, i16::MAX, i16::MIN].iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<_>>());
        out.clear();
        encode(&[1000, -1000, i16::MAX, i16::MIN], gain_q16(50), &mut out);
        let back: Vec<i16> = out.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(back, [250, -250, 8191, -8192]);
        out.clear();
        encode(&[1000, -1000], 0, &mut out);
        assert_eq!(out, [0; 4], "muted");
    }

    #[test]
    fn chunk_duration() {
        assert_eq!(chunk_us(48_000, &[0; 1920]), 20_000, "960 stereo frames at 48 kHz = 20 ms");
        assert_eq!(chunk_us(44_100, &[0; 882]), 10_000);
        assert_eq!(chunk_us(0, &[0; 2]), 1_000_000, "a zero rate doesn't divide by zero");
    }

    #[test]
    fn mirror_is_mono_at_24_khz() {
        // 20 ms of 48 kHz stereo: left 100, right 300 → mono 200, half the frames.
        let stereo: Vec<i16> = (0..960).flat_map(|_| [100, 300]).collect();
        let m = mirror_mono(48_000, &stereo);
        assert_eq!(m.len(), 480 * 2);
        assert!(m.chunks(2).all(|b| i16::from_le_bytes([b[0], b[1]]) == 200));
        // 44.1 kHz input still yields 24 kHz output (±1 frame).
        let frames = mirror_mono(44_100, &vec![0; 882 * 2]).len() / 2;
        assert!((479..=481).contains(&frames), "{frames}");
    }

    #[test]
    fn chime_is_short_and_not_clipped() {
        let c = chime();
        assert_eq!(c.len(), (RATE as usize * 70 / 100) * 2);
        assert!(c.iter().all(|s| s.unsigned_abs() <= 9000));
    }
}
