//! `sx-parser` — query language parser for sx.
//!
//! Provides the [`Filter`] AST, [`Query`] type, and a winnow-based grammar
//! used by both `sx-core` (for compilation) and file-index plugins (for
//! extracting filter constraints from `ctx.raw_query`).
//!
//! # Entry points
//!
//! ```rust,ignore
//! // From a search string — strictmode, fails on unknown namespaces:
//! let q = sx_parser::Query::from_str("e:rs c:todo")?;
//!
//! // From a search string with handler-registry context:
//! let ctx = registry.namespace_context();
//! let q = sx_parser::Query::from_str_with_context("calc:pi e:rs", roots, Some(&ctx))?;
//!
//! // From ctx.raw_query inside a file-index plugin — lenient, no registry needed:
//! let q = sx_parser::from_str_lenient(raw_query);
//! ```

pub mod ast;
pub mod grammar;

pub use ast::{
    CaseSensitivity, FileType, Filter, HandlerToken, HiddenMode, MatchExpr, MetaDepth, MetaFilter,
    MetaValue, NamespaceContext, ParseError, Query, RangeExpr, SearchOptions, Timestamp,
};
pub use grammar::{from_str_lenient, partition};
