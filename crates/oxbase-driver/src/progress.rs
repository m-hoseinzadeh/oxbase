//! [`ProgressSink`]: the channel long-running operations report [`Progress`] through
//! (PLAN §1, rule 4).

use oxbase_types::Progress;
use tokio::sync::mpsc;

/// Sending half of a progress channel, handed to a long-running operation next to its
/// [`CancellationToken`](crate::CancellationToken).
///
/// Reporting never blocks and never fails the operation: when the receiver is slow (channel
/// full) or gone, the event is dropped. Progress is advisory; results and errors travel through
/// the operation's return value. Cloning gives another handle to the same channel.
#[derive(Debug, Clone, Default)]
pub struct ProgressSink {
    tx: Option<mpsc::Sender<Progress>>,
}

impl ProgressSink {
    /// A sink that discards every event, for callers that do not show progress.
    pub fn discard() -> Self {
        Self { tx: None }
    }

    /// A new bounded channel holding up to `capacity` unread events (at least 1).
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<Progress>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Self { tx: Some(tx) }, rx)
    }

    /// Wraps an existing sender.
    pub fn from_sender(tx: mpsc::Sender<Progress>) -> Self {
        Self { tx: Some(tx) }
    }

    /// Reports `event`. Returns `true` if it was queued, `false` if it was dropped (no receiver,
    /// receiver closed, or channel full).
    pub fn report(&self, event: Progress) -> bool {
        self.tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(event).is_ok())
    }

    /// `true` when nobody can receive events any more (discarding sink or receiver dropped), so
    /// the operation can skip building them.
    pub fn is_closed(&self) -> bool {
        self.tx.as_ref().is_none_or(mpsc::Sender::is_closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_drops_everything() {
        let sink = ProgressSink::discard();
        assert!(sink.is_closed());
        assert!(!sink.report(Progress::new("x", 1)));
        assert!(ProgressSink::default().is_closed());
    }

    #[test]
    fn channel_delivers_and_drops_when_full_or_closed() {
        let (sink, mut rx) = ProgressSink::channel(2);
        assert!(!sink.is_closed());
        assert!(sink.report(Progress::new("x", 1)));
        assert!(sink.clone().report(Progress::new("x", 2)));
        // Full: dropped, the operation is not slowed down.
        assert!(!sink.report(Progress::new("x", 3)));
        assert_eq!(rx.try_recv().unwrap().done, 1);
        assert_eq!(rx.try_recv().unwrap().done, 2);
        assert!(rx.try_recv().is_err());

        drop(rx);
        assert!(sink.is_closed());
        assert!(!sink.report(Progress::new("x", 4)));
    }
}
