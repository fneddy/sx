//! Discovers and loads external `sx` handler plugins from `.so` / `.dll` files
//! at runtime using stabby's ABI-safe symbol loading.
//!
//! Each `.so` must export (via [`sx_plugin_api::declare_plugin!`]):
//! ```text
//! #[stabby::export]
//! pub extern "C" fn sx_plugin_init()
//!     -> stabby::dynptr!(Box<dyn SxHandler + Send + Sync>)
//! ```
//!
//! Loading sequence in [`PluginStore::try_load`]:
//! 1. **ELF probe** (pre-`dlopen`): read the file without executing any code and
//!    check that `sx_plugin_init_stabbied_v3_report` is exported.  Any `.so`
//!    that lacks this symbol is not an sx plugin — reject before constructors run.
//!    Symbol name verified against stabby 72.1.16 via `nm -D` on a built plugin.
//! 2. `dlopen` the file.
//! 3. Call `sx_plugin_init_stabbied_v3_report` — a pure data accessor generated
//!    by `#[stabby::export]`; returns the TypeReport without plugin business logic.
//! 4. Compare against the host's `DynHandler::REPORT`; reject on mismatch.
//! 5. Call `get_stabbied` to obtain the verified init function pointer and invoke it.
//!
//! The loaded `Library` must outlive the handler it produced. [`LoadedPlugin`]
//! keeps both together; [`PluginStore`] owns all loaded plugins for the
//! lifetime of the host process.

use std::{fs::File, path::Path, path::PathBuf, sync::Arc};

use libloading::Library;
use log::{debug, info, warn};
use memmap2::Mmap;
// stabby::boxed::Box must be in scope so `dynptr!(Box<dyn Trait>)` resolves to
// `stabby::boxed::Box` (which impls `IPtrOwned`) rather than `std::boxed::Box`.
// Use `std::boxed::Box` explicitly where the std type is needed.
#[allow(unused_imports)]
use stabby::boxed::Box;
use stabby::libloading::StabbyLibrary;
// SxHandlerDyn (stabby's generated vtable trait) must be in scope to call
// info/query/etc. on a dynptr value.
use sx_plugin_api::{SxHandler, SxHandlerDyn};

use crate::handler_registry::ConflictError;
use crate::HandlerRegistry;

// ── DynHandler type alias ─────────────────────────────────────────────────────

/// The stabby fat-pointer type returned by `sx_plugin_init`.
type DynHandler = stabby::dynptr!(Box<dyn SxHandler + Send + Sync>);

// ── LoadedPlugin ──────────────────────────────────────────────────────────────

/// A plugin library kept alive alongside its handler.
///
/// The handler holds vtable pointers into the `.so` — the `Library` **must**
/// not be dropped before the handler.  `LoadedPlugin` guarantees this by
/// holding both together and dropping `handler` before `lib`.
pub struct LoadedPlugin {
    /// The handler produced by this plugin.
    pub handler: Arc<dyn SxHandler>,
    /// The open library — must outlive `handler`.
    _lib: Library,
}

// ── PluginStore ───────────────────────────────────────────────────────────────

/// Owns all dynamically loaded plugins for the lifetime of the host process.
///
/// Call [`PluginStore::load_dir`] and/or [`PluginStore::load_file`] to
/// populate the store, then [`PluginStore::register_all`] to add every
/// successfully registered handler into a [`HandlerRegistry`].
///
/// **Important:** plugins that fail to register (namespace conflict) are dropped
/// immediately — their shared library is closed right away.  This prevents
/// `dlclose` happening later (at `PluginStore` drop time) from invalidating
/// vtable pointers of handlers that *did* register successfully.
#[derive(Default)]
pub struct PluginStore {
    plugins: Vec<LoadedPlugin>,
}

