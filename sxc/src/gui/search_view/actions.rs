//! Keyboard-action handlers for [`super::SearchView`].
//!
//! Each handler corresponds to one GPUI action registered in `gui/mod.rs`.
//! Heavy logic lives in `search.rs`; these methods are thin dispatchers.

use gpui::prelude::*;
use gpui::{Context, Focusable, Window};

use super::hints::HintState;
use super::{AcceptHint, Cancel, NavDown, NavUp, SearchView, Submit};

impl SearchView {
    // ── Submit (Enter) ────────────────────────────────────────────────────────

    /// Enter with a selection → activate it and quit.
    /// Enter without a selection → kick off the full (rooted) search.
    pub(super) fn on_submit(
        &mut self,
        _: &Submit,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_selection = self.list.read(cx).selected_action().is_some();
        if has_selection {
            self.list.read(cx).activate();
            cx.quit();
            return;
        }
        self.start_search(cx);
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub(super) fn on_nav_up(
        &mut self,
        _: &NavUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // list.update notifies ResultList; SearchView itself needs no re-render.
        self.list.update(cx, |list, _| list.move_up());
    }

    pub(super) fn on_nav_down(
        &mut self,
        _: &NavDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.list.update(cx, |list, _| list.move_down());
    }

    // ── Cancel (Escape) ───────────────────────────────────────────────────────

    /// If a search is running: stop it and refocus the input.
    /// If the input is empty: quit.
    /// Otherwise: clear the input and reset results so the reference panel
    /// shows again.
    ///
    /// NOTE: `InputState::set_value` suppresses `InputEvent::Change`, so the
    /// normal `start_live_search` subscriber never fires after `clear_input`.
    /// We call `start_live_search` ourselves so the empty-query path runs and
    /// the list is cleared.
    pub(super) fn on_cancel(
        &mut self,
        _: &Cancel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_running() {
            self.stop_search();
            window.focus(&self.input.focus_handle(cx), cx);
            cx.notify();
            return;
        }

        if self.query_text(cx).is_empty() {
            cx.quit();
        } else {
            self.clear_input(window, cx);
            // set_value does not emit InputEvent::Change, so drive the
            // empty-query reset manually.
            self.start_live_search(cx);
        }
    }

    // ── Accept hint (Tab) ─────────────────────────────────────────────────────

    /// Apply the first value hint if any exist; otherwise complete the namespace
    /// prefix if it is unambiguous.
    pub(super) fn on_accept_hint(
        &mut self,
        _: &AcceptHint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.hints.is_empty() {
            self.accept_hint(0, window, cx);
            return;
        }
        let query = self.query_text(cx);
        if let Some(registry) = &self.registry {
            if let Some(suffix) = HintState::namespace_suffix(&query, registry) {
                let new_value = format!("{query}{suffix}");
                self.input
                    .update(cx, |state, cx| state.set_value(new_value, window, cx));
                cx.notify();
            }
        }
    }

    /// Write completion `idx` into the input, then clear the hint list.
    fn accept_hint(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query_text(cx);
        let Some(new_value) = self.hints.apply(idx, &query) else {
            return;
        };
        self.input
            .update(cx, |state, cx| state.set_value(new_value, window, cx));
        self.hints.clear();
        cx.notify();
    }
}
