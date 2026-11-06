//! Jitter buffer between the network and the decoder.
//!
//! The network side pushes byte chunks as fast as they arrive (Icecast sends a burst of
//! ~20 s on connect, then real time). The decoder side reads at playback speed. Whatever
//! sits in between is audio we can keep playing while the network stalls or reconnects.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Upper bound on buffered compressed audio (~8 min of Opus, ~4 min of 128k MP3).
const CAPACITY: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Inner {
    chunks: VecDeque<Vec<u8>>,
    len: usize,
    closed: bool,
}

#[derive(Default)]
pub struct ByteQueue {
    inner: Mutex<Inner>,
    cv: Condvar,
    /// Total bytes handed to the reader (for bitrate estimation).
    consumed: AtomicU64,
}

impl ByteQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Append data, blocking while the buffer is full. Returns false if the queue was
    /// closed or `stop` was raised while waiting.
    pub fn push(&self, data: Vec<u8>, stop: &AtomicBool) -> bool {
        if data.is_empty() {
            return true;
        }
        let mut g = self.inner.lock().unwrap();
        while g.len >= CAPACITY && !g.closed {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            g = self.cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
        }
        if g.closed {
            return false;
        }
        g.len += data.len();
        g.chunks.push_back(data);
        self.cv.notify_all();
        true
    }

    /// No more data: the reader drains what's left, then sees EOF.
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.cv.notify_all();
    }

    pub fn buffered(&self) -> usize {
        self.inner.lock().unwrap().len
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }
}

/// `Read` adapter for the decoder thread.
pub struct QueueReader {
    queue: Arc<ByteQueue>,
    stop: Arc<AtomicBool>,
    current: Vec<u8>,
    pos: usize,
}

impl QueueReader {
    pub fn new(queue: Arc<ByteQueue>, stop: Arc<AtomicBool>) -> Self {
        Self { queue, stop, current: Vec::new(), pos: 0 }
    }
}

impl Read for QueueReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.current.len() {
            let mut g = self.queue.inner.lock().unwrap();
            loop {
                if self.stop.load(Ordering::Relaxed) {
                    return Ok(0);
                }
                if let Some(chunk) = g.chunks.pop_front() {
                    g.len -= chunk.len();
                    self.queue.cv.notify_all();
                    self.current = chunk;
                    self.pos = 0;
                    break;
                }
                if g.closed {
                    return Ok(0);
                }
                g = self.queue.cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
            }
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        self.queue.consumed.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_then_eof() {
        let q = ByteQueue::new();
        let stop = Arc::new(AtomicBool::new(false));
        assert!(q.push(b"hello ".to_vec(), &stop));
        assert!(q.push(b"world".to_vec(), &stop));
        q.close();
        let mut s = String::new();
        QueueReader::new(q.clone(), stop).read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello world");
        assert_eq!(q.consumed(), 11);
        assert!(!q.push(b"late".to_vec(), &AtomicBool::new(false)));
    }

    #[test]
    fn stop_unblocks_reader() {
        let q = ByteQueue::new();
        let stop = Arc::new(AtomicBool::new(true));
        let mut buf = [0u8; 4];
        assert_eq!(QueueReader::new(q, stop).read(&mut buf).unwrap(), 0);
    }
}
