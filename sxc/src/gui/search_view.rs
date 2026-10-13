//! `SearchView` — the root GPUI component for the sxp launcher window.
//!
//! The file is intentionally kept to struct definition + wiring + rendering.
//! The three concerns that would otherwise bloat it live next to it as sibling
//! submodule files in the `search_view/` directory:
//!
//! | File          | Responsibility                                        |
//! |---------------|-------------------------------------------------------|
//! | `hints.rs`    | `HintState` — inline Tab-completion                   |
//! | `search.rs`   | `StatusBar` model + live-search and full-search execution |
//! | `actions.rs`  | Keyboard-action handlers (Enter, Esc, ↑/↓, Tab)      |

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio_util::sync::DropGuard;

use gpui::prelude::*;
use gpui::{
    actions, div, App, Context, Entity, FocusHandle, Focusable, IntoElement, Keystroke, Render,
    SharedString, Subscription, Task, Window,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, IconName, Sizable};

use sx_core::HandlerRegistry;

use crate::shared::msg::ResultMsg;
use crate::shared::search::SearchHandle;

use super::result_list::ResultList;
use super::row_builder::SortKey;

use hints::HintState;
use search::StatusBar;

mod actions;
mod hints;
mod search;

// ── Actions ───────────────────────────────────────────────────────────────────

actions!(sxp, [Submit, Cancel, NavUp, NavDown, AcceptHint]);

pub const KEY_CONTEXT: &str = "SxpSearch";

/// How many `ResultMsg`s are drained from the channel in a single GPUI task
/// iteration before yielding back to the event loop.
const BATCH: usize = 256;

// ── Struct ────────────────────────────────────────────────────────────────────

pub struct SearchView {
    registry: Option<Arc<HandlerRegistry>>,
    plugins_loading: bool,
    pending_search: bool,
    root: PathBuf,

    input: Entity<InputState>,
    list: Entity<ResultList>,
    sort_keys: Vec<SortKey>,

    session: Option<SearchHandle>,
    search_task: Option<Task<()>>,
    live_search_task: Option<Task<()>>,
    /// RAII guard that cancels live-search `spawn_blocking` tasks.
    ///
    /// Dropping this (setting it to `None`) fires the live search's
    /// `CancellationToken`, which causes the plugin `SxResultSender` callbacks
    /// to return `false` immediately — no explicit `.cancel()` call needed.
    _live_search_guard: Option<DropGuard>,
    _loader_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    error: Option<SharedString>,

    /// Viewport size captured at window-open; used as the collapsed (input-only) size.
    collapsed_size: gpui::Size<gpui::Pixels>,
    /// Tracks the last applied expanded/collapsed state to skip redundant resizes.
    expanded: Option<bool>,

    hints: HintState,
}

// ── Constructor ───────────────────────────────────────────────────────────────

impl SearchView {
    /// `registry_rx` is a one-shot receiver for the `HandlerRegistry` that was
    /// started loading in the background before GPUI opened the window.
    /// Receiving it here instead of the raw `plugin_dirs` means plugin loading
    /// overlaps with GPU shader compilation (≈80 % of cold-start time).
    pub fn new(
        registry_rx: oneshot::Receiver<Arc<HandlerRegistry>>,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search… (↵ deep search)"));

        let list = cx.new(|cx| ResultList::new(cx));

        // Keyboard-driven live search: fire on every input change.
        let input_sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.update_hints(cx);
                this.start_live_search(cx);
            }
        });

        // Capture the initial viewport as the collapsed height before any resize.
        let collapsed_size = window.viewport_size();

        // Focus the input immediately so the user can type without clicking.
        input.update(cx, |state, cx| state.focus(window, cx));

        // Await the registry that was loading in the background.
        let loader_task = cx.spawn(async move |this, cx| {
            if let Ok(reg) = registry_rx.await {
                let _ = this.update(cx, |view, cx| {
                    log::info!("sxp: background plugin loading complete");
                    view.registry = Some(reg);
                    view.plugins_loading = false;
                    view.update_hints(cx);
                    if view.pending_search {
                        // Enter was pressed before plugins finished loading.
                        view.pending_search = false;
                        view.start_search(cx);
                    } else if !view.query_text(cx).is_empty() {
                        // User typed while plugins were loading; kick off live search now.
                        view.start_live_search(cx);
                    } else {
                        cx.notify();
                    }
                });
            }
        });

        Self {
            registry: None,
            plugins_loading: true,
            pending_search: false,
            root,
            input,
            list,
            sort_keys: Vec::new(),
            session: None,
            search_task: None,
            live_search_task: None,
            _live_search_guard: None,
            _loader_task: Some(loader_task),
            _subscriptions: vec![input_sub],
            error: None,
            collapsed_size,
            expanded: None,
            hints: HintState::new(),
        }
    }

    fn query_text(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_owned()
    }

    fn clear_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
    }
}

