//! # `sx-plugin-api`
//!
//! Stable ABI contract for `sx` handler plugins.
//!
//! Every type in this crate that crosses a plugin boundary is either:
//! - `#[repr(C)]` with only ABI-stable fields, or
//! - annotated with `#[stabby::stabby]` for stabby's layout-stable repr.
//!
//! ## Writing a plugin
//!
//! ```rust,ignore
//! use sx_plugin_api::{SxHandler, SxHandlerInfo, SxQueryContext, SxResultSender,
//!                     SxResult, SxBytes};
//!
//! struct MyHandler;
//!
//! impl SxHandler for MyHandler {
//!     extern "C" fn info(&self) -> SxHandlerInfo { /* ... */ }
//!     extern "C" fn query(&self, value: stabby::str::Str<'_>,
//!                         ctx: &SxQueryContext, sender: SxResultSender) { /* ... */ }
//! }
//!
//! #[stabby::export]
//! pub extern "C" fn sx_plugin_init()
//!     -> stabby::dynptr!(Box<dyn SxHandler + Send + Sync>)
//! {
//!     stabby::boxed::Box::new(MyHandler).into()
//! }
//! ```

// Re-export stabby so plugins only need to depend on sx-plugin-api.
pub use stabby;

/// UTF-8 byte buffer whose memory is owned by the side that allocated it.
///
/// The host allocates `SxBytes` values it builds; plugins allocate the values
/// they return. The rule is simple: **the allocator drops it**.
/// Never pass an `SxBytes` across the FFI boundary and then drop it on the
/// other side unless you are certain both sides use the same global allocator
/// (which is only guaranteed when both are compiled in the same `cargo build`
/// invocation — i.e. for built-in, non-dlopen plugins).
///
/// For Phase 1 (built-in plugins only) this is safe. Phase 8 will address
/// cross-allocator ownership with a `free` function pointer in the type.
#[stabby::stabby]
#[derive(Debug)]
pub struct SxBytes {
    pub ptr: *mut u8,
    pub len: u64,
    pub cap: u64,
}

impl Clone for SxBytes {
    /// Deep copy — allocates a fresh buffer and memcpys the content.
    /// Necessary because `SxBytes` has manual ownership semantics: a derived
    /// bitwise clone would alias the same allocation, causing a double-free
    /// when both copies are later dropped.
    fn clone(&self) -> Self {
        Self::from_vec(self.as_bytes().to_vec())
    }
}

impl Drop for SxBytes {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: same allocator guaranteed for Phase 1 (built-in plugins).
            // ptr/len/cap are exactly the fields of the original Vec<u8>.
            unsafe {
                drop(Vec::from_raw_parts(
                    self.ptr,
                    self.len as usize,
                    self.cap as usize,
                ));
            }
            self.ptr = std::ptr::null_mut();
        }
    }
}

// SAFETY: SxBytes is the layout of a Vec<u8>. Moving across thread/FFI
// boundaries has the same safety as moving a Vec<u8>.
unsafe impl Send for SxBytes {}
unsafe impl Sync for SxBytes {}

impl SxBytes {
    /// Construct from a `Vec<u8>`, consuming it.
    pub fn from_vec(v: Vec<u8>) -> Self {
        let mut v = std::mem::ManuallyDrop::new(v);
        Self {
            ptr: v.as_mut_ptr(),
            len: v.len() as u64,
            cap: v.capacity() as u64,
        }
    }

    /// Construct from a `&str` by cloning the bytes.
    pub fn from_str_bytes(s: &str) -> Self {
        Self::from_vec(s.as_bytes().to_vec())
    }

    /// View as a byte slice. Safe as long as the buffer is still alive.
    pub fn as_bytes(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
    }

    /// View as a `&str`. Returns an empty str for non-UTF-8 content.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// Consume and return the contents as a `Vec<u8>`.
    pub fn into_vec(self) -> Vec<u8> {
        let this = std::mem::ManuallyDrop::new(self);
        if this.ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: ptr/len/cap are exactly the fields of the original Vec<u8>.
        unsafe { Vec::from_raw_parts(this.ptr, this.len as usize, this.cap as usize) }
    }

