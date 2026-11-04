/// Errors produced by `Query::from_str` / `Query::from_str_with_roots`.
///
/// Re-exported from `sx-parser` so callers of `sx-core` don't need to depend
/// on `sx-parser` directly.
pub use sx_parser::ParseError;

/// Errors produced by `QueryBuilder::build()`.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum BuildError {
    #[error("no roots provided — call .root() or .roots() before .build()")]
    NoRootsProvided,

    #[error("conflicting range on field `{field}`")]
    ConflictingRange { field: String },

    #[error("invalid meta tag `{tag}`: {reason}")]
    InvalidMetaTag { tag: String, reason: String },

    #[error("invalid octal permission string `{value}`: {reason}")]
    InvalidPermOctal { value: String, reason: String },
}

/// Errors that can occur while searching or extracting content.
#[derive(Debug, thiserror::Error, Clone)]
pub enum SxError {
    /// I/O error, wrapped in `Arc` so `SxError` is cheaply `Clone`.
    /// `From<std::io::Error>` is implemented manually below.
    #[error("io error: {0}")]
    Io(std::sync::Arc<std::io::Error>),

    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("extraction failed: {0}")]
    Extraction(String),
}

impl SxError {
    /// Convenience constructor — wraps a plain [`std::io::Error`] in `Arc`.
    pub fn io(e: std::io::Error) -> Self {
        Self::Io(std::sync::Arc::new(e))
    }
}

impl From<std::io::Error> for SxError {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}
