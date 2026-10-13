//! Library surface of `sxc`.
//!
//! Module layout:
//!
//! ```text
//! shared/        ← used by BOTH frontends, no UI imports
//!   app.rs       — pure App state
//!   config.rs    — load ~/.config/sx/config.toml (theme + search dirs)
//!   msg.rs       — ResultMsg, From<SxResult>
//!   config.rs    — Config, Tui, Search, Theme, resolve_roots (shared config + helpers)
//!   wire.rs      — n_workers, wire_search (tokio tasks → ResultMsg channel)
//!   search.rs    — SearchHandle + SearchPhase (shared search lifecycle)
//!   app.rs       — App result buffer (TUI only; GUI uses Entity<ResultList>)
//!   registry.rs  — build_registry (plugin discovery)
//!
//! gui/           ← gpui frontend only  [feature = "gpui"]
//!   mod.rs       — run
//! ```
//!
//! The lib target exists solely as a test surface; the actual binary users are
//! `src/tui/sxc.rs` (TUI) and `src/gui/sxp.rs` (GUI GPUI).

// Dead-code warnings from the lib target are false positives — the items are
// all used by the bin targets.  Suppress outside test builds only so that real
// dead code introduced in test-only paths is still caught.
#![cfg_attr(not(test), allow(dead_code, unused_imports))]

// ── Shared ────────────────────────────────────────────────────────────────────
pub mod shared {
    pub mod app;
    pub mod config;
    pub mod icon;
    pub mod msg;
    pub mod registry;
    pub mod search;
    pub mod wire;
}

// ── GUI frontend (gpui) ───────────────────────────────────────────────────────
#[cfg(feature = "gpui")]
pub mod gui;
