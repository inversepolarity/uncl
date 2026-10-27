//! Frame scheduling primitive.
//!
//! The renderer used to redraw on a fixed 10 ms tick regardless of whether
//! anything had changed, which meant rebuilding (and diffing) every cell of the
//! screen ~100 times a second even when both shells were idle.
//!
//! Instead, PTY reader threads *raise* this signal when they have processed
//! bytes, and the event loop waits on `changed()` rather than sleeping. A raised
//! flag is sticky, so several chunks arriving between frames coalesce into one
//! repaint.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

#[derive(Debug, Default)]
pub struct Signal {
    raised: AtomicBool,
    notify: Notify,
}

impl Signal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark the screen as stale and wake the event loop.
    ///
    /// Safe to call from blocking threads; never blocks.
    pub fn raise(&self) {
        self.raised.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Consume the "stale" flag, returning whether a repaint is needed.
    pub fn take(&self) -> bool {
        self.raised.swap(false, Ordering::AcqRel)
    }

    /// Resolves as soon as the signal is raised.
    ///
    /// `Notify` stores one permit when there is no waiter, so a raise that
    /// happens between `take()` and this call is not lost.
    pub async fn changed(&self) {
        if self.raised.load(Ordering::Acquire) {
            return;
        }
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_is_destructive() {
        let signal = Signal::new();
        assert!(!signal.take());
        signal.raise();
        assert!(signal.take());
        assert!(!signal.take());
    }

    #[tokio::test]
    async fn changed_sees_a_raise_that_happened_first() {
        let signal = Signal::new();
        signal.raise();
        // must not hang: the permit was stored before we started waiting
        tokio::time::timeout(std::time::Duration::from_millis(50), signal.changed())
            .await
            .expect("raise before wait must not be lost");
    }

    #[tokio::test]
    async fn changed_wakes_on_later_raise() {
        let signal = Signal::new();
        let waiter = signal.clone();
        let handle = tokio::spawn(async move { waiter.changed().await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        signal.raise();
        tokio::time::timeout(std::time::Duration::from_millis(500), handle)
            .await
            .expect("waiter should have been woken")
            .expect("waiter task should not panic");
    }
}
