use std::sync::{Arc, Mutex};

use sx_plugin_api::{SxHandler as _, SxQueryContext, SxResultSender};
use sx_plugin_sys::SysHandler;

fn collect_metadata(value: &str) -> Vec<Vec<(String, String)>> {
    let h = SysHandler;
    let ctx = SxQueryContext::for_test();
    let received: Arc<Mutex<Vec<Vec<(String, String)>>>> = Arc::new(Mutex::new(Vec::new()));
    let recv2 = Arc::clone(&received);
    let sender = SxResultSender::from_fn(move |r| {
        let rows: Vec<(String, String)> = r
            .metadata
            .as_slice()
            .iter()
            .map(|e| {
                let k = e.key.as_str().to_owned();
                let v = e.value.clone().match_owned(
                    |t| t.as_str().to_owned(),
                    |n| format!("{n}"),
                    |b| format!("<{} bytes>", b.as_bytes().len()),
                );
                (k, v)
            })
            .collect();
        if !rows.is_empty() {
            recv2.lock().unwrap().push(rows);
        }
        true
    });
    h.query(sx_plugin_api::stabby::str::Str::from(value), &ctx, sender);
    Arc::try_unwrap(received).unwrap().into_inner().unwrap()
}

#[test]
fn handler_info() {
    let h = SysHandler;
    let info = h.info();
    assert_eq!(info.namespace.as_str(), "sys");
    assert_eq!(info.aliases.as_slice().len(), 0);
}

#[test]
fn disk_root_returns_structured() {
    let rows = collect_metadata("disk:/");
    // Should get exactly one structured result with a "path" and "total" key.
    assert!(!rows.is_empty(), "expected at least one disk result");
    let first = &rows[0];
    let has_path = first.iter().any(|(k, _)| k == "path");
    let has_total = first.iter().any(|(k, _)| k == "total");
    assert!(has_path, "expected 'path' key in disk result");
    assert!(has_total, "expected 'total' key in disk result");
}

#[test]
fn disk_nonexistent_returns_error() {
    let rows = collect_metadata("disk:/this_path_surely_does_not_exist_sx_test");
    assert!(!rows.is_empty());
    let first = &rows[0];
    let has_error = first.iter().any(|(k, _)| k == "error");
    assert!(
        has_error,
        "expected 'error' key for bad path, got: {first:?}"
    );
}

#[test]
fn process_with_known_name_returns_results() {
    let rows = collect_metadata("process:rust");
    assert!(!rows.is_empty(), "process query returned nothing at all");
}

#[test]
fn port_invalid_number_returns_error() {
    let rows = collect_metadata("port:not_a_number");
    assert!(!rows.is_empty());
    let first = &rows[0];
    let has_error = first.iter().any(|(k, _)| k == "error");
    assert!(
        has_error,
        "expected 'error' key for invalid port, got: {first:?}"
    );
}

#[test]
fn unknown_subcommand_returns_error() {
    let rows = collect_metadata("foobar:baz");
    assert!(!rows.is_empty());
    let first = &rows[0];
    let has_error = first.iter().any(|(k, _)| k == "error");
    assert!(has_error, "expected 'error' key for unknown sub-command");
}

#[test]
fn fmt_bytes_values() {
    use sx_plugin_sys::fmt_bytes;
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(512), "512 B");
    assert_eq!(fmt_bytes(1024), "1.0 KiB");
    assert_eq!(fmt_bytes(1024 * 1024), "1.0 MiB");
    assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1.0 GiB");
}
