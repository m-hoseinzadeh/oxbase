//! The cancel deadline of imports and exports.
//!
//! A cancel must end the job with its terminal event within 2 s (contract §5.2 and §5.4).
//! Everything that still runs after the cancel (letting the current call stop, rolling back,
//! ending the bulk load, dropping a table the import created, closing the work session,
//! deleting a partial file) shares one budget, [`CANCEL_BUDGET`], measured from the moment the
//! token fired. Each wait asks [`CancelClock::limit`] for its share; work that must not be cut
//! off (dropping a created table) goes on in the background once the budget is spent.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::CancellationToken;

/// Time from a cancel to the terminal event: under the promised 2 s, with a margin for sending
/// the events.
pub(crate) const CANCEL_BUDGET: Duration = Duration::from_millis(1_700);

/// When the job's token fired, and what is left of [`CANCEL_BUDGET`] since.
pub(crate) struct CancelClock {
    token: CancellationToken,
    at: Arc<OnceLock<Instant>>,
    watcher: JoinHandle<()>,
}

impl CancelClock {
    /// Starts watching `token` (a small task that notes the moment it fires).
    pub(crate) fn start(token: &CancellationToken) -> Self {
        let at = Arc::new(OnceLock::new());
        let watcher = tokio::spawn({
            let token = token.clone();
            let at = Arc::clone(&at);
            async move {
                token.cancelled().await;
                let _ = at.set(Instant::now());
            }
        });
        Self {
            token: token.clone(),
            at,
            watcher,
        }
    }

    /// The moment of the cancel, if the token fired.
    fn fired_at(&self) -> Option<Instant> {
        if self.token.is_cancelled() {
            // The watcher may not have run yet: the first caller to see the cancel notes it.
            Some(*self.at.get_or_init(Instant::now))
        } else {
            None
        }
    }

    /// `want`, cut to what is left of the budget once the token fired (`want` as is before).
    pub(crate) fn limit(&self, want: Duration) -> Duration {
        match self.fired_at() {
            None => want,
            Some(at) => want.min((at + CANCEL_BUDGET).saturating_duration_since(Instant::now())),
        }
    }
}

impl Drop for CancelClock {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_budget_runs_from_the_cancel() {
        let token = CancellationToken::new();
        let clock = CancelClock::start(&token);
        let long = Duration::from_secs(10);
        assert_eq!(clock.limit(long), long);
        token.cancel();
        let left = clock.limit(long);
        assert!(left <= CANCEL_BUDGET, "{left:?}");
        assert!(left > CANCEL_BUDGET / 2, "{left:?}");
        assert_eq!(
            clock.limit(Duration::from_millis(5)),
            Duration::from_millis(5)
        );
        tokio::time::sleep(CANCEL_BUDGET).await;
        assert_eq!(clock.limit(long), Duration::ZERO);
    }
}
