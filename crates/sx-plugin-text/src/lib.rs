//! Plain-text, source-code, EPUB, and compressed-text handler.
//!
//! `file_extensions` — walked by default.
//! `file_extensions_optional` — `gz xz bz2 zst zstd`, walked only when explicitly requested.

use std::collections::HashSet;

use once_cell::sync::Lazy;
use sx_core::extract::{decompress, read_epub_bytes_plaintext, Compression};
use sx_plugin_api::{
    stabby, SxBytes, SxExtractOutput, SxExtractorHandle, SxHandlerInfo, SxNamespacePolicy,
    SxQueryContext, SxResultSender,
};

fn init_logger() {
    let _ = env_logger::try_init();
}

/// Extensions walked by default. Compression wrappers are in `FILE_EXTENSIONS_OPTIONAL`.
pub static FILE_EXTENSIONS: &[&str] = &[
    // ── Plain text / docs ──────────────────────────────────────────────────
    "txt", "md", "rst", "adoc", "asciidoc", "log", "text", "org", "pod", "rdoc", "wiki", "creole",
    // ── EPUB ──────────────────────────────────────────────────────────────
    "epub",
    // ── Systems / low-level ───────────────────────────────────────────────
    "c", "h", "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "m", "mm", "rs", "go", "zig",
    "d", "v", "sv", "svh", "vhd", "vhdl",
    // ── JVM ───────────────────────────────────────────────────────────────
    "java", "kt", "kts", "scala", "sc", "groovy", "gradle", "clj", "cljs", "cljc",
    // ── CLR / .NET ────────────────────────────────────────────────────────
    "cs", "fs", "fsi", "fsx", "vb",
    // ── Scripting ─────────────────────────────────────────────────────────
    "py", "pyw", "pyi", "rb", "rake", "pl", "pm", "t", "php", "phtml", "lua", "tcl", "r", "jl",
    "nim", "cr",
    // ── Shell ─────────────────────────────────────────────────────────────
    "sh", "bash", "zsh", "fish", "ksh", "csh", "tcsh", "ps1", "psm1", "psd1", "bat", "cmd",
    // ── Web front-end ─────────────────────────────────────────────────────
    "js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx", "vue", "svelte", "astro", "coffee", "elm",
    "purs", "roc",
    // ── Functional ────────────────────────────────────────────────────────
    "hs", "lhs", "ml", "mli", "re", "rei", "res", "ex", "exs", "erl", "hrl", "gleam", "sml", "lisp",
    "lsp", "cl", "scm", "ss", "rkt", "hy", "el",
    // ── Data / query languages ─────────────────────────────────────────────
    "sql", "graphql", "gql", "proto", "thrift", "capnp", "fbs", "json", "tap", "lock", "css",
    "scss", "sass", "less",
    // ── Man-page sections ──────────────────────────────────────────────────
    "1", "2", "3", "4", "5", "6", "7", "8", "9",
    // ── Build / infra ─────────────────────────────────────────────────────
    "cmake", "make", "bazel", "bzl", "nix", "tf", "tfvars", "bicep", "sls",
    // ── Other compiled / embedded ─────────────────────────────────────────
    "swift", "dart", "pas", "pp", "asm", "s", "f90", "f95", "f03", "f08", "f", "for", "cob", "cbl",
    "ada", "adb", "ads", "pl1", "pro", "wasm", "wat", "ll", "mir", "sol", "move", "cairo", "aleo",
    "leo",
    // ── Markup / config (text-readable) ───────────────────────────────────
    "html", "htm", "xhtml", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf",
    "env",
    // "Dockerfile" and "Makefile" are file names, not extensions; they are
    // excluded by the extension-based walk model.
];

/// Compression extensions — walked only when explicitly requested (`ext:gz` etc.).
pub static FILE_EXTENSIONS_OPTIONAL: &[&str] = &["gz", "xz", "bz2", "zst", "zstd"];

static OWNED_EXTENSIONS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    FILE_EXTENSIONS
        .iter()
        .chain(FILE_EXTENSIONS_OPTIONAL.iter())
        .copied()
        .collect()
});

fn owns_extension(ext: &str) -> bool {
    OWNED_EXTENSIONS.contains(ext.to_ascii_lowercase().as_str())
}

fn is_binary_container(data: &[u8]) -> bool {
    infer::get(data)
        .map(|t| matches!(t.mime_type(), "application/epub+zip"))
        .unwrap_or(false)
}

