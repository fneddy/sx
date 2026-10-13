//! Central dispatch table for `sx` handler plugins.
//!
//! The registry is name-agnostic: each handler declares its own conflict
//! policy via [`SxNamespacePolicy`]; the registry enforces it uniformly.
//!
//! ## Namespace conflict policies
//!
//! | Policy | Behaviour |
//! |--------|-----------|
//! | `Exclusive` | First registration wins; a second attempt returns [`ConflictError`]. |
//! | `Shared`    | All registrations are kept; every handler is called on dispatch. |
//!
//! Alias conflicts log a warning and skip the alias; the primary namespace
//! registration is unaffected.

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use log::warn;
use sx_plugin_api::{
    SxHandler, SxHandlerInfo, SxNamespacePolicy, SxQueryContext, SxResult, SxResultSender,
};

// ── Error types ───────────────────────────────────────────────────────────────

/// Returned by [`HandlerRegistry::dispatch`] when no handler owns the namespace.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown namespace `{0}`")]
pub struct UnknownNamespace(pub Box<str>);

/// Returned by [`HandlerRegistry::register`] when a second handler tries to
/// claim a namespace whose existing owner declared [`SxNamespacePolicy::Exclusive`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("namespace `{ns}` is already exclusively owned — `{claimant}` cannot register it")]
pub struct ConflictError {
    pub ns: Box<str>,
    pub claimant: Box<str>,
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// Central dispatch table mapping namespace bytes to handler indices.
pub struct HandlerRegistry {
    /// All registered handlers in registration order.
    handlers: Vec<Arc<dyn SxHandler>>,
    /// namespace bytes → indices into `handlers` (>1 for Shared namespaces).
    index: HashMap<Box<[u8]>, Vec<usize>>,
    /// namespace bytes → the policy declared by the first registrant.
    /// Used to validate subsequent registrations.
    policies: HashMap<Box<[u8]>, SxNamespacePolicy>,
    /// Indices of handlers that declared `global_scope: true`.
    /// These are invoked for every bare query term in addition to their
    /// explicit namespace.
    global_handlers: Vec<usize>,
    /// Loaded plugin libraries that must remain open for the process lifetime.
    /// These are opaque `Box<dyn Any + Send + Sync>` so this crate doesn't need
    /// to depend on `libloading` at the registry level — the concrete type is erased.
    /// Keeps loaded-plugin handles alive so they are dropped after every
    /// registered handler has been removed.  The field name is intentionally
    /// not prefixed with `_`; the leading underscore convention in Rust means
    /// "intentionally unused", whereas this field is used for its Drop side-effect.
    owned_plugins: Vec<Box<dyn Any + Send + Sync>>,
}

impl HandlerRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
            index: HashMap::new(),
            policies: HashMap::new(),
            global_handlers: Vec::new(),
            owned_plugins: Vec::new(),
        }
    }

    /// Store an opaque plugin handle so its library stays open.
    ///
    /// Called by [`crate::PluginStore::register_all`] for every plugin that
    /// successfully registered.  The value is never read again — ownership
    /// keeps the `.so` mapped until the registry is dropped.
    pub(crate) fn keep_plugin<T: Any + Send + Sync + 'static>(&mut self, plugin: T) {
        self.owned_plugins.push(Box::new(plugin));
    }

    /// Register a handler.
    ///
    /// Calls `handler.info()` once to read the namespace, policy, and aliases.
    ///
    /// - `Exclusive`: first registration succeeds; any subsequent registration
    ///   of the same namespace returns [`Err(ConflictError)`].
    /// - `Shared`: every registration appends to the handler list; all are
    ///   called on dispatch.
    ///
    /// Alias conflicts always log a warning and skip the alias; the primary
    /// namespace registration is unaffected.
    pub fn register(&mut self, handler: Arc<dyn SxHandler>) -> Result<(), ConflictError> {
        let info: SxHandlerInfo = handler.info();
        let ns = info.namespace.as_str().to_owned();
        let policy = info.namespace_policy;

        // Validate before pushing so a ConflictError leaves the registry
        // consistent — no orphaned handler that `default_file_extensions()` would count.
        let idx = self.handlers.len();
        self.insert_namespace(ns.as_bytes(), idx, &ns, &policy)?;

        if info.global_scope {
            self.global_handlers.push(idx);
        }

        self.handlers.push(handler);

        for alias_bytes in info.aliases.as_slice() {
            let alias = alias_bytes.as_str().to_owned();
            if self.index.contains_key(alias.as_bytes()) {
                warn!(
                    "alias '{}' already claimed — skipping alias for handler '{}'",
                    alias, ns,
                );
            } else {
                let _ = self.insert_namespace(
                    alias.as_bytes(),
                    idx,
                    &alias,
                    &SxNamespacePolicy::Exclusive,
                );
            }
        }

        Ok(())
    }

    /// Dispatch a query token to all handlers registered for `namespace`.
    ///
    /// Every matching handler receives `value`, `ctx`, and a forwarding sender
    /// that writes into the same result stream as `sender`.
    /// Returns [`Err(UnknownNamespace)`] if no handler owns the namespace.
    ///
    /// If a handler panics while holding the shared sender lock, the lock is
    /// poisoned; subsequent sends log a warning and return `false` (stop) to
    /// the handler rather than propagating the panic.
    pub fn dispatch(
        &self,
        namespace: &[u8],
        value: &str,
        ctx: &SxQueryContext,
        sender: SxResultSender,
    ) -> Result<(), UnknownNamespace> {
        let indices = self
            .index
            .get(namespace)
            .ok_or_else(|| UnknownNamespace(String::from_utf8_lossy(namespace).into()))?;

        // Wrap in Arc<Mutex<…>> so multiple handlers (Shared namespace) can
        // forward into the same stream without SxResultSender: Clone.
        let shared = Arc::new(Mutex::new(sender));
        let value_str = sx_plugin_api::stabby::str::Str::from(value);

        for &idx in indices {
            let shared2 = Arc::clone(&shared);
            let fwd = SxResultSender::from_fn(move |result: SxResult| match shared2.lock() {
                Ok(guard) => guard.send(result),
                Err(_) => {
                    warn!("dispatch: sender mutex poisoned — dropping result");
                    false
                }
            });
            self.handlers[idx].query(value_str, ctx, fwd);
        }
        Ok(())
    }

    /// Returns the union of `file_extensions` (default-on) across all registered handlers.
    ///
    /// Used to build the automatic walk-tier Ext allow-list when no `ext:` filter
    /// is present in the query. Non-file handlers contribute nothing.
    pub fn default_file_extensions(&self) -> HashSet<Box<[u8]>> {
        let mut out = HashSet::new();
        for handler in &self.handlers {
            let info = handler.info();
            for ext in info.file_extensions.as_slice() {
                out.insert(ext.as_bytes().into());
            }
        }
        out
    }

    /// Returns the union of **all** extensions any registered handler can
    /// process — both `file_extensions` (default-on) and
    /// `file_extensions_optional` (opt-in).
    ///
    /// Used to validate user-supplied `ext:` filters: if the user writes
    /// `ext:gz` and no handler claims `.gz` in either list, a warning can be
    /// surfaced at parse time.
    pub fn all_file_extensions(&self) -> HashSet<Box<[u8]>> {
        let mut out = self.default_file_extensions();
        for handler in &self.handlers {
            let info = handler.info();
            for ext in info.file_extensions_optional.as_slice() {
                out.insert(ext.as_bytes().into());
            }
        }
        out
    }

    /// Returns `true` if any registered handler owns `ns`.
    pub fn knows_namespace(&self, ns: &[u8]) -> bool {
        self.index.contains_key(ns)
    }

    /// Returns all registered namespace strings.
    /// Used by the UI for namespace-prefix tab-completion.
    pub fn namespaces(&self) -> Vec<String> {
        self.index
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect()
    }

    /// Call `hints()` on all handlers registered for `namespace` and collect
    /// results as `(label, completion)` string pairs.
    /// Returns an empty vec if the namespace is unknown.
    pub fn hints_for_namespace(&self, ns: &[u8], partial_value: &str) -> Vec<(String, String)> {
        let Some(indices) = self.index.get(ns) else {
            log::debug!(
                "hints_for_namespace: namespace {:?} not found (known: {:?})",
                String::from_utf8_lossy(ns),
                self.index.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>(),
            );
            return vec![];
        };
        let value_str = sx_plugin_api::stabby::str::Str::from(partial_value);
        let mut out = Vec::new();
        for &idx in indices {
            let hints = self.handlers[idx].hints(value_str);
            log::debug!(
                "hints_for_namespace: handler {} returned {} hint(s)",
                idx,
                hints.items.len(),
            );
            for entry in hints.items.as_slice() {
                out.push((
                    entry.label.as_str().to_owned(),
                    entry.completion.as_str().to_owned(),
                ));
            }
        }
        out
    }

    /// Return the static `value_hint` string from the first handler registered
    /// for `namespace`, or an empty string if the namespace is unknown.
    pub fn value_hint_for_namespace(&self, ns: &[u8]) -> String {
        let Some(indices) = self.index.get(ns) else {
            return String::new();
        };
        self.handlers[indices[0]].info().value_hint.as_str().to_owned()
    }

    /// Returns the namespace bytes of every handler that declared
    /// `global_scope: true`, in registration order.
    ///
    /// The parser uses this to synthesise a `HandlerToken` per global handler
    /// for every bare query term so those handlers are invoked alongside the
    /// normal file-walk.
    pub fn global_namespaces(&self) -> Vec<Box<[u8]>> {
        self.global_handlers
            .iter()
            .map(|&idx| {
                let ns = self.handlers[idx].info().namespace;
                Box::from(ns.as_bytes())
            })
            .collect()
    }

    /// Returns all handlers that declared at least one default file extension
    /// (i.e. file handlers).  Used by `compile_with_registry` to build the
    /// per-invocation `SxExtractorHandle` list.
    pub fn file_handlers(&self) -> Vec<Arc<dyn SxHandler>> {
        self.handlers
            .iter()
            .filter(|h| {
                let info = h.info();
                !info.file_extensions.as_slice().is_empty()
                    || !info.file_extensions_optional.as_slice().is_empty()
            })
            .cloned()
            .collect()
    }

    // ── private ───────────────────────────────────────────────────────────────

    /// Insert `idx` into the namespace index for `ns`, honouring the stored
    /// policy of the first registrant (no prior → insert; Exclusive → error;
    /// Shared → append).
    fn insert_namespace(
        &mut self,
        ns: &[u8],
        idx: usize,
        ns_str: &str,
        policy: &SxNamespacePolicy,
    ) -> Result<(), ConflictError> {
        let key: Box<[u8]> = ns.into();

        match self.policies.get(&key) {
            None => {
                self.index.entry(key.clone()).or_default().push(idx);
                self.policies.insert(key, *policy);
            }
            Some(SxNamespacePolicy::Exclusive) => {
                let existing = &self.index[&key];
                let owner_ns = self.handlers[existing[0]].info().namespace;
                return Err(ConflictError {
                    ns: ns_str.into(),
                    claimant: owner_ns.as_str().into(),
                });
            }
            Some(SxNamespacePolicy::Shared) => {
                self.index.entry(key).or_default().push(idx);
            }
        }

        Ok(())
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
