//! # `sx-plugin-sys`
//!
//! Built-in system-query handler for `sx`.
//!
//! Implements the `sys:` namespace with three sub-commands parsed from the
//! query value:
//!
//! | Sub-command | Example | What it does |
//! |-------------|---------|--------------|
//! | `process:<name>` | `sys:process:nginx` | Lists running processes matching `<name>` |
//! | `port:<number>` | `sys:port:8080` | Shows what process is listening on a port |
//! | `disk:<path>` | `sys:disk:/` | Reports filesystem usage for the path |
//!
//! All results carry key/value pairs in the `metadata` field.
//! No filesystem write access; no network; local only.

use sx_plugin_api::{
    stabby, SxBytes, SxHandlerInfo, SxMetaEntry, SxNamespacePolicy, SxQueryContext, SxResult,
    SxResultSender,
};
use sysinfo::System;

fn init_logger() {
    let _ = env_logger::try_init();
}

/// `sys:process:<name>` — list running processes whose name contains `<name>`.
fn query_process(name: &str, sender: &SxResultSender) {
    log::debug!("sys: process query {:?}", name);
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let name_lower = name.to_lowercase();
    let mut found = false;
    let mut count = 0usize;

    for (pid, proc_) in sys.processes() {
        let proc_name = proc_.name().to_string_lossy();
        if proc_name.to_lowercase().contains(&name_lower) {
            log::trace!("sys: process match pid={} name={}", pid.as_u32(), proc_name);
            let mut entries = stabby::vec::Vec::new();
            entries.push(SxMetaEntry::text("pid", &pid.as_u32().to_string()));
            entries.push(SxMetaEntry::text("name", proc_name.as_ref()));
            entries.push(SxMetaEntry::number("cpu_pct", proc_.cpu_usage() as f64));
            entries.push(SxMetaEntry::number(
                "mem_kb",
                (proc_.memory() / 1024) as f64,
            ));
            entries.push(SxMetaEntry::text(
                "status",
                &format!("{:?}", proc_.status()),
            ));
            let exe = proc_
                .exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            if !exe.is_empty() {
                entries.push(SxMetaEntry::text("exe", &exe));
            }

            if !sender.send(SxResult {
                metadata: entries,
                ..SxResult::new("sys", 1.0)
            }) {
                log::debug!("sys: sender closed after {} process results", count);
                return; // host has stopped listening
            }
            found = true;
            count += 1;
        }
    }

    if !found {
        log::info!("sys: no processes matching {:?}", name);
        let mut entries = stabby::vec::Vec::new();
        entries.push(SxMetaEntry::text(
            "result",
            &format!("no processes matching '{name}'"),
        ));
        sender.send(SxResult {
            metadata: entries,
            ..SxResult::new("sys", 0.0)
        });
    } else {
        log::debug!("sys: process query {:?} returned {} result(s)", name, count);
    }
}

