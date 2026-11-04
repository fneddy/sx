//! `SearchView` — root GPUI component for the sxp launcher window.
//!
//! Sibling submodules:
//! * `hints.rs` — `HintState` (inline Tab-completion)
//! * `search.rs` — `StatusBar` + live/full-search execution
//! * `actions.rs` — keyboard-action handlers

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio_util::sync::DropGuard;

use gpui::prelude::*;
use gpui::{
    actions, div, App, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, Keystroke,
    Render, SharedString, Subscription, Task, Window,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::ActiveTheme;

use sx_core::HandlerRegistry;
use sx_plugin_api::SxHandlerInfo;

use crate::shared::search::SearchHandle;

use super::result_list::ResultList;
use super::row_builder::SortKey;

use hints::HintState;
use search::StatusBar;

mod actions;
mod hints;
mod search;

actions!(sxp, [Submit, Cancel, NavUp, NavDown, AcceptHint]);

pub const KEY_CONTEXT: &str = "SxpSearch";

/// Results drained from the channel per GPUI task iteration before yielding.
const BATCH: usize = 256;

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
    /// Dropping this fires the live search's `CancellationToken` via RAII.
    _live_search_guard: Option<DropGuard>,
    _loader_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    error: Option<SharedString>,

    hints: HintState,
}

impl SearchView {
    /// `registry_rx` receives the registry loaded in the background before the
    /// window opened, so plugin loading overlaps with GPU shader compilation.
    pub fn new(
        registry_rx: oneshot::Receiver<Arc<HandlerRegistry>>,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search… (↵ deep search)"));
        let list = cx.new(|cx| ResultList::new(cx));

        let input_sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.update_hints(cx);
                this.start_live_search(cx);
            }
        });

        input.update(cx, |state, cx| state.focus(window, cx));

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

impl Focusable for SearchView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl SearchView {
    /// Keep the input widget's loading indicator in sync with the search state.
    fn sync_loading(&mut self, status: &StatusBar, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |state, cx| state.set_loading(status.is_running(), window, cx));
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query_text(cx);
        let status = self.status_bar(cx);
        let muted = cx.theme().muted_foreground;

        self.sync_loading(&status, window, cx);

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
            .child({
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
            .child(Separator::horizontal())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .when(status.has_results(), |d| d.child(self.list.clone()))
                    .when(status.shows_quick_ref(), |d| {
                        let infos = self
                            .registry
                            .as_deref()
                            .map(|r| r.handlers_info())
                            .unwrap_or_default();
                        d.child(quick_ref(muted, infos))
                    }),
            )
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
                        .when(status.shows_deep_search_hint(query.is_empty()), |d| {
                            d.child(div().text_sm().text_color(muted).child("↵ deep search"))
                        }),
                )
            })
    }
}

/// Two-column row: muted label on the left, example on the right.
fn ref_row(label: impl Into<SharedString>, example: impl Into<SharedString>, muted: Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_baseline()
        .gap_3()
        .child(
            div()
                .text_sm()
                .text_color(muted)
                .flex_none()
                .w_40()
                .child(label.into()),
        )
        .child(div().text_sm().child(example.into()))
}

fn ref_sep() -> impl IntoElement {
    div().py_1().child(Separator::horizontal())
}

/// Build the label string for a handler row: `"ns: · alias:"`.
fn handler_label(info: &SxHandlerInfo) -> String {
    let mut label = info.namespace.as_str().to_owned();
    for alias in info.aliases.as_slice() {
        let a = alias.as_str();
        if !a.is_empty() {
            label.push_str(" · ");
            label.push_str(a);
            label.push(':');
        }
    }
    label.push(':');
    label
}

/// Prefers `namespace:value_hint`, falls back to `description`, then `namespace:`.
fn handler_example(info: &SxHandlerInfo) -> String {
    let ns = info.namespace.as_str();
    let hint = info.value_hint.as_str();
    let desc = info.description.as_str();
    if !hint.is_empty() {
        format!("{ns}:{hint}")
    } else if !desc.is_empty() {
        desc.to_owned()
    } else {
        format!("{ns}:")
    }
}

/// Idle-state reference card: file-filter cheat-sheet + loaded plugins + combinators.
fn quick_ref(muted: Hsla, handler_infos: Vec<SxHandlerInfo>) -> impl IntoElement {
    let mut el = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_4()
        .py_3()
        .child(ref_row("name: · n:", "n:main   n:!test", muted))
        .child(ref_row("path: · p:", "p:src   p:!vendor", muted))
        .child(ref_row("ext: · e:", "ext:rs,py   e:go", muted))
        .child(ref_row("type: · t:", "t:f   t:d   t:x   t:l", muted))
        .child(ref_row("hidden: · h:", "h:include   h:only", muted))
        .child(ref_row("size: · s:", "s:>10m   s:<1k", muted))
        .child(ref_row("modified: · m:", "m:7d   m:today   m:<30d", muted))
        .child(ref_row("content: · c:", "c:foo   c:!bar", muted))
        .child(ref_row("line: · l:", "l:foo   l:foo l:bar", muted));

    if !handler_infos.is_empty() {
        el = el.child(ref_sep());
        for info in &handler_infos {
            el = el.child(ref_row(handler_label(info), handler_example(info), muted));
        }
    }

    el.child(ref_sep())
        .child(ref_row("combine:", "or(a,b,c)   not(a)   a b c  (AND)", muted))
}
