//! Application configuration — loaded from `~/.config/sx/config.toml`.
//!
//! The file lives next to the plugins directory (`~/.config/sx/plugins/`).
//! When no file is found the default config is written to disk so the user
//! has a ready-to-edit starting point.
//!
//! ```toml
//! [search]
//! directories = ["/home/alice/Documents", "/home/alice/Projects"]
//!
//! [tui]
//! fps_searching = 20.0   # redraw rate while results are flowing
//! fps_done      = 60.0   # redraw rate once the search is complete
//!
//! [theme]
//! mode = "dark"   # "light" or "dark"
//!
//! [gpu]
//! # Set to true on virgl/virtio-GPU hosts to skip the Vulkan ICD scan and
//! # cut ~34 % off GPU-initialisation time (sets VK_LOADER_DRIVERS_DISABLE).
//! force_gl = false
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Theme ─────────────────────────────────────────────────────────────────────

/// Which colour scheme to use.
///
/// - `Light` — always light, regardless of the OS setting.
/// - `Dark` — always dark, regardless of the OS setting (default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Theme {
    /// Colour mode for the GUI frontend. Has no effect on the TUI.
    pub mode: ThemeMode,
}

// ── Sub-sections ──────────────────────────────────────────────────────────────

/// Shared search settings — apply to every frontend.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Search {
    /// Directories searched when `--path` is not supplied on the CLI.
    /// Falls back to the current working directory when the list is empty.
    pub directories: Vec<PathBuf>,
}

/// TUI-specific settings.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct Tui {
    /// Frames per second while results are still arriving.
    pub fps_searching: f32,
    /// Frames per second once the search has finished or been stopped.
    pub fps_done: f32,
}

impl Default for Tui {
    fn default() -> Self {
        Self {
            fps_searching: 20.0,
            fps_done: 60.0,
        }
    }
}

// ── GPU ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Gpu {
    /// Disable all Vulkan ICDs before wgpu starts, forcing it to use the GL
    /// backend.  Cuts ~34 % off startup time on virgl/virtio-GPU hosts by
    /// setting `VK_LOADER_DRIVERS_DISABLE=~all~`.
    pub force_gl: bool,
}

// ── Root config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Config {
    pub search: Search,
    pub tui: Tui,
    pub theme: Theme,
    pub gpu: Gpu,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolve the list of search root directories.
///
/// Priority (highest first):
/// 1. `cli_path` — an explicit `--path` flag passed on the command line.
/// 2. `config.search.directories` — the user's configured default roots.
/// 3. The current working directory as a last resort.
pub fn resolve_roots(cli_path: Option<PathBuf>, cfg: &Config) -> Vec<PathBuf> {
    if let Some(p) = cli_path {
        return vec![p];
    }
    if !cfg.search.directories.is_empty() {
        return cfg.search.directories.clone();
    }
    vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
}

// ── Loader ────────────────────────────────────────────────────────────────────

/// Load the config from `~/.config/sx/config.toml`.
///
/// If the file does not exist the default config is written to disk and
/// returned.  Parse errors fall back to defaults without writing.
pub fn load_or_default() -> Config {
    let Some(path) = dirs_next::config_dir().map(|d| d.join("sx").join("config.toml")) else {
        return Config::default();
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let cfg: Config = match toml::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("config: parse error ({e}) — using defaults");
                    Config::default()
                }
            };
            log::debug!("config: loaded theme.mode={:?}", cfg.theme.mode);
            cfg
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(text) = toml::to_string_pretty(&cfg) {
                let _ = std::fs::write(&path, text);
            }
            cfg
        }
        Err(_) => Config::default(),
    }
}
