//! `sx` — composable file search library.
//!
//! Build a [`Query`] via [`QueryBuilder::new()`] or `"e:rs".parse::<Query>()`, compile
//! it into a [`CompiledQuery`], then call [`CompiledQuery::run()`].  Clone the
//! `CompiledQuery` across as many spawned tasks as you want concurrency for;
//! the library coordinates them via a shared work-stealing backlog and never
//! spawns anything or picks a worker count itself.
//!
//! ```no_run
//! use sx_core::{Query, CompiledQuery};
//! use futures_util::StreamExt;
//!
//! # async fn example() {
//! let cq: CompiledQuery = QueryBuilder::new()
//!     .root("~/docs")
//!     .content("hashmap")
//!     .build()
//!     .unwrap()
//!     .compile();
//!
//! let mut stream = cq.run();
//! while let Some(m) = stream.next().await {
//!     println!("{}", m.path.display());
//! }
//! # }
//! ```

pub mod compiled;
mod error;
pub mod extract;
pub mod handler_registry;
pub mod loader;
pub mod parser;
mod query_builder;

pub use compiled::{CompiledContentFilter, CompiledQuery, CompiledWalkFilter, Match, QueryCompileExt};
pub use error::{BuildError, ParseError, SxError};
pub use handler_registry::{ConflictError, HandlerRegistry, UnknownNamespace};
pub use loader::{LoadedPlugin, PluginStore};
pub use parser::QueryRegistryExt;
pub use sx_parser::{
    CaseSensitivity, FileType, Filter, HandlerToken, HiddenMode, MatchExpr, MetaDepth, MetaFilter,
    MetaValue, NamespaceContext, Query, RangeExpr, SearchOptions, Timestamp,
};
pub use query_builder::QueryBuilder;
