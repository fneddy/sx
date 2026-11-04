//! # `sx-plugin-localsearch`
//!
//! LocalSearch / Tracker3 file-search handler for `sx`.
//!
//! Implements the `localsearch:` namespace. On query it reuses a persistent
//! D-Bus connection to the running `localsearch-3` daemon and issues a
//! full-text search using `fts:match` — the same mechanism that
//! `localsearch search` uses.  Results carry the file URI as `action`, the
//! file name as `title`, and the MIME type as `body`.
//!
//! ## Two prepared statements
//!
//! Both are compiled **once** at construction time:
//!
//! | Statement | Used when | Binds |
//! |-----------|-----------|-------|
//! | `stmt` | No file-system filters in query | `match`, `limit` |
//! | `stmt_filtered` | Any of `e:`, `mime:`, `p:`, `m:`, `s:` in query | above + skip flags + values |
//!
//! The fast path is taken for plain text queries; the filtered path is taken
//! when `extract_sparql_filters` detects at least one file-system constraint in
//! `ctx.raw_query`.
//!
//! ## SPARQL filter strategy
//!
//! `stmt_filtered` uses boolean `~skip_*` parameters for every constraint so a
//! single compiled plan covers all filter combinations.  SPARQL's `||`
//! short-circuits, so `FILTER(true || REGEX(...))` never evaluates the REGEX.
//! OPTIONALs for `?mtime` and `?fsize` add one extra B-tree lookup per row on
//! the same `nfo:FileDataObject` already joined for the URL — negligible cost.

use std::time::Instant;
use sx_parser::{from_str_lenient, Filter, RangeExpr, Timestamp};
use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxMetaEntry, SxNamespacePolicy, SxQueryContext, SxResult,
    SxResultSender,
};
use tracker::prelude::SparqlCursorExtManual;

// ── D-Bus service name ────────────────────────────────────────────────────────

const LOCALSEARCH_BUS: &str = "org.freedesktop.LocalSearch3";

// ── SPARQL prepared-statement templates ──────────────────────────────────────

/// Fast path — only `~match` and `~limit` are bound.
const SPARQL_TEMPLATE: &str = "\
    SELECT ?url ?mime \
    WHERE { \
      { \
        SELECT ?r ?mime WHERE { \
          GRAPH tracker:Documents { \
            ?r fts:match ~match . \
            OPTIONAL { ?r nie:mimeType ?mime } \
          } \
        } \
        ORDER BY DESC(fts:rank(?r)) \
        LIMIT ~limit \
      } \
      ?r nie:isStoredAs ?fo . \
      ?fo nie:url ?url . \
    }";

/// Filtered path — FTS subquery plus OPTIONAL metadata joins and seven boolean
/// skip-flag/value parameter pairs (one per supported file-system filter).
///
/// SPARQL `||` short-circuits so a `true` skip-flag never evaluates the
/// expensive operand.  OPTIONALs for `?mtime` / `?fsize` are cheap B-tree
/// lookups on the already-joined `nfo:FileDataObject`.
const SPARQL_FILTERED_TEMPLATE: &str = "\
    SELECT ?url ?mime \
    WHERE { \
      { \
        SELECT ?r ?mime WHERE { \
          GRAPH tracker:Documents { \
            ?r fts:match ~match . \
            OPTIONAL { ?r nie:mimeType ?mime } \
          } \
        } \
        ORDER BY DESC(fts:rank(?r)) \
        LIMIT ~limit \
      } \
      ?r nie:isStoredAs ?fo . \
      GRAPH tracker:FileSystem { \
        ?fo nie:url ?url . \
        OPTIONAL { ?fo nfo:fileLastModified ?mtime } \
        OPTIONAL { ?fo nfo:fileSize ?fsize } \
      } \
      FILTER(~skip_ext || REGEX(LCASE(STR(?url)), ~ext_pattern)) \
      FILTER(~skip_mime || CONTAINS(LCASE(STR(COALESCE(?mime, \"\"))), ~mime_lc)) \
      FILTER(~skip_path || CONTAINS(LCASE(STR(?url)), ~path_lc)) \
      FILTER(~skip_mtime_after  || !BOUND(?mtime) || ?mtime >= ~mtime_after) \
      FILTER(~skip_mtime_before || !BOUND(?mtime) || ?mtime <= ~mtime_before) \
      FILTER(~skip_size_min || !BOUND(?fsize) || ?fsize >= ~size_min) \
      FILTER(~skip_size_max || !BOUND(?fsize) || ?fsize <= ~size_max) \
    }";