    /// Consume and return the contents as a `String` (lossily if not UTF-8).
    pub fn into_string(self) -> String {
        String::from_utf8(self.into_vec())
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }
}

/// The value half of a metadata key/value pair.
#[stabby::stabby]
#[repr(stabby)]
#[derive(Clone, Debug)]
pub enum SxMetaValue {
    Text(SxBytes),
    Number(f64),
    /// Raw binary data (e.g. PNG image bytes). Not base64 — just bytes.
    Bytes(SxBytes),
}

/// A single metadata entry (key + value).
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxMetaEntry {
    pub key: SxBytes,
    pub value: SxMetaValue,
}

impl SxMetaEntry {
    /// Construct a text entry from `&str` key and value.
    pub fn text(key: &str, value: &str) -> Self {
        Self {
            key: SxBytes::from_str_bytes(key),
            value: SxMetaValue::Text(SxBytes::from_str_bytes(value)),
        }
    }

    /// Construct a numeric entry from a `&str` key and `f64` value.
    pub fn number(key: &str, value: f64) -> Self {
        Self {
            key: SxBytes::from_str_bytes(key),
            value: SxMetaValue::Number(value),
        }
    }

    /// Construct a raw-bytes entry (e.g. PNG image data).
    pub fn bytes(key: &str, value: Vec<u8>) -> Self {
        Self {
            key: SxBytes::from_str_bytes(key),
            value: SxMetaValue::Bytes(SxBytes::from_vec(value)),
        }
    }
}

/// A single result emitted by a handler.
///
/// All display information beyond `title` is carried in `metadata` as
/// well-known string keys. Renderers inspect the keys they understand and
/// ignore the rest, so new keys can be added without breaking old renderers.
///
/// ## Well-known metadata keys
///
/// | Key           | Value                                          | Used by         |
/// |---------------|------------------------------------------------|-----------------|
/// | `icon_png`    | raw PNG bytes (base64-encoded in Text variant) | graphical UIs   |
/// | `icon_symbol` | UTF-8 emoji / icon-font glyph                  | text renderers  |
/// | `value`       | scalar answer (calc result, temperature…)      | any renderer    |
/// | `file_path`   | absolute filesystem path — open with xdg-open  | file handlers   |
/// | `gio_id`      | GIO application ID — launch with gio            | app launcher    |
/// | `mime`        | MIME type string                               | file handlers   |
///
/// A result with no `title` and empty `metadata` is silently dropped by the host.
#[stabby::stabby(no_opt)]
#[derive(Clone, Debug)]
pub struct SxResult {
    /// The namespace of the handler that produced this result.
    pub source_namespace: SxBytes,
    /// Relevance score in `[0.0, 1.0]`. Handlers set this; the host ranks by it.
    pub score: f64,
    /// Short plain-text label — the primary display string for this result.
    pub title: stabby::option::Option<SxBytes>,
    /// All other data as key/value pairs. Use the well-known keys above.
    pub metadata: stabby::vec::Vec<SxMetaEntry>,
}

impl SxResult {
    /// Construct a minimal result with only a namespace and score set.
    pub fn new(source_namespace: &str, score: f64) -> Self {
        Self {
            source_namespace: SxBytes::from_str_bytes(source_namespace),
            score,
            title: stabby::option::Option::None(),
            metadata: stabby::vec::Vec::new(),
        }
    }
}

/// Helper: push a `Text` metadata entry onto an `SxResult`.
///
/// Equivalent to `result.metadata.push(SxMetaEntry::text(key, value))` but
/// shorter at call sites.
pub fn meta_text(result: &mut SxResult, key: &str, value: &str) {
    result.metadata.push(SxMetaEntry::text(key, value));
}

/// Helper: push a `Number` metadata entry onto an `SxResult`.
pub fn meta_number(result: &mut SxResult, key: &str, value: f64) {
    result.metadata.push(SxMetaEntry::number(key, value));
}

/// Helper: push raw PNG bytes as a `Bytes` metadata entry.
///
/// Graphical renderers extract the bytes directly; text renderers skip
/// `Bytes` entries entirely.
pub fn meta_icon_png(result: &mut SxResult, png: Vec<u8>) {
    result.metadata.push(SxMetaEntry::bytes("icon_png", png));
}

