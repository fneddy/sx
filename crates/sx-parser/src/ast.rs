//! Query language AST types, shared between `sx-core` and file-index plugins.

use std::path::PathBuf;
use time::OffsetDateTime;

/// Errors produced by `Query::from_str` / `Query::from_str_with_context`.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("unknown namespace `{token}`")]
    UnknownNamespace { token: String },

    #[error("invalid value `{value}` for namespace `{namespace}`: {reason}")]
    InvalidValue {
        namespace: String,
        value: String,
        reason: String,
    },

    #[error("malformed combinator: `{raw}`")]
    MalformedCombinator { raw: String },

    #[error("empty query")]
    EmptyQuery,
}

/// Parser's view of registered namespaces, passed in from `sx-core` at query time.
#[derive(Debug, Default, Clone)]
pub struct NamespaceContext {
    pub known: Vec<Box<[u8]>>,
    pub global: Vec<Box<[u8]>>,
    /// Subset of `global` with `is_file_index = true`. Used when the query has
    /// file-system-only filters so entity-search handlers are never invoked.
    pub global_file_index: Vec<Box<[u8]>>,
}

/// A byte-string match expression: value and optional negation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchExpr {
    pub value: Box<[u8]>,
    pub negated: bool,
}

impl MatchExpr {
    pub fn exact(value: impl AsRef<[u8]>) -> Self {
        Self {
            value: Box::from(value.as_ref()),
            negated: false,
        }
    }

    pub fn negated(mut self) -> Self {
        self.negated = !self.negated;
        self
    }
}

/// A numeric or timestamp range predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeExpr<T> {
    Gt(T),
    Lt(T),
    Eq(T),
    /// `lo < value < hi`
    Between(T, T),
}

/// Nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(pub u64);

impl Timestamp {
    pub fn from_offset_dt(dt: OffsetDateTime) -> Self {
        Self(dt.unix_timestamp_nanos().max(0) as u64)
    }

    pub fn now() -> Self {
        Self::from_offset_dt(OffsetDateTime::now_utc())
    }

    pub fn days_ago(n: u64) -> Self {
        let days = n.min(i64::MAX as u64) as i64;
        let dt = OffsetDateTime::now_utc() - time::Duration::days(days);
        Self::from_offset_dt(dt)
    }

    pub fn weeks_ago(n: u64) -> Self {
        let weeks = n.min(i64::MAX as u64) as i64;
        let dt = OffsetDateTime::now_utc() - time::Duration::weeks(weeks);
        Self::from_offset_dt(dt)
    }

    /// `n` approximate months (30 days each) before now.
    pub fn months_ago(n: u64) -> Self {
        Self::days_ago(n * 30)
    }

    pub fn from_date(year: i32, month: u8, day: u8) -> Result<Self, ParseError> {
        use time::{Date, Month, PrimitiveDateTime, Time};
        let month = Month::try_from(month).map_err(|_| ParseError::InvalidValue {
            namespace: "timestamp".into(),
            value: month.to_string(),
            reason: "month out of range 1–12".into(),
        })?;
        let date =
            Date::from_calendar_date(year, month, day).map_err(|e| ParseError::InvalidValue {
                namespace: "timestamp".into(),
                value: format!("{year}-{month}-{day}"),
                reason: e.to_string(),
            })?;
        Ok(Self::from_offset_dt(
            PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc(),
        ))
    }

    /// Midnight at the start of `day + 1` — exclusive upper bound for a full-day range.
    pub fn end_of_date(year: i32, month: u8, day: u8) -> Result<Self, ParseError> {
        use time::{Date, Month, PrimitiveDateTime, Time};
        let month = Month::try_from(month).map_err(|_| ParseError::InvalidValue {
            namespace: "timestamp".into(),
            value: month.to_string(),
            reason: "month out of range 1–12".into(),
        })?;
        let date = Date::from_calendar_date(year, month, day)
            .ok()
            .and_then(|d| d.next_day())
            .ok_or_else(|| ParseError::InvalidValue {
                namespace: "timestamp".into(),
                value: format!("{year}-{month}-{day}"),
                reason: "could not compute end of day".into(),
            })?;
        Ok(Self::from_offset_dt(
            PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc(),
        ))
    }

    pub fn today_start() -> Self {
        use time::{PrimitiveDateTime, Time};
        let now = OffsetDateTime::now_utc();
        Self::from_offset_dt(PrimitiveDateTime::new(now.date(), Time::MIDNIGHT).assume_utc())
    }

    /// Midnight at the start of tomorrow — exclusive upper bound for `mod:today`.
    pub fn tomorrow_start() -> Self {
        use time::{PrimitiveDateTime, Time};
        let now = OffsetDateTime::now_utc();
        let tomorrow = now
            .date()
            .next_day()
            .map(|d| PrimitiveDateTime::new(d, Time::MIDNIGHT).assume_utc())
            .unwrap_or_else(OffsetDateTime::now_utc);
        Self::from_offset_dt(tomorrow)
    }
}

