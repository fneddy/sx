use crate::error::BuildError;
use crate::query::{
    CaseSensitivity, FileType, Filter, HiddenMode, MatchExpr, MetaDepth, MetaFilter, MetaValue,
    Query, RangeExpr, SearchOptions, Timestamp,
};
use log::debug;
use std::path::PathBuf;

/// Which evaluation tier a filter leaf belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Walk,
    Content,
    /// Combinator contains children from both tiers.
    Mixed,
}

fn tier_of(f: &Filter) -> Tier {
    match f {
        // Walk-tier leaves
        Filter::Name(_)
        | Filter::Path(_)
        | Filter::Hidden(_)
        | Filter::Ext(_)
        | Filter::Size(_)
        | Filter::Modified(_)
        | Filter::Created(_)
        | Filter::Accessed(_)
        | Filter::PermOctal(_)
        | Filter::PermFlag(_)
        | Filter::Owner(_)
        | Filter::Group(_) => Tier::Walk,

        // FileType: walk-tier variants only
        Filter::Type(ft) => match ft {
            FileType::File | FileType::Dir | FileType::Symlink | FileType::Empty => Tier::Walk,
            FileType::Executable | FileType::Binary | FileType::Text => Tier::Content,
        },

        // Content-tier leaves
        Filter::Content(_) | Filter::Line(_) | Filter::Mime(_) | Filter::Meta(_) => Tier::Content,

        // Sentinel
        Filter::Any => Tier::Walk,

        // Combinators: derive from children
        Filter::Not(inner) => tier_of(inner),
        Filter::And(children) | Filter::Or(children) => {
            let mut has_walk = false;
            let mut has_content = false;
            for c in children {
                match tier_of(c) {
                    Tier::Walk => has_walk = true,
                    Tier::Content => has_content = true,
                    Tier::Mixed => {
                        has_walk = true;
                        has_content = true;
                    }
                }
            }
            match (has_walk, has_content) {
                (true, false) => Tier::Walk,
                (false, true) => Tier::Content,
                _ => Tier::Mixed,
            }
        }
    }
}

/// Partition a filter into `(walk_part, content_part)`.
/// `pub(crate)` so `parser.rs` can call it directly.
pub(crate) fn partition(f: Filter) -> (Filter, Filter) {
    // `And` nodes are split element-by-element in a single pass, without a
    // prior `tier_of` traversal, to avoid walking the tree twice.
    if let Filter::And(children) = f {
        let mut walk_children = Vec::new();
        let mut content_children = Vec::new();
        for child in children {
            let (w, c) = partition(child);
            if w != Filter::Any {
                walk_children.push(w);
            }
            if c != Filter::Any {
                content_children.push(c);
            }
        }
        let pack = |mut v: Vec<Filter>| match v.len() {
            0 => Filter::Any,
            1 => v.remove(0),
            _ => Filter::And(v),
        };
        return (pack(walk_children), pack(content_children));
    }

    // For every other node a single `tier_of` pass is sufficient.
    match tier_of(&f) {
        Tier::Walk => (f, Filter::Any),
        Tier::Content => (Filter::Any, f),
        // Mixed Or and mixed Not go entirely to the content tier.
        Tier::Mixed => (Filter::Any, f),
    }
}

/// Walk `f` and return an error if any `Filter::Meta` node is present.
///
/// `meta:` filtering is not yet implemented — building a query with it would
/// produce a silent always-pass at runtime.  Reject it here so callers get a
/// clear `BuildError` instead.
fn reject_meta(f: &Filter) -> Result<(), BuildError> {
    match f {
        Filter::Meta(mf) => Err(BuildError::InvalidMetaTag {
            tag: String::from_utf8_lossy(&mf.tag).into_owned(),
            reason: "meta: filtering is not yet implemented".into(),
        }),
        Filter::And(children) | Filter::Or(children) => {
            for c in children {
                reject_meta(c)?;
            }
            Ok(())
        }
        Filter::Not(inner) => reject_meta(inner),
        _ => Ok(()),
    }
}

// ── QueryBuilder ──────────────────────────────────────────────────────────────

/// Typed, chainable builder for `Query`. Each method maps 1-to-1 to a
/// `Filter` variant. Call `.build()` to partition the filter tree into
/// `walk_filter` / `content_filter` and freeze into a `Query`.
#[derive(Default)]
pub struct QueryBuilder {
    roots: Vec<PathBuf>,
    filters: Vec<Filter>,
    options: SearchOptions,
}

