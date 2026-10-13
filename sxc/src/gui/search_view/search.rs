//! Search-execution logic for [`super::SearchView`].
//!
//! Owns the `StatusBar` model, `SearchState` enum, and all methods that start,
//! drain and stop both the live (global-namespace) and full (file-walk) searches.

use gpui::SharedString;
use std::cmp::Ordering;
use std::sync::Arc;

use tokio::sync::mpsc;

use gpui::{App, Context};
use sx_core::Query;
use sx_plugin_api::{SxCaseSensitivity, SxQueryContext, SxResult, SxResultSender};

use crate::shared::msg::ResultMsg;
use crate::shared::search::{SearchHandle, SearchPhase};
use crate::shared::wire::{n_workers, wire_search};

use super::super::row_builder::{make_row, SortKey};
use super::{SearchView, BATCH};
// ── Status bar model ─────────────────────────────────────────────────────────

/// Everything the status bar needs, computed once per render frame.
pub(super) enum StatusBar {
    /// No query entered, no search active — hide the bar entirely.
    Hidden,
    /// Plugins still loading with a search queued.
    LoadingPlugins,
    /// Query is malformed.
    Error(SharedString),
    /// Workers are running; `n` results so far.
    Searching(usize),
    /// Workers finished or were stopped; `n` results total.
    Done(usize),
}

impl StatusBar {
    /// `true` when the status bar should be visible at all.
    pub(super) fn is_visible(&self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// `true` while search workers are still running (drives the spinner).
    pub(super) fn is_running(&self) -> bool {
        matches!(self, Self::Searching(_))
    }

    /// The label shown next to the spinner (or alone when idle).
    pub(super) fn label(&self) -> Option<SharedString> {
        match self {
            Self::Hidden => None,
            Self::LoadingPlugins => Some("Loading plugins…".into()),
            Self::Error(e) => Some(format!("Invalid query: {e}").into()),
            Self::Searching(0) => Some("Searching…".into()),
            Self::Searching(n) => Some(format!("{n} results").into()),
            Self::Done(n) => Some(format!("{n} results").into()),
        }
    }
}

impl SearchView {
    pub(super) fn is_running(&self) -> bool {
        self.session.as_ref().map(|s| s.is_running()).unwrap_or(false)
            || self.live_search_task.is_some()
    }

    pub(super) fn status_bar(&self, cx: &App) -> StatusBar {
        if let Some(e) = &self.error {
            return StatusBar::Error(e.clone());
        }
        if self.plugins_loading && self.pending_search {
            return StatusBar::LoadingPlugins;
        }
        let n = self.list.read(cx).len();
        let query_empty = self.query_text(cx).is_empty();
        if self.is_running() {
            StatusBar::Searching(n)
        } else if n > 0 || !query_empty {
            StatusBar::Done(n)
        } else {
            StatusBar::Hidden
        }
    }
}

// ── Search control ────────────────────────────────────────────────────────────

impl SearchView {
    /// Cancel all in-flight searches and drop their tasks.
    pub(super) fn stop_search(&mut self) {
        if let Some(s) = &mut self.session {
            s.stop();
        }
        // Dropping the guard cancels the live-search CancellationToken, causing
        // plugin SxResultSender callbacks to return false immediately.
        self._live_search_guard = None;
        self.search_task = None;
        self.live_search_task = None;
    }

    /// Insert `batch` into the sorted result list and notify.
    fn insert_results(&mut self, batch: &mut Vec<ResultMsg>, cx: &mut Context<Self>) {
        let n = batch.len();
        for msg in batch.drain(..) {
            let key = SortKey::from(&msg);
            let at = self
                .sort_keys
                .partition_point(|k| k.cmp(&key) != Ordering::Greater);
            self.sort_keys.insert(at, key);
            let row = make_row(&msg);
            self.list.update(cx, |list, _| list.insert(at, row));
        }
        log::debug!(
            "sxp: inserted {} results, total now {}",
            n,
            self.list.read(cx).len()
        );
        cx.notify();
    }

    /// Recompute `self.hints` from the current input and registry.
    pub(super) fn update_hints(&mut self, cx: &mut Context<Self>) {
        let Some(registry) = &self.registry else {
            self.hints.clear();
            cx.notify();
            return;
        };
        let query = self.query_text(cx);
        self.hints.update(&query, registry);
        cx.notify();
    }

    // ── Live search ───────────────────────────────────────────────────────────

