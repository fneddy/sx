//! Post-FFI message types for search results delivered to the UI.
//!
//! `sx-plugin-api` defines the ABI-stable, pointer-based types (`SxResult`,
//! `SxMetaValue`, …) that plugins write across the shared-library boundary.
//! The types in this module are their owned, host-side equivalents:
//!
//! | Plugin-API (ABI-stable)       | This module (owned)  |
//! |-------------------------------|----------------------|
//! | `SxMetaValue`                 | `MetaValue`          |
//! | `SxResult`                    | `ResultMsg`          |
//!
//! The conversion happens in `From<SxResult> for ResultMsg`.  Any change to
//! `SxResult` / `SxMetaValue` in `sx-plugin-api` must be mirrored here and in
//! that `From` impl.

use std::collections::HashMap;

/// The value of a metadata entry — mirrors [`sx_plugin_api::SxMetaValue`].
#[derive(Debug, Clone)]
pub enum MetaValue {
    Text(String),
    Number(f64),
    Bytes(Vec<u8>),
}

/// A single search result delivered to the UI — mirrors [`sx_plugin_api::SxResult`].
///
/// File hits use `namespace = "file"` and carry `file_path` in metadata.
#[derive(Debug, Clone)]
pub struct ResultMsg {
    pub namespace: String,
    pub score: f64,
    pub title: Option<String>,
    pub metadata: HashMap<String, MetaValue>,
}

impl ResultMsg {
    /// Construct a plain file-hit result from a path and a normalised score.
    pub fn from_file(path: String, score: f64) -> Self {
        let mut metadata = HashMap::new();
        metadata.insert("file_path".to_owned(), MetaValue::Text(path.clone()));
        Self {
            namespace: "file".to_owned(),
            score,
            title: Some(path),
            metadata,
        }
    }

    /// Look up a text metadata value by key. Returns `None` for missing keys
    /// or non-`Text` values.
    pub fn meta(&self, key: &str) -> Option<&str> {
        match self.metadata.get(key) {
            Some(MetaValue::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Look up a raw-bytes metadata value by key (e.g. `"icon_png"`).
    pub fn meta_bytes(&self, key: &str) -> Option<&[u8]> {
        match self.metadata.get(key) {
            Some(MetaValue::Bytes(b)) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Look up a numeric metadata value by key.
    pub fn meta_number(&self, key: &str) -> Option<f64> {
        match self.metadata.get(key) {
            Some(MetaValue::Number(n)) => Some(*n),
            _ => None,
        }
    }

    /// Returns `true` when the result carries no displayable content.
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.metadata.is_empty()
    }
}

impl From<sx_plugin_api::SxResult> for ResultMsg {
    fn from(res: sx_plugin_api::SxResult) -> Self {
        let namespace = res.source_namespace.into_string();
        let score = res.score;
        let title = res.title.match_owned(|b| Some(b.into_string()), || None);

        let metadata: HashMap<String, MetaValue> = res
            .metadata
            .into_iter()
            .map(|e| {
                let key = e.key.into_string();
                let value = e.value.match_owned(
                    |t| MetaValue::Text(t.into_string()),
                    |n| MetaValue::Number(n),
                    |b| MetaValue::Bytes(b.into_vec()),
                );
                (key, value)
            })
            .collect();

        Self {
            namespace,
            score,
            title,
            metadata,
        }
    }
}
