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
#[stabby::stabby]
#[derive(Debug)]
pub struct SxBytes {
    pub ptr: *mut u8,
    pub len: u64,
    pub cap: u64,
}

impl Clone for SxBytes {
    /// Deep copy — a bitwise clone would alias the allocation and double-free.
    fn clone(&self) -> Self {
        Self::from_vec(self.as_bytes().to_vec())
    }
}

impl Drop for SxBytes {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: built-in plugins share the process allocator.
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
    pub fn from_vec(v: Vec<u8>) -> Self {
        let mut v = std::mem::ManuallyDrop::new(v);
        Self {
            ptr: v.as_mut_ptr(),
            len: v.len() as u64,
            cap: v.capacity() as u64,
        }
    }

    pub fn from_str_bytes(s: &str) -> Self {
        Self::from_vec(s.as_bytes().to_vec())
    }

    pub fn as_bytes(&self) -> &[u8] {
        if self.ptr.is_null() || self.len == 0 {
            return &[];
        }
        unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    pub fn into_vec(self) -> Vec<u8> {
        let this = std::mem::ManuallyDrop::new(self);
        if this.ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: ptr/len/cap are exactly the fields of the original Vec<u8>.
        unsafe { Vec::from_raw_parts(this.ptr, this.len as usize, this.cap as usize) }
    }

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
    pub fn text(key: &str, value: &str) -> Self {
        Self {
            key: SxBytes::from_str_bytes(key),
            value: SxMetaValue::Text(SxBytes::from_str_bytes(value)),
        }
    }

    pub fn number(key: &str, value: f64) -> Self {
        Self {
            key: SxBytes::from_str_bytes(key),
            value: SxMetaValue::Number(value),
        }
    }

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

/// Case-sensitivity setting passed to handlers.
// Mirrors `sx_parser::CaseSensitivity`; kept separate for ABI isolation.
// The conversion lives in `sxc::shared::wire` — update both if variants change.
#[stabby::stabby]
#[repr(u8)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SxCaseSensitivity {
    /// Case-insensitive when the pattern is all-lowercase.
    Smart,
    Sensitive,
    Insensitive,
}

#[stabby::stabby]
#[derive(Clone)]
pub struct SxQueryContext {
    /// Filesystem roots to search. Empty for non-file handlers.
    pub roots: stabby::vec::Vec<SxBytes>,
    /// Maximum number of results to return. `0` = unlimited.
    pub max_results: u64,
    /// Timeout in milliseconds. `0` = unlimited.
    pub timeout_ms: u64,
    pub case_sensitivity: SxCaseSensitivity,
    /// Verbatim query string. File-index plugins parse it for their own filters;
    /// entity-search plugins should ignore it.
    pub raw_query: SxBytes,
}

impl SxQueryContext {
    pub fn for_test() -> Self {
        Self {
            roots: stabby::vec::Vec::new(),
            max_results: 0,
            timeout_ms: 0,
            case_sensitivity: SxCaseSensitivity::Smart,
            raw_query: SxBytes::from_str_bytes(""),
        }
    }
}

/// Opaque handle through which a handler pushes results to the host.
///
/// Raw-pointer indirection hides the concrete channel type from the ABI.
#[stabby::stabby]
pub struct SxResultSender {
    ptr: *mut (),
    /// Takes ownership of a heap-allocated `SxResult` via `Box::from_raw`.
    /// Returns `false` when the host has stopped listening.
    send_fn: extern "C" fn(*mut (), *mut SxResult) -> bool,
    drop_fn: extern "C" fn(*mut ()),
}

// SAFETY: The underlying sender is Send; handlers use it from one logical context.
unsafe impl Send for SxResultSender {}

impl SxResultSender {
    /// Host-side constructor. Never appears in the plugin ABI.
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