/// `sys:port:<number>` — show what is listening on a TCP port.
///
/// Parses `/proc/net/tcp` and `/proc/net/tcp6` on Linux, cross-platform
/// fallback via `sysinfo` socket information.
fn query_port(port_str: &str, sender: &SxResultSender) {
    log::debug!("sys: port query {:?}", port_str);
    let port: u16 = match port_str.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            log::warn!("sys: invalid port number: {:?}", port_str);
            let mut entries = stabby::vec::Vec::new();
            entries.push(SxMetaEntry::text(
                "error",
                &format!("invalid port number: '{port_str}'"),
            ));
            sender.send(SxResult {
                metadata: entries,
                ..SxResult::new("sys", 0.0)
            });
            return;
        }
    };

    #[cfg(target_os = "linux")]
    {
        match linux_port_listeners(port) {
            Some(Ok(results)) => {
                if results.is_empty() {
                    log::info!("sys: nothing listening on port {}", port);
                    let mut entries = stabby::vec::Vec::new();
                    entries.push(SxMetaEntry::text("port", &port.to_string()));
                    entries.push(SxMetaEntry::text(
                        "result",
                        "nothing listening on this port",
                    ));
                    sender.send(SxResult {
                        metadata: entries,
                        ..SxResult::new("sys", 0.0)
                    });
                } else {
                    log::debug!("sys: {} listener(s) on port {}", results.len(), port);
                    for row in results {
                        if !sender.send(SxResult {
                            metadata: row,
                            ..SxResult::new("sys", 1.0)
                        }) {
                            return;
                        }
                    }
                }
                return;
            }
            Some(Err(msg)) => {
                log::warn!("sys: linux_port_listeners error for port {}: {}", port, msg);
                let mut entries = stabby::vec::Vec::new();
                entries.push(SxMetaEntry::text("port", &port.to_string()));
                entries.push(SxMetaEntry::text("error", &msg));
                sender.send(SxResult {
                    metadata: entries,
                    ..SxResult::new("sys", 0.0)
                });
                return;
            }
            None => {
                log::debug!("sys: /proc/net/tcp absent, falling through to sysinfo fallback");
            }
        }
    }

    let listeners = sysinfo_port_listeners(port);
    if listeners.is_empty() {
        log::info!("sys: nothing listening on port {} (sysinfo fallback)", port);
        let mut entries = stabby::vec::Vec::new();
        entries.push(SxMetaEntry::text("port", &port.to_string()));
        entries.push(SxMetaEntry::text(
            "result",
            "nothing listening on this port",
        ));
        sender.send(SxResult {
            metadata: entries,
            ..SxResult::new("sys", 0.0)
        });
    } else {
        log::debug!(
            "sys: {} listener(s) on port {} (sysinfo fallback)",
            listeners.len(),
            port
        );
        for row in listeners {
            if !sender.send(SxResult {
                metadata: row,
                ..SxResult::new("sys", 1.0)
            }) {
                return;
            }
        }
    }
}

/// Query `/proc/net/tcp` and `/proc/net/tcp6` for listeners on `port` via `procfs`.
///
/// Return value:
/// - `Ok(rows)` — parsed successfully (rows may be empty = nothing listening)
/// - `Err(msg)` — `/proc/net/tcp` exists but could not be read (e.g. permission denied)
/// - `None`     — `/proc` is absent (non-Linux kernel or container without procfs)
#[cfg(target_os = "linux")]
fn linux_port_listeners(port: u16) -> Option<Result<Vec<stabby::vec::Vec<SxMetaEntry>>, String>> {
    use procfs::net::TcpState;
    use procfs::process::{all_processes, FDTarget};
    use std::collections::HashMap;

    // Build inode → (pid, comm) map from all open socket file descriptors.
    // Errors from individual processes (race: process exited, permission denied
    // on /proc/<pid>/fd for another user's process) are silently skipped.
    let mut inode_map: HashMap<u64, (i32, String)> = HashMap::new();
    if let Ok(procs) = all_processes() {
        for proc_result in procs {
            let Ok(proc) = proc_result else { continue };
            let (Ok(stat), Ok(fds)) = (proc.stat(), proc.fd()) else {
                continue;
            };
            for fd_result in fds {
                let Ok(fd_info) = fd_result else { continue };
                if let FDTarget::Socket(inode) = fd_info.target {
                    inode_map.insert(inode, (stat.pid, stat.comm.clone()));
                }
            }
        }
    }

    // Read tcp + tcp6 tables; treat a missing /proc/net as "not on Linux".
    let tcp4 = match procfs::net::tcp() {
        Ok(t) => t,
        Err(e) if is_not_found(&e) => return None,
        Err(e) => return Some(Err(format!("cannot read /proc/net/tcp: {e}"))),
    };
    let tcp6 = match procfs::net::tcp6() {
        Ok(t) => t,
        Err(e) if is_not_found(&e) => vec![],
        Err(e) => return Some(Err(format!("cannot read /proc/net/tcp6: {e}"))),
    };

    let mut results = Vec::new();
    for (proto, entry_list) in [("tcp4", tcp4), ("tcp6", tcp6)] {
        for tcp_entry in entry_list {
            if tcp_entry.state != TcpState::Listen {
                continue;
            }
            if tcp_entry.local_address.port() != port {
                continue;
            }
            let mut row = stabby::vec::Vec::new();
            row.push(SxMetaEntry::text("port", &port.to_string()));
            row.push(SxMetaEntry::text("proto", proto));
            row.push(SxMetaEntry::text("state", "LISTEN"));
            if let Some((pid, comm)) = inode_map.get(&tcp_entry.inode) {
                row.push(SxMetaEntry::text("pid", &pid.to_string()));
                row.push(SxMetaEntry::text("name", comm));
            } else {
                row.push(SxMetaEntry::text("pid", &tcp_entry.inode.to_string()));
                row.push(SxMetaEntry::text("name", "(unknown)"));
            }
            results.push(row);
        }
    }
    Some(Ok(results))
}

