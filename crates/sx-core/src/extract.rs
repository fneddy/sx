use std::fs;
use std::io::{self, Read};
use std::path::Path;

use log::{debug, trace, warn};

/// Compression format detected from file extension
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Xz,
    Bzip2,
    Zstd,
}

impl Compression {
    /// Detect compression from a file extension (case-insensitive).
    pub fn from_ext(ext: &str) -> Self {
        if ext.eq_ignore_ascii_case("gz") {
            Compression::Gzip
        } else if ext.eq_ignore_ascii_case("xz") {
            Compression::Xz
        } else if ext.eq_ignore_ascii_case("bz2") {
            Compression::Bzip2
        } else if ext.eq_ignore_ascii_case("zst") || ext.eq_ignore_ascii_case("zstd") {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Detect compression from magic bytes.
    pub fn from_magic(data: &[u8]) -> Self {
        detect_compression(data)
    }
}

/// Document format resolved from the (inner) extension. `Plain` covers
/// every extension without a dedicated reader — bytes pass through
/// as-is after decompression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Epub,
    Pdf,
    /// Modern Office formats handled by office_oxide (DOCX, XLSX, PPTX, ODT).
    Office(office_oxide::DocumentFormat),
    /// Legacy Office binary formats handled by office_oxide (DOC, XLS, PPT).
    LegacyOffice(office_oxide::DocumentFormat),
    Plain,
}

impl Format {
    fn from_ext(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "epub" => Format::Epub,
            "pdf" => Format::Pdf,
            "docx" => Format::Office(office_oxide::DocumentFormat::Docx),
            "xlsx" => Format::Office(office_oxide::DocumentFormat::Xlsx),
            "pptx" => Format::Office(office_oxide::DocumentFormat::Pptx),
            // ODT is not a DocumentFormat variant in office_oxide — treat as
            // DOCX internally (both are ZIP+XML; plain_text() handles it).
            "odt" => Format::Office(office_oxide::DocumentFormat::Docx),
            "doc" => Format::LegacyOffice(office_oxide::DocumentFormat::Doc),
            "xls" => Format::LegacyOffice(office_oxide::DocumentFormat::Xls),
            "ppt" => Format::LegacyOffice(office_oxide::DocumentFormat::Ppt),
            _ => Format::Plain,
        }
    }
}

/// Resolve compression + format from a path's extension chain.
/// Returns `None` for files without an extension.
fn resolve_extensions(path: &Path) -> Option<(Compression, Format)> {
    let file_name = path.file_name()?.to_str()?;

    let mut parts = file_name.rsplit('.');
    let last = parts.next()?; // rsplit always yields at least one part
    let prev = parts.next()?; // no second part -> no dot -> no extension

    let compression = Compression::from_ext(last);

    // If compressed, the format ext is the one before the compression suffix
    // (requires a third component; "file.gz" alone has no inner format ext).
    let format_ext = if compression != Compression::None && parts.next().is_some() {
        prev
    } else {
        last
    };

    Some((compression, Format::from_ext(format_ext)))
}

/// Detect compression format from magic bytes.
fn detect_compression(data: &[u8]) -> Compression {
    if let Some(kind) = infer::get(data) {
        match kind.mime_type() {
            "application/gzip" => return Compression::Gzip,
            "application/x-xz" => return Compression::Xz,
            "application/x-bzip2" => return Compression::Bzip2,
            "application/zstd" => return Compression::Zstd,
            _ => {}
        }
    }

    Compression::None
}

/// Detect document format from magic bytes. Returns `None` for plain text
/// (no magic bytes) — the extension is the fallback in that case.
fn detect_format(data: &[u8]) -> Option<Format> {
    let kind = infer::get(data)?;
    match kind.mime_type() {
        "application/pdf" => Some(Format::Pdf),
        "application/epub+zip" => Some(Format::Epub),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some(Format::Office(office_oxide::DocumentFormat::Docx))
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            Some(Format::Office(office_oxide::DocumentFormat::Xlsx))
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Some(Format::Office(office_oxide::DocumentFormat::Pptx))
        }
        "application/vnd.oasis.opendocument.text" => {
            Some(Format::Office(office_oxide::DocumentFormat::Docx))
        }
        "application/msword" => Some(Format::LegacyOffice(office_oxide::DocumentFormat::Doc)),
        _ => None,
    }
}

