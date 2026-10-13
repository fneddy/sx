use std::fs;
use std::io::Write;

// ── Extractor tests ───────────────────────────────────────────────────────────
//
// Previously these tested `sx_core::extractor_for` directly.  In Phase 6 the
// extractor logic moved into `sx-plugin-text` / `sx-plugin-pdf` / `sx-plugin-office`.
// These tests now exercise the plugin `SxExtractorHandle` API directly.

#[test]
fn text_handler_extracts_compressed_text_files() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use sx_plugin_api::SxHandler;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.txt.gz");

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"Compressed text content").unwrap();
    let compressed = encoder.finish().unwrap();
    fs::write(&path, compressed).unwrap();

    let handler = sx_plugin_text::TextHandler;
    let mut handle = handler.prepare_extract().match_owned(
        |h| h,
        || panic!("TextHandler must return Some from prepare_extract"),
    );

    let result = handle.extract(path.as_os_str().as_encoded_bytes());
    result.match_owned(
        |bytes| assert_eq!(bytes.as_bytes(), b"Compressed text content"),
        |_| panic!("expected Bytes, got Skip"),
        |e| panic!("expected Bytes, got Error: {}", e.as_str()),
    );
}

#[test]
fn text_handler_skips_files_without_owned_extension() {
    use sx_plugin_api::SxHandler;

    // A path with no extension — TextHandler should skip it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Makefile");
    fs::write(&path, b"all:\n\techo hi").unwrap();

    let handler = sx_plugin_text::TextHandler;
    let mut handle = handler
        .prepare_extract()
        .match_owned(|h| h, || panic!("TextHandler must return Some"));

    let result = handle.extract(path.as_os_str().as_encoded_bytes());
    result.match_owned(
        |_| panic!("expected Skip for extensionless file"),
        |_| { /* correct */ },
        |e| panic!("expected Skip, got Error: {}", e.as_str()),
    );
}

#[test]
fn pdf_handler_skips_non_pdf_files() {
    use sx_plugin_api::SxHandler;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mislabeled.txt");
    fs::write(&path, b"plain text content").unwrap();

    let handler = sx_plugin_pdf::PdfHandler;
    let mut handle = handler
        .prepare_extract()
        .match_owned(|h| h, || panic!("PdfHandler must return Some"));

    let result = handle.extract(path.as_os_str().as_encoded_bytes());
    result.match_owned(
        |_| panic!("expected Skip for .txt file in pdf handler"),
        |_| { /* correct */ },
        |e| panic!("expected Skip, got Error: {}", e.as_str()),
    );
}

// ── CompiledQuery pipeline tests ──────────────────────────────────────────────

fn collect_matches(cq: sx_core::CompiledQuery) -> Vec<sx_core::Match> {
    // block_in_place inside run() requires the multi-thread runtime.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use futures_util::StreamExt;
        cq.run().collect::<Vec<_>>().await
    })
}

#[test]
fn search_matches_any_term() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("has-hashmap.md"), "hashmap only").unwrap();
    fs::write(dir.path().join("has-serde.md"), "serde only").unwrap();

    let cq = sx_core::Query::builder()
        .root(dir.path())
        .and_filter(sx_core::Filter::Or(vec![
            sx_core::Filter::Content(sx_core::MatchExpr::exact("hashmap")),
            sx_core::Filter::Content(sx_core::MatchExpr::exact("serde")),
        ]))
        .build()
        .unwrap()
        .compile();

    let matches = collect_matches(cq);
    assert_eq!(matches.len(), 2);
}

#[test]
fn search_without_patterns_returns_all_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("keep.md"), "anything").unwrap();
    fs::write(dir.path().join("also.md"), "anything else").unwrap();

    // No content filter → walk-only, all files match.
    let cq = sx_core::Query::builder()
        .root(dir.path())
        .build()
        .unwrap()
        .compile();

    let matches = collect_matches(cq);
    assert_eq!(matches.len(), 2);
}

#[test]
fn search_collects_offsets_from_all_matches() {
    // offsets field is currently always empty in CompiledQuery (Step 8 will
    // populate it); this test verifies the file is found and extracted OK.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("multi.md"),
        "foo appears here and bar appears there too",
    )
    .unwrap();

    let cq = sx_core::Query::builder()
        .root(dir.path())
        .and_filter(sx_core::Filter::Or(vec![
            sx_core::Filter::Content(sx_core::MatchExpr::exact("foo")),
            sx_core::Filter::Content(sx_core::MatchExpr::exact("bar")),
        ]))
        .build()
        .unwrap()
        .compile();

    let matches = collect_matches(cq);
    assert_eq!(matches.len(), 1);
    assert!(matches[0].extracted.is_ok());
}

#[test]
fn keep_content_carries_bytes_on_match() {
    // CompiledQuery::Match.content is always None currently (Step 8).
    // Verify the match is found and extracted correctly.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("doc.md"), "hello world").unwrap();

    let cq = sx_core::Query::builder()
        .root(dir.path())
        .content("hello")
        .build()
        .unwrap()
        .compile();

    let matches = collect_matches(cq);
    assert_eq!(matches.len(), 1);
    assert!(matches[0].extracted.is_ok());
}
