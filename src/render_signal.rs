use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::layout::PaneId;

#[derive(Debug, Default)]
pub(crate) struct RenderRequest {
    pub(crate) generic: bool,
    pub(crate) pty_sources: HashSet<PaneId>,
    pub(crate) terminal_title_sources: HashSet<PaneId>,
}

/// Coalesces render requests while retaining enough origin information for the
/// headless server to discard PTY-only updates hidden from every client.
#[derive(Debug, Default)]
pub(crate) struct RenderSignal {
    pending: AtomicBool,
    request: Mutex<RenderRequest>,
}

impl RenderSignal {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn request_generic(&self) {
        let mut request = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        request.generic = true;
        self.pending.store(true, Ordering::Release);
    }

    /// Returns true when the signal becomes pending or a new PTY source joins it.
    ///
    /// A new source may be visible even when the existing pending sources are
    /// hidden, so the consumer must re-evaluate the coalesced request.
    pub(crate) fn request_pty(&self, pane_id: PaneId) -> bool {
        let mut request = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source_added = request.pty_sources.insert(pane_id);
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || source_added
    }

    pub(crate) fn has_generic_or_terminal_title(&self) -> bool {
        let request = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        request.generic || !request.terminal_title_sources.is_empty()
    }

    /// Checks pending PTY origins without allocating a source snapshot.
    /// Keep the predicate narrow because producers share this lock.
    pub(crate) fn has_pty_source_matching(
        &self,
        mut predicate: impl FnMut(PaneId) -> bool,
    ) -> bool {
        self.request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pty_sources
            .iter()
            .copied()
            .any(&mut predicate)
    }

    /// Coalesces terminal-title changes separately from ordinary PTY damage so
    /// consumers can update metadata without inspecting every pane.
    pub(crate) fn request_terminal_title(&self, pane_id: PaneId) -> bool {
        let mut request = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source_added = request.terminal_title_sources.insert(pane_id);
        let became_pending = !self.pending.swap(true, Ordering::AcqRel);
        became_pending || source_added
    }

    pub(crate) fn pending_terminal_title_sources(&self) -> HashSet<PaneId> {
        self.request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_title_sources
            .clone()
    }

    pub(crate) fn take(&self) -> RenderRequest {
        let mut request = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.pending.store(false, Ordering::Release);
        std::mem::take(&mut *request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_pty_sources_until_taken() {
        let signal = RenderSignal::new();
        let first = PaneId::from_raw(10);
        let second = PaneId::from_raw(20);

        assert!(signal.request_pty(first));
        assert!(!signal.request_pty(first));
        assert!(signal.request_pty(second));

        let request = signal.take();
        assert!(!request.generic);
        assert_eq!(request.pty_sources, HashSet::from([first, second]));
        assert!(request.terminal_title_sources.is_empty());
        assert!(!signal.is_pending());
    }

    #[test]
    fn terminal_title_source_wakes_pending_pty_work() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        assert!(signal.request_pty(pane_id));
        assert!(signal.request_terminal_title(pane_id));
        assert!(!signal.request_terminal_title(pane_id));
    }

    #[test]
    fn coalesces_terminal_title_sources_without_making_them_pty_damage() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        assert!(signal.request_terminal_title(pane_id));
        assert!(!signal.request_terminal_title(pane_id));
        assert_eq!(
            signal.pending_terminal_title_sources(),
            HashSet::from([pane_id])
        );

        let request = signal.take();
        assert!(request.pty_sources.is_empty());
        assert_eq!(request.terminal_title_sources, HashSet::from([pane_id]));
    }

    #[test]
    fn keeps_generic_and_pty_requests_distinct() {
        let signal = RenderSignal::new();
        let pane_id = PaneId::from_raw(10);

        signal.request_generic();
        assert!(signal.request_pty(pane_id));

        let request = signal.take();
        assert!(request.generic);
        assert_eq!(request.pty_sources, HashSet::from([pane_id]));
    }
}