// ── Focusable ─────────────────────────────────────────────────────────────────

impl Focusable for SearchView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

// ── Geometry ──────────────────────────────────────────────────────────────────

impl SearchView {
    /// Resize the window between collapsed (input-bar only) and expanded
    /// (input + list + status bar) heights whenever the desired state changes.
    ///
    /// Called at the top of `render` before the element tree is built.
    /// Mutates only `self.expanded` (a render-tracking field) and calls
    /// `window.resize`.  Does **not** call `cx.notify()` to avoid a re-render loop.
    fn sync_geometry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list_empty = self.list.read(cx).is_empty();
        let status = self.status_bar(cx);
        let wants_expanded = !list_empty || status.is_visible() || !self.hints.is_empty();

        if self.expanded == Some(wants_expanded) {
            return;
        }
        self.expanded = Some(wants_expanded);

        if wants_expanded {
            let h = self.collapsed_size.height * 6.0;
            log::debug!("sxp: resize → expand {}×{}", self.collapsed_size.width, h);
            window.resize(gpui::size(self.collapsed_size.width, h));
        } else {
            log::debug!(
                "sxp: resize → collapse {}×{}",
                self.collapsed_size.width,
                self.collapsed_size.height,
            );
            window.resize(self.collapsed_size);
        }
    }
}

impl SearchView {
    fn sync_loading(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let loading = self.status_bar(cx).is_running();
        self.input
            .update(cx, |state, cx| state.set_loading(loading, window, cx));
    }
}

// ── Render ────────────────────────────────────────────────────────────────────

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_geometry(window, cx);
        self.sync_loading(window, cx);

        let list_empty = self.list.read(cx).is_empty();
        let status = self.status_bar(cx);
        // "↵ deep search" only when idle with results and no error.
        let show_hint = matches!(status, StatusBar::Done(_)) && self.error.is_none();
        let muted = cx.theme().muted_foreground;

        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.input.focus_handle(cx))
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_nav_up))
            .on_action(cx.listener(Self::on_nav_down))
            .on_action(cx.listener(Self::on_accept_hint))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .p_4()
            .gap_2()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // ── Search input ──────────────────────────────────────────────────
            .child({
                let query = self.query_text(cx);
                let suffix_text: Option<SharedString> = self.hints.suffix_text().or_else(|| {
                    self.registry
                        .as_deref()
                        .and_then(|r| HintState::namespace_suffix(&query, r))
                });
                Input::new(&self.input)
                    .cleanable(true)
                    .when_some(suffix_text, |inp, text| {
                        inp.suffix(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(div().text_sm().text_color(muted).child(text))
                                .child(Kbd::new(Keystroke::parse("tab").unwrap())),
                        )
                    })
            })
            // ── Result list ───────────────────────────────────────────────────
            .when(!list_empty, |d| {
                d.child(Separator::horizontal()).child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.list.clone()),
                )
            })
            // ── Status bar ────────────────────────────────────────────────────
            .when(status.is_visible(), |d| {
                d.child(Separator::horizontal()).child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .w_full()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .when(status.is_running(), |d| d.child(Spinner::new()))
                                .when_some(status.label(), |d, label| {
                                    d.child(div().text_sm().child(label))
                                }),
                        )
                        .when(show_hint, |d| {
                            d.child(div().text_sm().text_color(muted).child("↵ deep search"))
                        }),
                )
            })
    }
}

// ── Runtime helper ────────────────────────────────────────────────────────────

/// Thin wrapper so submodules write `super::runtime()` instead of reaching
/// three levels up to the `gui` module.
fn runtime() -> &'static tokio::runtime::Runtime {
    super::runtime()
}
