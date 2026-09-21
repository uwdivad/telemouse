//! The process-wide "stop serving" signal.
//!
//! A console control event arrives on Windows' handler thread
//! ([`telemouse_core::shutdown`]), and a handler that reports the event as
//! *handled* — which this process must, so that teardown happens here rather
//! than wherever Windows' default handler would take it — suppresses the
//! default "terminate now" behaviour. Something in the process therefore has
//! to act on it, and in a server that something is the accept loop and every
//! task holding a socket.
//!
//! So the handler does one thing: [`Shutdown::fire`]. Everyone else waits on
//! [`Shutdown::wait`] — the HTTP server's graceful-shutdown future, the
//! deadline that bounds it, and each connected WebSocket, which answers with a
//! `Close` frame instead of leaving a page staring at a socket that will not
//! speak again.
//!
//! Cheap to clone (one `Arc`), safe to fire from any thread, and firing twice
//! is the same as firing once.

use std::sync::Arc;

use tokio::sync::watch;

/// A handle on the signal. Clone it into whatever needs to hear about a stop.
#[derive(Debug, Clone)]
pub struct Shutdown {
    tx: Arc<watch::Sender<bool>>,
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }

    /// Ask the process to stop serving. Idempotent, non-blocking, and callable
    /// from a console handler thread that is not the runtime's.
    pub fn fire(&self) {
        // `send_replace` rather than `send`: a signal must not be lost just
        // because nothing happens to be waiting at this instant.
        self.tx.send_replace(true);
    }

    /// Whether [`Self::fire`] has been called.
    pub fn fired(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolve once the signal has fired — at once if it already has, so a
    /// task that starts during the stop does not wait for a second one.
    ///
    /// Each call takes its own receiver (a version read and an `Arc` clone, no
    /// allocation), which is what makes this usable as a `select!` arm that is
    /// rebuilt on every iteration.
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow_and_update() {
            // The sender lives in this struct, so the only way `changed`
            // fails is a closed channel that cannot happen; treat it as fired.
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn waiting_ends_when_the_signal_fires() {
        let s = Shutdown::new();
        assert!(!s.fired());
        let waiter = tokio::spawn({
            let s = s.clone();
            async move { s.wait().await }
        });
        // Not before.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        s.fire();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("wait must return on the signal")
            .unwrap();
        assert!(s.fired());
    }

    /// A task that only starts waiting after the signal must not hang: on a
    /// stop, new WebSocket clients and new requests are still being accepted
    /// for as long as the listener lives.
    #[tokio::test]
    async fn waiting_after_the_fact_returns_at_once() {
        let s = Shutdown::new();
        s.fire();
        s.fire(); // idempotent
        tokio::time::timeout(Duration::from_secs(5), s.wait())
            .await
            .expect("an already-fired signal must not block");
    }

    #[tokio::test]
    async fn every_clone_hears_one_firing() {
        let s = Shutdown::new();
        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { s.wait().await })
            })
            .collect();
        s.fire();
        for w in waiters {
            tokio::time::timeout(Duration::from_secs(5), w)
                .await
                .expect("every clone must be released")
                .unwrap();
        }
    }
}
