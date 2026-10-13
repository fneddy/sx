//! Scrollable, keyboard-navigable result list widget.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    div, img, rems, uniform_list, App, Context, FocusHandle, Focusable, IntoElement,
    Render, RenderImage, ScrollStrategy, SharedString, UniformListScrollHandle, Window,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::h_flex;

// ── Row data ──────────────────────────────────────────────────────────────────

/// What happens when a row is activated.
///
/// Marked `#[non_exhaustive]` so new activation kinds can be added without
/// breaking exhaustive `match` arms in downstream code.
#[non_exhaustive]
#[derive(Clone)]
pub enum ActionKind {
    GioId(SharedString),
    FilePath(SharedString),
}

impl ActionKind {
    pub fn label(&self) -> &str {
        match self {
            Self::GioId(s) | Self::FilePath(s) => s.as_ref(),
        }
    }
}

/// Display data for one result row.
///
/// Marked `#[non_exhaustive]` so new display fields can be added without
/// forcing all construction sites to be updated simultaneously.
#[non_exhaustive]
#[derive(Clone)]
pub struct RowData {
    pub icon: Option<Arc<RenderImage>>,
    pub symbol: Option<SharedString>,
    pub label: SharedString,
    pub sub_label: Option<SharedString>,
    pub action: Option<ActionKind>,
}

const ICON_SIZE: f32 = 1.75; // rems

// ── ResultList entity ─────────────────────────────────────────────────────────

pub struct ResultList {
    rows: Vec<RowData>,
    /// Stable per-row identity, parallel to `rows`.
    /// Each call to `insert` consumes one id from `next_id`; ids are never
    /// reused across a `clear()` so reinserted items always get fresh identity.
    ids: Vec<u64>,
    next_id: u64,
    selected: Option<usize>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
}

impl ResultList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            rows: Vec::new(),
            ids: Vec::new(),
            next_id: 0,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Insert a row at `index`, assigning it a stable unique id.
    /// Adjusts the selection index so the same row stays selected after insertion.
    pub fn insert(&mut self, index: usize, row: RowData) {
        if let Some(sel) = self.selected {
            if index <= sel {
                self.selected = Some(sel + 1);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.rows.insert(index, row);
        self.ids.insert(index, id);
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.ids.clear();
        // next_id is intentionally NOT reset: ids must remain unique across
        // repeated clear/insert cycles within the same ResultList lifetime.
        self.selected = None;
    }

    pub fn selected_action(&self) -> Option<&ActionKind> {
        self.selected
            .and_then(|i| self.rows.get(i))
            .and_then(|r| r.action.as_ref())
    }

    pub fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None | Some(0) => 0,
            Some(i) => i - 1,
        });
        self.scroll_to_selected();
    }

    pub fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1).min(self.rows.len() - 1),
        });
        self.scroll_to_selected();
    }

    pub fn activate(&self) {
        if let Some(action) = self.selected_action() {
            super::activation::activate(action);
        }
    }

    fn scroll_to_selected(&self) {
        if let Some(i) = self.selected {
            self.scroll.scroll_to_item(i, ScrollStrategy::Center);
        }
    }
}

impl Focusable for ResultList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResultList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected;

        // Derive row height from the current rem size so the list scales
        // correctly when the user (or accessibility settings) changes the base
        // font.  3 rem gives a comfortable two-line row at the default 16 px
        // rem and remains proportional at any other size.
        let row_h = window.rem_size() * 3.0;

        let list = uniform_list(
            "results",
            self.rows.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, _cx| {
                range
                    .map(|ix| {
                        let row = this.rows[ix].clone();
                        // Use the stable domain id, not the mutable position index,
                        // so GPUI preserves per-row state (hover, focus, animation)
                        // when items are reordered by incoming higher-scored results.
                        let id = this.ids[ix];
                        let is_selected = selected == Some(ix);

                        // ── Icon / symbol ─────────────────────────────────────
                        let icon_el: Option<gpui::AnyElement> = if let Some(image) = row.icon {
                            Some(
                                div()
                                    .w(rems(ICON_SIZE))
                                    .h(rems(ICON_SIZE))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(img(image).w(rems(ICON_SIZE)).h(rems(ICON_SIZE)))
                                    .into_any_element(),
                            )
                        } else {
                            row.symbol.map(|sym| {
                                div()
                                    .w(rems(ICON_SIZE))
                                    .h(rems(ICON_SIZE))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(sym)
                                    .into_any_element()
                            })
                        };

                        // ── Label + sub-label ─────────────────────────────────
                        let text = div()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .child(div().truncate().child(row.label))
                            .when_some(row.sub_label, |d, sub| {
                                d.child(div().truncate().text_sm().child(sub))
                            });

                        // ── Single horizontal flex row as the ListItem child ──
                        // ListItem wraps all children in `div().w_full().children(…)`
                        // which is a block (non-flex) div.  Passing icon + text as
                        // separate children would stack them vertically.  Instead we
                        // compose them into one `h_flex` row so they sit side-by-side.
                        let row_content = h_flex()
                            .w_full()
                            .h(row_h)
                            .items_center()
                            .gap_2()
                            .when_some(icon_el, |d, el| d.child(el))
                            .child(text);

                        let mut item = ListItem::new(("row", id))
                            .h(row_h)
                            .selected(is_selected)
                            .child(row_content);

                        if let Some(kind) = row.action {
                            item = item.on_click(move |_, _, _| {
                                super::activation::activate(&kind);
                            });
                        }

                        item
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
        .size_full();

        div().size_full().child(list)
    }
}
