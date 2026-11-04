//! [`SearchHandle`] — shared search-lifecycle type used by all frontends.

use tokio_util::sync::DropGuard;

use sx_core::CompiledQuery;

/// The lifecycle state of one search run.
///
/// Transitions: `Running → Done` or `Running → Stopped`. Never resets —
/// create a new [`SearchHandle`] for each search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// Workers are running and results are flowing.
    Running,
    /// All workers finished normally.
    Done,
    /// The search was stopped before completion.
    Stopped,
}

/// Owns a compiled query for the duration of one search run.
///
/// Dropping the handle (or calling [`stop`](SearchHandle::stop)) cancels all
/// workers via the RAII [`DropGuard`].
pub struct SearchHandle {
    cq: CompiledQuery,
    phase: SearchPhase,
    /// Cancels the query's `CancellationToken` when dropped.
    ///
    /// Held as `Option` so [`stop`](SearchHandle::stop) can drop it eagerly
    /// while the rest of `SearchHandle` stays alive.
    _guard: Option<DropGuard>,
}

impl SearchHandle {
    pub fn new(cq: CompiledQuery) -> Self {
        let guard = cq.token().drop_guard();
        Self {
            cq,
            phase: SearchPhase::Running,
            _guard: Some(guard),
        }
    }

    pub fn compiled_query(&self) -> &CompiledQuery {
        &self.cq
    }

    pub fn phase(&self) -> SearchPhase {
        self.phase
    }

    pub fn is_running(&self) -> bool {
        self.phase == SearchPhase::Running
    }

    /// Drop the guard immediately (fires the `CancellationToken`) and mark Stopped.
    /// Safe to call more than once.
    pub fn stop(&mut self) {
        self._guard = None;
        self.phase = SearchPhase::Stopped;
    }

    /// Mark as Done (workers finished naturally). Guard is left in place.
    pub fn finish(&mut self) {
        self.phase = SearchPhase::Done;
    }
}
