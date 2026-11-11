//! A live broadcast session and the words that describe it in history and status.

use crate::stream::EndReason;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What started a live session (stored in history as its `as_str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    /// The user pressed Play.
    Manual,
    /// The website's live push.
    Sse,
    /// The Icecast status poll (also used to resume after a recording or a switch).
    Poll,
    /// A direct connection attempt while both signal sources are down.
    Probe,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Sse => "sse",
            Self::Poll => "poll",
            Self::Probe => "probe",
        }
    }
}

/// Why the player itself ended a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopReason {
    /// A recording or the Quran took over the speaker.
    Recording,
    /// The user pressed Stop.
    Stopped,
    /// The user chose another masjid.
    Switched,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Stopped => "stopped",
            Self::Switched => "switched",
        }
    }
}

/// A live broadcast being played.
pub(super) struct LiveSession {
    pub token: u64,
    pub stop: Arc<AtomicBool>,
    pub mount: String,
    pub trigger: Trigger,
    /// History entry, once audio has started.
    pub hist_id: Option<u64>,
    pub reconnects: u32,
    /// Set when the player asked the session to end.
    pub stop_reason: Option<StopReason>,
    /// Buffered audio, published by the session.
    pub buffer_ms: Arc<AtomicU64>,
}

impl LiveSession {
    /// Ask the session to end; it reports `Ended` when it has.
    pub fn request_stop(&mut self, reason: StopReason) {
        self.stop_reason = Some(reason);
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How a session's end is recorded in history.
pub(super) fn end_label(reason: &EndReason, stop: Option<StopReason>) -> &'static str {
    match (reason, stop) {
        (_, Some(r)) => r.as_str(),
        (EndReason::Finished, None) => "ended",
        (EndReason::Stopped, None) => "stopped",
        (EndReason::NotLive, None) => "notlive",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_the_stored_history_words() {
        assert_eq!(
            [Trigger::Manual, Trigger::Sse, Trigger::Poll, Trigger::Probe].map(Trigger::as_str),
            ["manual", "sse", "poll", "probe"]
        );
        assert_eq!(end_label(&EndReason::Finished, None), "ended");
        assert_eq!(end_label(&EndReason::Stopped, None), "stopped");
        assert_eq!(end_label(&EndReason::NotLive, None), "notlive");
        // The player's own reason wins over what the session saw.
        assert_eq!(end_label(&EndReason::Stopped, Some(StopReason::Switched)), "switched");
        assert_eq!(end_label(&EndReason::Finished, Some(StopReason::Recording)), "recording");
        assert_eq!(end_label(&EndReason::Stopped, Some(StopReason::Stopped)), "stopped");
    }
}
