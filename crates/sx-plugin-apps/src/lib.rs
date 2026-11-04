//! # `sx-plugin-apps`
//!
//! Built-in installed-application search handler for `sx`.
//!
//! Enumerates every installed application via [`gio::AppInfo::all()`] —
//! the same source desktop environments use — then scores and filters
//! against the query string.  Each result carries:
//!
//! - `icon_png`    — PNG thumbnail rendered from the app's icon theme entry
//! - `icon_symbol` — Unicode emoji fallback for text renderers
//! - `title`       — application display name
//! - `action`      — absolute path to the `.desktop` file (for `xdg-open`)

use gio::glib::object::Cast;
use gio::prelude::AppInfoExt;
use std::time::Instant;
use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxMetaEntry, SxNamespacePolicy, SxQueryContext,
    SxResult, SxResultSender,
};

/// Icon size in pixels used for lookups and SVG/PNG renders.
const ICON_SIZE: u32 = 128;

// ── Icon resolution ───────────────────────────────────────────────────────────

/// Resolve a `gio::Icon` to raw PNG bytes at [`ICON_SIZE`] × [`ICON_SIZE`].
///
/// Handles `ThemedIcon` (freedesktop icon lookup) and `FileIcon` (direct path).
/// SVGs are rasterised via `resvg`; all rasters are normalised with Lanczos3.
fn icon_png(icon: &gio::Icon) -> Option<Vec<u8>> {
    let path = if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        themed.names().into_iter().find_map(|name| {
            freedesktop_icons::lookup(name.as_str())
                .with_size(ICON_SIZE as u16)
                .with_cache()
                .find()
        })?
    } else {
        let file_icon = icon.downcast_ref::<gio::FileIcon>()?;
        use gio::prelude::FileExt;
        file_icon.file().path()?
    };

    let bytes = std::fs::read(&path).ok()?;

    let png_bytes = if path.extension().and_then(|e| e.to_str()) == Some("svg") {
        svg_to_png(&bytes)?
    } else {
        bytes
    };

    normalise_to_icon_size(&png_bytes)
}

fn normalise_to_icon_size(png: &[u8]) -> Option<Vec<u8>> {
    use image::{imageops::FilterType, ImageFormat};
    use std::io::Cursor;

    let img = image::load_from_memory(png).ok()?;
    let out = if img.width() == ICON_SIZE && img.height() == ICON_SIZE {
        img
    } else {
        img.resize_exact(ICON_SIZE, ICON_SIZE, FilterType::Lanczos3)
    };

    let mut buf = Vec::new();
    out.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .ok()?;
    Some(buf)
}

