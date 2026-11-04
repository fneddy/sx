//! Registry-aware parse helpers.
//!
//! The grammar lives in `sx-parser`.  This thin wrapper adds
//! `Query::from_str_with_registry` via an extension trait so callers that
//! have a `HandlerRegistry` don't need to build a [`NamespaceContext`] manually.

use crate::handler_registry::HandlerRegistry;
use std::path::PathBuf;
use sx_parser::{NamespaceContext, ParseError, Query};

/// Extension methods on [`Query`] that require an `sx-core` [`HandlerRegistry`].
///
/// Import this trait alongside [`Query`] to use
/// `Query::from_str_with_registry(…)`.
pub trait QueryRegistryExt {
    fn from_str_with_registry(
        input: &str,
        roots: Vec<PathBuf>,
        registry: Option<&HandlerRegistry>,
    ) -> Result<Query, ParseError>;
}

impl QueryRegistryExt for Query {
    fn from_str_with_registry(
        input: &str,
        roots: Vec<PathBuf>,
        registry: Option<&HandlerRegistry>,
    ) -> Result<Query, ParseError> {
        let ctx: Option<NamespaceContext> = registry.map(|r| r.namespace_context());
        Query::from_str_with_context(input, roots, ctx.as_ref())
    }
}
