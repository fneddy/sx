//! Search wiring shared by both frontends.
//!
//! [`wire_search`] starts all background workers for a [`CompiledQuery`] and
//! feeds results into a [`ResultMsg`] channel.  Both `sxc` (TUI) and `sxg`
//! (GUI) call this identically — neither frontend touches this file.
//!
//! ## Concurrency model
//!
//! * **File workers** — `ncpu - 2` tokio tasks, each driving
//!   [`CompiledQuery::run()`] on a clone of the compiled query.  The first
//!   task to be polled atomically claims the walker role; the rest become pure
//!   content workers that steal from the shared injector.  All I/O (file reads,
//!   plugin extraction) is blocking but brief; `tokio::task::spawn_blocking` is
//!   not needed because `run()` yields cooperatively every 64 steals so the
//!   executor remains responsive.
//!
//! * **Handler tasks** — one `tokio::task::spawn_blocking` per handler token.
//!   Plugin `dispatch()` is a synchronous FFI call and must not run on a
//!   cooperative task.  Each blocking task pushes [`ResultMsg`] messages
//!   directly into `tx`.
//!
//! ## Cancellation
//!
//! Cancellation is driven by [`CompiledQuery::token()`], a [`CancellationToken`]
//! shared (via `Arc`) across all query clones.  Calling [`CompiledQuery::cancel()`]
//! fires the token, which:
//!
//! * wakes file workers that are blocked in the `Steal::Empty` `select!` inside
//!   [`CompiledQuery::run()`] without any extra `notify_waiters` call;
//! * causes the `token.run_until_cancelled(…)` wrappers here to drop their
//!   inner futures at the next `await` point;
//! * causes the `SxResultSender` callback in blocking handler tasks to return
//!   `false` on the next plugin callback.
//!
//! `wire_search` never cancels the query itself — that responsibility belongs
//! entirely to the owner of the [`SearchHandle`] (via its [`DropGuard`]).
//!
//! [`DropGuard`]: tokio_util::sync::DropGuard

use futures_util::StreamExt;
use tokio::sync::mpsc;

use sx_core::CompiledQuery;

use crate::shared::msg::ResultMsg;

/// Returns the number of file-search worker tasks to use.
///
/// Uses all logical CPUs minus 2 (one reserved for the UI task, one for the
/// OS scheduler headroom), with a minimum of 1.
pub fn n_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2))
        .unwrap_or(1)
        .max(1)
}

/// Start all background work for a compiled query and wire results into `tx`.
///
/// Spawns:
/// * `n` tokio tasks, each driving [`CompiledQuery::run()`] on a clone of the
///   compiled query.  File hits arrive as [`ResultMsg`] with `namespace = "file"`.
/// * One `tokio::task::spawn_blocking` per handler token.  Each calls the
///   plugin's synchronous `dispatch()` and sends [`ResultMsg`] items.
///
/// The caller must drop its own clone of `tx` after this returns so the
/// channel closes when the last worker finishes.
pub fn wire_search(cq: &CompiledQuery, n: usize, tx: mpsc::UnboundedSender<ResultMsg>) {
    log::debug!(
        "wire: starting search — {} file-worker(s), {} handler token(s)",
        n,
        cq.handler_tokens.len()
    );

    // ── Handler tokens (calc, localsearch, apps, …) ───────────────────────────
    // Plugin dispatch() is a blocking FFI call — use spawn_blocking so it
    // doesn't starve cooperative tasks on the tokio executor.
    if let Some(registry) = cq.registry.clone() {
        let ctx = sx_plugin_api::SxQueryContext {
            roots: sx_plugin_api::stabby::vec::Vec::new(),
            max_results: 0,
            timeout_ms: 0,
            case_sensitivity: match cq.options.case {
                sx_core::CaseSensitivity::Smart => sx_plugin_api::SxCaseSensitivity::Smart,
                sx_core::CaseSensitivity::Sensitive => sx_plugin_api::SxCaseSensitivity::Sensitive,
                sx_core::CaseSensitivity::Insensitive => {
                    sx_plugin_api::SxCaseSensitivity::Insensitive
                }
            },
        };

        for token in &cq.handler_tokens {
            // Clone the CancellationToken — cheap (just an Arc clone), avoids
            // a full CompiledQuery clone solely for the cancellation signal.
            let cancel = cq.token();
            let registry = registry.clone();
            let tx = tx.clone();
            let namespace = token.namespace.clone();
            let value = String::from_utf8_lossy(&token.value).into_owned();
            let ns_str = String::from_utf8_lossy(&namespace).into_owned();
            let ctx = ctx.clone();

            log::debug!("wire: dispatching handler {}:{:?}", ns_str, value);
            tokio::task::spawn_blocking(move || {
                let sender = sx_plugin_api::SxResultSender::from_fn(move |res| {
                    // Return false immediately if cancelled or the consumer is gone.
                    if cancel.is_cancelled() || tx.is_closed() {
                        return false;
                    }
                    let m: ResultMsg = res.into();
                    if !m.is_empty() {
                        log::debug!(
                            "wire: handler result ns={} score={:.4} title={:?}",
                            m.namespace,
                            m.score,
                            m.title
                                .as_deref()
                                .or_else(|| m.meta("value"))
                                .unwrap_or("(none)")
                        );
                        tx.send(m).is_ok()
                    } else {
                        true
                    }
                });
                if let Err(e) = registry.dispatch(&namespace, &value, &ctx, sender) {
                    log::warn!("wire: dispatch error for {}: {}", ns_str, e);
                }
            });
        }
    }

    // ── File search workers ───────────────────────────────────────────────────
    // Each task drives cq.clone().run() — the first one polled claims the
    // walker role; the rest steal content work from the shared injector.
    //
    // `token.run_until_cancelled(...)` wraps the stream-consuming loop so that
    // when the query is cancelled the task exits at the next `await` point
    // inside `run()` without any explicit `is_cancelled()` poll in this loop.
    let workers = n.max(1);
    log::debug!("wire: spawning {} file-search worker task(s)", workers);
    for i in 0..workers {
        let cq = cq.clone();
        let tx = tx.clone();
        let cancel = cq.token();
        tokio::spawn(async move {
            log::trace!("wire: file worker {} started", i);
            let mut count = 0usize;
            cancel
                .run_until_cancelled(async move {
                    let mut stream = cq.run();
                    while let Some(m) = stream.next().await {
                        if tx.is_closed() {
                            break;
                        }
                        if m.extracted.is_ok() {
                            let msg = ResultMsg::from_file(m.path.display().to_string(), m.score);
                            log::debug!("wire: file hit score={:.4} path={:?}", m.score, m.path);
                            if tx.send(msg).is_err() {
                                log::debug!(
                                    "wire: file worker {} — channel closed after {} hit(s)",
                                    i,
                                    count
                                );
                                break;
                            }
                            count += 1;
                        } else if let Err(ref e) = m.extracted {
                            log::warn!(
                                "wire: file worker {} extraction error for {:?}: {}",
                                i,
                                m.path,
                                e
                            );
                        }
                    }
                })
                .await;
            log::trace!("wire: file worker {} done ({} hit(s))", i, count);
        });
    }
}