/// Case-sensitivity setting passed to handlers.
#[stabby::stabby]
#[repr(u8)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SxCaseSensitivity {
    /// Case-insensitive when the pattern is all-lowercase.
    Smart,
    Sensitive,
    Insensitive,
}

/// Context the host provides to every `query()` call.
///
/// File handlers use `roots`; non-file handlers (calc, weather, …) ignore it.
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxQueryContext {
    /// Filesystem roots to search. Empty for non-file handlers.
    pub roots: stabby::vec::Vec<SxBytes>,
    /// Maximum number of results to return. `0` = unlimited.
    pub max_results: u64,
    /// Timeout in milliseconds. `0` = unlimited.
    pub timeout_ms: u64,
    pub case_sensitivity: SxCaseSensitivity,
}

impl SxQueryContext {
    /// Construct a minimal context suitable for unit tests.
    pub fn for_test() -> Self {
        Self {
            roots: stabby::vec::Vec::new(),
            max_results: 0,
            timeout_ms: 0,
            case_sensitivity: SxCaseSensitivity::Smart,
        }
    }
}

/// Opaque handle through which a handler pushes results to the host.
///
/// The raw-pointer indirection means the concrete sender type never appears in
/// the ABI — the host can change its channel implementation without breaking
/// compiled plugins.
///
/// All fields are `extern "C"` function pointers with `IStable` arguments,
/// making the struct itself `IStable` once annotated with `#[stabby::stabby]`.
#[stabby::stabby]
pub struct SxResultSender {
    /// Opaque pointer to the channel sender.
    ptr: *mut (),
    /// `fn(ptr, result_ptr)` — takes ownership of a heap-allocated `SxResult`
    /// (via `Box::from_raw`) and delivers it; returns `false` if host quit.
    /// Passing by pointer avoids the `improper_ctypes` lint on stabby enums.
    send_fn: extern "C" fn(*mut (), *mut SxResult) -> bool,
    /// `fn(ptr)` — drops the sender, signalling end-of-stream to the host.
    drop_fn: extern "C" fn(*mut ()),
}

// SAFETY: The underlying sender is Send; handlers use it from one logical context.
unsafe impl Send for SxResultSender {}

impl SxResultSender {
    /// Create an `SxResultSender` from any `Send` closure that accepts results.
    ///
    /// This is the constructor the host uses — it never appears in the plugin ABI.
    pub fn from_fn<F>(f: F) -> Self
    where
        F: FnMut(SxResult) -> bool + Send + 'static,
    {
        let boxed: Box<dyn FnMut(SxResult) -> bool + Send> = Box::new(f);
        let ptr = Box::into_raw(Box::new(boxed)) as *mut ();

        extern "C" fn send_fn(ptr: *mut (), result_ptr: *mut SxResult) -> bool {
            // SAFETY: `result_ptr` was heap-allocated by `send()` below and
            // ownership is transferred here.
            let result = unsafe { *Box::from_raw(result_ptr) };
            // SAFETY: `ptr` was constructed by `Box::into_raw` above and is
            // only dropped in `drop_fn`.
            let f = unsafe { &mut *(ptr as *mut Box<dyn FnMut(SxResult) -> bool + Send>) };
            f(result)
        }

        extern "C" fn drop_fn(ptr: *mut ()) {
            // SAFETY: reconstruct and drop the box.
            unsafe {
                drop(Box::from_raw(
                    ptr as *mut Box<dyn FnMut(SxResult) -> bool + Send>,
                ))
            };
        }

        Self {
            ptr,
            send_fn,
            drop_fn,
        }
    }

    /// Send one result to the host.
    ///
    /// Returns `true` if the host is still listening, `false` if it has quit
    /// or the channel is closed (the handler should stop sending when `false`).
    pub fn send(&self, result: SxResult) -> bool {
        // Heap-allocate to pass through the extern "C" boundary without
        // triggering improper_ctypes on the stabby enum payload.
        let result_ptr = Box::into_raw(Box::new(result));
        (self.send_fn)(self.ptr, result_ptr)
    }
}

