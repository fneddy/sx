//! ABI spike integration test for `sx-plugin-calc` — Phase 8 version.
//!
//! This test validates the full stabby ABI contract:
//! 1. Builds `libsx_plugin_calc.so` via `cargo build`
//! 2. Opens the `.so` with `libloading::Library` (raw dlopen)
//! 3. Loads `sx_plugin_init` via `StabbyLibrary::get_stabbied` — this
//!    performs a TypeReport layout check before returning the symbol
//! 4. Calls the init fn to obtain a `stabby::dynptr!(Box<dyn SxHandler>)`
//! 5. Calls `handler.query("2^32", &ctx, sender)` through the stabby vtable
//! 6. Asserts the channel receives exactly one `SxResult { payload: Scalar("4294967296") }`
//!
//! This validates: symbol export + TypeReport canary generation
//! (`#[stabby::export]`), layout-safe symbol resolution (`get_stabbied`),
//! stabby vtable dispatch across the FFI boundary, `SxResultSender`
//! opaque-pointer round-trip, and `SxResult` ownership.

// The `dynptr!` macro requires `stabby::boxed::Box` in scope to resolve `Box`
// to the stabby heap box (which implements `IPtrOwned`).
use stabby::boxed::Box;

use libloading::Library;
use stabby::libloading::StabbyLibrary;
use sx_plugin_api::{stabby, SxQueryContext, SxResult, SxResultSender};

/// Locate the compiled `.so` for this crate.
fn so_path() -> std::path::PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo test");
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent()
        .expect("manifest dir has a parent");
    workspace_root
        .join("target")
        .join("debug")
        .join("libsx_plugin_calc.so")
}

/// Build the cdylib if not already built.
fn ensure_so_built() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by cargo test");
    let status = std::process::Command::new("cargo")
        .args(["build", "--package", "sx-plugin-calc"])
        .current_dir(&manifest_dir)
        .status()
        .expect("failed to run cargo build");
    assert!(status.success(), "cargo build for sx-plugin-calc failed");
}

#[test]
fn abi_spike_query_2_pow_32_returns_scalar_4294967296() {
    ensure_so_built();

    let path = so_path();
    assert!(
        path.exists(),
        "compiled .so not found at {}: did cargo build succeed?",
        path.display()
    );

    // ── 1. dlopen ─────────────────────────────────────────────────────────────
    // SAFETY: We control the `.so` — it was just built from this workspace.
    let lib =
        unsafe { Library::new(&path) }.unwrap_or_else(|e| panic!("dlopen {:?} failed: {e}", path));

    // ── 2. Load sx_plugin_init via stabby's TypeReport check ──────────────────
    // `get_stabbied` looks up `sx_plugin_init_stabbied_v3`, passes the host's
    // TypeReport to the plugin, and returns `Err` if the layout doesn't match.
    // This is the ABI safety guarantee stabby provides over raw `#[no_mangle]`.
    type DynHandler = stabby::dynptr!(Box<dyn sx_plugin_api::SxHandler + Send + Sync>);
    let init_fn: stabby::libloading::Symbol<extern "C" fn() -> DynHandler> =
        unsafe { lib.get_stabbied::<extern "C" fn() -> DynHandler>(b"sx_plugin_init") }
            .expect("get_stabbied: sx_plugin_init not found or TypeReport mismatch");

    // ── 3. Call init fn to obtain the stabby-boxed handler ────────────────────
    let handler: DynHandler = (*init_fn)();

    // ── 4. Build a result channel ─────────────────────────────────────────────
    use std::sync::{Arc, Mutex};
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received2 = Arc::clone(&received);

    let sender = SxResultSender::from_fn(move |result: SxResult| {
        let value = result.metadata.iter().find(|e| e.key.as_str() == "value")
            .and_then(|e| e.value.clone().match_owned(|t| Some(t.into_string()), |_| None, |_| None));
        if let Some(s) = value {
            received2.lock().unwrap().push(s);
        }
        true
    });

    // ── 5. Call query through the stabby vtable ───────────────────────────────
    use sx_plugin_api::SxHandlerDyn as _;
    let ctx = SxQueryContext::for_test();
    handler.query(stabby::str::Str::from("2^32"), &ctx, sender);

    // ── 6. Assert ─────────────────────────────────────────────────────────────
    let results = received.lock().unwrap();
    assert_eq!(
        results.as_slice(),
        &["4294967296"],
        "expected exactly one Scalar(\"4294967296\") result, got: {:?}",
        *results
    );

    // `handler` drops here — runs the stabby drop vtable entry, which calls
    // the plugin's allocator to free the CalcHandler struct.
    // `lib` drops after `handler` — dlclose is safe only after all vtable
    // pointers are done being used.
}
