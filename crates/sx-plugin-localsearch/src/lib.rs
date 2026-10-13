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
//! The connection **and** a compiled `SparqlStatement` are opened **once** at
//! handler construction time and reused across all subsequent queries.  Using
//! a prepared statement (rather than a raw `conn.query()` string) matches the
//! approach taken by the `localsearch search` CLI and avoids the per-row
//! D-Bus round-trip overhead that makes cursor iteration ~20 ms/row.
//!
//! ## Usage
//!
//! ```text
//! localsearch:readme       → all indexed files matching "readme"
//! localsearch:report 2024  → files matching both "report" AND "2024"
//! ```
//!
//! Multiple query words are joined with spaces; `fts:match` treats them as a
//! logical AND by default.

use std::time::Instant;
use sx_plugin_api::{
    meta_text, stabby, SxBytes, SxHandlerInfo, SxNamespacePolicy, SxQueryContext, SxResult,
    SxResultSender,
};
use tracker::prelude::SparqlCursorExtManual;

// ── D-Bus service name ────────────────────────────────────────────────────────

const LOCALSEARCH_BUS: &str = "org.freedesktop.LocalSearch3";

// ── SPARQL prepared-statement template ───────────────────────────────────────

/// SPARQL query with named placeholders (`~match`, `~limit`).
///
/// Key design points:
/// - `fts:match` is on the *content* objects (`nfo:TextDocument`, etc.) which
///   live in the `tracker:Documents` named graph.  The *storage* objects
///   (`nfo:FileDataObject`, with `nie:url`) live in `tracker:FileSystem`.
///   Querying `?r a nfo:FileDataObject ; fts:match …` only returns the 2 files
///   whose FTS data sits directly on the storage node; all the rest are missed.
/// - `nie:isStoredAs` links the content object to the storage object.
/// - The `ORDER BY DESC(fts:rank(?r))` must live in a subquery scoped to the
///   `tracker:Documents` graph.  Placing it at the outer level (after the
///   cross-graph `nie:isStoredAs` join) forces a full sort across both graphs
///   and causes ~90 ms per row (~3 s for 35 rows).  The subquery + outer join
///   pattern runs in ~35 ms total.
/// - `~match` — bound via `SparqlStatement::bind_string`; no string
///   interpolation, so no injection risk and no escaping needed.
/// - `~limit` — bound via `SparqlStatement::bind_int`.
///
/// Note: `fts:rank(?r)` is used only in `ORDER BY` — reading it back via
/// `cursor.double()` from a computed `SELECT` column returns 1.0 for every
/// row (Tracker cursor limitation).  Scores are assigned by row position
/// instead: see [`row_score`].
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

/// Convert a 0-based row position into a base score in `(0.0, 0.5]`.
///
/// Row 0 (best FTS rank) → 0.5, row 1 → 0.499…, etc., using an exponential
/// decay so the first few results are meaningfully separated.  The ceiling of
/// 0.5 guarantees that *all* localsearch results score below every apps result
/// (which uses scores in `[0.8, 1.0]`).
#[inline]
fn row_score(row: usize) -> f64 {
    // 0.5 * e^(-row * 0.05) — decays slowly so results aren't all squashed
    // to the same value, but stays well below 0.5 after ~14 rows.
    0.5 * (-0.05 * row as f64).exp()
}

/// Virtual row advance applied to PDF results.
///
/// Instead of multiplying the score (which can never overcome the gap between
/// adjacent FTS rows), we pretend the PDF arrived this many rows *earlier* in
/// the FTS ranking before calling [`row_score`].  The result is capped at 0.5
/// so the guarantee that localsearch results always rank below apps results
/// (which score in `[0.8, 1.0]`) is preserved.
///
/// With a decay of `e^{-0.05}` per row ≈ 5.1 % per step, an advance of 10
/// rows means a PDF at FTS position N scores the same as a non-PDF at position
/// N-10, comfortably beating all non-PDFs within 9 rows above it.
const PDF_ROW_ADVANCE: usize = 10;

// ── Thread-safety wrapper ─────────────────────────────────────────────────────

/// `SparqlStatement` is a GObject refcount type; `tracker-rs` does not add
/// `unsafe impl Send + Sync` for it the way it does for `SparqlConnection`.
/// We only ever call the statement from the single thread that owns the handler
/// (plugin queries are dispatched serially), so the assertion is sound.
struct SendSyncStatement(tracker::SparqlStatement);
unsafe impl Send for SendSyncStatement {}
unsafe impl Sync for SendSyncStatement {}

// ── Handler ───────────────────────────────────────────────────────────────────

/// LocalSearch full-text-search handler.
///
/// Holds a persistent D-Bus connection and a compiled `SparqlStatement` so
/// both the handshake cost and the query-parse/plan cost are paid only once.
pub struct LocalSearchHandler {
    /// Compiled prepared statement derived from [`SPARQL_TEMPLATE`].
    /// `None` when the daemon was not running at construction time (lazy
    /// fallback: retry on first query).
    stmt: Option<SendSyncStatement>,
}

impl Default for LocalSearchHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSearchHandler {
    pub fn new() -> Self {
        // Initialise env_logger for this plugin's own `log` crate instance.
        let _ = env_logger::try_init();

        let t = Instant::now();
        let stmt = match tracker::SparqlConnection::bus_new(LOCALSEARCH_BUS, None, None) {
            Err(_) => {
                log::warn!(
                    "localsearch: daemon not running ({:.1}ms); will retry on first query",
                    t.elapsed().as_secs_f64() * 1000.0
                );
                None
            }
            Ok(conn) => {
                log::info!(
                    "localsearch: D-Bus connection opened in {:.1}ms",
                    t.elapsed().as_secs_f64() * 1000.0
                );
                let t2 = Instant::now();
                match conn.query_statement(SPARQL_TEMPLATE, gio::Cancellable::NONE) {
                    Ok(s) => {
                        log::debug!(
                            "localsearch: statement compiled in {:.1}ms",
                            t2.elapsed().as_secs_f64() * 1000.0
                        );
                        Some(SendSyncStatement(s))
                    }
                    Err(e) => {
                        log::warn!("localsearch: failed to compile statement: {e}");
                        None
                    }
                }
            }
        };

        Self { stmt }
    }