impl Drop for SxResultSender {
    fn drop(&mut self) {
        (self.drop_fn)(self.ptr);
    }
}

/// How the registry should handle a second handler attempting to register the
/// same namespace.
///
/// The handler declares this in its [`SxHandlerInfo`]; the registry enforces
/// it without knowing anything about specific namespace names.
///
/// - `Exclusive`: first registration wins (use for namespaces with one correct
///   implementation: `calc`, `weather`, …).
/// - `Shared`: all registrations kept; every handler is called (use for
///   complementary namespaces like `meta` where EXIF + ID3 + PDF all fit).
#[stabby::stabby]
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SxNamespacePolicy {
    /// First registration wins; any subsequent attempt returns [`ConflictError`].
    Exclusive,
    /// All registrations are kept; every registered handler is called on dispatch.
    Shared,
}

/// Describes a handler to the registry. Returned once by `SxHandler::info()`.
///
/// ## Extension lists
///
/// Two separate extension lists give fine-grained control over the default
/// walk-tier allow-list:
///
/// | Field | Meaning | Walk behaviour |
/// |-------|---------|----------------|
/// | `file_extensions` | Extensions walked **by default** | Included in the automatic walk-list whenever this handler is registered |
/// | `file_extensions_optional` | Extensions this handler **can** process but does **not** walk by default | Only walked when the user explicitly requests them (e.g. `ext:gz` in the query, or a future config opt-in) |
///
/// Non-file handlers (calc, weather, …) leave both lists empty.
///
/// ### Example — a hypothetical man-page handler
///
/// ```text
/// file_extensions:          ["1","2","3","4","5","6","7","8","9"]  ← walked by default
/// file_extensions_optional: ["gz"]                                  ← only when user writes ext:gz
/// ```
// `no_opt` disables stabby's niche optimisation for this struct.
// The niche optimiser relies on a single non-zero field being available, but
// `SxHandlerInfo` contains several stabby enums (SxResultKind,
// SxNamespacePolicy) whose niche requirements conflict.  Every other `#[stabby::stabby]`
// type in this crate uses the default (no_opt is absent) because they either
// have plain `#[repr(u8)]` enums or scalar fields that expose a safe niche.
#[stabby::stabby(no_opt)]
#[derive(Clone, Debug)]
pub struct SxHandlerInfo {
    /// Informational version string for the handler itself (shown in UI).
    pub handler_version: SxBytes,
    /// Primary namespace this handler claims (e.g. `b"calc"`, `b"weather"`).
    pub namespace: SxBytes,
    /// Short aliases (e.g. `["="]` for calc).
    pub aliases: stabby::vec::Vec<SxBytes>,
    pub description: SxBytes,
    /// Short example shown when the user has typed `namespace:` but not yet
    /// entered a value.  Use a concrete example (e.g. `"London"`) rather than
    /// abstract syntax.  Empty string means no static hint.
    pub value_hint: SxBytes,
    /// File extensions walked by default when this handler is registered.
    /// Core unions these across all registered handlers to build the automatic
    /// walk-tier Ext allow-list.  Empty for non-file handlers.
    pub file_extensions: stabby::vec::Vec<SxBytes>,
    /// File extensions this handler *can* process but does **not** walk by
    /// default.  Included in the walk-tier only when the user explicitly
    /// requests them via an `ext:` filter or a future config opt-in.
    /// Empty for non-file handlers.
    pub file_extensions_optional: stabby::vec::Vec<SxBytes>,
    /// How the registry handles a second handler claiming the same namespace.
    /// See [`SxNamespacePolicy`] for the two options.
    pub namespace_policy: SxNamespacePolicy,
    /// When `true` the host dispatches this handler for every bare query term
    /// (i.e. queries with no `namespace:` prefix) in addition to explicit
    /// `namespace:value` calls.
    ///
    /// Set this only for handlers that are fast enough to respond within the
    /// interactive typing latency budget (sub-100 ms).  Examples: app launchers
    /// that enumerate a pre-loaded list, or daemon-backed search endpoints that
    /// hold an in-memory index.  Slow handlers (network calls, disk-heavy scans)
    /// must leave this `false` and require the explicit namespace prefix.
    pub global_scope: bool,
}