fn extract_path(path: &[u8]) -> SxExtractOutput {
    let path_str = match std::str::from_utf8(path) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("text: path is not valid UTF-8, skipping");
            return SxExtractOutput::Skip(false);
        }
    };
    let std_path = std::path::Path::new(path_str);

    let file_name = match std_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => {
            log::debug!("text: no file name component for {:?}, skipping", path_str);
            return SxExtractOutput::Skip(false);
        }
    };

    let mut parts = file_name.rsplit('.');
    let last = match parts.next() {
        Some(e) => e,
        None => {
            log::debug!("text: no extension for {:?}, skipping", path_str);
            return SxExtractOutput::Skip(false);
        }
    };
    let prev = parts.next(); // may be None for single-component names

    let compression = Compression::from_ext(last);
    let inner_ext = if compression != Compression::None {
        match prev {
            Some(e) if parts.next().is_some() => e, // e.g. foo.txt.gz → txt
            _ => last,                              // e.g. foo.gz
        }
    } else {
        last
    };

    if !owns_extension(inner_ext) {
        log::trace!(
            "text: extension {:?} not owned, skipping {:?}",
            inner_ext,
            path_str
        );
        return SxExtractOutput::Skip(false);
    }

    log::debug!(
        "text: extracting {:?} (compression={:?}, inner_ext={:?})",
        path_str,
        compression,
        inner_ext
    );

    let raw = match std::fs::read(std_path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("text: read error for {:?}: {}", path_str, e);
            return SxExtractOutput::Error(SxBytes::from_str_bytes(&e.to_string()));
        }
    };

    let raw_is_container = is_binary_container(&raw);

    // Prefer extension over magic to avoid treating .run/.bin self-extractors as gz.
    let compression = if compression == Compression::None {
        Compression::from_magic(&raw)
    } else {
        compression
    };

    let data = match decompress(raw, compression) {
        Ok(d) => d,
        Err(e) => {
            log::warn!("text: decompression error for {:?}: {}", path_str, e);
            return SxExtractOutput::Error(SxBytes::from_str_bytes(&e.to_string()));
        }
    };

    if !raw_is_container && data.contains(&b'\0') {
        log::debug!("text: binary content detected in {:?}, skipping", path_str);
        return SxExtractOutput::Skip(false);
    }

    if inner_ext.eq_ignore_ascii_case("epub") {
        log::debug!("text: extracting EPUB {:?}", path_str);
        return match read_epub_bytes_plaintext(&data) {
            Ok(bytes) => {
                log::debug!(
                    "text: EPUB extraction ok ({} bytes) for {:?}",
                    bytes.len(),
                    path_str
                );
                SxExtractOutput::Bytes(SxBytes::from_vec(bytes))
            }
            Err(e) => {
                log::warn!("text: EPUB extraction error for {:?}: {}", path_str, e);
                SxExtractOutput::Error(SxBytes::from_str_bytes(&e.to_string()))
            }
        };
    }

    log::debug!(
        "text: plain extraction ok ({} bytes) for {:?}",
        data.len(),
        path_str
    );
    SxExtractOutput::Bytes(SxBytes::from_vec(data))
}

pub struct TextHandler;

impl sx_plugin_api::SxHandler for TextHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        let mut file_extensions = stabby::vec::Vec::new();
        for e in FILE_EXTENSIONS {
            file_extensions.push(SxBytes::from_str_bytes(e));
        }
        let mut file_extensions_optional = stabby::vec::Vec::new();
        for e in FILE_EXTENSIONS_OPTIONAL {
            file_extensions_optional.push(SxBytes::from_str_bytes(e));
        }
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("text"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Plain text, source code, EPUB, and compressed text files",
            ),
            value_hint: SxBytes::from_str_bytes(""),
            file_extensions,
            file_extensions_optional,
            namespace_policy: SxNamespacePolicy::Exclusive,
            global_scope: false,
            is_file_index: false,
        }
    }

    extern "C" fn query(
        &self,
        _value: stabby::str::Str<'_>,
        _ctx: &SxQueryContext,
        _sender: SxResultSender,
    ) {
    }

    extern "C" fn prepare_extract(&self) -> stabby::option::Option<SxExtractorHandle> {
        init_logger();
        stabby::option::Option::Some(SxExtractorHandle::from_fn(extract_path))
    }

    extern "C" fn hints(
        &self,
        _partial_value: stabby::str::Str<'_>,
    ) -> sx_plugin_api::SxHints {
        sx_plugin_api::SxHints::empty()
    }
}

sx_plugin_api::declare_plugin!(TextHandler);