// ── SPARQL filter binding values ──────────────────────────────────────────────

/// File-system constraints ready for SPARQL binding.
///
/// Built from `ctx.raw_query` by [`extract_sparql_filters`] using the
/// canonical `sx-parser` Filter AST — no hand-rolled parsing.
#[derive(Default)]
struct SpqFilters {
    /// Lowercased extensions without the dot (e.g. `["pdf", "docx"]`).
    extensions: Vec<String>,
    /// MIME substring (e.g. `"image/"`).
    mime_contains: String,
    /// Path substring (e.g. `"/home/user/documents"`).
    path_contains: String,
    /// Modified-after: Unix seconds. `0` = unconstrained.
    mtime_after_secs: i64,
    /// Modified-before: Unix seconds. `0` = unconstrained.
    mtime_before_secs: i64,
    /// Minimum size in bytes. `0` = unconstrained.
    size_min_bytes: i64,
    /// Maximum size in bytes. `i64::MAX` = unconstrained.
    size_max_bytes: i64,
}

impl SpqFilters {
    fn is_any_active(&self) -> bool {
        !self.extensions.is_empty()
            || !self.mime_contains.is_empty()
            || !self.path_contains.is_empty()
            || self.mtime_after_secs != 0
            || self.mtime_before_secs != 0
            || self.size_min_bytes != 0
            || self.size_max_bytes != i64::MAX
    }

    fn ext_regex_pattern(&self) -> String {
        format!("\\.({})$", self.extensions.join("|"))
    }
}

// ── Filter extraction from real AST ──────────────────────────────────────────

/// Parse `raw` leniently via `sx-parser` and walk the `walk_filter` tree to
/// extract the file-system constraints this plugin can honour in SPARQL.
///
/// Unknown tokens and syntax errors are silently ignored (lenient mode).
/// Negated match expressions are skipped — SPARQL NOT is not yet supported.
fn extract_sparql_filters(raw: &str) -> SpqFilters {
    let mut f = SpqFilters {
        size_max_bytes: i64::MAX,
        ..Default::default()
    };
    let query = from_str_lenient(raw);
    collect_filter(&query.walk_filter, &mut f);
    f
}

fn collect_filter(filter: &Filter, f: &mut SpqFilters) {
    match filter {
        Filter::Ext(exts) => {
            for ext in exts {
                let s = String::from_utf8_lossy(ext).to_lowercase();
                if !f.extensions.contains(&s) {
                    f.extensions.push(s);
                }
            }
        }
        Filter::Mime(expr) if !expr.negated => {
            if f.mime_contains.is_empty() {
                f.mime_contains = String::from_utf8_lossy(&expr.value).to_lowercase();
            }
        }
        Filter::Path(expr) if !expr.negated => {
            if f.path_contains.is_empty() {
                f.path_contains = String::from_utf8_lossy(&expr.value).into_owned();
            }
        }
        // Modified, Created, Accessed all map to mtime in the SPARQL model.
        Filter::Modified(range) | Filter::Created(range) | Filter::Accessed(range) => {
            apply_mtime(range, f);
        }
        Filter::Size(range) => apply_size(range, f),
        Filter::And(children) => {
            for child in children {
                collect_filter(child, f);
            }
        }
        // Not, Or, Name, Content, Line, Hidden, Type, PermOctal, PermFlag,
        // Owner, Group, Meta, Any — nothing to extract for SPARQL binding.
        _ => {}
    }
}

/// Map a `RangeExpr<Timestamp>` onto the `mtime_after_secs` / `mtime_before_secs`
/// SPARQL bounds.  `Timestamp.0` is nanoseconds since the Unix epoch.
fn apply_mtime(range: &RangeExpr<Timestamp>, f: &mut SpqFilters) {
    let to_secs = |ts: &Timestamp| (ts.0 / 1_000_000_000) as i64;
    match range {
        RangeExpr::Gt(ts) => f.mtime_after_secs = to_secs(ts),
        RangeExpr::Lt(ts) => f.mtime_before_secs = to_secs(ts),
        RangeExpr::Eq(ts) => {
            f.mtime_after_secs = to_secs(ts);
            f.mtime_before_secs = to_secs(ts);
        }
        RangeExpr::Between(lo, hi) => {
            f.mtime_after_secs = to_secs(lo);
            f.mtime_before_secs = to_secs(hi);
        }
    }
}

