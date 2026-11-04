//! Office document handler (DOCX, XLSX, PPTX, ODT, DOC, XLS, PPT).

use sx_core::extract::{extractor_for, is_office_magic, Extractor};
use sx_plugin_api::{
    stabby, SxBytes, SxExtractOutput, SxExtractorHandle, SxHandlerInfo, SxNamespacePolicy,
    SxQueryContext, SxResultSender,
};

fn init_logger() {
    let _ = env_logger::try_init();
}

pub static FILE_EXTENSIONS: &[&str] = &["docx", "xlsx", "pptx", "odt", "doc", "xls", "ppt"];

fn extract_path(path: &[u8]) -> SxExtractOutput {
    let path_str = match std::str::from_utf8(path) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("office: path is not valid UTF-8, skipping");
            return SxExtractOutput::Skip(false);
        }
    };
    let std_path = std::path::Path::new(path_str);

    let ext = std_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let by_ext = FILE_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext));

    if !by_ext {
        use std::io::Read;
        let mut header = [0u8; 8];
        let n = std::fs::File::open(std_path)
            .and_then(|mut f| f.read(&mut header))
            .unwrap_or(0);
        if !is_office_magic(&header[..n]) {
            log::trace!(
                "office: not an Office file (by ext or magic): {:?}",
                path_str
            );
            return SxExtractOutput::Skip(false);
        }
    }

    let raw = match std::fs::read(std_path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("office: read error for {:?}: {}", path_str, e);
            return SxExtractOutput::Error(SxBytes::from_str_bytes(&e.to_string()));
        }
    };

    let extractor = if by_ext {
        match extractor_for(std_path) {
            Some(e) => e,
            None => {
                log::debug!(
                    "office: no extractor for {:?} (extension {:?}), skipping",
                    path_str,
                    ext
                );
                return SxExtractOutput::Skip(false);
            }
        }
    } else if is_office_magic(&raw) {
        match Extractor::office_for_ext(ext) {
            Some(e) => e,
            // Unknown extension + Office magic: fall back to Docx (handles all OOXML variants).
            None => match Extractor::office_for_ext("docx") {
                Some(e) => e,
                None => {
                    log::debug!(
                        "office: no Docx extractor available, skipping {:?}",
                        path_str
                    );
                    return SxExtractOutput::Skip(false);
                }
            },
        }
    } else {
        log::trace!(
            "office: not an Office file (by ext or magic): {:?}",
            path_str
        );
        return SxExtractOutput::Skip(false);
    };

    log::debug!("office: extracting {:?}", path_str);

    match extractor.extract_from_bytes(raw) {
        Ok(bytes) => {
            log::debug!(
                "office: extraction ok ({} bytes) for {:?}",
                bytes.len(),
                path_str
            );
            SxExtractOutput::Bytes(SxBytes::from_vec(bytes))
        }
        Err(e) => {
            log::warn!("office: extraction error for {:?}: {}", path_str, e);
            SxExtractOutput::Error(SxBytes::from_str_bytes(&e.to_string()))
        }
    }
}

pub struct OfficeHandler;

impl sx_plugin_api::SxHandler for OfficeHandler {
    extern "C" fn info(&self) -> SxHandlerInfo {
        let mut file_extensions = stabby::vec::Vec::new();
        for e in FILE_EXTENSIONS {
            file_extensions.push(SxBytes::from_str_bytes(e));
        }
        SxHandlerInfo {
            handler_version: SxBytes::from_str_bytes(env!("CARGO_PKG_VERSION")),
            namespace: SxBytes::from_str_bytes("office"),
            aliases: stabby::vec::Vec::new(),
            description: SxBytes::from_str_bytes(
                "Office document handler (DOCX, XLSX, PPTX, ODT, DOC, XLS, PPT)",
            ),
            value_hint: SxBytes::from_str_bytes(""),
            file_extensions,
            file_extensions_optional: stabby::vec::Vec::new(),
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

sx_plugin_api::declare_plugin!(OfficeHandler);
