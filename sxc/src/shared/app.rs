//! Result buffer for the TUI frontend.
//!
//! The GUI uses a different storage model (GPUI `Entity<ResultList>` with
//! online sorted insertion), so this type lives here rather than in `shared/`.

use crate::shared::msg::ResultMsg;

pub struct App {
    /// Score-descending list of all results (handler results + file hits).
    pub results: Vec<ResultMsg>,
    /// File hits buffered until the next flush.
    pub pending: Vec<ResultMsg>,
    pub total: usize,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
            pending: Vec::new(),
            total: 0,
        }
    }

    /// Drains `pending` into `results` in sorted order (score DESC, title-len ASC).
    ///
    /// Uses a single sort + merge rather than repeated `insert` calls so that
    /// flushing n pending items into m existing results costs O((n+m) log(n+m))
    /// instead of O(n·m).
    pub fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        log::trace!(
            "app: flushing {} pending file hit(s) into {} existing result(s)",
            self.pending.len(),
            self.results.len()
        );
        self.results.append(&mut self.pending);
        self.results.sort_by(|a, b| {
            // Primary: descending score.
            let ord = b
                .score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            // Tiebreak: ascending title length.
            let la = a.title.as_deref().map(str::len).unwrap_or(0);
            let lb = b.title.as_deref().map(str::len).unwrap_or(0);
            la.cmp(&lb)
        });
    }

    /// Buffer file hits in `pending`; insert handler results immediately at their sorted position.
    pub fn apply(&mut self, msg: ResultMsg) {
        if msg.namespace == "file" {
            self.total += 1;
            log::trace!(
                "app: buffering file hit #{} {:?}",
                self.total,
                msg.title.as_deref().unwrap_or("(no title)")
            );
            self.pending.push(msg);
        } else {
            let pos = self.results.partition_point(|e| e.score >= msg.score);
            log::debug!(
                "app: insert handler ns={} score={:.4} pos={}/{} title={:?}",
                msg.namespace,
                msg.score,
                pos,
                self.results.len(),
                msg.title
                    .as_deref()
                    .or_else(|| msg.meta("value"))
                    .unwrap_or("(none)")
            );
            self.results.insert(pos, msg);
        }
    }
}
