//! Start at the live edge.
//!
//! On connect Icecast sends a fixed-size *byte* burst of recent audio, then real time. For a
//! ~35 kbps Opus mount that is ~28 s of audio, for a 64 kbps MP3 mount ~17 s, so a player
//! that plays the burst from the start is that far behind live. We keep only the newest
//! `delay` of it (whole Ogg pages / whole MP3 frames) and drop the rest.

use std::time::Duration;

/// Opus granule positions count 48 kHz samples.
const OPUS_RATE: i64 = 48_000;
const FLAG_CONTINUED: u8 = 0x01;

/// Granule of an Ogg page (bytes 6..14), or -1 if it isn't a page.
fn granule(page: &[u8]) -> i64 {
    page.get(6..14).map_or(-1, |b| i64::from_le_bytes(b.try_into().unwrap()))
}

/// Keep the stream headers (the leading granule-0 pages) and the newest `delay` of audio
/// pages, starting at a page that begins a fresh packet.
pub fn opus_pages(pages: Vec<Vec<u8>>, delay: Duration) -> Vec<Vec<u8>> {
    let headers = pages.iter().take_while(|p| granule(p) == 0).count();
    let Some(newest) = pages.iter().map(|p| granule(p)).max() else { return pages };
    let cutoff = newest - delay.as_millis() as i64 * OPUS_RATE / 1000;
    let audio = &pages[headers..];
    let Some(first) = audio
        .iter()
        .position(|p| granule(p) >= cutoff && p.get(5).is_some_and(|f| f & FLAG_CONTINUED == 0))
    else {
        return pages;
    };
    if first == 0 {
        return pages;
    }
    let mut out: Vec<Vec<u8>> = pages[..headers].to_vec();
    out.extend_from_slice(&audio[first..]);
    out
}

/// One MP3 frame header: (frame length in bytes, duration in µs).
pub(crate) fn mp3_frame(h: &[u8]) -> Option<(usize, u64)> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 3 = MPEG1, 2 = MPEG2, 0 = MPEG2.5
    let layer = (h[1] >> 1) & 3; // 1 = Layer III
    let br_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    if version == 1 || layer != 1 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    const BR_V1: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BR_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let mpeg1 = version == 3;
    let kbps = if mpeg1 { BR_V1[br_idx] } else { BR_V2[br_idx] };
    let sr = [44_100u32, 48_000, 32_000][sr_idx] >> if mpeg1 { 0 } else if version == 2 { 1 } else { 2 };
    let pad = u32::from((h[2] >> 1) & 1);
    let (len, samples) = if mpeg1 { (144 * kbps * 1000 / sr + pad, 1152) } else { (72 * kbps * 1000 / sr + pad, 576) };
    Some((len as usize, samples * 1_000_000 / u64::from(sr)))
}

/// Offset in `data` at which to start so that about `delay` of MP3 audio remains.
/// Only cuts at a frame boundary confirmed by the next frame's header; 0 if unsure.
pub fn mp3_start(data: &[u8], delay: Duration) -> usize {
    // Find the first frame whose successor is also a frame (avoids false syncs).
    let mut pos = 0;
    let found = loop {
        if pos + 4 > data.len() {
            return 0;
        }
        if let Some((len, _)) = mp3_frame(&data[pos..]) {
            if mp3_frame(data.get(pos + len..).unwrap_or(&[])).is_some() {
                break pos;
            }
        }
        pos += 1;
    };
    // Walk the frames.
    let mut frames: Vec<(usize, u64)> = Vec::new();
    let mut at = found;
    while let Some((len, us)) = mp3_frame(&data[at..]) {
        if at + len > data.len() || len < 4 {
            break;
        }
        frames.push((at, us));
        at += len;
    }
    let want = delay.as_micros() as u64;
    let mut have = 0u64;
    for &(off, us) in frames.iter().rev() {
        have += us;
        if have >= want {
            return off;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(granule: i64, flags: u8) -> Vec<u8> {
        let mut p = b"OggS".to_vec();
        p.push(0);
        p.push(flags);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&[0; 13]);
        p
    }

    #[test]
    fn opus_keeps_headers_and_newest_audio() {
        let mut pages = vec![page(0, 2), page(0, 0)];
        for s in 1..=30 {
            pages.push(page(s * OPUS_RATE, 0)); // one page per second
        }
        pages[2 + 26] = page(27 * OPUS_RATE, FLAG_CONTINUED); // continued: not a valid start
        let out = opus_pages(pages, Duration::from_secs(3));
        let g: Vec<i64> = out.iter().map(|p| granule(p) / OPUS_RATE).collect();
        assert_eq!(g, vec![0, 0, 28, 29, 30]);
    }

    #[test]
    fn opus_short_burst_is_untouched() {
        let pages = vec![page(0, 2), page(0, 0), page(OPUS_RATE, 0)];
        assert_eq!(opus_pages(pages.clone(), Duration::from_secs(3)), pages);
    }

    #[test]
    fn mp3_cuts_at_a_frame() {
        // MPEG1 Layer III, 64 kbps, 48 kHz, no padding: 192-byte frames of 24 ms.
        let frame = |fill: u8| {
            let mut f = vec![0xFF, 0xFB, 0x54, 0x00];
            f.resize(192, fill);
            f
        };
        let mut data = vec![1, 2, 3]; // junk before the first sync
        for i in 0..100u8 {
            data.extend(frame(i));
        }
        assert_eq!(mp3_frame(&data[3..]), Some((192, 24_000)));
        let start = mp3_start(&data, Duration::from_millis(240)); // 10 frames
        assert_eq!(start, 3 + 90 * 192);
        assert_eq!(mp3_start(&data, Duration::from_secs(60)), 3);
    }
}
