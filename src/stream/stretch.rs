//! Playback speed without changing pitch (for Quran recitation): streaming WSOLA on
//! interleaved stereo PCM.
//!
//! The input is cut into 30 ms segments taken every `hop × speed`, and written out every
//! `hop` with a 10 ms crossfade. Each segment's start is nudged (up to 8 ms) to where it best
//! matches the end of the previous one, so voices stay smooth instead of warbling. The search
//! correlates every second sample, which keeps it to a few percent of one Pi Zero 2 core.

const SEG_MS: u32 = 30;
const OVERLAP_MS: u32 = 10;
const SEEK_MS: u32 = 8;

pub struct Stretch {
    /// Unconsumed input, interleaved L/R.
    input: Vec<f32>,
    /// Read position into `input`, in frames (fractional: input hop = output hop × speed).
    pos: f64,
    /// Last `overlap` frames of the previous segment, to crossfade into the next.
    tail: Vec<f32>,
    seg: usize,
    overlap: usize,
    seek: usize,
}

impl Stretch {
    pub fn new(rate: u32) -> Self {
        let frames = |ms: u32| (rate * ms / 1000).max(8) as usize;
        Self { input: Vec::new(), pos: 0.0, tail: Vec::new(), seg: frames(SEG_MS), overlap: frames(OVERLAP_MS), seek: frames(SEEK_MS) }
    }

    /// Feed interleaved stereo; returns whatever stretched audio is ready (may be empty).
    pub fn process(&mut self, stereo: &[i16], speed: f32) -> Vec<i16> {
        let speed = f64::from(speed.clamp(0.5, 2.0));
        self.input.extend(stereo.iter().map(|&s| f32::from(s)));
        let frames = self.input.len() / 2;
        let (seg, ov, seek) = (self.seg, self.overlap, self.seek);
        let hop_out = seg - ov;
        let mut out: Vec<f32> = Vec::new();
        loop {
            let p = self.pos as usize;
            if p + seg + seek > frames {
                break;
            }
            let k = if self.tail.is_empty() { 0 } else { self.best_offset(p) };
            let start = p + k;
            let at = |f: usize, c: usize| self.input[(start + f) * 2 + c];
            if self.tail.is_empty() {
                for f in 0..hop_out {
                    out.extend_from_slice(&[at(f, 0), at(f, 1)]);
                }
            } else {
                for f in 0..ov {
                    let w = (f as f32 + 0.5) / ov as f32;
                    for c in 0..2 {
                        out.push(self.tail[f * 2 + c] * (1.0 - w) + at(f, c) * w);
                    }
                }
                for f in ov..hop_out {
                    out.extend_from_slice(&[at(f, 0), at(f, 1)]);
                }
            }
            self.tail = self.input[(start + hop_out) * 2..(start + seg) * 2].to_vec();
            self.pos += hop_out as f64 * speed;
        }
        // Forget input that no future segment can use.
        let drop = (self.pos as usize).min(frames);
        self.input.drain(..drop * 2);
        self.pos -= drop as f64;
        out.into_iter().map(|v| v.round().clamp(-32768.0, 32767.0) as i16).collect()
    }

    /// Offset (frames) within the seek window where the input best continues `tail`.
    fn best_offset(&self, p: usize) -> usize {
        let mono = |v: &[f32], f: usize| v[f * 2] + v[f * 2 + 1];
        let (mut best, mut best_k) = (f32::MIN, 0);
        for k in (0..self.seek).step_by(2) {
            let (mut dot, mut energy) = (0.0f32, 1e-3f32);
            for f in (0..self.overlap).step_by(2) {
                let x = mono(&self.input, p + k + f);
                dot += mono(&self.tail, f) * x;
                energy += x * x;
            }
            let score = dot / energy.sqrt();
            if score > best {
                (best, best_k) = (score, k);
            }
        }
        best_k
    }
}

#[cfg(test)]
mod tests {
    use super::Stretch;

    fn tone(frames: usize, rate: u32) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let v = ((i as f32 * 220.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect()
    }

    /// Output length follows 1/speed (within a segment), fed in decoder-sized chunks.
    #[test]
    fn duration_scales_with_speed() {
        let rate = 48_000;
        let input = tone(rate as usize * 4, rate); // 4 s
        for speed in [0.75f32, 1.25, 1.5, 2.0] {
            let mut st = Stretch::new(rate);
            let out: usize = input.chunks(1920).map(|c| st.process(c, speed).len()).sum();
            let got = out as f32 / 2.0 / rate as f32;
            let want = 4.0 / speed;
            assert!((got - want).abs() < 0.1, "speed {speed}: {got:.3} s, want {want:.3} s");
        }
    }

    /// A steady tone stays a steady tone: no clicks from the splices.
    #[test]
    fn splices_are_smooth() {
        let rate = 48_000;
        let mut st = Stretch::new(rate);
        let out = st.process(&tone(rate as usize, rate), 1.5);
        let max_step = out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs()).max().unwrap();
        // A 220 Hz sine at amplitude 8000 changes by at most ~230 per sample at 48 kHz.
        assert!(max_step < 700, "largest jump {max_step}");
    }
}
