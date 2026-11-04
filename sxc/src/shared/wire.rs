//! Search wiring shared by both frontends.
//!
//! [`wire_search`] starts all background workers for a [`CompiledQuery`] and
//! feeds results into a [`ResultMsg`] channel.
//!
//! ## Concurrency model
//!
//! * **File workers** — `ncpu - 2` tokio tasks, each driving [`CompiledQuery::run()`].
//!   The first task polled atomically claims the walker role; the rest steal from
//!   the shared injector.  Workers yield cooperatively every 64 steals.
//!
//! * **Handler tasks** — one `tokio::task::spawn_blocking` per handler token.
//!   Plugin `dispatch()` is a synchronous FFI call and must not run on a cooperative task.
//!
//! ## Cancellation
//!
//! Driven by the [`CancellationToken`] in [`CompiledQuery::token()`].  Firing it:
//! * wakes file workers blocked in the `Steal::Empty` `select!`;
//! * causes `token.run_until_cancelled(…)` wrappers to drop at the next `await`;
//! * causes handler `SxResultSender` callbacks to return `false`.
//!
//! Cancellation is the responsibility of the [`SearchHandle`] owner, not `wire_search`.

use futures_util::StreamExt;
use tokio::sync::mpsc;

use sx_core::CompiledQuery;

use crate::shared::msg::ResultMsg;

/// Convert the parser's case-sensitivity enum to the plugin-API equivalent.
///
/// The two types are separate because `sx-plugin-api` must not depend on `sx-parser`
/// (ABI isolation). Keep in sync when either enum changes.
fn to_sx_case(c: sx_core::CaseSensitivity) -> sx_plugin_api::SxCaseSensitivity {
    match c {
        sx_core::CaseSensitivity::Smart       => sx_plugin_api::SxCaseSensitivity::Smart,
        sx_core::CaseSensitivity::Sensitive   => sx_plugin_api::SxCaseSensitivity::Sensitive,
        sx_core::CaseSensitivity::Insensitive => sx_plugin_api::SxCaseSensitivity::Insensitive,
    }
}

/// Returns the number of file-search worker tasks to use.
///
/// All logical CPUs minus 2 (one for the UI task, one for OS headroom), minimum 1.
pub fn n_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2))
        .unwrap_or(1)
        .max(1)
}

/// Start all background work for a compiled query and wire results into `tx`.
///
/// Spawns `n` file-worker tasks and one `spawn_blocking` task per handler token.
/// The caller must drop its own `tx` clone after this returns so the channel
/// closes when the last worker finishes.
pub fn wire_search(cq: &CompiledQuery, n: usize, tx: mpsc::UnboundedSender<ResultMsg>) {
    log::debug!(
        "wire: starting search — {} file-worker(s), {} handler token(s)",
        n,
        cq.handler_tokens.len()
    );

    // Plugin dispatch() is a blocking FFI call — use spawn_blocking.
    if let Some(registry) = cq.registry.clone() {
        let ctx = sx_plugin_api::SxQueryContext {
            roots: sx_plugin_api::stabby::vec::Vec::new(),
            max_results: 0,
            timeout_ms: 0,
            case_sensitivity: to_sx_case(cq.options.case.clone()),
            raw_query: sx_plugin_api::SxBytes::from_str_bytes(&cq.raw_query),
        };

        for token in &cq.handler_tokens {
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

    // Each task drives cq.clone().run() — first polled claims the walker role,
    // rest steal from the shared injector.
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
