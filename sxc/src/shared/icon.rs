//! Symbol selection for search results.
//!
//! [`symbol_for_result`] is the single entry point used by both the TUI and GUI.
//! It returns a BMP Unicode character that can be rendered by any standard font.
//!
//! Priority order:
//! 1. `icon_symbol` metadata — set by the plugin (weather, apps fallback).
//! 2. `file_path` metadata   — extension-based lookup for file results.
//! 3. `namespace` fallback   — one symbol per known plugin namespace.
//! 4. `None`                 — unknown namespace with no path metadata.
//!
//! Note: `gio_id` is a desktop application ID (e.g. `"org.gnome.Nautilus.desktop"`),
//! not a file path.  It is ignored for symbol selection; the `icon_symbol` set by
//! the apps plugin always takes priority for those results anyway.

use std::path::Path;

use crate::shared::msg::ResultMsg;

/// Return a BMP Unicode symbol for a search result, or `None` for unknown namespaces.
///
/// Consulted in order:
/// 1. `icon_symbol` metadata key (set by plugins that have a meaningful symbol).
/// 2. `file_path` metadata key — extension lookup on the path value.
/// 3. Namespace-based fallback for known plugin namespaces.
pub fn symbol_for_result(r: &ResultMsg) -> Option<&str> {
    if let Some(sym) = r.meta("icon_symbol") {
        return Some(sym);
    }
    if let Some(path) = r.meta("file_path") {
        return Some(symbol_for_path(path));
    }
    match r.namespace.as_str() {
        "file"        => Some("□"),
        "calc"        => Some("∑"),
        "sys"         => Some("⚙"),
        "weather"     => Some("○"),
        "apps"        => Some("■"),
        "localsearch" => Some("○"),
        _             => None,
    }
}

/// Return a BMP Unicode symbol for the file at `path`.
///
/// Strips a leading `file://` prefix so callers can pass raw `file_path` values
/// directly without pre-processing.  Never fails — returns `"□"` when the
/// extension is unknown.
pub fn symbol_for_path(path: &str) -> &'static str {
    let path = path.strip_prefix("file://").unwrap_or(path);
    symbol_from_ext(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
    )
}

// ── extension table ───────────────────────────────────────────────────────────

fn symbol_from_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        // Documents
        "pdf" => "□",
        "doc" | "docx" | "odt" => "□",
        "xls" | "xlsx" | "ods" | "csv" => "●",
        "ppt" | "pptx" | "odp" => "▶",
        "epub" => "□",
        "txt" | "md" | "rst" | "adoc" => "□",
        // Code / markup
        "rs" | "go" | "py" | "js" | "ts" | "c" | "h" | "cpp" | "java" | "rb" | "sh" | "bash"
        | "zsh" | "fish" => "≡",
        "html" | "htm" | "css" | "xml" => "○",
        "json" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "env" => "⚙",
        // Images
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" => "◇",
        // Audio
        "mp3" | "ogg" | "flac" | "wav" | "aac" | "m4a" | "opus" => "♪",
        // Video
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" => "▶",
        // Archives
        "zip" | "gz" | "tar" | "bz2" | "xz" | "zst" | "7z" | "rar" | "deb" | "rpm" => "■",
        // Executables
        "so" | "dll" | "exe" | "bin" => "⚙",
        _ => "□",
    }
}