impl PluginStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `dir` for `*.so` (Linux/macOS) or `*.dll` (Windows) files and
    /// attempt to load each as an `sx` plugin.
    ///
    /// Files that fail to load (missing symbol, ABI mismatch, OS error) emit a
    /// `warn!` log and are skipped — a single bad plugin never prevents the
    /// host from starting.
    ///
    /// Candidates are loaded sequentially to avoid glibc dynamic linker
    /// lock (`_dl_load_lock`) contention and thread-pool startup overhead.
    pub fn load_dir(&mut self, dir: &Path) {
        let ext = if cfg!(target_os = "windows") {
            "dll"
        } else {
            "so"
        };
        let prefix = if cfg!(target_os = "windows") {
            "sx_plugin_"
        } else {
            "libsx_plugin_"
        };

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                // Non-existent plugin dir is normal on a fresh install — debug only.
                log::debug!("plugin dir {:?}: {e}", dir);
                return;
            }
        };

        // Collect all filename-matching candidates.
        let candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                p.extension().and_then(|e| e.to_str()) == Some(ext) && name.starts_with(prefix)
            })
            .collect();

        let n_candidates = candidates.len();
        debug!("plugin dir {:?}: {} candidate(s)", dir, n_candidates);

        let mut loaded = Vec::with_capacity(n_candidates);
        for path in &candidates {
            match self.try_load(path) {
                Ok(plugin) => {
                    let ns = plugin.handler.info().namespace.as_str().to_owned();
                    info!("loaded plugin '{}' from {:?}", ns, path);
                    loaded.push(plugin);
                }
                Err(e) => {
                    warn!("ignoring plugin {:?}: {}", path, e);
                }
            }
        }

        debug!(
            "plugin dir {:?}: {}/{} loaded",
            dir,
            loaded.len(),
            n_candidates
        );

        self.plugins.extend(loaded);
    }

    /// Load a single plugin file.  Logs a warning and returns on any error.
    pub fn load_file(&mut self, path: &Path) {
        match self.try_load(path) {
            Ok(plugin) => {
                let ns = plugin.handler.info().namespace.as_str().to_owned();
                info!("loaded plugin '{}' from {:?}", ns, path);
                self.plugins.push(plugin);
            }
            Err(e) => {
                warn!("ignoring plugin {:?}: {}", path, e);
            }
        }
    }

    /// Register all successfully loaded handlers into `registry`.
    ///
    /// Consumes `self` so that plugins which fail to register (namespace
    /// conflict) are **dropped immediately** — their `.so` is closed right
    /// away.  This prevents a later `dlclose` from invalidating vtable
    /// pointers of handlers that were successfully registered.
    pub fn register_all(self, registry: &mut HandlerRegistry) {
        for plugin in self.plugins {
            match registry.register(Arc::clone(&plugin.handler)) {
                Ok(()) => {
                    // Transfer ownership into the registry so the .so stays
                    // mapped for the process lifetime.
                    registry.keep_plugin(plugin);
                }
                Err(ConflictError { ns, claimant }) => {
                    warn!(
                        "plugin for '{}' conflicts with existing handler '{}' — skipping",
                        ns, claimant
                    );
                    // Drop here: dlclose now, not later when PluginStore drops.
                }
            }
        }
    }

    fn try_load(
        &self,
        path: &Path,
    ) -> Result<LoadedPlugin, std::boxed::Box<dyn std::error::Error + Send + Sync>> {
        // Step 1 — ELF probe (pre-dlopen).
        // Read the file without executing any code and verify the report symbol
        // exists.  This is the only guard against dlopen crashing on a stale or
        // corrupt .so whose global constructors are broken.
        if !has_report_symbol(path) {
            return Err(
                "not an sx plugin: missing `sx_plugin_init_stabbied_v3_report` symbol".into(),
            );
        }

        // Step 2 — dlopen.
        // SAFETY: ELF probe confirmed the file has the expected sx plugin symbol.
        let lib = unsafe { Library::new(path) }?;

        // Step 3 — call the pure report accessor.
        // `sx_plugin_init_stabbied_v3_report` is generated by `#[stabby::export]`
        // and returns the plugin's TypeReport without running any plugin logic.
        let report_fn = unsafe {
            lib.get::<extern "C" fn() -> &'static stabby::abi::report::TypeReport>(
                b"sx_plugin_init_stabbied_v3_report\0",
            )
        }?;

        // Step 4 — compare TypeReports.
        let plugin_report = report_fn();
        let host_report = <extern "C" fn() -> DynHandler as stabby::abi::IStable>::REPORT;
        if plugin_report != host_report {
            return Err(
                "ABI mismatch (plugin built against a different sx-plugin-api version)".into(),
            );
        }

        // Step 5 — verified: call get_stabbied to obtain the init fn pointer.
        let init_fn: stabby::libloading::Symbol<extern "C" fn() -> DynHandler> =
            unsafe { lib.get_stabbied::<extern "C" fn() -> DynHandler>(b"sx_plugin_init")? };

        let dyn_handler: DynHandler = (*init_fn)();
        let handler: Arc<dyn SxHandler> = Arc::new(StabbyHandlerWrapper(dyn_handler));

        Ok(LoadedPlugin { handler, _lib: lib })
    }
}