/// Output returned by [`SxExtractorHandle::extract`].
///
/// Three variants: extracted bytes, skip (file not in scope), or error.
#[stabby::stabby]
#[repr(stabby)]
#[derive(Clone, Debug)]
pub enum SxExtractOutput {
    /// The handler produced extracted bytes for this file.
    /// The host runs content filters against these bytes.
    Bytes(SxBytes),
    /// The handler cannot process this file — skip it silently.
    /// Returned when the file extension / magic bytes do not match.
    Skip(bool),
    /// Extraction was attempted but failed (corrupt file, I/O error, …).
    /// The host surfaces this as an error match rather than silently dropping.
    Error(SxBytes),
}

/// Opaque per-thread file extractor handle.
///
/// The concrete extractor lives behind a raw pointer so the `SxHandler` vtable
/// never needs to name a `dyn Trait` — keeping the type `IStable`.
///
/// `prepare_extract` is called once per worker thread. The returned handle is
/// owned exclusively by that thread for the full search — no shared state on
/// the hot per-file path. Non-file handlers return `None()`.
#[stabby::stabby]
pub struct SxExtractorHandle {
    /// Opaque pointer to the extractor state (allocated by the plugin).
    ptr: *mut (),
    /// `fn(ptr, path_ptr, path_len, out_ptr)` — writes the result into a
    /// heap-allocated `SxExtractOutput` (via `Box::into_raw`) to avoid the
    /// `improper_ctypes` lint on the stabby enum return type.
    extract_fn: extern "C" fn(*mut (), *const u8, u64, *mut SxExtractOutput),
    /// `fn(ptr)` — drops the extractor state.
    drop_fn: extern "C" fn(*mut ()),
}

// SAFETY: The extractor state is Send; each handle is owned by one thread.
unsafe impl Send for SxExtractorHandle {}

impl SxExtractorHandle {
    /// Construct an `SxExtractorHandle` from any `Send` closure.
    ///
    /// Used by file-handler plugins to wrap their extractor state.
    pub fn from_fn<F>(f: F) -> Self
    where
        F: FnMut(&[u8]) -> SxExtractOutput + Send + 'static,
    {
        #[allow(clippy::type_complexity)]
        let boxed: Box<dyn FnMut(&[u8]) -> SxExtractOutput + Send> = Box::new(f);
        let ptr = Box::into_raw(Box::new(boxed)) as *mut ();

        extern "C" fn extract_fn(
            ptr: *mut (),
            path_ptr: *const u8,
            path_len: u64,
            out_ptr: *mut SxExtractOutput,
        ) {
            // SAFETY: path slice reconstructed from caller's pointer+len.
            let path = unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) };
            let f = unsafe { &mut *(ptr as *mut Box<dyn FnMut(&[u8]) -> SxExtractOutput + Send>) };
            let result = f(path);
            // SAFETY: out_ptr was allocated by the caller via Box::into_raw.
            unsafe { out_ptr.write(result) };
        }

        extern "C" fn drop_fn(ptr: *mut ()) {
            unsafe {
                drop(Box::from_raw(
                    ptr as *mut Box<dyn FnMut(&[u8]) -> SxExtractOutput + Send>,
                ))
            }
        }

        Self {
            ptr,
            extract_fn,
            drop_fn,
        }
    }

    /// Extract searchable content from the file at `path` (raw bytes).
    pub fn extract(&mut self, path: &[u8]) -> SxExtractOutput {
        // Stack slot written by extract_fn via pointer — no heap allocation.
        let mut out = std::mem::MaybeUninit::<SxExtractOutput>::uninit();
        (self.extract_fn)(self.ptr, path.as_ptr(), path.len() as u64, out.as_mut_ptr());
        // SAFETY: `extract_fn` always initialises `out`.
        unsafe { out.assume_init() }
    }
}

impl Drop for SxExtractorHandle {
    fn drop(&mut self) {
        (self.drop_fn)(self.ptr);
    }
}

// ── Hint types ────────────────────────────────────────────────────────────────