    /// Return the cached prepared statement, or compile a fresh one if we
    /// didn't have a connection at construction time.  Returns `None` if the
    /// daemon is still unavailable.
    fn statement(&self) -> Option<tracker::SparqlStatement> {
        if let Some(ref s) = self.stmt {
            // Clone the GObject reference — cheap refcount bump, no I/O.
            return Some(s.0.clone());
        }
        // No cached statement: daemon was not running at construction time.
        // Try to open a fresh connection and compile the statement now.
        log::debug!("localsearch: no cached statement, attempting fresh connect");
        let t = Instant::now();
        let conn = tracker::SparqlConnection::bus_new(LOCALSEARCH_BUS, None, None).ok()?;
        log::debug!(
            "localsearch: fresh connect ok ({:.1}ms)",
            t.elapsed().as_secs_f64() * 1000.0
        );
        match conn.query_statement(SPARQL_TEMPLATE, gio::Cancellable::NONE) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("localsearch: failed to compile statement on retry: {e}");
                None
            }
        }
    }
}

impl sx_plugin_api::SxHandler for LocalSearchHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("localsearch"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Search indexed files via the LocalSearch/Tracker3 daemon (localsearch:<terms>)",
            ),
            value_hint: SxBytes::from_str_bytes("search terms"),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: true,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        let t_query = Instant::now();
        let terms = value.as_ref().trim();
        log::debug!("localsearch: query({:?})", terms);

        if terms.is_empty() {
            return;
        }

        // Resolve limit: 0 in ctx means "unlimited"; cap at 500 for SPARQL.
        let limit: i64 = match ctx.max_results {
            0 => 500,
            n => n as i64,
        };

        let stmt = match self.statement() {
            Some(s) => s,
            None => {
                log::warn!("localsearch: no connection available, skipping query");
                return;
            }
        };
        log::debug!(
            "localsearch: statement ready at {:.1}ms",
            t_query.elapsed().as_secs_f64() * 1000.0
        );

        // Bind parameters — no string escaping needed; the tracker library
        // handles value encoding internally.
        stmt.bind_string("match", terms);
        stmt.bind_int("limit", limit);

        let t_exec = Instant::now();
        let cursor = match stmt.execute(gio::Cancellable::NONE) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("localsearch: statement execute failed: {e}");
                return;
            }
        };
        log::debug!(
            "localsearch: execute in {:.1}ms",
            t_exec.elapsed().as_secs_f64() * 1000.0
        );

        let mut count = 0usize;
        loop {
            let t_next = Instant::now();
            match cursor.next(gio::Cancellable::NONE) {
                Ok(true) => {}
                Ok(false) => {
                    log::debug!(
                        "localsearch: cursor exhausted after {count} rows; \
                         total {:.1}ms",
                        t_query.elapsed().as_secs_f64() * 1000.0
                    );
                    break;
                }
                Err(e) => {
                    log::warn!("localsearch: cursor.next error: {e}");
                    break;
                }
            }
            log::trace!(
                "localsearch: cursor.next() row {count} in {:.2}ms",
                t_next.elapsed().as_secs_f64() * 1000.0
            );

            // Column 0: nie:url  (file:///… URI)
            // Column 1: nie:mimeType (may be unbound)
            let url = match cursor.string(0) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let mime = cursor.string(1).map(|s| s.to_string());
            // Score by row position: row 0 (best FTS rank) → 0.5, decaying.
            // Stays in (0, 0.5] so all localsearch results rank below apps
            // results which are in [0.8, 1.0].
            // PDFs are scored as if they arrived PDF_ROW_ADVANCE rows earlier,
            // so they float well above non-PDF results at nearby FTS ranks.
            let is_pdf = mime.as_deref() == Some("application/pdf");
            let effective_row = if is_pdf {
                count.saturating_sub(PDF_ROW_ADVANCE)
            } else {
                count
            };
            let score = row_score(effective_row).min(0.5);

            // Derive a human-readable title from the last path segment.
            let title = url
                .rsplit('/')
                .next()
                .and_then(|s| {
                    let decoded = percent_decode(s);
                    if decoded.is_empty() {
                        None
                    } else {
                        Some(decoded)
                    }
                })
                .unwrap_or_else(|| url.clone());

            log::trace!(
                "localsearch: row {count} url={url} mime={} score={score:.4}",
                mime.as_deref().unwrap_or("(none)")
            );
            count += 1;

            let mut result = SxResult {
                title: stabby::option::Option::Some(SxBytes::from_str_bytes(&title)),
                ..SxResult::new("localsearch", score)
            };
            meta_text(&mut result, "file_path", &url);
            if let Some(m) = mime {
                meta_text(&mut result, "mime", &m);
            }

            if !sender.send(result) {
                log::debug!(
                    "localsearch: sender closed after {count} rows at {:.1}ms",
                    t_query.elapsed().as_secs_f64() * 1000.0
                );
                return;
            }
        }
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(
        &self,
        _partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        sx_plugin_api::SxHints::empty()
    }
}

/// Minimal percent-decode: replace `%XX` sequences with the corresponding byte.
/// Only handles valid ASCII printable results; leaves anything else as-is.
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
