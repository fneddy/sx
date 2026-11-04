//! Central dispatch table for `sx` handler plugins.
//!
//! Each handler declares its own conflict policy via [`SxNamespacePolicy`];
//! the registry enforces it uniformly.
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

/// Central dispatch table mapping namespace bytes to handler indices.
pub struct HandlerRegistry {
    /// All registered handlers in registration order.
    handlers: Vec<Arc<dyn SxHandler>>,
    /// namespace → handler indices (>1 for Shared namespaces).
    index: HashMap<Box<[u8]>, Vec<usize>>,
    /// Stored policy per namespace, used to validate subsequent registrations.
    policies: HashMap<Box<[u8]>, SxNamespacePolicy>,
    /// Indices of handlers with `global_scope: true`.
    global_handlers: Vec<usize>,
    /// Indices of handlers that have both `global_scope: true` and `is_file_index: true`.
    /// Derived at registration time to avoid an O(n·m) cross-scan at query time.
    global_file_index_handlers: Vec<usize>,
    /// Loaded plugin library handles kept alive until the registry drops.
    /// Opaque so this crate does not depend on `libloading`.
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
            global_file_index_handlers: Vec::new(),
            owned_plugins: Vec::new(),
        }
    }

    /// Keep a plugin handle alive until the registry drops.
    ///
    /// The value is never read again — ownership keeps the `.so` mapped.
    pub(crate) fn keep_plugin<T: Any + Send + Sync + 'static>(&mut self, plugin: T) {
        self.owned_plugins.push(Box::new(plugin));
    }

    /// Register a handler.
    ///
    /// Calls `handler.info()` once to read the namespace, policy, and aliases.
    /// `Exclusive` namespaces reject a second registration with [`ConflictError`];
    /// `Shared` namespaces accumulate all registrants.
    /// Alias conflicts log a warning and skip the alias.
    pub fn register(&mut self, handler: Arc<dyn SxHandler>) -> Result<(), ConflictError> {
        let info: SxHandlerInfo = handler.info();
        let ns = info.namespace.as_str().to_owned();
        let policy = info.namespace_policy;

        // Validate first — a ConflictError must leave the registry untouched.
        let idx = self.handlers.len();
        self.insert_namespace(ns.as_bytes(), idx, &ns, &policy)?;

        if info.global_scope {
            self.global_handlers.push(idx);
        }
        if info.global_scope && info.is_file_index {
            self.global_file_index_handlers.push(idx);
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
    /// Returns [`Err(UnknownNamespace)`] if no handler owns the namespace.
    /// If a handler panics while holding the shared sender lock, subsequent sends
    /// log a warning and return `false` rather than propagating the panic.
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

        // Arc<Mutex<…>> lets Shared-namespace handlers forward into the same stream
        // without requiring SxResultSender: Clone.
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

    /// Returns the union of default `file_extensions` across all registered handlers.
    ///
    /// Used to build the automatic walk-tier Ext allow-list when no `ext:` filter is present.
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

    /// Returns the union of all extensions any registered handler can process
    /// (both default and optional).
    ///
    /// Used to validate user-supplied `ext:` filters.
    pub fn all_file_extensions(&self) -> HashSet<Box<[u8]>> {
        let mut out = HashSet::new();
        for handler in &self.handlers {
            let info = handler.info();
            for ext in info.file_extensions.as_slice() {
                out.insert(ext.as_bytes().into());
            }
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

    /// Snapshot of `SxHandlerInfo` for every registered handler, in registration order.
    pub fn handlers_info(&self) -> Vec<SxHandlerInfo> {
        self.handlers.iter().map(|h| h.info()).collect()
    }

    /// All registered namespace strings (for tab-completion).
    pub fn namespaces(&self) -> Vec<String> {
        self.index
            .keys()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .collect()
    }

    /// Call `hints()` on all handlers for `namespace` and return `(label, completion)` pairs.
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

    /// Return the static `value_hint` from the first handler for `namespace`.
    pub fn value_hint_for_namespace(&self, ns: &[u8]) -> String {
        let Some(indices) = self.index.get(ns) else {
            return String::new();
        };
        self.handlers[indices[0]].info().value_hint.as_str().to_owned()
    }

    /// Namespace bytes of every handler with `global_scope: true`, in registration order.
    ///
    /// The parser synthesises a `HandlerToken` per global handler for every bare
    /// query term so those handlers are invoked alongside the file walk.
    pub fn global_namespaces(&self) -> Vec<Box<[u8]>> {
        self.global_handlers
            .iter()
            .map(|&idx| {
                let ns = self.handlers[idx].info().namespace;
                Box::from(ns.as_bytes())
            })
            .collect()
    }

    /// Global-scope namespaces restricted to file-index handlers (`is_file_index: true`).
    pub fn global_file_index_namespaces(&self) -> Vec<Box<[u8]>> {
        self.global_file_index_handlers
            .iter()
            .map(|&idx| {
                let ns = self.handlers[idx].info().namespace;
                Box::from(ns.as_bytes())
            })
            .collect()
    }

    /// Build the [`NamespaceContext`] that the `sx-parser` grammar needs.
    pub fn namespace_context(&self) -> sx_parser::NamespaceContext {
        sx_parser::NamespaceContext {
            known: self.index.keys().cloned().collect(),
            global: self.global_namespaces(),
            global_file_index: self.global_file_index_namespaces(),
        }
    }

    /// All file handlers (those with at least one declared extension).
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

    /// Insert `idx` into the namespace index for `ns`, honouring the stored policy.
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
