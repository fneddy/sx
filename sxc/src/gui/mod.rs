//! GUI frontend — gpui-kit / gpui-pre 0.3.9.
//!
//! - `search_view.rs` — root window component, search orchestration
//! - `result_list.rs` — scrollable, keyboard-navigable result list
//! - `row_builder.rs` — converts `ResultMsg` → `RowData` for the list

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::prelude::*;
use gpui::{px, size, App, Focusable, KeyBinding, WindowBounds, WindowOptions};
use gpui_kit::component::theme::{Theme, ThemeMode as KitThemeMode};

use crate::shared::config::ThemeMode;

mod activation;
mod result_list;
mod row_builder;
mod search_view;

use search_view::{AcceptHint, Cancel, NavDown, NavUp, SearchView, Submit, KEY_CONTEXT};

// ── Shared tokio runtime ──────────────────────────────────────────────────────

pub(super) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(plugin_dirs: Vec<PathBuf>, root: PathBuf, theme_mode: ThemeMode) {
    log::info!("sxp: starting, root={}", root.display());
    let _ = runtime();

    // Kick off plugin loading NOW — before gpui_kit::application().run() touches
    // the GPU.  The entire EGL init + Mesa shader compilation window (~80 % of
    // startup) becomes free overlap time for dlopen-ing plugins and connecting to
    // D-Bus services (Tracker et al.).
    let (registry_tx, registry_rx) =
        tokio::sync::oneshot::channel::<std::sync::Arc<sx_core::HandlerRegistry>>();
    runtime().spawn_blocking(move || {
        let reg = crate::shared::registry::build_registry(&plugin_dirs);
        let _ = registry_tx.send(reg);
    });

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
        gpui_kit::init(cx);
        gpui_component::init(cx);

        // For Light/Dark: override the default (init() always sets Light).
        // For System: do nothing here — we read window.appearance() after the
        // window opens (when the OS value is available) and call Theme::change
        // exactly once from there.
        log::debug!("sxp: theme_mode={:?}", theme_mode);
        match theme_mode {
            ThemeMode::Light => Theme::change(KitThemeMode::Light, None, cx),
            ThemeMode::Dark => Theme::change(KitThemeMode::Dark, None, cx),
        }

        cx.bind_keys([
            KeyBinding::new("enter", Submit, Some(KEY_CONTEXT)),
            KeyBinding::new("escape", Cancel, Some(KEY_CONTEXT)),
            KeyBinding::new("up", NavUp, Some(KEY_CONTEXT)),
            KeyBinding::new("down", NavDown, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-j", NavDown, Some(KEY_CONTEXT)),
            KeyBinding::new("ctrl-k", NavUp, Some(KEY_CONTEXT)),
            KeyBinding::new("tab", AcceptHint, Some(KEY_CONTEXT)),
        ]);

        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(660.), px(100.)), cx)),
            app_id: Some("sxp".to_string()),
            ..Default::default()
        };

        gpui_kit::open_window(opts, cx, move |window, cx| {
            cx.new(|cx| SearchView::new(registry_rx, root, window, cx))
        })
        .expect("sxp: failed to open window");

        cx.activate(true);
    });

    log::info!("sxp: GPUI main loop exited");
}