// ── StabbyHandlerWrapper ──────────────────────────────────────────────────────

/// Bridges a `stabby::dynptr!(Box<dyn SxHandler>)` into a plain
/// `Arc<dyn SxHandler>` that the host's `HandlerRegistry` holds.
///
/// All method calls forward through the stabby vtable.
struct StabbyHandlerWrapper(DynHandler);

// SAFETY: `DynHandler` is `Box<dyn SxHandler + Send + Sync>` under stabby,
// which is Send + Sync.
unsafe impl Send for StabbyHandlerWrapper {}
unsafe impl Sync for StabbyHandlerWrapper {}

impl SxHandler for StabbyHandlerWrapper {
    extern "C" fn info(&self) -> sx_plugin_api::SxHandlerInfo {
        self.0.info()
    }

    extern "C" fn query(
        &self,
        value: sx_plugin_api::stabby::str::Str<'_>,
        ctx: &sx_plugin_api::SxQueryContext,
        sender: sx_plugin_api::SxResultSender,
    ) {
        self.0.query(value, ctx, sender)
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> sx_plugin_api::stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        self.0.prepare_extract()
    }

    extern "C" fn hints(
        &self,
        partial_value: sx_plugin_api::stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        self.0.hints(partial_value)
    }
}

// ── ELF/PE symbol probe ───────────────────────────────────────────────────────

/// Returns `true` if the file at `path` exports `sx_plugin_init_stabbied_v3_report`.
///
/// This uses memory-mapping (`mmap`) rather than reading the entire file into
/// memory — the library is never `dlopen`-ed, so no global constructors run.
/// A stale or corrupt `.so` cannot crash the host process during this check.
fn has_report_symbol(path: &Path) -> bool {
    const REQUIRED: &[u8] = b"sx_plugin_init_stabbied_v3_report";

    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            debug!("symbol probe: could not open {:?}: {e}", path);
            return false;
        }
    };

    // SAFETY: We only read from the mapped memory to parse the ELF export table.
    let mmap = match unsafe { Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            debug!("symbol probe: could not mmap {:?}: {e}", path);
            return false;
        }
    };

    use object::Object;
    let obj = match object::File::parse(&mmap[..]) {
        Ok(o) => o,
        Err(e) => {
            debug!(
                "symbol probe: {:?} is not a recognised object file: {e}",
                path
            );
            return false;
        }
    };

    obj.exports()
        .map(|exports| {
            exports
                .flatten()
                .any(|sym| matches!(sym.name(), object::NameOrOrdinal::Name(n) if n == REQUIRED))
        })
        .unwrap_or(false)
}
