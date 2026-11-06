//! Live detection, three independent layers:
//!  1. [`sse`]  — the website's instant "online" push (only while waiting, only when welcome).
//!  2. [`poll`] — Icecast status JSON every ~20 s with jitter and back-off.
//!  3. The stream itself — the player treats actual audio as the final truth.

pub mod poll;
pub mod sse;

pub use poll::{LiveMap, is_live};

/// ±20 % randomisation so devices never synchronise their requests.
pub(crate) fn jitter(secs: u64) -> u64 {
    use rand::Rng;
    let s = secs.max(1) as f64;
    (s * rand::rng().random_range(0.8..1.2)).round().max(1.0) as u64
}