/// Map a `RangeExpr<u64>` (bytes) onto the `size_min_bytes` / `size_max_bytes`
/// SPARQL bounds.
fn apply_size(range: &RangeExpr<u64>, f: &mut SpqFilters) {
    match range {
        RangeExpr::Gt(n) => f.size_min_bytes = *n as i64,
        RangeExpr::Lt(n) => f.size_max_bytes = *n as i64,
        RangeExpr::Eq(n) => {
            f.size_min_bytes = *n as i64;
            f.size_max_bytes = *n as i64;
        }
        RangeExpr::Between(lo, hi) => {
            f.size_min_bytes = *lo as i64;
            f.size_max_bytes = *hi as i64;
        }
    }
}

// ── Scoring helpers ───────────────────────────────────────────────────────────

#[inline]
fn row_score(row: usize) -> f64 {
    (0.5 * (-0.05 * row as f64).exp()).min(0.5)
}

const PDF_ROW_ADVANCE: usize = 10;

// ── Thread-safety wrapper ─────────────────────────────────────────────────────

struct SendSyncStatement(tracker::SparqlStatement);
unsafe impl Send for SendSyncStatement {}
unsafe impl Sync for SendSyncStatement {}

// ── Handler ───────────────────────────────────────────────────────────────────

pub struct LocalSearchHandler {
    stmt: Option<SendSyncStatement>,
    stmt_filtered: Option<SendSyncStatement>,
}

impl Default for LocalSearchHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSearchHandler {
    pub fn new() -> Self {
        let _ = env_logger::try_init();
        let t = Instant::now();
        let (stmt, stmt_filtered) =
            match tracker::SparqlConnection::bus_new(LOCALSEARCH_BUS, None, None) {
                Err(_) => {
                    log::warn!(
                        "localsearch: daemon not running ({:.1}ms); will retry on first query",
                        t.elapsed().as_secs_f64() * 1000.0
                    );
                    (None, None)
                }
                Ok(conn) => {
                    log::info!(
                        "localsearch: D-Bus connection opened in {:.1}ms",
                        t.elapsed().as_secs_f64() * 1000.0
                    );
                    let base = conn
                        .query_statement(SPARQL_TEMPLATE, gio::Cancellable::NONE)
                        .map_err(|e| log::warn!("localsearch: base stmt: {e}"))
                        .ok()
                        .map(SendSyncStatement);
                    let filtered = conn
                        .query_statement(SPARQL_FILTERED_TEMPLATE, gio::Cancellable::NONE)
                        .map_err(|e| log::warn!("localsearch: filtered stmt: {e}"))
                        .ok()
                        .map(SendSyncStatement);
                    (base, filtered)
                }
            };
        Self { stmt, stmt_filtered }
    }

    fn statement(&self) -> Option<tracker::SparqlStatement> {
        if let Some(ref s) = self.stmt {
            return Some(s.0.clone());
        }
        let conn = tracker::SparqlConnection::bus_new(LOCALSEARCH_BUS, None, None).ok()?;
        conn.query_statement(SPARQL_TEMPLATE, gio::Cancellable::NONE).ok()
    }

    fn filtered_statement(&self) -> Option<tracker::SparqlStatement> {
        if let Some(ref s) = self.stmt_filtered {
            return Some(s.0.clone());
        }
        let conn = tracker::SparqlConnection::bus_new(LOCALSEARCH_BUS, None, None).ok()?;
        conn.query_statement(SPARQL_FILTERED_TEMPLATE, gio::Cancellable::NONE)
            .ok()
    }
}

use sx_plugin_api::SxHandler;

impl SxHandler for LocalSearchHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("localsearch"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Search indexed files via the LocalSearch/Tracker3 daemon",
            ),
            value_hint: SxBytes::from_str_bytes("search terms"),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: true,
            is_file_index: true,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        let t = Instant::now();
        let terms = value.as_ref().trim();
        if terms.is_empty() {
            return;
        }

        let limit = match ctx.max_results {
            0 => 500,
            n => n as i64,
        };

        let raw = String::from_utf8_lossy(ctx.raw_query.as_bytes());
        let filters = extract_sparql_filters(&raw);

        if filters.is_any_active() {
            log::debug!("localsearch: filtered query for {:?}", terms);
            self.query_filtered(terms, limit, &filters, sender, t);
        } else {
            self.query_base(terms, limit, sender, t);
        }
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(&self, _partial_value: stabby::str::Str<'_>) -> sx_plugin_api::SxHints {
        sx_plugin_api::SxHints::empty()
    }
}