/// A single completion suggestion returned by [`SxHandler::hints`].
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxHintEntry {
    /// Short text displayed in the hint chip.
    pub label: SxBytes,
    /// Text inserted into the input when this hint is accepted (Tab or click).
    pub completion: SxBytes,
}

/// A list of completion hints returned by [`SxHandler::hints`].
///
/// An empty list means the handler has no completions to offer for the current
/// partial value — the static [`SxHandlerInfo::value_hint`] is shown instead.
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxHints {
    pub items: stabby::vec::Vec<SxHintEntry>,
}

impl SxHints {
    /// Construct an empty hint list.
    pub fn empty() -> Self {
        Self {
            items: stabby::vec::Vec::new(),
        }
    }
}

/// The core trait every `sx` plugin must implement.
///
/// All methods are `extern "C"` as required by stabby's vtable machinery.
/// The unchecked `#[stabby::stabby]` form is used because `SxQueryContext`
/// contains a `stabby::vec::Vec` whose `IStable` bound can only be verified
/// at vtable instantiation time, not at trait definition time.
#[stabby::stabby]
pub trait SxHandler: Send + Sync {
    /// Return metadata describing this handler. Called once at load time.
    extern "C" fn info(&self) -> SxHandlerInfo;

    /// Execute a query token.
    ///
    /// `value` is the part after the namespace colon, e.g. for `calc:2+2`
    /// this receives `"2+2"`. The handler pushes results into `sender` and
    /// returns when done. It may spawn internal threads or tasks; the host
    /// never touches `sender` after this call returns.
    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        ctx: &SxQueryContext,
        sender: SxResultSender,
    );

    /// Return a per-thread file extractor, or `None` if this handler does not
    /// process files.
    ///
    /// Called **once per worker thread** at query compile time.  The returned
    /// [`SxExtractorHandle`] is owned exclusively by that thread for the
    /// duration of the search — no synchronization on the hot per-file path.
    ///
    /// - Non-file handlers (calc, weather, sys, …): return `None()`.
    /// - File handlers (text, pdf, office, …): return `Some(handle)` where the
    ///   handle's closure captures any per-thread extractor state.  The host
    ///   calls `handle.extract(path)` once per candidate file.
    ///
    /// The default implementation returns `None` — non-file handlers need not
    /// override this method.  File handlers override it and return `Some`.
    extern "C" fn prepare_extract(&self) -> stabby::option::Option<SxExtractorHandle>;

    /// Return completion hints for a partial value string.
    ///
    /// Called on every keystroke while the cursor is in this handler's
    /// namespace. **Must return in under a millisecond** — no I/O, no network,
    /// no IPC. Handlers backed by remote/disk data should return [`SxHints::empty()`].
    extern "C" fn hints(&self, partial_value: stabby::str::Str<'_>) -> SxHints;
}

/// Declare the plugin entry point for an `sx` handler.
///
/// Every plugin crate must call this macro exactly once at the crate root,
/// passing the expression that constructs the handler:
///
/// ```rust,ignore
/// sx_plugin_api::declare_plugin!(MyHandler);
/// // or with a constructor call:
/// sx_plugin_api::declare_plugin!(MyHandler::new());
/// ```
///
/// The macro emits `mod _plugin_init { ... }` containing the `#[stabby::export]`
/// function `sx_plugin_init` that `stabby` resolves at load time.  Using a
/// single definition here rather than hand-rolling it in every crate prevents
/// copy-paste divergence in this `unsafe`-adjacent FFI boilerplate.
#[macro_export]
macro_rules! declare_plugin {
    ($handler:expr) => {
        mod _plugin_init {
            // stabby::boxed::Box must shadow std::boxed::Box so that
            // the `dynptr!` macro resolves to the stabby Box, not std's.
            use $crate::stabby::boxed::Box;

            #[$crate::stabby::export]
            pub extern "C" fn sx_plugin_init(
            ) -> $crate::stabby::dynptr!(Box<dyn $crate::SxHandler + Send + Sync>) {
                // The handler expression is captured from the outer scope.
                // Using `#[allow(unused_imports)]` avoids spurious warnings
                // when `super` items are brought into scope.
                #[allow(unused_imports)]
                use super::*;
                Box::new($handler).into()
            }
        }
    };
}
