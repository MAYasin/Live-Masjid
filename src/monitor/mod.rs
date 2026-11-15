//! Live detection, two independent layers:
//!  1. [`poll`] — Icecast status JSON every ~20 s with jitter and back-off.
//!  2. The stream itself — the player probes it directly when the poll is down for a while,
//!     and treats actual audio as the final truth.
//!
//! The website's "online" push (www.livemasjid.com/stateQuery.php) is deliberately not used:
//! the site kept refusing it, and the Icecast servers alone are enough to detect a broadcast.

pub mod poll;

pub use poll::{LiveMap, is_live};

/// ±20 % randomisation so devices never synchronise their requests.
pub(crate) fn jitter(secs: u64) -> u64 {
    use rand::RngExt;
    let s = secs.max(1) as f64;
    (s * rand::rng().random_range(0.8..1.2)).round().max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_bounds() {
        for secs in [1u64, 2, 15, 20, 300, 3600] {
            let lo = (secs as f64 * 0.8).round().max(1.0) as u64;
            let hi = (secs as f64 * 1.2).round().max(1.0) as u64;
            for _ in 0..500 {
                let j = jitter(secs);
                assert!((lo..=hi).contains(&j), "jitter({secs}) = {j}, want {lo}..={hi}");
            }
        }
    }

    #[test]
    fn jitter_never_zero() {
        assert_eq!(jitter(0), 1);
        assert_eq!(jitter(1), 1);
    }

    #[test]
    fn jitter_varies() {
        let seen: std::collections::HashSet<u64> = (0..200).map(|_| jitter(300)).collect();
        assert!(seen.len() > 1, "jitter should not be constant");
    }
}