    /// Start a live (global-namespace) search for the current query text.
    ///
    /// Fires on every keystroke. Cancels any previously running live **or full**
    /// search before starting fresh, so stale results never bleed into the new
    /// result list.
    pub(super) fn start_live_search(&mut self, cx: &mut Context<Self>) {
        let query_str = self.query_text(cx);

        // stop_search() cancels both live_search_task and search_task (any
        // in-flight full search), preventing stale results from being inserted
        // into the list that is about to be cleared below.
        self.stop_search();

        if query_str.is_empty() {
            self.list.update(cx, |list, _| list.clear());
            self.sort_keys.clear();
            self.error = None;
            self.session = None;
            cx.notify();
            return;
        }

        if self.plugins_loading {
            return;
        }

        let Some(registry) = &self.registry else {
            return;
        };
        let registry = Arc::clone(registry);
        let global_ns = registry.global_namespaces();
        if global_ns.is_empty() {
            cx.notify();
            return;
        }

        self.list.update(cx, |list, _| list.clear());
        self.sort_keys.clear();
        self.error = None;
        cx.notify();

        let (tx, mut rx) = mpsc::unbounded_channel::<ResultMsg>();
        let ctx = SxQueryContext {
            roots: sx_plugin_api::stabby::vec::Vec::new(),
            max_results: 0,
            timeout_ms: 0,
            case_sensitivity: SxCaseSensitivity::Smart,
        };

        // One token per live search.  A DropGuard derived from it is stored
        // in `_live_search_guard`; dropping the guard (in stop_search) cancels
        // the token, causing plugin callbacks to return false immediately.
        let live_token = tokio_util::sync::CancellationToken::new();

        {
            let _enter = super::runtime().enter();
            for ns in global_ns {
                let cancel = live_token.clone();
                let registry = Arc::clone(&registry);
                let tx = tx.clone();
                let value = query_str.clone();
                let ctx = ctx.clone();
                tokio::task::spawn_blocking(move || {
                    let sender = SxResultSender::from_fn(move |res: SxResult| {
                        if cancel.is_cancelled() || tx.is_closed() {
                            return false;
                        }
                        let m: ResultMsg = res.into();
                        if !m.is_empty() {
                            tx.send(m).is_ok()
                        } else {
                            true
                        }
                    });
                    let ns_str = String::from_utf8_lossy(&ns).into_owned();
                    if let Err(e) = registry.dispatch(&ns, &value, &ctx, sender) {
                        log::warn!("sxp: live dispatch error for {}: {}", ns_str, e);
                    }
                });
            }
        }
        drop(tx);

        // Consumes live_token — all task clones already captured above.
        self._live_search_guard = Some(live_token.drop_guard());
        // Notify immediately so GPUI renders the spinner before the first
        // batch of results arrives (which may happen in the same frame otherwise).
        cx.notify();
        self.live_search_task = Some(cx.spawn(async move |this, cx| {
            let mut buf: Vec<ResultMsg> = Vec::with_capacity(BATCH);
            loop {
                buf.clear();
                if rx.recv_many(&mut buf, BATCH).await == 0 {
                    break;
                }
                if this
                    .update(cx, |view, cx| view.insert_results(&mut buf, cx))
                    .is_err()
                {
                    return;
                }
            }
            // Delay clearing the task handle by one frame so the spinner stays
            // visible for at least one rendered frame even when the search
            // completes near-instantly (e.g. pure handler queries like "e:pdf").
            let _ = this.update(cx, |view, cx| {
                view.live_search_task = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(std::time::Duration::from_millis(150)).await;
                    let _ = this.update(cx, |view, cx| {
                        view.live_search_task = None;
                        cx.notify();
                    });
                }));
                cx.notify();
            });
        }));
    }

    // ── Full search ───────────────────────────────────────────────────────────

    /// Start a full (rooted file-walk) query for the current query text.
    ///
    /// Called when the user presses Enter.  If plugins are still loading the
    /// search is queued and started automatically when they finish.
    pub(super) fn start_search(&mut self, cx: &mut Context<Self>) {
        let query_str = self.query_text(cx);
        if query_str.is_empty() {
            return;
        }

        if self.plugins_loading {
            log::info!("sxp: plugins still loading; search queued for {query_str:?}");
            self.pending_search = true;
            cx.notify();
            return;
        }

        let Some(registry) = &self.registry else {
            return;
        };
        let registry = Arc::clone(registry);

        log::info!("sxp: starting search for {query_str:?}");

        self.stop_search();
        self.list.update(cx, |list, _| list.clear());
        self.sort_keys.clear();
        self.error = None;
        cx.notify();

        let query = match Query::from_str_with_registry(
            &query_str,
            vec![self.root.clone()],
            Some(&registry),
        ) {
            Ok(q) => q,
            Err(e) => {
                log::warn!("sxp: invalid query {query_str:?}: {e}");
                self.error = Some(e.to_string().into());
                return;
            }
        };

        let cq = query.compile_with_registry(Some(Arc::clone(&registry)));
        let (tx, mut rx) = mpsc::unbounded_channel::<ResultMsg>();

        {
            let _enter = super::runtime().enter();
            wire_search(&cq, n_workers(), tx);
        }

        self.session = Some(SearchHandle::new(cq));
        // Notify immediately so the spinner renders before the first result batch.
        cx.notify();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let mut buf: Vec<ResultMsg> = Vec::with_capacity(BATCH);
            loop {
                buf.clear();
                if rx.recv_many(&mut buf, BATCH).await == 0 {
                    break;
                }
                if this
                    .update(cx, |view, cx| view.insert_results(&mut buf, cx))
                    .is_err()
                {
                    return;
                }
            }
            let _ = this.update(cx, |view, cx| {
                if let Some(s) = &mut view.session {
                    s.finish();
                }
                view.search_task = None;
                cx.notify();
            });
        }));
    }
}
