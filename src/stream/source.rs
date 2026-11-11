//! What every playback source (live broadcast, recording, Quran file) has in common: it runs on
//! its own thread and reports events to the player, tagged with the session they belong to, so
//! late events from a session the player has already replaced are recognised and ignored.

use tokio::sync::mpsc::UnboundedSender;

/// The player has gone away (the service is shutting down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// A sender that tags every event with its session's token.
pub struct Events<E> {
    token: u64,
    tx: UnboundedSender<(u64, E)>,
}

// Manual impl: a derive would needlessly require `E: Clone`.
impl<E> Clone for Events<E> {
    fn clone(&self) -> Self {
        Self { token: self.token, tx: self.tx.clone() }
    }
}

impl<E> Events<E> {
    pub fn new(token: u64, tx: &UnboundedSender<(u64, E)>) -> Self {
        Self { token, tx: tx.clone() }
    }

    /// Report an event for this session.
    ///
    /// # Errors
    /// [`Closed`] once the player's receiver is gone.
    pub fn send(&self, event: E) -> Result<(), Closed> {
        self.tx.send((self.token, event)).map_err(|_| Closed)
    }
}

/// A playback source: blocking work that runs on a thread of its own until it ends or is stopped.
pub trait Source: Send + 'static {
    /// Thread name (shows in `top -H` and in panics).
    const THREAD: &'static str;

    /// Play until done; report progress through the params' [`Events`].
    fn run(self);
}

/// Start `source` on its own thread.
///
/// # Panics
/// If the OS refuses to create a thread (out of memory); the service restarts.
pub fn spawn<S: Source>(source: S) {
    std::thread::Builder::new()
        .name(S::THREAD.into())
        .spawn(move || source.run())
        .expect("spawn playback thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_tagged_with_their_session() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let a = Events::new(1, &tx);
        let b = Events::new(2, &tx);
        a.clone().send("x").unwrap();
        b.send("y").unwrap();
        assert_eq!(rx.try_recv().unwrap(), (1, "x"));
        assert_eq!(rx.try_recv().unwrap(), (2, "y"));
        drop(rx);
        assert_eq!(a.send("z"), Err(Closed));
    }
}
