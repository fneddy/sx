//! Inline hint / completion state for the search input.
//!
//! [`HintState`] owns the list of completion candidates that appear as ghost
//! text in the input suffix while the user types inside a handler namespace
//! (e.g. `sys:` or `apps:`).  It is completely self-contained: callers hand it
//! the current query string and a registry reference and get back display text
//! or a new input value.

use gpui::SharedString;
use sx_core::HandlerRegistry;

// ── Hint datum ────────────────────────────────────────────────────────────────

/// A single completion candidate.
#[derive(Clone)]
pub struct Hint {
    /// Display label shown in the input ghost text  (e.g. `"process:"`).
    pub label: SharedString,
    /// Text substituted into the input when the hint is accepted via Tab.
    pub completion: SharedString,
}

// ── HintState ─────────────────────────────────────────────────────────────────

/// All inline-completion state owned by [`super::SearchView`].
pub struct HintState {
    hints: Vec<Hint>,
}

impl HintState {
    pub fn new() -> Self {
        Self { hints: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.hints.is_empty()
    }

    pub fn hints(&self) -> &[Hint] {
        &self.hints
    }

    pub fn clear(&mut self) {
        self.hints.clear();
    }

    // ── Update ────────────────────────────────────────────────────────────────

    /// Recompute the hint list from `query` and `registry`.
    ///
    /// Examines the last whitespace-delimited token.  If it contains `:` the
    /// part before is the namespace and the part after is the partial value.
    pub fn update(&mut self, query: &str, registry: &HandlerRegistry) {
        let last_token = query.split_whitespace().last().unwrap_or("");
        self.hints = if let Some(colon) = last_token.find(':') {
            let ns = &last_token[..colon];
            let partial = &last_token[colon + 1..];
            registry
                .hints_for_namespace(ns.as_bytes(), partial)
                .into_iter()
                .take(4)
                .map(|(label, completion)| Hint {
                    label: SharedString::from(label),
                    completion: SharedString::from(completion),
                })
                .collect()
        } else {
            Vec::new()
        };
    }

    // ── Rendering helper ──────────────────────────────────────────────────────

    /// Ghost-text string for the input suffix, e.g. `"process:  ·  port:  ·  disk:"`.
    /// Returns `None` when there are no hints.
    pub fn suffix_text(&self) -> Option<SharedString> {
        if self.hints.is_empty() {
            return None;
        }
        Some(
            self.hints
                .iter()
                .map(|h| h.label.as_ref())
                .collect::<Vec<_>>()
                .join("  ·  ")
                .into(),
        )
    }

    // ── Acceptance ────────────────────────────────────────────────────────────

    /// Apply the completion at `idx` to `query` and return the new input string.
    ///
    /// Replaces everything after the first `:` in the last whitespace token:
    /// - `"apps:Fire"` + `"Firefox"` → `"apps:Firefox"`
    /// - `"sys:disk:"` + `"disk:/"` → `"sys:disk:/"`
    ///
    /// Returns `None` when `idx` is out of range.
    pub fn apply(&self, idx: usize, query: &str) -> Option<String> {
        let completion = &self.hints.get(idx)?.completion;
        let token_start = query
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let before_token = &query[..token_start];
        let last_token = &query[token_start..];
        let new_value = if let Some(colon) = last_token.find(':') {
            let ns = &last_token[..colon];
            format!("{before_token}{ns}:{completion}")
        } else {
            format!("{query}:{completion}")
        };
        Some(new_value)
    }

    // ── Namespace tab-completion ──────────────────────────────────────────────

    /// If the last word of `query` is an unambiguous prefix of exactly one
    /// registered namespace, return the remaining suffix including the trailing
    /// `:`.  Returns `None` when zero or more than one namespace matches.
    pub fn namespace_suffix(query: &str, registry: &HandlerRegistry) -> Option<SharedString> {
        let last_word = query.split_whitespace().last().unwrap_or("");
        if last_word.is_empty() || last_word.contains(':') {
            return None;
        }
        let matches: Vec<String> = registry
            .namespaces()
            .into_iter()
            .filter(|ns| ns.starts_with(last_word) && ns.len() > last_word.len())
            .collect();
        if matches.len() == 1 {
            Some(format!("{}:", &matches[0][last_word.len()..]).into())
        } else {
            None
        }
    }
}
