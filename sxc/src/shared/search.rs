//! [`SearchHandle`] — shared search-lifecycle type used by all frontends.
//!
//! Both the TUI and GUI keep one `SearchHandle` per active search.  It owns
//! the [`CompiledQuery`] and tracks the search phase.  Dropping the handle
//! (or calling [`SearchHandle::stop`]) cancels all workers via a RAII
//! [`DropGuard`]: when the guard is dropped it fires the query's
//! [`CancellationToken`], which:
//!
//! * wakes file-search workers blocked in the `Steal::Empty` `select!`, and
//! * causes handler (plugin) tasks' `SxResultSender` callbacks to return
//!   `false` on the next plugin call.
//!
//! No explicit `Drop` impl is needed — the field drop takes care of it.

use tokio_util::sync::DropGuard;

use sx_core::CompiledQuery;

// ── Phase ─────────────────────────────────────────────────────────────────────

/// The lifecycle state of one search run.
///
/// Transitions: `Running → Done` (channel closed naturally)
///              `Running → Stopped` (user cancelled)
/// The phase never goes backwards and is never reset — create a new
/// [`SearchHandle`] for each new search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// Workers are running and results are flowing.
    Running,
    /// All workers finished normally; the result channel closed on its own.
    Done,
    /// The user (or the frontend) stopped the search before it completed.
    Stopped,
}

// ── Handle ────────────────────────────────────────────────────────────────────

/// Owns a compiled query for the duration of one search run.
///
/// Frontends store `SearchHandle` (TUI, one-shot) or `Option<SearchHandle>`
/// (GUI, repeated searches).  Setting the `Option` to `None` cancels via the
/// `_guard` field's [`DropGuard`] — no explicit `cancel()` call needed.
pub struct SearchHandle {
    cq: CompiledQuery,
    phase: SearchPhase,
    /// Cancels the query's [`CancellationToken`] when dropped.
    ///
    /// Held as `Option` so that [`stop`] can drop it eagerly (immediate
    /// cancellation) while the rest of `SearchHandle` stays alive.  When
    /// `SearchHandle` itself drops, any remaining `Some` guard is dropped then,
    /// ensuring workers are always cancelled.
    _guard: Option<DropGuard>,
}

impl SearchHandle {
    /// Wrap a freshly compiled query; phase starts as [`SearchPhase::Running`].
    pub fn new(cq: CompiledQuery) -> Self {
        let guard = cq.token().drop_guard();
        Self {
            cq,
            phase: SearchPhase::Running,
            _guard: Some(guard),
        }
    }

    /// Borrow the compiled query (e.g. to pass to [`wire_search`]).
    pub fn compiled_query(&self) -> &CompiledQuery {
        &self.cq
    }

    /// Current phase of this search run.
    pub fn phase(&self) -> SearchPhase {
        self.phase
    }

    /// `true` while workers are still running.
    pub fn is_running(&self) -> bool {
        self.phase == SearchPhase::Running
    }

    /// Cancel workers and mark as [`SearchPhase::Stopped`].
    ///
    /// Drops the `DropGuard` immediately, which fires the `CancellationToken`.
    /// Safe to call more than once; subsequent calls are no-ops.
    pub fn stop(&mut self) {
        self._guard = None; // drop → cancel
        self.phase = SearchPhase::Stopped;
    }

    /// Mark as [`SearchPhase::Done`] (workers finished naturally).
    ///
    /// The guard is left in place and will cancel the (already-exhausted)
    /// token when `SearchHandle` eventually drops — a harmless no-op.
    pub fn finish(&mut self) {
        self.phase = SearchPhase::Done;
    }
}