impl std::str::FromStr for Timestamp {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "today" {
            return Ok(Self::today_start());
        }
        if let Some(rest) = s.strip_suffix('d') {
            if let Ok(n) = rest.parse::<u64>() {
                return Ok(Self::days_ago(n));
            }
        }
        if let Some(rest) = s.strip_suffix('w') {
            if let Ok(n) = rest.parse::<u64>() {
                return Ok(Self::weeks_ago(n));
            }
        }
        if let Some(rest) = s.strip_suffix('m') {
            if let Ok(n) = rest.parse::<u64>() {
                return Ok(Self::months_ago(n));
            }
        }
        if let Some(rest) = s.strip_suffix('y') {
            if let Ok(n) = rest.parse::<u64>() {
                return Ok(Self::months_ago(n * 12));
            }
        }
        let mut it = s.splitn(4, '-');
        if let (Some(y_s), Some(m_s), Some(d_s), None) =
            (it.next(), it.next(), it.next(), it.next())
        {
            if let (Ok(y), Ok(m), Ok(d)) = (
                y_s.parse::<i32>(),
                m_s.parse::<u8>(),
                d_s.parse::<u8>(),
            ) {
                return Self::from_date(y, m, d);
            }
        }
        Err(ParseError::InvalidValue {
            namespace: "timestamp".into(),
            value: s.to_string(),
            reason: r#"expected "today", "<n>d/w/m/y", or "YYYY-MM-DD""#.into(),
        })
    }
}

/// Controls whether hidden filesystem entries (leading-dot names) are visited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HiddenMode {
    /// Include hidden entries alongside non-hidden ones.
    Include,
    /// Exclude hidden entries (library default).
    Exclude,
    /// Return only hidden entries.
    Only,
}

/// Filesystem entry type.
///
/// Walk-tier: `File`, `Dir`, `Symlink`, `Empty` (no file read needed).
/// Content-tier: `Executable`, `Binary`, `Text` (require file read or execute-bit check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    /// Empty file (size == 0).
    Empty,
    /// Execute bit set.
    Executable,
    /// Detected as binary by magic bytes.
    Binary,
    /// Not detected as binary (plain text).
    Text,
}

/// A filter against an embedded metadata tag (EXIF, PDF info dict, ID3, …).
#[derive(Debug, Clone, PartialEq)]
pub struct MetaFilter {
    /// Tag name as raw bytes, e.g. `b"author"`, `b"iso"`, `b"bpm"`.
    pub tag: Box<[u8]>,
    pub expr: MetaValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetaValue {
    Text(MatchExpr),
    Number(RangeExpr<f64>),
    Bool(bool),
}

/// The recursive filter AST.  Walk-tier leaves need no file open; content-tier
/// leaves require extraction.  Combinator nodes may span both tiers; the
/// partition into `walk_filter`/`content_filter` is automatic at compile time.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    // Walk-tier (stat only)
    Name(MatchExpr),
    Path(MatchExpr),
    Hidden(HiddenMode),
    /// Extension allow-list (OR within the vec).
    Ext(Vec<Box<[u8]>>),
    Size(RangeExpr<u64>),
    Modified(RangeExpr<Timestamp>),
    Created(RangeExpr<Timestamp>),
    Accessed(RangeExpr<Timestamp>),
    /// Walk-tier: `File/Dir/Symlink/Empty`.  Content-tier: `Executable/Binary/Text`.
    Type(FileType),
    /// Exact octal permission check: `(mode & 0o7777) == value`.
    PermOctal(u32),
    /// Has-bits permission check: `(mode & mask) != 0`.
    PermFlag(u32),
    Owner(MatchExpr),
    Group(MatchExpr),

    // Content-tier (requires file read)
    Content(MatchExpr),
    /// Like `Content` but evaluated per line; multiple `Line` terms in `And` must co-occur on one line.
    Line(MatchExpr),
    Mime(MatchExpr),
    Meta(MetaFilter),

    // Combinators
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),

    /// Always true.  Identity for an empty walk or content filter.
    Any,
}

/// Walk execution options.  Not part of the filter predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    /// `None` = unlimited depth.
    pub depth: Option<usize>,
    pub follow_symlinks: bool,
    pub respect_ignores: bool,
    pub case: CaseSensitivity,
    pub include_binary_content: bool,
    pub meta_depth: MetaDepth,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            depth: None,
            follow_symlinks: false,
            respect_ignores: true,
            case: CaseSensitivity::Smart,
            include_binary_content: false,
            meta_depth: MetaDepth::Shallow,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseSensitivity {
    /// Insensitive when the pattern is all-lowercase; sensitive otherwise.
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaDepth {
    /// `stat()` and magic-byte MIME only.
    Shallow,
    /// Full parsing: EXIF, PDF info dict, ID3, video headers.
    Full,
}

/// A `namespace:value` token destined for a non-file handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerToken {
    pub namespace: Box<[u8]>,
    pub value: Box<[u8]>,
}

/// Top-level query. Pre-partitioned into `walk_filter` and `content_filter` at parse time.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub roots: Vec<PathBuf>,
    pub walk_filter: Filter,
    pub content_filter: Filter,
    pub options: SearchOptions,
    pub handler_tokens: Vec<HandlerToken>,
    /// Verbatim query string, passed to file-index plugins for their own parsing.
    pub raw_query: String,
}
