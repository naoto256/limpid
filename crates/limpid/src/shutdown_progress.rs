//! Runtime-generation-local evidence of completed shutdown work.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

const IDLE: u8 = 0;
const ACTIVE: u8 = 1;
const DIRTY: u8 = 2;
const CLOSED: u8 = 3;

/// A coalescing latch, not a delivery counter or a durable-ACK guarantee.
/// Each Runtime owns a fresh instance; clones belong only to that generation.
#[derive(Clone, Default)]
pub(crate) struct ShutdownProgress(Arc<AtomicU8>);

impl ShutdownProgress {
    /// Record completed work only while shutdown is being observed. Steady-state
    /// calls perform one relaxed read; the latch carries no payload to publish.
    pub(crate) fn mark(&self) {
        if self.0.load(Ordering::Relaxed) == ACTIVE {
            let _ = self
                .0
                .compare_exchange(ACTIVE, DIRTY, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    pub(crate) fn begin(&self) -> Observation<'_> {
        assert!(
            self.0
                .compare_exchange(IDLE, ACTIVE, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok(),
            "a runtime generation may observe shutdown only once"
        );
        Observation(self)
    }
}

pub(crate) struct Observation<'a>(&'a ShutdownProgress);

impl Observation<'_> {
    pub(crate) fn take(&self) -> bool {
        self.0
            .0
            .compare_exchange(DIRTY, ACTIVE, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

impl Drop for Observation<'_> {
    fn drop(&mut self) {
        // A concurrent mark can only change ACTIVE to DIRTY, never reopen CLOSED.
        self.0.0.store(CLOSED, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_work_is_not_replayed_during_shutdown() {
        let progress = ShutdownProgress::default();
        progress.mark();
        let observing = progress.begin();
        assert!(!observing.take());
        progress.mark();
        assert!(observing.take());
        assert!(!observing.take());
    }

    #[test]
    fn completed_work_coalesces_but_waiting_is_not_progress() {
        let progress = ShutdownProgress::default();
        let observing = progress.begin();
        for _ in 0..100 {
            progress.mark();
        }
        assert!(observing.take());
        for _ in 0..100 {
            assert!(!observing.take());
        }
    }

    #[test]
    fn ended_generation_cannot_publish_into_the_next() {
        let old = ShutdownProgress::default();
        let observing = old.begin();
        old.mark();
        drop(observing);
        old.mark();
        let next = ShutdownProgress::default();
        let observing = next.begin();
        assert!(!observing.take());
        old.mark();
        assert!(!observing.take());
        next.mark();
        assert!(observing.take());
    }

    #[test]
    fn concurrent_marks_cannot_reopen_a_closed_generation() {
        let progress = ShutdownProgress::default();
        let observing = progress.begin();
        let worker = progress.clone();
        let thread = std::thread::spawn(move || {
            for _ in 0..10_000 {
                worker.mark();
            }
        });
        drop(observing);
        thread.join().unwrap();
        assert_eq!(progress.0.load(Ordering::Relaxed), CLOSED);
    }
}