fn svg_to_png(svg: &[u8]) -> Option<Vec<u8>> {
    use resvg::usvg;
    use tiny_skia::Pixmap;

    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = Pixmap::new(ICON_SIZE, ICON_SIZE)?;
    let svg_size = tree.size();
    let transform = tiny_skia::Transform::from_scale(
        ICON_SIZE as f32 / svg_size.width(),
        ICON_SIZE as f32 / svg_size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

// ── Symbol fallback ───────────────────────────────────────────────────────────

fn app_symbol(id: &str) -> &'static str {
    let id = id.to_ascii_lowercase();
    let id = id.as_str();
    if id.contains("browser")
        || id.contains("firefox")
        || id.contains("chrome")
        || id.contains("chromium")
        || id.contains("epiphany")
    {
        "○"  // web / globe
    } else if id.contains("terminal")
        || id.contains("console")
        || id.contains("shell")
        || id.contains("alacritty")
        || id.contains("kitty")
        || id.contains("wezterm")
    {
        "≡"  // terminal / prompt
    } else if id.contains("code")
        || id.contains("editor")
        || id.contains("vim")
        || id.contains("emacs")
        || id.contains("gedit")
        || id.contains("kate")
    {
        "≡"  // code editor
    } else if id.contains("music")
        || id.contains("rhythmbox")
        || id.contains("spotify")
        || id.contains("clementine")
        || id.contains("amarok")
    {
        "♪"  // music
    } else if id.contains("video")
        || id.contains("vlc")
        || id.contains("mpv")
        || id.contains("totem")
        || id.contains("player")
    {
        "▶"  // video / play
    } else if id.contains("image")
        || id.contains("photo")
        || id.contains("gimp")
        || id.contains("inkscape")
        || id.contains("eog")
        || id.contains("shotwell")
    {
        "◇"  // image
    } else if id.contains("mail")
        || id.contains("thunder")
        || id.contains("evolution")
        || id.contains("geary")
    {
        "□"  // mail / document
    } else if id.contains("file")
        || id.contains("manager")
        || id.contains("nautilus")
        || id.contains("dolphin")
        || id.contains("thunar")
    {
        "■"  // file manager
    } else if id.contains("calc")
        || id.contains("libreoffice")
        || id.contains("writer")
        || id.contains("impress")
        || id.contains("office")
    {
        "□"  // office / document
    } else if id.contains("setting") || id.contains("config") || id.contains("control") {
        "⚙"  // settings / gear
    } else {
        "■"  // generic app
    }
}

// ── App filtering ─────────────────────────────────────────────────────────────

/// A single application that matches the current query, with its display
/// fields and relevance score pre-computed.
struct AppMatch {
    app: gio::AppInfo,
    name: String,
    id: String,
    score: f64,
}

/// Return all installed apps that match `query`, ordered by insertion order.
///
/// Filters out apps that have opted out of launcher display (`should_show`).
/// When `query` is empty every visible app gets a default score of `0.9`.
fn filter_apps(query: &str) -> Vec<AppMatch> {
    let query_lc = query.trim().to_ascii_lowercase();
    gio::AppInfo::all()
        .into_iter()
        .filter(|app| app.should_show())
        .filter_map(|app| {
            let name = app.name().to_string();
            let id = app.id().map(|s| s.to_string()).unwrap_or_default();
            let desc = app.description().map(|s| s.to_string()).unwrap_or_default();
            let name_lc = name.to_ascii_lowercase();
            let id_lc = id.to_ascii_lowercase();
            let desc_lc = desc.to_ascii_lowercase();
            let score = if query_lc.is_empty() {
                0.9
            } else if name_lc.starts_with(&query_lc) {
                1.0
            } else if name_lc.contains(&query_lc) {
                0.9
            } else if id_lc.contains(&query_lc) {
                0.8
            } else if desc_lc.contains(&query_lc) {
                0.7
            } else {
                return None;
            };
            Some(AppMatch { app, name, id, score })
        })
        .collect()
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Installed-application search handler.
pub struct AppsHandler;

impl AppsHandler {
    pub fn new() -> Self {
        let _ = env_logger::try_init();
        Self
    }
}

impl Default for AppsHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl sx_plugin_api::SxHandler for AppsHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("apps"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Search installed desktop applications (apps:<name>)",
            ),
            value_hint: SxBytes::from_str_bytes(""),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: true,
            is_file_index: false,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        let t = Instant::now();
        let query = value.as_ref().trim().to_lowercase();
        log::debug!("apps: query({:?})", query);

        let matches = filter_apps(&query);
        log::debug!("apps: {} matching applications", matches.len());

        let mut sent = 0usize;
        let mut skipped_icon = 0usize;

        for m in matches {
            // Icon — skip apps with no resolvable icon.
            let png = match m.app.icon().as_ref().and_then(icon_png) {
                Some(p) => p,
                None => {
                    log::trace!("apps: no icon for {:?} (id={}), skipping", m.name, m.id);
                    skipped_icon += 1;
                    continue;
                }
            };

            log::debug!("apps: sending {:?} id={} score={}", m.name, m.id, m.score);

            let mut result = SxResult {
                title: stabby::option::Option::Some(SxBytes::from_str_bytes(&m.name)),
                ..SxResult::new("apps", m.score)
            };
            result.metadata.push(SxMetaEntry::bytes("icon_png", png));
            result.metadata.push(SxMetaEntry::text("icon_symbol", app_symbol(&m.id)));
            result.metadata.push(SxMetaEntry::text("gio_id", &m.id));

            if !sender.send(result) {
                log::debug!(
                    "apps: sender closed after {sent} results ({:.1}ms)",
                    t.elapsed().as_secs_f64() * 1000.0
                );
                return;
            }
            sent += 1;
        }

        log::debug!(
            "apps: done — sent={sent} skipped_icon={skipped_icon} ({:.1}ms)",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(
        &self,
        partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        let query = partial_value.as_ref().trim().to_lowercase();
        if query.is_empty() {
            // Too many apps to list all — wait until the user starts typing.
            return sx_plugin_api::SxHints::empty();
        }
        let mut items = stabby::vec::Vec::new();
        for m in filter_apps(&query).into_iter().take(5) {
            items.push(sx_plugin_api::SxHintEntry {
                label:      SxBytes::from_str_bytes(&m.name),
                completion: SxBytes::from_str_bytes(&m.name),
            });
        }
        sx_plugin_api::SxHints { items }
    }
}

sx_plugin_api::declare_plugin!(AppsHandler::new());