impl QueryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Roots ─────────────────────────────────────────────────────────────────

    pub fn root(mut self, path: impl Into<PathBuf>) -> Self {
        self.roots.push(path.into());
        self
    }

    pub fn roots(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.roots.extend(paths.into_iter().map(Into::into));
        self
    }

    // ── Filename ──────────────────────────────────────────────────────────────

    pub fn name(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters.push(Filter::Name(MatchExpr::exact(value)));
        self
    }

    pub fn name_not(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters
            .push(Filter::Name(MatchExpr::exact(value).negated()));
        self
    }

    // ── Full path ─────────────────────────────────────────────────────────────

    pub fn path(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters.push(Filter::Path(MatchExpr::exact(value)));
        self
    }

    pub fn path_not(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters
            .push(Filter::Path(MatchExpr::exact(value).negated()));
        self
    }

    // ── Hidden ────────────────────────────────────────────────────────────────

    pub fn hidden(mut self, mode: HiddenMode) -> Self {
        self.filters.push(Filter::Hidden(mode));
        self
    }

    // ── Extension ─────────────────────────────────────────────────────────────

    pub fn ext(mut self, exts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Self {
        let list: Vec<Box<[u8]>> = exts.into_iter().map(|e| Box::from(e.as_ref())).collect();
        self.filters.push(Filter::Ext(list));
        self
    }

    pub fn ext_not(mut self, exts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> Self {
        let list: Vec<Box<[u8]>> = exts.into_iter().map(|e| Box::from(e.as_ref())).collect();
        self.filters.push(Filter::Not(Box::new(Filter::Ext(list))));
        self
    }

    // ── Content ───────────────────────────────────────────────────────────────

    pub fn content(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters.push(Filter::Content(MatchExpr::exact(value)));
        self
    }

    pub fn content_not(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters
            .push(Filter::Content(MatchExpr::exact(value).negated()));
        self
    }

    pub fn line(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters.push(Filter::Line(MatchExpr::exact(value)));
        self
    }

    // ── Size ──────────────────────────────────────────────────────────────────

    pub fn size_gt(mut self, bytes: u64) -> Self {
        self.filters.push(Filter::Size(RangeExpr::Gt(bytes)));
        self
    }

    pub fn size_lt(mut self, bytes: u64) -> Self {
        self.filters.push(Filter::Size(RangeExpr::Lt(bytes)));
        self
    }

    pub fn size_eq(mut self, bytes: u64) -> Self {
        self.filters.push(Filter::Size(RangeExpr::Eq(bytes)));
        self
    }

    pub fn size_between(mut self, min_bytes: u64, max_bytes: u64) -> Self {
        self.filters
            .push(Filter::Size(RangeExpr::Between(min_bytes, max_bytes)));
        self
    }

    // ── Timestamps ────────────────────────────────────────────────────────────

    pub fn modified_after(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Modified(RangeExpr::Gt(ts)));
        self
    }

    pub fn modified_before(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Modified(RangeExpr::Lt(ts)));
        self
    }

    pub fn modified_between(mut self, after: Timestamp, before: Timestamp) -> Self {
        self.filters
            .push(Filter::Modified(RangeExpr::Between(after, before)));
        self
    }

    pub fn created_after(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Created(RangeExpr::Gt(ts)));
        self
    }

    pub fn created_before(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Created(RangeExpr::Lt(ts)));
        self
    }

    pub fn accessed_after(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Accessed(RangeExpr::Gt(ts)));
        self
    }

    pub fn accessed_before(mut self, ts: Timestamp) -> Self {
        self.filters.push(Filter::Accessed(RangeExpr::Lt(ts)));
        self
    }

    // ── File type ─────────────────────────────────────────────────────────────

    pub fn file_type(mut self, t: FileType) -> Self {
        self.filters.push(Filter::Type(t));
        self
    }

    // ── MIME ──────────────────────────────────────────────────────────────────

    pub fn mime(mut self, value: impl AsRef<[u8]>) -> Self {
        self.filters.push(Filter::Mime(MatchExpr::exact(value)));
        self
    }

    // ── Permissions ───────────────────────────────────────────────────────────

    /// Exact-match permission bits. `octal` is an octal string, e.g. `"644"`.
    ///
    /// Returns `Err(BuildError::InvalidPermOctal)` if `octal` is not a valid
    /// octal number.  For pre-parsed values use `Filter::PermOctal(v)` directly
    /// via [`and_filter`](Self::and_filter).
    pub fn try_perm_octal(mut self, octal: &str) -> Result<Self, BuildError> {
        let v = u32::from_str_radix(octal, 8).map_err(|e| BuildError::InvalidPermOctal {
            value: octal.to_owned(),
            reason: e.to_string(),
        })?;
        self.filters.push(Filter::PermOctal(v));
        Ok(self)
    }

    /// Has-bits permission check. `mask` is the bitmask (e.g. `0o111` for any-execute).
    pub fn perm_flag(mut self, mask: u32) -> Self {
        self.filters.push(Filter::PermFlag(mask));
        self
    }

    // ── Ownership ─────────────────────────────────────────────────────────────

    pub fn owner(mut self, name_or_uid: impl AsRef<[u8]>) -> Self {
        self.filters
            .push(Filter::Owner(MatchExpr::exact(name_or_uid)));
        self
    }

    pub fn group(mut self, name_or_gid: impl AsRef<[u8]>) -> Self {
        self.filters
            .push(Filter::Group(MatchExpr::exact(name_or_gid)));
        self
    }

    // ── Metadata ──────────────────────────────────────────────────────────────

    pub fn meta_text(mut self, tag: impl AsRef<[u8]>, expr: MatchExpr) -> Self {
        self.filters.push(Filter::Meta(MetaFilter {
            tag: Box::from(tag.as_ref()),
            expr: MetaValue::Text(expr),
        }));
        self
    }

    pub fn meta_number_gt(mut self, tag: impl AsRef<[u8]>, value: f64) -> Self {
        self.filters.push(Filter::Meta(MetaFilter {
            tag: Box::from(tag.as_ref()),
            expr: MetaValue::Number(RangeExpr::Gt(value)),
        }));
        self
    }

    pub fn meta_number_lt(mut self, tag: impl AsRef<[u8]>, value: f64) -> Self {
        self.filters.push(Filter::Meta(MetaFilter {
            tag: Box::from(tag.as_ref()),
            expr: MetaValue::Number(RangeExpr::Lt(value)),
        }));
        self
    }

    pub fn meta_number_eq(mut self, tag: impl AsRef<[u8]>, value: f64) -> Self {
        self.filters.push(Filter::Meta(MetaFilter {
            tag: Box::from(tag.as_ref()),
            expr: MetaValue::Number(RangeExpr::Eq(value)),
        }));
        self
    }

    pub fn meta_bool(mut self, tag: impl AsRef<[u8]>, value: bool) -> Self {
        self.filters.push(Filter::Meta(MetaFilter {
            tag: Box::from(tag.as_ref()),
            expr: MetaValue::Bool(value),
        }));
        self
    }

    // ── Logic combinators ─────────────────────────────────────────────────────

    /// Push a pre-built `Filter` subtree, AND'd with everything else.
    pub fn and_filter(mut self, f: Filter) -> Self {
        self.filters.push(f);
        self
    }

    /// Combine the **last** accumulated filter with `f` in an `Or`.
    ///
    /// **Order matters.** This method modifies only the previously pushed
    /// filter, not the whole accumulated list.  For example:
    ///
    /// ```text
    /// builder.name("foo").size_gt(1024).or_filter(Filter::Name(...("bar")))
    /// //  => And(Name("foo"),  Or(Size(>1024),  Name("bar")))
    /// //                       ^ only this part is OR'd
    /// ```
    ///
    /// To OR an entire compound expression, build the `Filter` manually and
    /// push it with [`and_filter`](Self::and_filter).
    pub fn or_filter(mut self, f: Filter) -> Self {
        if let Some(last) = self.filters.pop() {
            self.filters.push(Filter::Or(vec![last, f]));
        } else {
            self.filters.push(f);
        }
        self
    }

    /// Negate the **last** accumulated filter by wrapping it in `Not`.
    ///
    /// Same ordering caveat as [`or_filter`](Self::or_filter): only the
    /// previously pushed filter is negated.
    pub fn not_last(mut self) -> Self {
        if let Some(last) = self.filters.pop() {
            self.filters.push(Filter::Not(Box::new(last)));
        }
        self
    }

    // ── Options ───────────────────────────────────────────────────────────────

    pub fn depth(mut self, max: usize) -> Self {
        self.options.depth = Some(max);
        self
    }

    pub fn follow_symlinks(mut self, yes: bool) -> Self {
        self.options.follow_symlinks = yes;
        self
    }

    pub fn respect_ignores(mut self, yes: bool) -> Self {
        self.options.respect_ignores = yes;
        self
    }

    pub fn case(mut self, sensitivity: CaseSensitivity) -> Self {
        self.options.case = sensitivity;
        self
    }

    pub fn include_binary_content(mut self, yes: bool) -> Self {
        self.options.include_binary_content = yes;
        self
    }

    pub fn meta_depth(mut self, depth: MetaDepth) -> Self {
        self.options.meta_depth = depth;
        self
    }

    // ── Build ─────────────────────────────────────────────────────────────────

    /// Partition the accumulated filters into `walk_filter` / `content_filter`
    /// and freeze into an immutable `Query`.
    ///
    /// Returns `BuildError::NoRootsProvided` if no roots were set.
    pub fn build(self) -> Result<Query, BuildError> {
        if self.roots.is_empty() {
            return Err(BuildError::NoRootsProvided);
        }

        let filters = self.filters;
        let n_filters = filters.len();
        let combined = match filters.len() {
            0 => Filter::Any,
            1 => filters.into_iter().next().unwrap(),
            _ => Filter::And(filters),
        };

        reject_meta(&combined)?;
        let (walk_filter, content_filter) = partition(combined);

        debug!(
            "query_builder: build — {} root(s), {} filter(s), walk={}, content={}",
            self.roots.len(),
            n_filters,
            if walk_filter == Filter::Any {
                "Any"
            } else {
                "Some"
            },
            if content_filter == Filter::Any {
                "Any"
            } else {
                "Some"
            },
        );

        Ok(Query {
            roots: self.roots,
            walk_filter,
            content_filter,
            options: self.options,
            handler_tokens: vec![],
        })
    }
}
