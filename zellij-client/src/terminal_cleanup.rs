//! Per-client cleanup arbitration adapted from KudoLayton/zellij 5f0d771b3
//! and e09487a06. A panic during cleanup must not recursively run it again.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Clone, Default)]
pub(crate) struct TerminalCleanupGuard(Arc<AtomicBool>);

impl TerminalCleanupGuard {
    pub(crate) fn run_once(&self, cleanup: impl FnOnce()) {
        if !self.0.swap(true, Ordering::SeqCst) {
            cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_cleanup() {
        let guard = TerminalCleanupGuard::default();
        let mut calls = 0;
        guard.run_once(|| calls += 1);
        guard.clone().run_once(|| calls += 1);
        assert_eq!(calls, 1);
    }

    #[test]
    fn simultaneous_exit_paths_only_clean_up_once() {
        let guard = TerminalCleanupGuard::default();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let guard = guard.clone();
                let calls = &calls;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    guard.run_once(|| {
                        calls.fetch_add(1, Ordering::SeqCst);
                    });
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn reentrant_cleanup_is_ignored() {
        let guard = TerminalCleanupGuard::default();
        guard.run_once(|| guard.run_once(|| panic!("recursive terminal teardown")));
    }

    #[test]
    fn new_client_has_independent_cleanup() {
        let first = TerminalCleanupGuard::default();
        first.run_once(|| {});
        let mut ran = false;
        TerminalCleanupGuard::default().run_once(|| ran = true);
        assert!(ran);
    }
}