/// Returns `true` when the `procfs` error indicates a missing file.
#[cfg(target_os = "linux")]
fn is_not_found(e: &procfs::ProcError) -> bool {
    matches!(e, procfs::ProcError::Io(io_err, _)
        if io_err.kind() == std::io::ErrorKind::NotFound)
}

/// Cross-platform fallback for port listeners.
///
/// On Linux the caller already returns early via `linux_port_listeners`, so
/// this path only runs on non-Linux platforms where `/proc/net/tcp` is not
/// available. Rather than returning an empty vec (which the caller turns into a
/// false "nothing listening" result), return a single "not supported" row so
/// the TUI shows the real reason.
fn sysinfo_port_listeners(port: u16) -> Vec<stabby::vec::Vec<SxMetaEntry>> {
    let _ = port;
    let mut row = stabby::vec::Vec::new();
    row.push(SxMetaEntry::text(
        "result",
        "port lookup not supported on this platform",
    ));
    vec![row]
}

/// `sys:disk:<path>` — report filesystem usage via `statvfs`.
fn query_disk(path: &str, sender: &SxResultSender) {
    let path = if path.is_empty() { "/" } else { path };
    log::debug!("sys: disk query {:?}", path);
    let result = statvfs_usage(path);

    match result {
        Ok(rows) => {
            log::debug!("sys: disk query {:?} returned {} row(s)", path, rows.len());
            for row in rows {
                if !sender.send(SxResult {
                    metadata: row,
                    ..SxResult::new("sys", 1.0)
                }) {
                    return;
                }
            }
        }
        Err(msg) => {
            log::warn!("sys: disk query {:?} failed: {}", path, msg);
            let mut entries = stabby::vec::Vec::new();
            entries.push(SxMetaEntry::text("path", path));
            entries.push(SxMetaEntry::text("error", &msg));
            sender.send(SxResult {
                metadata: entries,
                ..SxResult::new("sys", 0.0)
            });
        }
    }
}

/// Call `statvfs(2)` and return human-readable filesystem stats.
fn statvfs_usage(path: &str) -> Result<Vec<stabby::vec::Vec<SxMetaEntry>>, String> {
    use std::ffi::CString;
    let c_path = CString::new(path).map_err(|e| e.to_string())?;

    // SAFETY: statvfs is a well-specified POSIX syscall; c_path is a valid
    // null-terminated string and stat is zero-initialised.
    #[cfg(unix)]
    {
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("statvfs({path}): {err}"));
        }

        let block_size = stat.f_bsize as u64;
        let total_bytes = stat.f_blocks as u64 * block_size;
        let free_bytes = stat.f_bfree as u64 * block_size;
        let avail_bytes = stat.f_bavail as u64 * block_size;
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        let use_pct = if total_bytes > 0 {
            (used_bytes as f64 / total_bytes as f64) * 100.0
        } else {
            0.0
        };

        let mut entries = stabby::vec::Vec::new();
        entries.push(SxMetaEntry::text("path", path));
        entries.push(SxMetaEntry::text("total", &fmt_bytes(total_bytes)));
        entries.push(SxMetaEntry::text("used", &fmt_bytes(used_bytes)));
        entries.push(SxMetaEntry::text("free", &fmt_bytes(free_bytes)));
        entries.push(SxMetaEntry::text("available", &fmt_bytes(avail_bytes)));
        entries.push(SxMetaEntry::number(
            "use_pct",
            (use_pct * 10.0).round() / 10.0,
        ));
        entries.push(SxMetaEntry::number("inodes_free", stat.f_ffree as f64));

        Ok(vec![entries])
    }

    #[cfg(not(unix))]
    {
        let _ = c_path;
        Err("statvfs is not supported on this platform".to_owned())
    }
}