    /// Returns `false` when the channel is closed; the handler should stop sending.
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
// no_opt: SxHandlerInfo contains several stabby enums with conflicting niche
// requirements; default niche optimisation would miscompile the layout.
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
    /// Short example shown after `namespace:`. Use a concrete value, not abstract syntax.
    pub value_hint: SxBytes,
    /// Extensions walked by default. Core unions these to build the automatic walk-tier allow-list.
    pub file_extensions: stabby::vec::Vec<SxBytes>,
    /// Extensions this handler can process but does not walk by default.
    /// Only included in the walk-tier when the user explicitly requests them.
    pub file_extensions_optional: stabby::vec::Vec<SxBytes>,
    pub namespace_policy: SxNamespacePolicy,
    /// When `true`, the host dispatches this handler for every bare query term
    /// (no `namespace:` prefix). Only set for handlers fast enough for live-search
    /// latency (sub-100 ms) — not for network calls or disk-heavy scans.
    pub global_scope: bool,
    /// When `true`, results from this handler can satisfy file-system filter tokens
    /// (`e:`, `m:`, `s:`, …). The host skips handlers where this is `false`
    /// when the query contains file-system-only filters.
    pub is_file_index: bool,
}

/// Output from [`SxExtractorHandle::extract`]: bytes, skip (not in scope), or error.
#[stabby::stabby]
#[repr(stabby)]
#[derive(Clone, Debug)]
pub enum SxExtractOutput {
    Bytes(SxBytes),
    /// File extension / magic bytes did not match — skip silently.
    Skip(bool),
    /// Extraction failed (corrupt file, I/O error, …); surfaced as an error result.
    Error(SxBytes),
}

/// Opaque per-thread file extractor handle.
///
/// One handle per worker thread — no shared state on the per-file hot path.
/// Non-file handlers return `None()` from `prepare_extract`.
#[stabby::stabby]
pub struct SxExtractorHandle {
    ptr: *mut (),
    extract_fn: extern "C" fn(*mut (), *const u8, u64, *mut SxExtractOutput),
    drop_fn: extern "C" fn(*mut ()),
}

// SAFETY: The extractor state is Send; each handle is owned by one thread.
unsafe impl Send for SxExtractorHandle {}

impl SxExtractorHandle {
    /// Wrap a `Send` closure as an extractor handle. Used by file-handler plugins.
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

    pub fn extract(&mut self, path: &[u8]) -> SxExtractOutput {
        // Written via pointer; no heap allocation.
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

/// A single completion suggestion returned by [`SxHandler::hints`].
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxHintEntry {
    /// Short text displayed in the hint chip.
    pub label: SxBytes,
    /// Text inserted into the input when this hint is accepted (Tab or click).
    pub completion: SxBytes,
}

/// Completion hints from [`SxHandler::hints`]. Empty means no dynamic completions;
/// the static [`SxHandlerInfo::value_hint`] is shown instead.
#[stabby::stabby]
#[derive(Clone, Debug)]
pub struct SxHints {
    pub items: stabby::vec::Vec<SxHintEntry>,
}

impl SxHints {
    pub fn empty() -> Self {
        Self {
            items: stabby::vec::Vec::new(),
        }
    }
}

/// Core trait every plugin must implement. Methods are `extern "C"` for stabby's vtable.
#[stabby::stabby]
pub trait SxHandler: Send + Sync {
    extern "C" fn info(&self) -> SxHandlerInfo;

    /// Execute a query token. `value` is the part after the colon (e.g. `"2+2"` for `calc:2+2`).
    /// Push results into `sender`; `sender` is released when this returns.
    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        ctx: &SxQueryContext,
        sender: SxResultSender,
    );

    /// Return a per-thread extractor, or `None` for non-file handlers.
    /// Called once per worker thread; the handle is owned by that thread
    /// for the whole search — no synchronisation on the per-file hot path.
    extern "C" fn prepare_extract(&self) -> stabby::option::Option<SxExtractorHandle>;

    /// Return hints for a partial value string.
    /// **Must be < 1 ms** — no I/O, network, or IPC.
    extern "C" fn hints(&self, partial_value: stabby::str::Str<'_>) -> SxHints;
}

/// Emits the `#[stabby::export]` entry point `sx_plugin_init`.
/// Call once at the crate root: `declare_plugin!(MyHandler)`.
#[macro_export]
macro_rules! declare_plugin {
    ($handler:expr) => {
        mod _plugin_init {
            // stabby::boxed::Box must shadow std::boxed::Box here.
            use $crate::stabby::boxed::Box;

            #[$crate::stabby::export]
            pub extern "C" fn sx_plugin_init(
            ) -> $crate::stabby::dynptr!(Box<dyn $crate::SxHandler + Send + Sync>) {
                #[allow(unused_imports)]
                use super::*;
                Box::new($handler).into()
            }
        }
    };
}