impl LocalSearchHandler {
    fn query_base(&self, terms: &str, limit: i64, sender: SxResultSender, t: Instant) {
        let Some(stmt) = self.statement() else {
            log::warn!("localsearch: no connection");
            return;
        };
        stmt.bind_string("match", terms);
        stmt.bind_int("limit", limit);
        run_cursor(stmt, sender, t);
    }

    fn query_filtered(
        &self,
        terms: &str,
        limit: i64,
        ff: &SpqFilters,
        sender: SxResultSender,
        t: Instant,
    ) {
        let Some(stmt) = self.filtered_statement() else {
            log::warn!("localsearch: no filtered statement");
            return;
        };

        let ext_active = !ff.extensions.is_empty();
        let ext_pattern = if ext_active {
            ff.ext_regex_pattern()
        } else {
            String::new()
        };
        stmt.bind_boolean("skip_ext", !ext_active);
        stmt.bind_string("ext_pattern", &ext_pattern);

        let mime_active = !ff.mime_contains.is_empty();
        stmt.bind_boolean("skip_mime", !mime_active);
        stmt.bind_string("mime_lc", &ff.mime_contains);

        let path_active = !ff.path_contains.is_empty();
        stmt.bind_boolean("skip_path", !path_active);
        stmt.bind_string("path_lc", &ff.path_contains.to_lowercase());

        let ma = ff.mtime_after_secs != 0;
        stmt.bind_boolean("skip_mtime_after", !ma);
        if let Ok(dt) = glib::DateTime::from_unix_utc(ff.mtime_after_secs) {
            stmt.bind_datetime("mtime_after", &dt);
        }

        let mb = ff.mtime_before_secs != 0;
        stmt.bind_boolean("skip_mtime_before", !mb);
        if let Ok(dt) = glib::DateTime::from_unix_utc(ff.mtime_before_secs) {
            stmt.bind_datetime("mtime_before", &dt);
        }

        stmt.bind_boolean("skip_size_min", ff.size_min_bytes == 0);
        stmt.bind_int("size_min", ff.size_min_bytes);

        stmt.bind_boolean("skip_size_max", ff.size_max_bytes == i64::MAX);
        stmt.bind_int("size_max", ff.size_max_bytes);

        stmt.bind_string("match", terms);
        stmt.bind_int("limit", limit);
        run_cursor(stmt, sender, t);
    }
}

// ── Cursor iteration ──────────────────────────────────────────────────────────

fn run_cursor(stmt: tracker::SparqlStatement, sender: SxResultSender, t: Instant) {
    let cursor = match stmt.execute(gio::Cancellable::NONE) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("localsearch: execute failed: {e}");
            return;
        }
    };
    let mut count = 0usize;
    loop {
        match cursor.next(gio::Cancellable::NONE) {
            Ok(true) => {}
            Ok(false) => {
                log::debug!(
                    "localsearch: {count} rows in {:.1}ms",
                    t.elapsed().as_secs_f64() * 1000.0
                );
                break;
            }
            Err(e) => {
                log::warn!("localsearch: cursor error: {e}");
                break;
            }
        }
        let url = match cursor.string(0) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let mime = cursor.string(1).map(|s| s.to_string());

        let is_pdf = mime.as_deref() == Some("application/pdf");
        let score = row_score(if is_pdf {
            count.saturating_sub(PDF_ROW_ADVANCE)
        } else {
            count
        });

        let title = url
            .rsplit('/')
            .next()
            .map(percent_decode)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| url.clone());

        let mut result = SxResult {
            title: stabby::option::Option::Some(SxBytes::from_str_bytes(&title)),
            ..SxResult::new("localsearch", score)
        };
        result.metadata.push(SxMetaEntry::text("file_path", &url));
        if let Some(m) = mime {
            result.metadata.push(SxMetaEntry::text("mime", &m));
        }

        if !sender.send(result) {
            break;
        }
        count += 1;
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

sx_plugin_api::declare_plugin!(LocalSearchHandler::new());
