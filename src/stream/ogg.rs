//! Ogg page splitter that makes reconnects seamless.
//!
//! After a reconnect Icecast replays the stream headers plus a ~20 s burst of audio we
//! have (mostly) already played. Every Ogg page carries a granule position (sample
//! counter), so we forward only pages *after* the last one we passed on — the decoder
//! sees one continuous logical stream: no gap, no repeated audio.
//!
//! If the serial number changes, the broadcaster restarted the encoder: that is a new
//! logical stream which needs a fresh decoder.

const CAPTURE: &[u8; 4] = b"OggS";
const HEADER_LEN: usize = 27;
const FLAG_CONTINUED: u8 = 0x01;
/// Resync guard: drop junk if no page boundary is found in this much data.
const MAX_PENDING: usize = 256 * 1024;

#[derive(Debug, PartialEq)]
pub enum Feed {
    Ok,
    /// A different logical stream started; contains the bytes from that page onwards.
    NewStream(Vec<u8>),
}

#[derive(Default)]
pub struct OggFilter {
    pending: Vec<u8>,
    serial: Option<u32>,
    last_granule: i64,
    /// Within the current connection: have we reached new audio yet?
    accepting: bool,
}

struct Page {
    len: usize,
    flags: u8,
    granule: i64,
    serial: u32,
}

impl OggFilter {
    pub fn new() -> Self {
        Self { accepting: true, last_granule: -1, ..Default::default() }
    }

    /// Call when a new HTTP connection for the same mount starts delivering bytes.
    pub fn reconnected(&mut self) {
        self.pending.clear();
        self.accepting = self.serial.is_none();
    }

    /// Feed raw bytes; complete pages to forward are appended to `out`.
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<Vec<u8>>) -> Feed {
        self.pending.extend_from_slice(data);
        let mut start = 0;
        loop {
            let buf = &self.pending[start..];
            let Some(off) = find_capture(buf) else {
                // Keep a possible partial "OggS" at the tail.
                start += buf.len().saturating_sub(3);
                break;
            };
            let page_at = start + off;
            let Some(page) = parse_page(&self.pending[page_at..]) else {
                start = page_at;
                break; // incomplete page, wait for more data
            };
            let bytes = &self.pending[page_at..page_at + page.len];

            match self.serial {
                None => self.serial = Some(page.serial),
                Some(s) if s != page.serial => {
                    let rest = self.pending[page_at..].to_vec();
                    self.pending.clear();
                    return Feed::NewStream(rest);
                }
                Some(_) => {}
            }

            if !self.accepting {
                // Skip replayed headers and audio we already forwarded. Resume at the
                // first page that both starts a fresh packet and is newer than the last.
                if page.granule > self.last_granule && page.flags & FLAG_CONTINUED == 0 {
                    self.accepting = true;
                }
            }
            if self.accepting {
                out.push(bytes.to_vec());
                if page.granule >= 0 {
                    self.last_granule = self.last_granule.max(page.granule);
                }
            }
            start = page_at + page.len;
        }
        self.pending.drain(..start.min(self.pending.len()));
        if self.pending.len() > MAX_PENDING {
            self.pending.clear();
        }
        Feed::Ok
    }
}

fn find_capture(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == CAPTURE)
}

fn parse_page(buf: &[u8]) -> Option<Page> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let segments = buf[26] as usize;
    if buf.len() < HEADER_LEN + segments {
        return None;
    }
    let body: usize = buf[HEADER_LEN..HEADER_LEN + segments].iter().map(|&b| b as usize).sum();
    let len = HEADER_LEN + segments + body;
    if buf.len() < len {
        return None;
    }
    Some(Page {
        len,
        flags: buf[5],
        granule: i64::from_le_bytes(buf[6..14].try_into().ok()?),
        serial: u32::from_le_bytes(buf[14..18].try_into().ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(serial: u32, granule: i64, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(b"OggS");
        p.push(0);
        p.push(flags);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&serial.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes()); // sequence
        p.extend_from_slice(&0u32.to_le_bytes()); // crc (not checked here)
        p.push(1);
        p.push(payload.len() as u8);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn forwards_everything_first_time_and_handles_split_input() {
        let mut f = OggFilter::new();
        let mut data = page(7, 0, 2, b"OpusHead");
        data.extend(page(7, 960, 0, b"a"));
        let mut out = Vec::new();
        let (a, b) = data.split_at(30);
        assert_eq!(f.feed(a, &mut out), Feed::Ok);
        assert_eq!(f.feed(b, &mut out), Feed::Ok);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn reconnect_skips_headers_and_replayed_audio() {
        let mut f = OggFilter::new();
        let mut out = Vec::new();
        for p in [page(7, 0, 2, b"H"), page(7, 0, 0, b"T"), page(7, 960, 0, b"1"), page(7, 1920, 0, b"2")] {
            f.feed(&p, &mut out);
        }
        out.clear();
        f.reconnected();
        let mut burst = page(7, 0, 2, b"H");
        burst.extend(page(7, 0, 0, b"T"));
        burst.extend(page(7, 960, 0, b"1"));
        burst.extend(page(7, 1920, 0, b"2"));
        burst.extend(page(7, 2880, 0, b"3"));
        burst.extend(page(7, 3840, 0, b"4"));
        f.feed(&burst, &mut out);
        assert_eq!(out.len(), 2, "only pages 3 and 4 are new");
        assert!(out[0].ends_with(b"3"));
    }

    #[test]
    fn new_serial_is_a_new_stream() {
        let mut f = OggFilter::new();
        let mut out = Vec::new();
        f.feed(&page(7, 960, 0, b"1"), &mut out);
        let p = page(9, 0, 2, b"H");
        assert_eq!(f.feed(&p, &mut out), Feed::NewStream(p.clone()));
    }

    #[test]
    fn skips_junk_before_capture() {
        let mut f = OggFilter::new();
        let mut out = Vec::new();
        let mut data = b"garbage".to_vec();
        data.extend(page(7, 960, 0, b"1"));
        f.feed(&data, &mut out);
        assert_eq!(out.len(), 1);
    }
}