/// Format byte count as a human-readable string (e.g. `12.3 GiB`).
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    for (i, unit) in UNITS.iter().enumerate() {
        if v < 1024.0 || i == UNITS.len() - 1 {
            if i == 0 {
                return format!("{v} {unit}");
            }
            return format!("{:.1} {unit}", v);
        }
        v /= 1024.0;
    }
    format!("{bytes} B")
}

/// Stateless system-query handler.
pub struct SysHandler;

impl sx_plugin_api::SxHandler for SysHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("sys"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Query running processes, open ports, and disk usage \
                 (sys:process:<name>, sys:port:<n>, sys:disk:<path>)",
            ),
            value_hint: SxBytes::from_str_bytes("process:nginx"),
            file_extensions: stabby::vec::Vec::new(),
            file_extensions_optional: stabby::vec::Vec::new(),
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: false,
        }
    }

    extern "C" fn query(
        &self,
        value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        sender: SxResultSender,
    ) {
        init_logger();
        let v = value.as_ref();
        log::debug!("sys: query({:?})", v);
        if let Some(rest) = v.strip_prefix("process:") {
            query_process(rest, &sender);
        } else if let Some(rest) = v.strip_prefix("port:") {
            query_port(rest, &sender);
        } else if let Some(rest) = v.strip_prefix("disk:") {
            query_disk(rest, &sender);
        } else {
            log::warn!("sys: unknown sub-command {:?}", v);
            let mut entries = stabby::vec::Vec::new();
            entries.push(SxMetaEntry::text(
                "error",
                &format!(
                    "unknown sys sub-command '{v}'. \
                     Use: sys:process:<name>, sys:port:<n>, sys:disk:<path>"
                ),
            ));
            sender.send(SxResult {
                metadata: entries,
                ..SxResult::new("sys", 0.0)
            });
        }
    }

    extern "C" fn prepare_extract(
        &self,
    ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
        stabby::option::Option::None()
    }

    extern "C" fn hints(
        &self,
        partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        let v = partial_value.as_ref();

        // Sub-commands: prefix, chip label, completion text
        const SUBCMDS: &[(&str, &str, &str)] = &[
            ("process:", "process:  — by name",   "process:"),
            ("port:",    "port:  — by number",     "port:"),
            ("disk:",    "disk:  — by path",       "disk:"),
        ];

        // Already inside a sub-command?
        for &(prefix, _, _) in SUBCMDS {
            if let Some(rest) = v.strip_prefix(prefix) {
                if rest.is_empty() {
                    // Right after the sub-command colon — show a concrete example.
                    let (label, completion) = match prefix {
                        "process:" => ("process:nginx", "process:nginx"),
                        "port:"    => ("port:8080",     "port:8080"),
                        _          => ("disk:/",        "disk:/"),
                    };
                    let mut items = stabby::vec::Vec::new();
                    items.push(sx_plugin_api::SxHintEntry {
                        label:      SxBytes::from_str_bytes(label),
                        completion: SxBytes::from_str_bytes(completion),
                    });
                    return sx_plugin_api::SxHints { items };
                }
                // User is typing a real value — no hints.
                return sx_plugin_api::SxHints::empty();
            }
        }

        // Not inside a sub-command yet: show all (or prefix-filtered) sub-commands.
        let mut items = stabby::vec::Vec::new();
        for &(prefix, label, completion) in SUBCMDS {
            if v.is_empty() || prefix.starts_with(v) {
                items.push(sx_plugin_api::SxHintEntry {
                    label:      SxBytes::from_str_bytes(label),
                    completion: SxBytes::from_str_bytes(completion),
                });
            }
        }
        sx_plugin_api::SxHints { items }
    }
}

sx_plugin_api::declare_plugin!(SysHandler);
