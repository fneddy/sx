//! Mapping from [`ResultMsg`] to the display-ready [`RowData`] / [`SortKey`]
//! types consumed by [`super::result_list::ResultList`].

use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;

use gpui::{RenderImage, SharedString};

use crate::shared::msg::ResultMsg;

use super::result_list::{ActionKind, RowData};

// ── Sort key ──────────────────────────────────────────────────────────────────

pub struct SortKey {
    pub score: f64,
    pub section: u8,
    pub title_len: usize,
}

impl From<&ResultMsg> for SortKey {
    fn from(msg: &ResultMsg) -> Self {
        Self {
            score: msg.score,
            section: u8::from(msg.namespace == "file"),
            title_len: msg.title.as_deref().map_or(0, str::len),
        }
    }
}

impl SortKey {
    pub fn cmp(&self, other: &Self) -> Ordering {
        self.section
            .cmp(&other.section)
            .then_with(|| other.score.total_cmp(&self.score))
            .then_with(|| self.title_len.cmp(&other.title_len))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn row_symbol(r: &ResultMsg) -> Option<&str> {
    crate::shared::icon::symbol_for_result(r)
}

/// The activation kind for a result: gio_id takes priority over file_path.
fn row_action(r: &ResultMsg) -> Option<ActionKind> {
    if let Some(id) = r.meta("gio_id") {
        return Some(ActionKind::GioId(id.to_owned().into()));
    }
    if let Some(path) = r.meta("file_path") {
        return Some(ActionKind::FilePath(path.to_owned().into()));
    }
    None
}

/// Format a metadata entry as `"key: value"`, or `None` for `Bytes`.
///
/// Central definition — used by both [`metadata_summary`] and [`make_row`].
fn meta_entry_display(key: &str, value: &crate::shared::msg::MetaValue) -> Option<String> {
    use crate::shared::msg::MetaValue;
    match value {
        MetaValue::Text(v)   => Some(format!("{key}: {v}")),
        MetaValue::Number(n) => Some(format!("{key}: {n}")),
        MetaValue::Bytes(_)  => None,
    }
}

/// Build a short human-readable summary string from a result's metadata.
///
/// Weather results get a formatted line like "partly cloudy · 18.5°C · 12 km/h".
/// Other results fall back to the first metadata key/value pair.
fn metadata_summary(r: &ResultMsg) -> Option<String> {
    // Weather: condition + temp + windspeed
    if let Some(cond) = r.meta("condition") {
        let mut s = cond.to_owned();
        if let Some(t) = r.meta_number("temp_c") {
            s.push_str(&format!(" · {}°C", t));
        }
        if let Some(w) = r.meta_number("windspeed_kph") {
            s.push_str(&format!(" · {} km/h", w));
        }
        return Some(s);
    }
    // Generic fallback: first Text entry that isn't icon_symbol
    r.metadata
        .iter()
        .filter(|(k, _)| *k != "icon_symbol")
        .find_map(|(k, v)| meta_entry_display(k, v))
}

/// Decode raw PNG bytes into a `RenderImage` (BGRA frames) for GPUI rendering.
///
/// # Note
/// This runs on the GPUI foreground task inside `insert_results`. For small
/// app-icon PNGs (typically 32×32 or 64×64) this is negligible. If icons grow
/// to large artwork sizes, move the decode into the Tokio blocking worker that
/// produces `ResultMsg` and send pre-decoded bytes (or an `Arc<RenderImage>`
/// via a wrapper type) over the channel instead.
fn png_to_render_image(png: &[u8]) -> Option<Arc<RenderImage>> {
    let img = image::load_from_memory(png).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let mut bgra = img.into_raw();
    // RGBA → BGRA: swap R and B channels in-place.
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let frame = image::Frame::new(image::ImageBuffer::from_raw(w, h, bgra)?);
    Some(Arc::new(RenderImage::new(vec![frame])))
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn make_row(r: &ResultMsg) -> RowData {
    let raw_label: String = r
        .title
        .clone()
        .or_else(|| r.meta("value").map(str::to_owned))
        .unwrap_or_else(|| {
            r.metadata
                .iter()
                .find_map(|(k, v)| meta_entry_display(k, v))
                .unwrap_or_default()
        });

    let is_file = r.namespace == "file";

    let (label, file_parent) = if is_file {
        let p = Path::new(&raw_label);
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
        let parent = p
            .parent()
            .filter(|p| *p != Path::new(""))
            .map(|p| p.to_string_lossy().into_owned());
        (name.unwrap_or(raw_label), parent)
    } else {
        (raw_label, None)
    };

    let action = row_action(r);

    let sub_label = if is_file {
        file_parent
    } else if let Some(ref a) = action {
        // Show the action target as context (e.g. file path or app id), unless
        // it duplicates the label.
        let s = a.label();
        if !s.is_empty() && s != label {
            Some(s.to_owned())
        } else {
            None
        }
    } else {
        // No action: build a compact summary from well-known metadata keys,
        // then fall back to the first metadata pair, then the namespace.
        metadata_summary(r).or_else(|| Some(r.namespace.clone()))
    };

    let symbol = row_symbol(r).map(|s| SharedString::from(s.to_owned()));
    log::debug!(
        "make_row: ns={} label={:?} symbol={:?} has_png={}",
        r.namespace,
        label,
        symbol,
        r.meta_bytes("icon_png").is_some()
    );
    RowData {
        icon: r.meta_bytes("icon_png").and_then(png_to_render_image),
        symbol,
        label: label.into(),
        sub_label: sub_label.map(Into::into),
        action,
    }
}
