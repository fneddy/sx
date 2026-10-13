//! Compiled walk-tier and content-tier filters.
//!
//! [`CompiledWalkFilter`] and [`CompiledContentFilter`] are built once per
//! query from the respective [`crate::query::Filter`] sub-trees.  All
//! AhoCorasick automata and hash sets are constructed at compile time; the
//! hot-path `matches()` calls do no allocation and no construction.

use log::{debug, trace};
#[cfg(unix)]
use uzers::{get_group_by_gid, get_user_by_uid};

use crate::query::{
    CaseSensitivity, FileType, Filter, HiddenMode, MatchExpr, MetaFilter, RangeExpr, SearchOptions,
    Timestamp,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use memchr::memmem;
use std::collections::HashSet;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

// ── Internal match helpers ────────────────────────────────────────────────────

/// Compiled form of a single `MatchExpr`.
///
/// Uses `memchr::memmem::Finder` rather than `AhoCorasick` — cheaper to
/// construct and faster to search for a single pattern.  `AhoCorasick` is
/// reserved for the content tier where many patterns are scanned in one pass.
struct CompiledMatch {
    finder: memmem::Finder<'static>,
    negated: bool,
    /// Whether the finder needle was lowercased for case-insensitive matching.
    /// Stored so `matches_bytes` knows when to lowercase the haystack too.
    insensitive: bool,
}

impl CompiledMatch {
    fn from_expr(expr: &MatchExpr, case: AcCase) -> Self {
        let insensitive = case == AcCase::Insensitive;
        let pattern: Box<[u8]> = if insensitive {
            expr.value.iter().map(|b| b.to_ascii_lowercase()).collect()
        } else {
            expr.value.clone()
        };
        Self {
            finder: memmem::Finder::new(&pattern).into_owned(),
            negated: expr.negated,
            insensitive,
        }
    }

    fn matches_bytes(&self, haystack: &[u8]) -> bool {
        let found = self.find_raw(haystack);
        if self.negated {
            !found
        } else {
            found
        }
    }

    /// Search without applying `negated`. Use when the same pattern must be
    /// tested against multiple haystacks before negation is applied once.
    fn find_raw(&self, haystack: &[u8]) -> bool {
        if self.insensitive {
            with_lowercase(haystack, |lower| self.finder.find(lower).is_some())
        } else {
            self.finder.find(haystack).is_some()
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AcCase {
    Sensitive,
    Insensitive,
}

/// Internal recursive filter node.
enum FilterNode {
    Any,
    Name(CompiledMatch),
    Path(CompiledMatch),
    Hidden(HiddenMode),
    Ext(HashSet<Box<[u8]>>),
    Size(RangeExpr<u64>),
    Modified(RangeExpr<Timestamp>),
    Created(RangeExpr<Timestamp>),
    Accessed(RangeExpr<Timestamp>),
    Type(FileType),
    PermOctal(u32),
    PermFlag(u32),
    Owner(CompiledMatch),
    Group(CompiledMatch),
    And(Vec<FilterNode>),
    Or(Vec<FilterNode>),
    Not(Box<FilterNode>),
}

/// Pre-compiled walk-tier filter.
pub struct CompiledWalkFilter(FilterNode);

impl FilterNode {
    fn from_filter(f: &Filter, case: AcCase) -> Self {
        match f {
            Filter::Any => Self::Any,

            Filter::Name(expr) => Self::Name(CompiledMatch::from_expr(expr, case)),
            Filter::Path(expr) => Self::Path(CompiledMatch::from_expr(expr, case)),

            Filter::Hidden(mode) => Self::Hidden(mode.clone()),

            Filter::Ext(list) => {
                let set: HashSet<Box<[u8]>> = list.iter().cloned().collect();
                Self::Ext(set)
            }

            Filter::Size(range) => Self::Size(range.clone()),

            Filter::Modified(range) => Self::Modified(range.clone()),
            Filter::Created(range) => Self::Created(range.clone()),
            Filter::Accessed(range) => Self::Accessed(range.clone()),

            Filter::Type(ft) => Self::Type(ft.clone()),

            Filter::PermOctal(v) => Self::PermOctal(*v),
            Filter::PermFlag(v) => Self::PermFlag(*v),
            // Usernames and group names are case-sensitive on Unix; ignore the
            // global SearchOptions::case setting here.
            Filter::Owner(expr) => Self::Owner(CompiledMatch::from_expr(expr, AcCase::Sensitive)),
            Filter::Group(expr) => Self::Group(CompiledMatch::from_expr(expr, AcCase::Sensitive)),

            Filter::And(children) => {
                let compiled: Vec<_> = children
                    .iter()
                    .map(|c| Self::from_filter(c, case))
                    .collect();
                Self::And(compiled)
            }
            Filter::Or(children) => {
                let compiled: Vec<_> = children
                    .iter()
                    .map(|c| Self::from_filter(c, case))
                    .collect();
                Self::Or(compiled)
            }
            Filter::Not(inner) => Self::Not(Box::new(Self::from_filter(inner, case))),

            // Content-tier leaves have no walk-tier role; treat as always-true.
            Filter::Content(_) | Filter::Line(_) | Filter::Mime(_) | Filter::Meta(_) => Self::Any,
        }
    }

    /// Returns `true` when this node can meaningfully prune a directory.
    /// Only used by the `Not` arm of `dir_passes`.
    fn is_dir_predicate(&self) -> bool {
        match self {
            Self::Hidden(_) => true,
            // Positive Name/Path must not prune — the match may be in a subdirectory.
            Self::Name(cm) | Self::Path(cm) => cm.negated,
            Self::And(children) | Self::Or(children) => {
                children.iter().any(|c| c.is_dir_predicate())
            }
            Self::Not(inner) => inner.is_dir_predicate(),
            _ => false,
        }
    }

    /// Returns `false` when a negated Name/Path/Hidden predicate definitively
    /// excludes a directory.  All other leaves return `true` — they must not
    /// prune directories.
    fn dir_passes(&self, entry: &walkdir::DirEntry) -> bool {
        match self {
            Self::Any => true,
            Self::Path(cm) => {
                if cm.negated {
                    let path = entry.path().as_os_str().as_encoded_bytes();
                    cm.matches_bytes(path)
                } else {
                    true
                }
            }
            Self::Name(cm) => {
                if cm.negated {
                    let name = entry.file_name().as_encoded_bytes();
                    cm.matches_bytes(name)
                } else {
                    true
                }
            }
            Self::Hidden(mode) => {
                let is_hidden = entry.file_name().as_encoded_bytes().first().copied() == Some(b'.');
                match mode {
                    HiddenMode::Include => true,
                    HiddenMode::Exclude => !is_hidden,
                    HiddenMode::Only => is_hidden,
                }
            }
            Self::Ext(_)
            | Self::Size(_)
            | Self::Modified(_)
            | Self::Created(_)
            | Self::Accessed(_)
            | Self::Type(_)
            | Self::PermOctal(_)
            | Self::PermFlag(_)
            | Self::Owner(_)
            | Self::Group(_) => true,

            Self::And(children) => children.iter().all(|c| c.dir_passes(entry)),
            Self::Or(children) => children.iter().any(|c| c.dir_passes(entry)),
            Self::Not(inner) => {
                if inner.is_dir_predicate() {
                    !inner.dir_passes(entry)
                } else {
                    true
                }
            }
        }
    }

    fn matches(&self, entry: &walkdir::DirEntry) -> bool {
        let result = self.matches_inner(entry);
        trace!(
            "walk filter: {} => {}",
            entry.path().display(),
            if result { "PASS" } else { "FAIL" }
        );
        result
    }

    fn matches_inner(&self, entry: &walkdir::DirEntry) -> bool {
        match self {
            Self::Any => true,

            Self::Name(cm) => {
                let name = entry.file_name().as_encoded_bytes();
                cm.matches_bytes(name)
            }

            Self::Path(cm) => {
                let path = entry.path().as_os_str().as_encoded_bytes();
                let result = cm.matches_bytes(path);
                trace!(
                    "  Path predicate (negated={}) on '{}' => {}",
                    cm.negated,
                    entry.path().display(),
                    result
                );
                result
            }

            Self::Hidden(mode) => {
                let is_hidden = entry.file_name().as_encoded_bytes().first().copied() == Some(b'.');
                match mode {
                    HiddenMode::Include => true,
                    HiddenMode::Exclude => !is_hidden,
                    HiddenMode::Only => is_hidden,
                }
            }

            Self::Ext(set) => {
                // Stored lowercase; compare case-insensitively to match `Main.RS` against `e:rs`.
                let raw_ext = entry
                    .path()
                    .extension()
                    .map(|e| e.as_encoded_bytes())
                    .unwrap_or_default();
                set.iter().any(|stored| {
                    stored.len() == raw_ext.len()
                        && stored
                            .iter()
                            .zip(raw_ext.iter())
                            .all(|(a, b)| a.eq_ignore_ascii_case(b))
                })
            }

            Self::Size(range) => {
                let Ok(meta) = entry.metadata() else {
                    return false;
                };
                range_matches_u64(range, meta.len())
            }

            Self::Modified(range) => {
                let Ok(meta) = entry.metadata() else {
                    return false;
                };
                let Ok(mtime) = meta.modified() else {
                    return false;
                };
                range_matches_ts(range, system_time_to_timestamp(mtime))
            }

            Self::Created(range) => {
                let Ok(meta) = entry.metadata() else {
                    return false;
                };
                let Ok(ctime) = meta.created() else {
                    return false;
                };
                range_matches_ts(range, system_time_to_timestamp(ctime))
            }

            Self::Accessed(range) => {
                let Ok(meta) = entry.metadata() else {
                    return false;
                };
                let Ok(atime) = meta.accessed() else {
                    return false;
                };
                range_matches_ts(range, system_time_to_timestamp(atime))
            }

            Self::Type(ft) => {
                let ftype = entry.file_type();
                match ft {
                    FileType::File => ftype.is_file(),
                    FileType::Dir => ftype.is_dir(),
                    FileType::Symlink => ftype.is_symlink(),
                    FileType::Empty => {
                        ftype.is_file() && entry.metadata().map(|m| m.len() == 0).unwrap_or(false)
                    }
                    FileType::Executable | FileType::Binary | FileType::Text => true,
                }
            }

            Self::PermOctal(expected) => {
                #[cfg(unix)]
                {
                    let Ok(meta) = entry.metadata() else {
                        return false;
                    };
                    (meta.permissions().mode() & 0o7777) == *expected
                }
                #[cfg(not(unix))]
                {
                    let _ = expected;
                    true
                }
            }
            Self::PermFlag(mask) => {
                #[cfg(unix)]
                {
                    let Ok(meta) = entry.metadata() else {
                        return false;
                    };
                    (meta.permissions().mode() & mask) != 0
                }
                #[cfg(not(unix))]
                {
                    let _ = mask;
                    true
                }
            }

            Self::Owner(cm) => {
                #[cfg(unix)]
                {
                    let Ok(meta) = entry.metadata() else {
                        return false;
                    };
                    let uid = meta.uid();
                    // Match against numeric UID or resolved username; apply negation once.
                    let uid_str = uid.to_string();
                    let found_uid = cm.find_raw(uid_str.as_bytes());
                    let found_name = get_user_by_uid(uid)
                        .map(|u| cm.find_raw(u.name().as_encoded_bytes()))
                        .unwrap_or(false);
                    let found = found_uid || found_name;
                    if cm.negated {
                        !found
                    } else {
                        found
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = cm;
                    true
                }
            }

            Self::Group(cm) => {
                #[cfg(unix)]
                {
                    let Ok(meta) = entry.metadata() else {
                        return false;
                    };
                    let gid = meta.gid();
                    let gid_str = gid.to_string();
                    let found_gid = cm.find_raw(gid_str.as_bytes());
                    let found_name = get_group_by_gid(gid)
                        .map(|g| cm.find_raw(g.name().as_encoded_bytes()))
                        .unwrap_or(false);
                    let found = found_gid || found_name;
                    if cm.negated {
                        !found
                    } else {
                        found
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = cm;
                    true
                }
            }

            Self::And(children) => children.iter().all(|c| c.matches_inner(entry)),
            Self::Or(children) => children.iter().any(|c| c.matches_inner(entry)),
            Self::Not(inner) => {
                let inner_result = inner.matches_inner(entry);
                trace!(
                    "  Not(inner) on '{}': inner={} => {}",
                    entry.path().display(),
                    inner_result,
                    !inner_result
                );
                !inner_result
            }
        }
    }
}

impl CompiledWalkFilter {
    /// Build a `CompiledWalkFilter` from a `Filter` subtree.
    ///
    /// Should be called on `Query::walk_filter`.  Content-tier leaves that
    /// appear here are compiled to pass-through (always true) — they will be
    /// re-evaluated by the content-tier filter.
    pub fn from_filter(f: &Filter) -> Self {
        Self(FilterNode::from_filter(f, AcCase::Sensitive))
    }

    /// Same as [`from_filter`] but honours `CaseSensitivity` for the given
    /// representative pattern bytes (used by the walk engine).
    pub fn from_filter_case(f: &Filter, case: &CaseSensitivity, pattern: &[u8]) -> Self {
        let ac_case = resolve_case(case, pattern.iter().copied());
        Self(FilterNode::from_filter(f, ac_case))
    }

    /// Evaluate the compiled filter against a [`walkdir::DirEntry`].
    ///
    /// Returns `true` if the entry satisfies the filter.
    pub fn matches(&self, entry: &walkdir::DirEntry) -> bool {
        self.0.matches(entry)
    }

    /// Returns `false` if a Path/Name predicate definitively excludes this
    /// directory — used to prune before descent.  File-only leaves never
    /// cause a directory to be pruned.
    pub(crate) fn dir_passes(&self, entry: &walkdir::DirEntry) -> bool {
        self.0.dir_passes(entry)
    }
}

// ── Range helpers ─────────────────────────────────────────────────────────────

fn range_matches_u64(range: &RangeExpr<u64>, value: u64) -> bool {
    match range {
        RangeExpr::Gt(n) => value > *n,
        RangeExpr::Lt(n) => value < *n,
        RangeExpr::Eq(n) => value == *n,
        RangeExpr::Between(lo, hi) => value > *lo && value < *hi,
    }
}

fn range_matches_ts(range: &RangeExpr<Timestamp>, value: Timestamp) -> bool {
    match range {
        RangeExpr::Gt(t) => value > *t,
        RangeExpr::Lt(t) => value < *t,
        RangeExpr::Eq(t) => value == *t,
        RangeExpr::Between(lo, hi) => value > *lo && value < *hi,
    }
}

/// Convert a [`std::time::SystemTime`] to a [`Timestamp`], saturating at zero for pre-epoch times.
fn system_time_to_timestamp(st: std::time::SystemTime) -> Timestamp {
    let nanos = st
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Timestamp(nanos.min(u64::MAX as u128) as u64)
}

/// Resolve `CaseSensitivity::Smart` against pattern bytes.
/// Returns `Insensitive` when every byte is lowercase or non-alpha, `Sensitive` otherwise.
fn resolve_case(case: &CaseSensitivity, bytes: impl IntoIterator<Item = u8>) -> AcCase {
    match case {
        CaseSensitivity::Sensitive => AcCase::Sensitive,
        CaseSensitivity::Insensitive => AcCase::Insensitive,
        CaseSensitivity::Smart => {
            if bytes.into_iter().all(|b| !b.is_ascii_uppercase()) {
                AcCase::Insensitive
            } else {
                AcCase::Sensitive
            }
        }
    }
}

struct SubstringPattern {
    negated: bool,
    /// `Some` for `Line` patterns (per-line memmem finder); `None` for `Content`.
    line_finder: Option<memmem::Finder<'static>>,
}

enum ContentNode {
    Any,
    /// Walk-tier leaf already evaluated; always passes here.
    WalkPassthrough,
    Content(usize), // index into SubstringPattern table
    Line(usize),    // index into SubstringPattern table
    /// MIME type check via `infer` magic-byte detection.
    Mime(CompiledMatch),
    /// Not yet implemented; always passes.
    Meta(Box<MetaFilter>),
    /// `Executable` checks the execute bit; `Binary`/`Text` use `infer`.
    ContentType(FileType),
    Name(CompiledMatch),
    Path(CompiledMatch),
    And(Vec<ContentNode>),
    Or(Vec<ContentNode>),
    Not(Box<ContentNode>),
}

/// Pre-compiled content-tier filter.
///
/// All `Content` and `Line` patterns are collected into a single `AhoCorasick`
/// automaton so the whole set is scanned in one pass over the extracted bytes.
pub struct CompiledContentFilter {
    /// `None` when there are no substring patterns (only Mime / Meta / Type).
    ac: Option<AhoCorasick>,
    patterns: Vec<SubstringPattern>,
    root: ContentNode,
    insensitive: bool,
}

impl CompiledContentFilter {
    pub fn from_filter_opts(f: &Filter, opts: &SearchOptions) -> Self {
        let mut patterns: Vec<SubstringPattern> = Vec::new();
        let mut ac_values: Vec<Box<[u8]>> = Vec::new();
        let root = build_content_node(f, &mut patterns, &mut ac_values, opts);

        let ac_case = resolve_case(&opts.case, ac_values.iter().flat_map(|v| v.iter()).copied());

        // MatchKind::Standard reports all overlapping matches; LeftmostFirst
        // would silently skip patterns sharing a start position.
        let ac_patterns: Vec<&[u8]> = ac_values.iter().map(|v| v.as_ref()).collect();
        let ac = if ac_patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .ascii_case_insensitive(ac_case == AcCase::Insensitive)
                    .match_kind(MatchKind::Standard) // must be Standard for overlapping
                    .build(ac_patterns)
                    .expect("AhoCorasick build never fails for non-empty pattern list"),
            )
        };

        Self {
            ac,
            patterns,
            root,
            insensitive: ac_case == AcCase::Insensitive,
        }
    }

    /// Build with default settings. Convenience for tests.
    pub fn from_filter(f: &Filter) -> Self {
        Self::from_filter_opts(f, &SearchOptions::default())
    }

    /// `true` when the filter is `Filter::Any` — no file content needs to be read.
    pub fn is_trivially_true(&self) -> bool {
        matches!(self.root, ContentNode::Any)
    }

    /// Returns `(matched, hit_count)` for the extracted `content` of `path`.
    ///
    /// `scratch` is reused across calls to avoid a per-file allocation.
    pub fn matches(&self, path: &Path, content: &[u8], scratch: &mut Vec<bool>) -> (bool, usize) {
        let n = self.patterns.len();
        scratch.clear();
        scratch.resize(n, false);
        let mut hit_count: usize = 0;
        if let Some(ref ac) = self.ac {
            for m in ac.find_overlapping_iter(content) {
                scratch[m.pattern().as_usize()] = true;
                hit_count += 1;
            }
        }
        let ac_hits = scratch.as_slice();

        if log::log_enabled!(log::Level::Debug) {
            debug!(
                "content scan '{}': {} bytes, {} patterns, ac_hits={:?}, hit_count={}",
                path.display(),
                content.len(),
                n,
                ac_hits,
                hit_count,
            );
        }

        let result = eval_content_node(
            &self.root,
            &self.patterns,
            content,
            ac_hits,
            path,
            self.insensitive,
        );
        debug!(
            "content filter '{}' => {}",
            path.display(),
            if result { "MATCH" } else { "no match" }
        );
        (result, if result { hit_count } else { 0 })
    }
}

#[allow(clippy::only_used_in_recursion)]
fn build_content_node(
    f: &Filter,
    patterns: &mut Vec<SubstringPattern>,
    ac_values: &mut Vec<Box<[u8]>>,
    opts: &SearchOptions,
) -> ContentNode {
    match f {
        Filter::Any => ContentNode::Any,

        Filter::Content(expr) => {
            let idx = patterns.len();
            patterns.push(SubstringPattern {
                negated: expr.negated,
                line_finder: None,
            });
            ac_values.push(expr.value.clone());
            ContentNode::Content(idx)
        }
        Filter::Line(expr) => {
            let idx = patterns.len();
            let ac_case = resolve_case(&opts.case, expr.value.iter().copied());
            let needle: Box<[u8]> = if ac_case == AcCase::Insensitive {
                expr.value.iter().map(|b| b.to_ascii_lowercase()).collect()
            } else {
                expr.value.clone()
            };
            let line_finder = Some(memmem::Finder::new(&needle).into_owned());
            patterns.push(SubstringPattern {
                negated: expr.negated,
                line_finder,
            });
            ac_values.push(expr.value.clone());
            ContentNode::Line(idx)
        }
        // MIME strings are lowercase by spec; always use Sensitive.
        Filter::Mime(expr) => ContentNode::Mime(CompiledMatch::from_expr(expr, AcCase::Sensitive)),
        Filter::Meta(mf) => ContentNode::Meta(Box::new(mf.clone())),

        Filter::Type(ft @ (FileType::Executable | FileType::Binary | FileType::Text)) => {
            ContentNode::ContentType(ft.clone())
        }

        Filter::Name(expr) => {
            let case = resolve_case(&opts.case, expr.value.iter().copied());
            ContentNode::Name(CompiledMatch::from_expr(expr, case))
        }
        Filter::Path(expr) => {
            let case = resolve_case(&opts.case, expr.value.iter().copied());
            ContentNode::Path(CompiledMatch::from_expr(expr, case))
        }

        Filter::Hidden(_)
        | Filter::Ext(_)
        | Filter::Size(_)
        | Filter::Modified(_)
        | Filter::Created(_)
        | Filter::Accessed(_)
        | Filter::Type(_)
        | Filter::PermOctal(_)
        | Filter::PermFlag(_)
        | Filter::Owner(_)
        | Filter::Group(_) => ContentNode::WalkPassthrough,

        Filter::And(children) => {
            let nodes = children
                .iter()
                .map(|c| build_content_node(c, patterns, ac_values, opts))
                .collect();
            ContentNode::And(nodes)
        }
        Filter::Or(children) => {
            let nodes = children
                .iter()
                .map(|c| build_content_node(c, patterns, ac_values, opts))
                .collect();
            ContentNode::Or(nodes)
        }
        Filter::Not(inner) => ContentNode::Not(Box::new(build_content_node(
            inner, patterns, ac_values, opts,
        ))),
    }
}

/// Lowercase `bytes` into a fixed 512-byte stack buffer (no heap allocation
/// for the common case) and pass the result to `f`.  Inputs longer than 512
/// bytes fall back to a `Vec` allocation.
#[inline]
fn with_lowercase<R>(bytes: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
    // 512 bytes covers the vast majority of file names and source-code lines
    // with zero heap allocation; only unusually long inputs allocate.
    if bytes.len() <= 512 {
        let mut buf = [0u8; 512];
        let lower = &mut buf[..bytes.len()];
        for (dst, &src) in lower.iter_mut().zip(bytes.iter()) {
            *dst = src.to_ascii_lowercase();
        }
        f(lower)
    } else {
        let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
        f(&lower)
    }
}

fn line_contains(finder: &memmem::Finder<'_>, line: &[u8], insensitive: bool) -> bool {
    if insensitive {
        with_lowercase(line, |lower| finder.find(lower).is_some())
    } else {
        finder.find(line).is_some()
    }
}

/// Returns whether any line in `content` satisfies the `Line` pattern.
fn line_pattern_hits(pat: &SubstringPattern, content: &[u8], insensitive: bool) -> bool {
    let finder = match &pat.line_finder {
        Some(f) => f,
        None => return pat.negated,
    };
    let found = content
        .split(|&b| b == b'\n')
        .any(|line| line_contains(finder, line, insensitive));
    if pat.negated {
        !found
    } else {
        found
    }
}

/// Returns `true` if some line in `content` satisfies all `Line` patterns simultaneously.
/// Used for `l:foo l:bar` — both must appear on the same line.
fn any_line_has_all(
    indices: &[usize],
    patterns: &[SubstringPattern],
    content: &[u8],
    insensitive: bool,
) -> bool {
    content.split(|&b| b == b'\n').any(|line| {
        indices.iter().all(|&idx| {
            let pat = &patterns[idx];
            let finder = match &pat.line_finder {
                Some(f) => f,
                None => return false,
            };
            let found = line_contains(finder, line, insensitive);
            if pat.negated {
                !found
            } else {
                found
            }
        })
    })
}

fn eval_content_node(
    node: &ContentNode,
    patterns: &[SubstringPattern],
    content: &[u8],
    ac_hits: &[bool],
    path: &Path,
    insensitive: bool,
) -> bool {
    match node {
        ContentNode::Any | ContentNode::WalkPassthrough => true,

        ContentNode::Name(cm) => {
            let name = path
                .file_name()
                .map(|n| n.as_encoded_bytes())
                .unwrap_or_default();
            cm.matches_bytes(name)
        }
        ContentNode::Path(cm) => {
            let p = path.as_os_str().as_encoded_bytes();
            cm.matches_bytes(p)
        }

        ContentNode::Content(idx) => {
            let pat = &patterns[*idx];
            let found = ac_hits[*idx];
            if pat.negated {
                !found
            } else {
                found
            }
        }

        ContentNode::Line(idx) => line_pattern_hits(&patterns[*idx], content, insensitive),

        ContentNode::Mime(cm) => {
            let mime = infer::get(content)
                .map(|t| t.mime_type().as_bytes())
                .unwrap_or(b"application/octet-stream");
            cm.matches_bytes(mime)
        }

        ContentNode::ContentType(ft) => {
            match ft {
                FileType::Executable => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        path.metadata()
                            .map(|m| m.permissions().mode() & 0o111 != 0)
                            .unwrap_or(false)
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = path;
                        true
                    }
                }
                FileType::Binary => infer::get(content).is_some(),
                // `infer` returns None for plain text (no magic signature).
                FileType::Text => infer::get(content).is_none(),
                _ => true,
            }
        }

        ContentNode::Meta(_mf) => true, // not yet implemented

        ContentNode::And(children) => {
            // When all children are Line nodes, require them on the same line.
            let all_line_indices: Option<Vec<usize>> = children
                .iter()
                .map(|c| {
                    if let ContentNode::Line(idx) = c {
                        Some(*idx)
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(indices) = all_line_indices {
                return any_line_has_all(&indices, patterns, content, insensitive);
            }
            children
                .iter()
                .all(|c| eval_content_node(c, patterns, content, ac_hits, path, insensitive))
        }
        ContentNode::Or(children) => children
            .iter()
            .any(|c| eval_content_node(c, patterns, content, ac_hits, path, insensitive)),
        ContentNode::Not(inner) => {
            !eval_content_node(inner, patterns, content, ac_hits, path, insensitive)
        }
    }
}

use crate::error::SxError;
use crate::extract::extractor_for;
use crate::query::Query;
use atomic_enum::atomic_enum;
use crossbeam_deque::{Injector, Steal};
use futures_core::Stream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

/// Directories skipped unconditionally when `HiddenMode::Exclude` is active.
static ARTIFACT_DIRS_UNCONDITIONAL: &[&str] = &[
    "node_modules",
    "__pycache__",
    "cmake-build-debug",
    "cmake-build-release",
    "DerivedData",
    "_build",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Directories skipped only when a marker file exists in the parent directory.
/// Each entry is `(dir_name, &[marker_files])`; an empty marker list means always skip.
static ARTIFACT_DIRS_CONDITIONAL: &[(&str, &[&str])] = &[
    ("target", &["Cargo.toml", "Cargo.lock"]), // Rust
    ("dist", &["package.json"]),               // Node/JS/TS
    ("venv", &["requirements.txt", "pyproject.toml", "setup.py"]), // Python venv
    ("env", &["requirements.txt", "pyproject.toml", "setup.py"]), // Python venv
    (
        "build",
        &["build.gradle", "build.gradle.kts", "pom.xml", "gradlew"],
    ), // Java/Android
    ("out", &["build.gradle", "build.gradle.kts", "pom.xml"]), // Java/Android
    ("classes", &["build.gradle", "build.gradle.kts", "pom.xml"]), // Java/Android
    ("vendor", &["go.mod"]),                   // Go
    ("bin", &[".csproj", ".fsproj", ".sln"]),  // .NET
    ("obj", &[".csproj", ".fsproj"]),          // .NET
    ("deps", &["mix.exs"]),                    // Elixir/Erlang
    ("tmp", &[]),                              // always skip
];

fn is_artifact_dir(entry: &walkdir::DirEntry) -> bool {
    let name = match std::str::from_utf8(entry.file_name().as_encoded_bytes()) {
        Ok(s) => s,
        Err(_) => return false,
    };

    if ARTIFACT_DIRS_UNCONDITIONAL.contains(&name) {
        return true;
    }

    for &(dir, markers) in ARTIFACT_DIRS_CONDITIONAL {
        if name == dir {
            if markers.is_empty() {
                return true;
            }
            if let Some(parent) = entry.path().parent() {
                for &marker in markers {
                    if marker.starts_with('.') {
                        // Extension marker (e.g. ".csproj"): look for any sibling file
                        // with that extension, comparing directly without allocating.
                        if let Ok(rd) = std::fs::read_dir(parent) {
                            for sibling in rd.flatten() {
                                if sibling
                                    .path()
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .is_some_and(|e| e == &marker[1..])
                                {
                                    return true;
                                }
                            }
                        }
                    } else if parent.join(marker).exists() {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// A file that satisfied the query.
#[derive(Debug, Clone)]
pub struct Match {
    pub path: PathBuf,
    /// `Err` when extraction failed but the path passed walk filters — surfaced
    /// so callers can report it rather than silently dropping the entry.
    pub extracted: Result<(), SxError>,
    /// Relevance score in `[0.0, 1.0)` derived from `n / (n + 1)` where `n` is
    /// the raw AhoCorasick hit count.  `0.0` for walk-only matches.
    pub score: f64,
    pub content: Option<Vec<u8>>,
}

#[inline]
fn normalise_score(raw: usize) -> f64 {
    if raw == 0 {
        0.0
    } else {
        raw as f64 / (raw as f64 + 1.0)
    }
}

/// Ready-to-execute form of a [`Query`]. Cheap to clone — all inner state is `Arc`.
///
/// Clone across worker tasks; whichever is polled first becomes the walker.
/// A `CompiledQuery` is single-use; call [`Query::compile_with_registry`] for a new search.
#[derive(Clone)]
pub struct CompiledQuery {
    pub roots: Vec<PathBuf>,
    pub options: SearchOptions,
    pub walk: Arc<CompiledWalkFilter>,
    /// `None` when `content_filter` was `Filter::Any` — workers skip extraction.
    pub content: Option<Arc<CompiledContentFilter>>,
    hidden_mode: HiddenMode,
    work: Arc<WorkState>,
    pub handler_tokens: Vec<crate::query::HandlerToken>,
    /// `None` when compiled without a registry (falls back to built-in extractors).
    ///
    /// # Drop-order invariant
    /// `file_handlers` MUST be declared before `registry`. Rust drops struct
    /// fields in declaration order, so this ensures every `Arc<dyn SxHandler>`
    /// clone held here is released *before* the registry drops its
    /// `owned_plugins` (which calls `dlclose`). Dropping `registry` first would
    /// unload the `.so` while `StabbyHandlerWrapper::drop` still needs to call
    /// through the plugin vtable → SIGSEGV.
    file_handlers: Option<Arc<Vec<Arc<dyn sx_plugin_api::SxHandler>>>>,
    pub registry: Option<Arc<crate::handler_registry::HandlerRegistry>>,
    /// `false` for pure handler queries (e.g. `"calc:pi^2"`) — skips the file walk.
    has_file_terms: bool,
}

/// Phase of the walker within one [`CompiledQuery::run`] call-group.
///
/// Transitions are one-way: `Unclaimed → Walking → Done`.
///
/// [`#[atomic_enum]`](atomic_enum::atomic_enum) generates `AtomicWalkPhase`,
/// replacing the old `walker_claimed: AtomicBool` + `walking_done: AtomicBool`
/// pair with a single typed atomic field.  All `as u8` casts and the manual
/// `WalkPhase::load()` helper are gone.
#[atomic_enum]
#[derive(PartialEq)]
enum WalkPhase {
    /// No worker has claimed the walker role yet.
    Unclaimed = 0,
    /// One worker won the election and is actively pushing paths.
    Walking = 1,
    /// The walker finished (or the query has no file terms).
    Done = 2,
}

struct WorkState {
    injector: Injector<PathBuf>,
    /// Counts paths pushed into `injector` but not yet stolen by a worker.
    /// Incremented before `injector.push`, decremented immediately on a
    /// successful `steal`.  The termination condition
    /// `walk_phase == Done && in_flight == 0` is only sound because the
    /// increment always precedes the push.
    in_flight: AtomicUsize,
    /// Combined walker-election + completion flag.
    ///
    /// Replaces the old `walker_claimed: AtomicBool` + `walking_done: AtomicBool`
    /// pair: those two fields were always two halves of the same one-way state
    /// machine, so one typed `AtomicWalkPhase` (generated by `#[atomic_enum]`)
    /// is clearer and requires no manual `as u8` casts.
    walk_phase: AtomicWalkPhase,
    /// Drives cooperative cancellation for all workers on this query.
    ///
    /// Replaces the old `cancelled: AtomicBool`:
    /// - Workers wait in a `tokio::select!` that races `work_available`
    ///   against `token.cancelled()`, so no manual `notify_waiters()` is
    ///   needed on cancellation.
    /// - Outer tasks wrap their stream-consuming loop with
    ///   `token.run_until_cancelled(...)`, removing explicit `is_cancelled()`
    ///   poll-checks from every loop iteration.
    token: CancellationToken,
    /// Notified when a path is pushed to `injector` or when walking completes.
    work_available: Notify,
}

impl WorkState {
    fn new() -> Self {
        Self {
            injector: Injector::new(),
            in_flight: AtomicUsize::new(0),
            walk_phase: AtomicWalkPhase::new(WalkPhase::Unclaimed),
            token: CancellationToken::new(),
            work_available: Notify::new(),
        }
    }
}

impl CompiledQuery {
    /// Returns a lightweight clone of this query's [`CancellationToken`].
    ///
    /// Callers should wrap it in a [`DropGuard`] (`token().drop_guard()`) so
    /// that cancellation is driven by ownership rather than explicit calls.
    /// Pass individual clones to `spawn_blocking` handler tasks so they can
    /// check [`CancellationToken::is_cancelled`] without holding a full
    /// `CompiledQuery` clone.
    ///
    /// [`DropGuard`]: tokio_util::sync::DropGuard
    pub fn token(&self) -> CancellationToken {
        self.work.token.clone()
    }

    fn file_matches_with_handles(
        &self,
        path: &Path,
        handles: &mut Option<Vec<sx_plugin_api::SxExtractorHandle>>,
        scratch: &mut Vec<bool>,
    ) -> Option<Match> {
        let ccf = match &self.content {
            None => {
                debug!(
                    "file_matches '{}': no content filter => MATCH",
                    path.display()
                );
                return Some(Match {
                    path: path.to_path_buf(),
                    extracted: Ok(()),
                    score: 0.0,
                    content: None,
                });
            }
            Some(ccf) => ccf,
        };

        if let Some(ref mut hdls) = handles {
            let path_bytes = path.as_os_str().as_encoded_bytes();

            for handle in hdls.iter_mut() {
                let output = handle.extract(path_bytes);
                let decision: Option<Option<Match>> = output.match_owned(
                    |sx_bytes| {
                        // Probe extracted bytes, not the raw file: document plugins
                        // extract null-free text from containers (PDF/DOCX/ZIP) that
                        // themselves contain null bytes.
                        if !self.options.include_binary_content
                            && sx_bytes.as_bytes().contains(&b'\0')
                        {
                            debug!("file_matches '{}': binary content => SKIP", path.display());
                            return Some(None);
                        }
                        let (matched, raw) = ccf.matches(path, sx_bytes.as_bytes(), scratch);
                        if matched {
                            let score = normalise_score(raw);
                            debug!("file_matches '{}': plugin content matched (raw={} score={:.4}) => MATCH", path.display(), raw, score);
                            Some(Some(Match {
                                path: path.to_path_buf(),
                                extracted: Ok(()),
                                score,
                                content: None,
                            }))
                        } else {
                            debug!("file_matches '{}': plugin content did not match => SKIP", path.display());
                            Some(None)
                        }
                    },
                    // `None` means this handler did not claim the file; fall through to the next.
                    |_skip| None,
                    |msg| {
                        let err_str = msg.into_string();
                        debug!("file_matches '{}': plugin extraction error: {}", path.display(), err_str);
                        Some(Some(Match {
                            path: path.to_path_buf(),
                            extracted: Err(SxError::io(std::io::Error::other(err_str))),
                            score: 0.0,
                            content: None,
                        }))
                    },
                );
                if let Some(r) = decision {
                    return r;
                }
            }
            debug!(
                "file_matches '{}': no plugin claimed file => SKIP",
                path.display()
            );
            return None;
        }

        let extractor = match extractor_for(path) {
            Some(e) => e,
            None => {
                debug!(
                    "file_matches '{}': no extractor (no extension) => SKIP",
                    path.display()
                );
                return None;
            }
        };

        let raw_bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                debug!(
                    "file_matches '{}': read error: {} => reported as error match",
                    path.display(),
                    e
                );
                return Some(Match {
                    path: path.to_path_buf(),
                    extracted: Err(SxError::from(e)),
                    score: 0.0,
                    content: None,
                });
            }
        };

        if !self.options.include_binary_content
            && !extractor.raw_is_binary_container()
            && raw_bytes[..raw_bytes.len().min(4096)].contains(&b'\0')
        {
            debug!("file_matches '{}': binary => SKIP", path.display());
            return None;
        }

        match extractor.extract_from_bytes(raw_bytes) {
            Ok(bytes) => {
                let (matched, raw) = ccf.matches(path, &bytes, scratch);
                if matched {
                    let score = normalise_score(raw);
                    debug!(
                        "file_matches '{}': content matched (raw={} score={:.4}) => MATCH",
                        path.display(),
                        raw,
                        score
                    );
                    Some(Match {
                        path: path.to_path_buf(),
                        extracted: Ok(()),
                        score,
                        content: None,
                    })
                } else {
                    debug!(
                        "file_matches '{}': content did not match => SKIP",
                        path.display()
                    );
                    None
                }
            }
            Err(e) => {
                debug!(
                    "file_matches '{}': extraction error: {} => reported as error match",
                    path.display(),
                    e
                );
                Some(Match {
                    path: path.to_path_buf(),
                    extracted: Err(SxError::from(e)),
                    score: 0.0,
                    content: None,
                })
            }
        }
    }

    /// Returns an async stream of [`Match`] values, yielded as they are found.
    pub fn run(&self) -> std::pin::Pin<Box<dyn Stream<Item = Match> + Send + '_>> {
        Box::pin(async_stream::stream! {
            let mut handles: Option<Vec<sx_plugin_api::SxExtractorHandle>> =
                self.file_handlers.as_ref().map(|fh| {
                    fh.iter()
                        .filter_map(|h| {
                            h.prepare_extract().match_owned(
                                Some,
                                || None,
                            )
                        })
                        .collect()
                });
            let mut scratch: Vec<bool> = Vec::new();

            if !self.has_file_terms {
                // No file walk — jump straight to Done so content workers can
                // observe the termination condition immediately.
                self.work
                    .walk_phase
                    .store(WalkPhase::Done, Ordering::Release);
                self.work.work_available.notify_waiters();
            }

            // Walker election: first worker to CAS Unclaimed→Walking wins.
            let is_walker = self
                .work
                .walk_phase
                .compare_exchange(
                    WalkPhase::Unclaimed,
                    WalkPhase::Walking,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok();

            if is_walker && self.has_file_terms {
                // Transitions walk_phase to Done even if the stream is dropped
                // mid-walk (cancellation, channel close, etc.).
                struct WalkingDoneGuard<'a>(&'a AtomicWalkPhase, &'a Notify);
                impl Drop for WalkingDoneGuard<'_> {
                    fn drop(&mut self) {
                        self.0.store(WalkPhase::Done, Ordering::Release);
                        self.1.notify_waiters();
                    }
                }
                let _guard = WalkingDoneGuard(
                    &self.work.walk_phase,
                    &self.work.work_available,
                );

                // Yield cooperatively every N entries so other tasks remain responsive.
                // `block_in_place` is intentionally avoided: it panics on current-thread runtimes.
                let yield_interval = 1024usize;
                let mut since_yield = 0usize;

                for root in &self.roots {
                    let walk_filter = self.walk.clone();
                    let hidden_mode = self.hidden_mode.clone();
                    let walker = WalkDir::new(root)
                        .follow_links(self.options.follow_symlinks)
                        .max_depth(self.options.depth.unwrap_or(usize::MAX))
                        .into_iter()
                        .filter_entry(move |e| {
                            if e.file_type().is_dir() {
                                // Never prune the root itself — depth 0 is the
                                // search root supplied by the caller, not a
                                // candidate to filter.
                                if e.depth() == 0 {
                                    return true;
                                }
                                if hidden_mode == HiddenMode::Exclude {
                                    let name = e.file_name().as_encoded_bytes();
                                    if name.first() == Some(&b'.') {
                                        return false;
                                    }
                                    if is_artifact_dir(e) {
                                        return false;
                                    }
                                }
                                walk_filter.dir_passes(e)
                            } else {
                                if hidden_mode == HiddenMode::Exclude
                                    && e.file_name().as_encoded_bytes().first() == Some(&b'.')
                                {
                                    return false;
                                }
                                walk_filter.matches(e)
                            }
                        });

                    for entry in walker.filter_map(|e| e.ok()) {
                        if self.work.token.is_cancelled() {
                            break;
                        }
                        if !entry.file_type().is_file() {
                            continue;
                        }
                        self.work.in_flight.fetch_add(1, Ordering::Relaxed);
                        self.work.injector.push(entry.into_path());
                        self.work.work_available.notify_one();

                        since_yield += 1;
                        if since_yield >= yield_interval {
                            since_yield = 0;
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }

            const STEAL_YIELD_INTERVAL: usize = 64;
            let mut steals_since_yield = 0usize;
            loop {
                if self.work.token.is_cancelled() {
                    break;
                }
                match self.work.injector.steal() {
                    Steal::Success(path) => {
                        // Release so the walker's prior `fetch_add` (Relaxed) and
                        // any writes done in this task are visible to the
                        // termination check below.
                        self.work.in_flight.fetch_sub(1, Ordering::Release);
                        let result = tokio::task::block_in_place(|| {
                            self.file_matches_with_handles(&path, &mut handles, &mut scratch)
                        });
                        if let Some(m) = result {
                            yield m;
                        }
                        steals_since_yield += 1;
                        if steals_since_yield >= STEAL_YIELD_INTERVAL {
                            steals_since_yield = 0;
                            tokio::task::yield_now().await;
                        }
                    }
                    Steal::Empty
                        if self.work.walk_phase.load(Ordering::Acquire) == WalkPhase::Done
                            && self.work.in_flight.load(Ordering::Acquire) == 0 =>
                    {
                        break;
                    }
                    Steal::Empty => {
                        // Race new-work notification against cancellation so
                        // workers wake immediately on cancel without a separate
                        // `notify_waiters()` call in `cancel()`.
                        tokio::select! {
                            _ = self.work.work_available.notified() => {}
                            _ = self.work.token.cancelled() => break,
                        }
                    }
                    Steal::Retry => tokio::task::yield_now().await,
                }
            }
        })
    }
}

impl Query {
    /// Compile into the ready-to-execute form, building all automata once.
    pub fn compile(self) -> CompiledQuery {
        self.compile_with_registry(None)
    }

    /// Compile, attaching a [`HandlerRegistry`] for non-file token dispatch.
    pub fn compile_with_registry(
        self,
        registry: Option<Arc<crate::handler_registry::HandlerRegistry>>,
    ) -> CompiledQuery {
        // A bare query (no walk/content filters, no handler tokens) still needs
        // a file walk, so `handler_tokens.is_empty()` forces `has_file_terms`
        // true in that case.
        let has_file_terms = self.walk_filter != Filter::Any
            || self.content_filter != Filter::Any
            || self.handler_tokens.is_empty();

        // Inject an Ext allow-list from handler defaults when no explicit ext: filter was given.
        let walk_filter = if let Some(ref reg) = registry {
            let defaults = reg.default_file_extensions();
            if has_file_terms && !defaults.is_empty() && !filter_has_ext(&self.walk_filter) {
                let ext_list: Vec<Box<[u8]>> = defaults.into_iter().collect();
                let allow = Filter::Ext(ext_list);
                match self.walk_filter {
                    Filter::Any => allow,
                    other => Filter::And(vec![allow, other]),
                }
            } else {
                self.walk_filter
            }
        } else {
            self.walk_filter
        };

        let file_handlers: Option<Arc<Vec<Arc<dyn sx_plugin_api::SxHandler>>>> = registry
            .as_ref()
            .map(|reg| Arc::new(reg.file_handlers().into_iter().collect::<Vec<_>>()));

        let walk = Arc::new(CompiledWalkFilter::from_filter_case(
            &walk_filter,
            &self.options.case,
            &collect_match_bytes(&walk_filter),
        ));

        let content = if self.content_filter == Filter::Any {
            None
        } else {
            Some(Arc::new(CompiledContentFilter::from_filter_opts(
                &self.content_filter,
                &self.options,
            )))
        };

        let hidden_mode = extract_hidden_mode(&walk_filter);

        CompiledQuery {
            roots: self.roots,
            options: self.options,
            walk,
            content,
            hidden_mode,
            work: Arc::new(WorkState::new()),
            handler_tokens: self.handler_tokens,
            registry,
            file_handlers,
            has_file_terms,
        }
    }
}

/// Extract the first `HiddenMode` in the filter tree; defaults to `Exclude`.
fn extract_hidden_mode(f: &Filter) -> HiddenMode {
    match f {
        Filter::Hidden(mode) => mode.clone(),
        Filter::And(children) | Filter::Or(children) => children
            .iter()
            .find_map(|c| match extract_hidden_mode(c) {
                HiddenMode::Exclude => None,
                mode => Some(mode),
            })
            .unwrap_or(HiddenMode::Exclude),
        Filter::Not(inner) => extract_hidden_mode(inner),
        _ => HiddenMode::Exclude,
    }
}

fn filter_has_ext(f: &Filter) -> bool {
    match f {
        Filter::Ext(_) => true,
        Filter::And(children) | Filter::Or(children) => children.iter().any(filter_has_ext),
        // A negated extension filter excludes extensions; it does not positively
        // select one, so it must not suppress the automatic allow-list injection.
        Filter::Not(_) => false,
        _ => false,
    }
}

fn collect_match_bytes(f: &Filter) -> Vec<u8> {
    let mut out = Vec::new();
    collect_match_bytes_inner(f, &mut out);
    out
}

fn collect_match_bytes_inner(f: &Filter, out: &mut Vec<u8>) {
    match f {
        Filter::Name(e) | Filter::Path(e) | Filter::Owner(e) | Filter::Group(e) => {
            out.extend_from_slice(&e.value)
        }
        Filter::And(v) | Filter::Or(v) => v.iter().for_each(|c| collect_match_bytes_inner(c, out)),
        Filter::Not(inner) => collect_match_bytes_inner(inner, out),
        _ => {}
    }
}