/// Decompress `data` according to `compression`.
pub fn decompress(data: Vec<u8>, compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data),

        Compression::Gzip => {
            use flate2::read::GzDecoder;
            debug!("extract: decompressing {} bytes with gzip", data.len());
            let mut decoder = GzDecoder::new(&data[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            debug!("extract: gzip decompressed to {} bytes", decompressed.len());
            Ok(decompressed)
        }

        Compression::Xz => {
            use xz2::read::XzDecoder;
            debug!("extract: decompressing {} bytes with xz", data.len());
            let mut decoder = XzDecoder::new(&data[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            debug!("extract: xz decompressed to {} bytes", decompressed.len());
            Ok(decompressed)
        }

        Compression::Bzip2 => {
            use bzip2_rs::DecoderReader;
            debug!("extract: decompressing {} bytes with bzip2", data.len());
            let mut decoder = DecoderReader::new(&data[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            debug!(
                "extract: bzip2 decompressed to {} bytes",
                decompressed.len()
            );
            Ok(decompressed)
        }

        Compression::Zstd => {
            use ruzstd::decoding::StreamingDecoder;
            debug!("extract: decompressing {} bytes with zstd", data.len());
            let cursor = std::io::Cursor::new(&data[..]);
            let mut decoder = StreamingDecoder::new(cursor)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed)?;
            debug!("extract: zstd decompressed to {} bytes", decompressed.len());
            Ok(decompressed)
        }
    }
}

/// Extract plain text from an EPUB archive (concatenates all markup resources).
pub fn read_epub_bytes_plaintext(data: &[u8]) -> io::Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(data);
    let mut doc = epub::doc::EpubDoc::from_reader(cursor)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let resource_ids: Vec<String> = doc.resources.keys().cloned().collect();
    let mut result = Vec::new();

    for resource_id in resource_ids {
        let Some(mime) = doc.get_resource_mime(&resource_id) else {
            continue;
        };

        let is_markup = matches!(
            mime.as_str(),
            "application/xhtml+xml" | "text/html" | "application/xml" | "text/xml"
        );

        if !is_markup {
            continue;
        }

        if let Some((content, _mime)) = doc.get_resource(&resource_id) {
            if !result.is_empty() {
                result.push(b'\n');
            }
            result.extend_from_slice(&content);
        }
    }

    Ok(result)
}

fn read_office_text(data: Vec<u8>, fmt: office_oxide::DocumentFormat) -> io::Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(data);
    let doc = office_oxide::Document::from_reader(cursor, fmt)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(doc.plain_text().into_bytes())
}

/// Returns the appropriate extractor for a path, or `None` if there is no extension.
pub fn extractor_for(path: &Path) -> Option<Extractor> {
    resolve_extensions(path).map(|(compression, format)| Extractor {
        compression,
        format,
    })
}

/// Unified extractor: handles compression and format dispatch.
#[derive(Debug, Clone, Copy)]
pub struct Extractor {
    compression: Compression,
    format: Format,
}

/// Returns `true` if `data` looks like a PDF by magic bytes.
pub fn is_pdf_magic(data: &[u8]) -> bool {
    infer::get(data)
        .map(|t| t.mime_type() == "application/pdf")
        .unwrap_or(false)
}

/// Returns `true` if `data` looks like an Office document by magic bytes.
/// Covers OOXML (DOCX/XLSX/PPTX), ODT, and legacy DOC/XLS/PPT formats.
pub fn is_office_magic(data: &[u8]) -> bool {
    infer::get(data)
        .map(|t| {
            matches!(
                t.mime_type(),
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                    | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                    | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                    | "application/vnd.oasis.opendocument.text"
                    | "application/msword"
            )
        })
        .unwrap_or(false)
}

impl Extractor {
    /// Create an extractor for a raw PDF (no compression).
    pub fn pdf() -> Self {
        Self {
            compression: Compression::None,
            format: Format::Pdf,
        }
    }

    /// Create an extractor for an Office document resolved from its extension.
    ///
    /// Returns `None` for extensions not handled by `office_oxide`
    /// (i.e. not one of `docx xlsx pptx odt doc xls ppt`).
    pub fn office_for_ext(ext: &str) -> Option<Self> {
        let fmt = Format::from_ext(ext);
        match fmt {
            Format::Office(_) | Format::LegacyOffice(_) => {}
            _ => return None,
        }
        Some(Self {
            compression: Compression::None,
            format: fmt,
        })
    }

    /// Returns `true` when the raw file bytes are inherently binary even
    /// though the format is fully searchable after extraction.
    ///
    /// PDF, Office (ZIP-based), EPUB (ZIP-based), and all binary Office
    /// formats contain null bytes in their raw container, so the null-byte
    /// binary probe must be bypassed for them.
    pub fn raw_is_binary_container(&self) -> bool {
        matches!(
            self.format,
            Format::Pdf | Format::Office(_) | Format::Epub | Format::LegacyOffice(_)
        )
    }

    /// Extract searchable content from the file as raw bytes.
    pub fn extract(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.extract_from_bytes(fs::read(path)?)
    }

    /// Extract searchable content from already-loaded raw bytes.
    ///
    /// Callers that have already read the file (e.g. for a binary probe) use
    /// this to avoid a redundant `fs::read`.
    pub fn extract_from_bytes(&self, raw_bytes: Vec<u8>) -> std::io::Result<Vec<u8>> {
        // Fall back to magic-byte detection only when the extension gave no
        // hint. This prevents .run/.bin self-extractors (which embed gzip) from
        // being accidentally decompressed.
        let compression = if self.compression == Compression::None {
            let detected = detect_compression(&raw_bytes);
            if detected != Compression::None {
                trace!("extract: magic-detected compression {:?}", detected);
            }
            detected
        } else {
            self.compression
        };

        let data = decompress(raw_bytes, compression)?;

        // Magic-byte format detection overrides the extension-based guess;
        // extension stays the fallback for formats without magic bytes.
        let format = if let Some(f) = detect_format(&data) {
            trace!("extract: magic-detected format {:?}", f);
            f
        } else {
            self.format
        };

        debug!(
            "extract: format={:?}, {} bytes of data to process",
            format,
            data.len()
        );

        match format {
            Format::Epub => {
                debug!("extract: reading EPUB plain text");
                let result = read_epub_bytes_plaintext(&data);
                if let Ok(ref bytes) = result {
                    debug!("extract: EPUB yielded {} bytes", bytes.len());
                } else if let Err(ref e) = result {
                    warn!("extract: EPUB extraction error: {}", e);
                }
                result
            }

            // ReadingOrder::TopToBottom skips multi-column XY-Cut analysis —
            // unnecessary for substring search and significantly faster.
            Format::Pdf => {
                use pdf_oxide::document::ReadingOrder;
                debug!("extract: reading PDF");
                let doc = pdf_oxide::PdfDocument::from_bytes(data)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                let n = doc
                    .page_count()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                debug!("extract: PDF has {} page(s)", n);
                let mut out = Vec::new();
                for i in 0..n {
                    match doc.extract_spans_with_reading_order(i, ReadingOrder::TopToBottom) {
                        Ok(spans) => {
                            for span in &spans {
                                if !span.text.is_empty() {
                                    out.extend_from_slice(span.text.as_bytes());
                                    out.push(b' ');
                                }
                            }
                            out.push(b'\n');
                        }
                        // A single unreadable page (e.g. scanned image) is not
                        // fatal — skip it and continue with the rest.
                        Err(e) => {
                            warn!("extract: PDF page {} extraction error: {}", i, e);
                            continue;
                        }
                    }
                }
                debug!("extract: PDF yielded {} bytes", out.len());
                Ok(out)
            }

            Format::Office(fmt) | Format::LegacyOffice(fmt) => {
                debug!("extract: reading Office document ({:?})", fmt);
                let result = read_office_text(data, fmt);
                if let Ok(ref bytes) = result {
                    debug!("extract: Office yielded {} bytes", bytes.len());
                } else if let Err(ref e) = result {
                    warn!("extract: Office extraction error: {}", e);
                }
                result
            }
            Format::Plain => {
                debug!("extract: plain text, {} bytes pass-through", data.len());
                Ok(data)
            }
        }
    }
}
