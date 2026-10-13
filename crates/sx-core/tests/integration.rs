// ── CompiledQuery::run() integration tests ────────────────────────────────────
//
// These tests use a real temp directory, compile a Query, call run(), and
// collect matches via a single-task sequential stream poll.

#[cfg(test)]
mod compiled_query_tests {
    use futures_util::StreamExt;
    use sx_core::{CompiledQuery, Filter, HiddenMode, MatchExpr, Query};
    use tempfile::TempDir;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Collect all matched paths from a CompiledQuery, sorted for determinism.
    async fn run_sorted(cq: CompiledQuery) -> Vec<String> {
        let mut names: Vec<String> = cq
            .run()
            .map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
            .await;
        names.sort();
        names
    }

    // ── walk-only (no content scan) ───────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_only_ext_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.path().join("lib.rs"), b"pub fn lib() {}").unwrap();
        std::fs::write(dir.path().join("build.py"), b"print('hi')").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .ext(["rs"])
            .build()
            .unwrap()
            .compile();

        assert!(
            cq.content.is_none(),
            "walk-only query must have no content filter"
        );
        let names = run_sorted(cq).await;
        assert_eq!(names, ["lib.rs", "main.rs"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_only_name_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), b"").unwrap();
        std::fs::write(dir.path().join("lib.rs"), b"").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .name("main")
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["main.rs"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_only_size_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("big.txt"), b"hello world!!!").unwrap(); // 14 bytes
        std::fs::write(dir.path().join("tiny.txt"), b"hi").unwrap(); // 2 bytes

        let cq = Query::builder()
            .root(dir.path())
            .size_gt(5)
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["big.txt"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_only_hidden_exclude() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .hidden(HiddenMode::Exclude)
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["visible.txt"]);
    }

    /// Dot-files are excluded by default even when there is no explicit
    /// `Hidden` filter in the query — the walker applies `HiddenMode::Exclude`
    /// implicitly at the `filter_entry` level.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dot_files_excluded_by_default() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::write(dir.path().join(".gitignore"), b"target/").unwrap();
        std::fs::write(dir.path().join("main.rs"), b"fn main() {}").unwrap();

        // No hidden filter — just an ext filter.
        let cq = Query::builder()
            .root(dir.path())
            .ext(["rs", "env", "gitignore"])
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(
            names,
            ["main.rs"],
            "dot-files must be excluded without explicit hidden:include"
        );
    }

    /// `hidden:include` overrides the implicit exclusion and dot-files appear.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dot_files_visible_with_hidden_include() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), b"SECRET=1").unwrap();
        std::fs::write(dir.path().join("main.rs"), b"fn main() {}").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .hidden(HiddenMode::Include)
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, [".env", "main.rs"]);
    }

    // ── content filter ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_filter_exact_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("has_todo.rs"),
            b"// TODO: fix this\nfn foo() {}",
        )
        .unwrap();
        std::fs::write(dir.path().join("clean.rs"), b"// all good\nfn bar() {}").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .content("TODO")
            .build()
            .unwrap()
            .compile();

        assert!(
            cq.content.is_some(),
            "content query must have a content filter"
        );
        let names = run_sorted(cq).await;
        assert_eq!(names, ["has_todo.rs"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_filter_negated() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("generated.rs"), b"// generated by protoc").unwrap();
        std::fs::write(dir.path().join("handwritten.rs"), b"fn main() {}").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .and_filter(Filter::Content(MatchExpr::exact("generated").negated()))
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["handwritten.rs"]);
    }

    // ── binary content guard ─────────────────────────────────────────────────

    /// Binary files (null byte in first 8 KiB) are skipped by default even
    /// when a content filter is present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_files_excluded_by_default() {
        let dir = TempDir::new().unwrap();
        // A file with a null byte — detected as binary.
        let mut binary = b"ELF\x00binary data".to_vec();
        binary.extend_from_slice(b"TODO"); // pattern is present but should be ignored
        std::fs::write(dir.path().join("binary.elf"), &binary).unwrap();
        // A plain text file that does match.
        std::fs::write(dir.path().join("source.rs"), b"// TODO: fix").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .content("TODO")
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(
            names,
            ["source.rs"],
            "binary file must be excluded from content scan"
        );
    }

    /// With `include_binary_content(true)`, binary files are scanned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binary_files_included_when_opted_in() {
        let dir = TempDir::new().unwrap();
        let binary = b"ELF\x00binary data TODO".to_vec();
        std::fs::write(dir.path().join("binary.elf"), &binary).unwrap();
        std::fs::write(dir.path().join("source.rs"), b"// TODO: fix").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .content("TODO")
            .include_binary_content(true)
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["binary.elf", "source.rs"]);
    }

    // ── combined walk + content ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_and_content_combined() {
        let dir = TempDir::new().unwrap();
        // .rs files
        std::fs::write(dir.path().join("a.rs"), b"fn unwrap() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"fn clean() {}").unwrap();
        // .go file — should be excluded by ext filter before content is even read
        std::fs::write(dir.path().join("c.go"), b"fn unwrap() {}").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .ext(["rs"])
            .content("unwrap")
            .build()
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["a.rs"]);
    }

    // ── compile() → run() shared-work ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiled_query_clone_shares_work() {
        // Clones share the same WorkState — cloning before run() lets multiple
        // tasks cooperate on one walk.  Here we verify that two clones driven
        // concurrently with tokio::join! together collect all files exactly once.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"world").unwrap();

        let cq = Query::builder()
            .root(dir.path())
            .ext(["txt"])
            .build()
            .unwrap()
            .compile();

        let cq2 = cq.clone();
        let (mut r1, mut r2): (Vec<String>, Vec<String>) = tokio::join!(
            async {
                cq.run()
                    .map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect()
                    .await
            },
            async {
                cq2.run()
                    .map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned())
                    .collect()
                    .await
            }
        );
        // Together the two streams must cover all files exactly once.
        let mut combined = {
            r1.append(&mut r2);
            r1
        };
        combined.sort();
        assert_eq!(combined, ["a.txt", "b.txt"]);
    }

    // ── from_str → compile → run ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn from_str_compile_run() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), b"fn main() { todo!() }").unwrap();
        std::fs::write(dir.path().join("lib.rs"), b"pub fn helper() {}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"todo: remember this").unwrap();

        // "e:rs c:todo" — "todo" is all-lowercase so Smart → insensitive;
        // "todo!()" in main.rs and "todo:" in notes.txt both match the substring.
        // But the ext:rs filter ensures only .rs files pass the walk tier.
        let cq = Query::from_str_with_roots("e:rs c:todo", vec![dir.path().to_owned()])
            .unwrap()
            .compile();

        let names = run_sorted(cq).await;
        assert_eq!(names, ["main.rs"]);
    }
}

// ── QueryBuilder + partitioner tests ─────────────────────────────────────────

#[cfg(test)]
mod query_builder_tests {
    use sx_core::{BuildError, Filter, MatchExpr, Query, RangeExpr};

    // Helper: build a query from a root "." plus whatever filters the builder adds.
    macro_rules! q {
        ($builder:expr) => {
            $builder.root(".").build().expect("build failed")
        };
    }

    // ── Mixed And splits correctly ────────────────────────────────────────────

    #[test]
    fn and_splits_walk_and_content() {
        // ext:rs + content:unwrap → walk: Ext, content: Content
        let query = q!(Query::builder().ext(["rs"]).content("unwrap"));
        assert_eq!(
            query.walk_filter,
            Filter::Ext(vec![Box::from(b"rs" as &[u8])])
        );
        assert_eq!(
            query.content_filter,
            Filter::Content(MatchExpr::exact("unwrap"))
        );
    }

    #[test]
    fn and_three_filters_splits_correctly() {
        // ext:rs + size_gt(100) + content:todo
        // walk: And([Ext, Size]), content: Content
        let query = q!(Query::builder().ext(["rs"]).size_gt(100).content("todo"));
        assert_eq!(
            query.walk_filter,
            Filter::And(vec![
                Filter::Ext(vec![Box::from(b"rs" as &[u8])]),
                Filter::Size(RangeExpr::Gt(100)),
            ])
        );
        assert_eq!(
            query.content_filter,
            Filter::Content(MatchExpr::exact("todo"))
        );
    }

    #[test]
    fn multiple_path_nots_and_size_produce_and_in_walk() {
        // size_gt + path_not(".git") + path_not("node_modules") → walk: And([...]), content: Any
        let query = q!(Query::builder()
            .size_gt(100 * 1024 * 1024)
            .path_not(".git")
            .path_not("node_modules"));
        assert_eq!(query.content_filter, Filter::Any);
        // walk_filter should be And with 3 children
        match &query.walk_filter {
            Filter::And(children) => assert_eq!(children.len(), 3),
            other => panic!("expected And, got {other:?}"),
        }
    }

    // ── Mixed Or stays whole in content_filter ────────────────────────────────

    #[test]
    fn mixed_or_goes_entirely_to_content() {
        // Or([Ext(pdf), Content(invoice)]) — mixed tier → content_filter unchanged
        let mixed_or = Filter::Or(vec![
            Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
            Filter::Content(MatchExpr::exact("invoice")),
        ]);
        let query = q!(Query::builder().and_filter(mixed_or.clone()));
        assert_eq!(query.walk_filter, Filter::Any);
        assert_eq!(query.content_filter, mixed_or);
    }

    #[test]
    fn mixed_or_plus_content_filter_both_go_to_content() {
        // The concept example: or_filter(Ext) + content("invoice")
        // walk: Any, content: And([Or([Ext(pdf), Ext(docx)]), Content(invoice)])
        let or_node = Filter::Or(vec![
            Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
            Filter::Ext(vec![Box::from(b"docx" as &[u8])]),
        ]);
        let query = q!(Query::builder().and_filter(or_node).content("invoice"));
        // The Or is walk-only (both Ext), so it stays in walk_filter
        match &query.walk_filter {
            Filter::Or(children) => assert_eq!(children.len(), 2),
            other => panic!("expected Or in walk, got {other:?}"),
        }
        assert_eq!(
            query.content_filter,
            Filter::Content(MatchExpr::exact("invoice"))
        );
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn build_without_roots_returns_error() {
        let err = Query::builder().content("test").build();
        assert_eq!(err, Err(BuildError::NoRootsProvided));
    }

    // ── Meta builder methods — not yet implemented, build() must reject ───────

    #[test]
    fn meta_number_gt_goes_to_content_filter() {
        // meta: filters are not implemented; build() must return InvalidMetaTag.
        let result = Query::builder()
            .root(".")
            .ext(["pdf"])
            .meta_number_gt("pages", 10.0)
            .build();
        assert!(
            matches!(result, Err(BuildError::InvalidMetaTag { ref tag, .. }) if tag == "pages"),
            "expected InvalidMetaTag for 'pages', got: {result:?}",
        );
    }

    #[test]
    fn meta_bool_goes_to_content_filter() {
        // meta: filters are not implemented; build() must return InvalidMetaTag.
        let result = Query::builder().root(".").meta_bool("gps", true).build();
        assert!(
            matches!(result, Err(BuildError::InvalidMetaTag { ref tag, .. }) if tag == "gps"),
            "expected InvalidMetaTag for 'gps', got: {result:?}",
        );
    }
}

// ── Parser tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod parser_tests {
    use sx_core::{FileType, Filter, HiddenMode, MatchExpr, ParseError, Query, RangeExpr};

    fn parse(s: &str) -> Query {
        Query::from_str(s).unwrap_or_else(|e| panic!("parse failed for {s:?}: {e}"))
    }

    fn parse_err(s: &str) -> ParseError {
        Query::from_str(s).expect_err("expected parse error")
    }

    // ── Bare term → Or(Name, Content) ────────────────────────────────────────

    #[test]
    fn bare_term_becomes_name_or_content() {
        // A bare word matches files named "report" OR containing "report".
        // The Or is mixed-tier so the partitioner moves it entirely to content.
        let q = parse("report");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Name(MatchExpr::exact("report")),
                Filter::Content(MatchExpr::exact("report")),
            ])
        );
    }

    #[test]
    fn bare_term_with_explicit_content_filter() {
        // "bar content:foo" → walk: Any, content: And(Or(Name(bar),Content(bar)), Content(foo))
        let q = parse("bar content:foo");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::And(vec![
                Filter::Or(vec![
                    Filter::Name(MatchExpr::exact("bar")),
                    Filter::Content(MatchExpr::exact("bar")),
                ]),
                Filter::Content(MatchExpr::exact("foo")),
            ])
        );
    }

    // ── Quoted strings ────────────────────────────────────────────────────────

    #[test]
    fn quoted_bare_term_with_spaces() {
        let q = parse("\"Alice Smith\"");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Name(MatchExpr::exact("Alice Smith")),
                Filter::Content(MatchExpr::exact("Alice Smith")),
            ])
        );
    }

    #[test]
    fn quoted_value_in_namespace() {
        let q = parse("content:\"hello world\"");
        assert_eq!(
            q.content_filter,
            Filter::Content(MatchExpr::exact("hello world"))
        );
    }

    #[test]
    fn quoted_value_with_escaped_quote() {
        let q = parse(r#"content:"say \"hi\"""#);
        assert_eq!(
            q.content_filter,
            Filter::Content(MatchExpr::exact(r#"say "hi""#))
        );
    }

    #[test]
    fn quoted_name_with_spaces() {
        let q = parse(r#"name:"my file.txt""#);
        assert_eq!(q.walk_filter, Filter::Name(MatchExpr::exact("my file.txt")));
    }

    #[test]
    fn quoted_value_in_or_combinator() {
        let q = parse(r#"or(content:"foo bar",content:"baz qux")"#);
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Content(MatchExpr::exact("foo bar")),
                Filter::Content(MatchExpr::exact("baz qux")),
            ])
        );
    }

    #[test]
    fn quoted_name_preserves_internal_space() {
        let q = parse(r#"n:"my file""#);
        assert_eq!(q.walk_filter, Filter::Name(MatchExpr::exact("my file")));
    }

    #[test]
    fn quoted_content_preserves_internal_space() {
        let q = parse(r#"c:"foo bar baz""#);
        assert_eq!(
            q.content_filter,
            Filter::Content(MatchExpr::exact("foo bar baz"))
        );
    }

    #[test]
    fn quoted_path_becomes_root() {
        // Absolute path with spaces — promoted to a root, not a filter.
        let q = parse(r#"p:"/home/my user/docs""#);
        assert_eq!(
            q.walk_filter,
            Filter::Any,
            "absolute quoted p: must not leave a filter"
        );
        assert!(
            q.roots
                .contains(&std::path::PathBuf::from("/home/my user/docs")),
            "absolute quoted path must become a root, got: {:?}",
            q.roots
        );
    }

    #[test]
    fn quoted_line_preserves_internal_space() {
        let q = parse(r#"l:"error: file not found""#);
        assert_eq!(
            q.content_filter,
            Filter::Line(MatchExpr::exact("error: file not found")),
        );
    }

    #[test]
    fn quoted_meta_still_errors() {
        // meta: is unimplemented; a quoted value must not change that.
        assert!(Query::from_str(r#"meta:author:"Alice Smith""#).is_err());
    }

    // ── name: namespace ───────────────────────────────────────────────────────

    #[test]
    fn name_exact() {
        let q = parse("name:report.pdf");
        assert_eq!(q.walk_filter, Filter::Name(MatchExpr::exact("report.pdf")));
    }

    #[test]
    fn name_negated() {
        let q = parse("name:!generated");
        assert_eq!(
            q.walk_filter,
            Filter::Name(MatchExpr::exact("generated").negated())
        );
    }

    // ── path: namespace ───────────────────────────────────────────────────────

    #[test]
    fn path_negated_stays_as_filter() {
        let q = parse("path:!node_modules");
        assert_eq!(
            q.walk_filter,
            Filter::Path(MatchExpr::exact("node_modules").negated())
        );
    }

    // ── hidden: namespace ─────────────────────────────────────────────────────

    #[test]
    fn hidden_only() {
        let q = parse("hidden:only");
        assert_eq!(q.walk_filter, Filter::Hidden(HiddenMode::Only));
    }

    #[test]
    fn hidden_true() {
        let q = parse("hidden:true");
        assert_eq!(q.walk_filter, Filter::Hidden(HiddenMode::Include));
    }

    #[test]
    fn hidden_false() {
        let q = parse("hidden:false");
        assert_eq!(q.walk_filter, Filter::Hidden(HiddenMode::Exclude));
    }

    // ── ext: namespace ────────────────────────────────────────────────────────

    #[test]
    fn ext_single() {
        let q = parse("ext:rs");
        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
    }

    #[test]
    fn ext_comma_list() {
        let q = parse("ext:rs,go,ts");
        assert_eq!(
            q.walk_filter,
            Filter::Ext(vec![
                Box::from(b"rs" as &[u8]),
                Box::from(b"go" as &[u8]),
                Box::from(b"ts" as &[u8]),
            ])
        );
    }

    // ── content: namespace ────────────────────────────────────────────────────

    #[test]
    fn content_exact() {
        let q = parse("content:TODO");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(q.content_filter, Filter::Content(MatchExpr::exact("TODO")));
    }

    #[test]
    fn content_negated() {
        let q = parse("content:!generated");
        assert_eq!(
            q.content_filter,
            Filter::Content(MatchExpr::exact("generated").negated())
        );
    }

    // ── line: namespace ───────────────────────────────────────────────────────

    #[test]
    fn line_exact() {
        let q = parse("line:TODO");
        assert_eq!(q.content_filter, Filter::Line(MatchExpr::exact("TODO")));
    }

    // ── size: namespace ───────────────────────────────────────────────────────

    #[test]
    fn size_gt_bytes() {
        let q = parse("size:>1024");
        assert_eq!(q.walk_filter, Filter::Size(RangeExpr::Gt(1024)));
    }

    #[test]
    fn size_gt_mb() {
        let q = parse("size:>10mb");
        assert_eq!(q.walk_filter, Filter::Size(RangeExpr::Gt(10 * 1024 * 1024)));
    }

    #[test]
    fn size_lt_kb() {
        let q = parse("size:<512k");
        assert_eq!(q.walk_filter, Filter::Size(RangeExpr::Lt(512 * 1024)));
    }

    #[test]
    fn size_missing_modifier_defaults_to_eq() {
        let q = parse("size:1024");
        assert_eq!(q.walk_filter, Filter::Size(RangeExpr::Eq(1024)));
    }

    // ── modified/created/accessed: namespace ──────────────────────────────────

    #[test]
    fn modified_gt_relative() {
        let q = parse("modified:>7d");
        assert!(matches!(q.walk_filter, Filter::Modified(RangeExpr::Gt(_))));
    }

    #[test]
    fn modified_lt_weeks() {
        let q = parse("modified:<2w");
        assert!(matches!(q.walk_filter, Filter::Modified(RangeExpr::Lt(_))));
    }

    #[test]
    fn created_gt_absolute() {
        let q = parse("created:>2024-01-15");
        assert!(matches!(q.walk_filter, Filter::Created(RangeExpr::Gt(_))));
    }

    #[test]
    fn accessed_lt_months() {
        let q = parse("accessed:<3m");
        assert!(matches!(q.walk_filter, Filter::Accessed(RangeExpr::Lt(_))));
    }

    // ── type: namespace ───────────────────────────────────────────────────────

    #[test]
    fn type_file() {
        let q = parse("type:f");
        assert_eq!(q.walk_filter, Filter::Type(FileType::File));
    }

    #[test]
    fn type_dir() {
        let q = parse("type:d");
        assert_eq!(q.walk_filter, Filter::Type(FileType::Dir));
    }

    #[test]
    fn type_executable_goes_to_content() {
        let q = parse("type:x");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(q.content_filter, Filter::Type(FileType::Executable));
    }

    // ── mime: namespace ───────────────────────────────────────────────────────

    #[test]
    fn mime_exact() {
        let q = parse("mime:image/png");
        assert_eq!(
            q.content_filter,
            Filter::Mime(MatchExpr::exact("image/png"))
        );
    }

    // ── perm/owner/group: namespace ───────────────────────────────────────────

    #[test]
    fn perm_negated_is_invalid() {
        assert!(Query::from_str("perm:!world-write").is_err());
    }

    #[test]
    fn perm_octal_exact() {
        let q = parse("perm:644");
        assert_eq!(q.walk_filter, Filter::PermOctal(0o644));
    }

    #[test]
    fn perm_flag_x() {
        let q = parse("perm:+x");
        assert_eq!(q.walk_filter, Filter::PermFlag(0o111));
    }

    #[test]
    fn owner_exact() {
        let q = parse("owner:alice");
        assert_eq!(q.walk_filter, Filter::Owner(MatchExpr::exact("alice")));
    }

    #[test]
    fn group_exact() {
        let q = parse("group:wheel");
        assert_eq!(q.walk_filter, Filter::Group(MatchExpr::exact("wheel")));
    }

    // ── meta: namespace — not yet implemented, must error ─────────────────────

    #[test]
    fn meta_bool_errors() {
        assert!(Query::from_str("meta:gps:true").is_err());
    }

    #[test]
    fn meta_number_errors() {
        assert!(Query::from_str("meta:iso:>800").is_err());
    }

    #[test]
    fn meta_text_errors() {
        assert!(Query::from_str("meta:author:Alice").is_err());
    }

    // ── Combinators ───────────────────────────────────────────────────────────

    #[test]
    fn or_walk_only() {
        let q = parse("or(ext:pdf,ext:docx)");
        assert_eq!(
            q.walk_filter,
            Filter::Or(vec![
                Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
                Filter::Ext(vec![Box::from(b"docx" as &[u8])]),
            ])
        );
        assert_eq!(q.content_filter, Filter::Any);
    }

    #[test]
    fn or_content_only() {
        let q = parse("or(content:foo,content:bar)");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Content(MatchExpr::exact("foo")),
                Filter::Content(MatchExpr::exact("bar")),
            ])
        );
    }

    #[test]
    fn or_mixed_tier_goes_to_content() {
        let q = parse("or(ext:pdf,content:invoice)");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
                Filter::Content(MatchExpr::exact("invoice")),
            ])
        );
    }

    #[test]
    fn or_with_bare_term_inside() {
        let q = parse("or(ext:pdf,report)");
        assert_eq!(q.walk_filter, Filter::Any);
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
                Filter::Or(vec![
                    Filter::Name(MatchExpr::exact("report")),
                    Filter::Content(MatchExpr::exact("report")),
                ]),
            ])
        );
    }

    #[test]
    fn or_anded_with_ext() {
        let q = parse("ext:rs or(content:todo,content:fixme)");
        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert_eq!(
            q.content_filter,
            Filter::Or(vec![
                Filter::Content(MatchExpr::exact("todo")),
                Filter::Content(MatchExpr::exact("fixme")),
            ])
        );
    }

    #[test]
    fn not_wrapping_or() {
        let q = parse("not(or(ext:pdf,ext:docx))");
        assert_eq!(
            q.walk_filter,
            Filter::Not(Box::new(Filter::Or(vec![
                Filter::Ext(vec![Box::from(b"pdf" as &[u8])]),
                Filter::Ext(vec![Box::from(b"docx" as &[u8])]),
            ])))
        );
        assert_eq!(q.content_filter, Filter::Any);
    }

    #[test]
    fn not_combinator() {
        let q = parse("not(path:node_modules)");
        assert_eq!(
            q.walk_filter,
            Filter::Not(Box::new(Filter::Path(MatchExpr::exact("node_modules"))))
        );
    }

    #[test]
    fn multiple_tokens_are_anded() {
        let q = parse("ext:rs content:unwrap");
        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert_eq!(
            q.content_filter,
            Filter::Content(MatchExpr::exact("unwrap"))
        );
    }

    // ── Absolute path → root promotion ───────────────────────────────────────

    #[test]
    fn absolute_path_becomes_root() {
        let q = parse("hidden:only path:/home");
        assert_eq!(q.walk_filter, Filter::Hidden(HiddenMode::Only));
        assert_eq!(q.content_filter, Filter::Any);
        assert!(
            q.roots.contains(&std::path::PathBuf::from("/home")),
            "absolute path:/home must become a root, got: {:?}",
            q.roots
        );
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn empty_input_returns_error() {
        assert!(matches!(parse_err(""), ParseError::EmptyQuery));
    }

    #[test]
    fn whitespace_only_returns_error() {
        assert!(matches!(parse_err("   "), ParseError::EmptyQuery));
    }

    #[test]
    fn unknown_namespace_returns_error() {
        assert!(matches!(
            parse_err("bogus:value"),
            ParseError::UnknownNamespace { .. }
        ));
    }

    #[test]
    fn malformed_or_too_few_args() {
        assert!(matches!(
            parse_err("or(ext:rs)"),
            ParseError::MalformedCombinator { .. }
        ));
    }
}

use futures_util::StreamExt;
use std::fs;
use sx_core::Query;

fn setup_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("hashmap_notes.md"),
        "notes about HashMap and BTreeMap",
    )
    .unwrap();
    fs::write(
        dir.path().join("readme.md"),
        "just a readme, nothing special",
    )
    .unwrap();
    fs::write(
        dir.path().join("serde_guide.md"),
        "serde derive macros for HashMap",
    )
    .unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(
        dir.path().join("sub/deep.md"),
        "deeply nested HashMap reference",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_task_sequential_run() {
    let dir = setup_tree();
    let cq = Query::builder()
        .root(dir.path())
        .content("HashMap")
        .build()
        .unwrap()
        .compile();

    let mut stream = cq.run();
    let mut found = vec![];
    while let Some(m) = stream.next().await {
        found.push(m.path);
    }

    assert_eq!(
        found.len(),
        3,
        "hashmap_notes.md, serde_guide.md, sub/deep.md should match"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clones_split_work_correctly() {
    let dir = setup_tree();
    let cq = Query::builder()
        .root(dir.path())
        .content("HashMap")
        .build()
        .unwrap()
        .compile();

    const WORKERS: usize = 8;
    let mut handles = vec![];
    for _ in 0..WORKERS {
        let cq = cq.clone();
        handles.push(tokio::spawn(async move {
            let mut stream = cq.run();
            let mut local = vec![];
            while let Some(m) = stream.next().await {
                local.push(m.path);
            }
            local
        }));
    }

    let mut all = vec![];
    for h in handles {
        all.extend(h.await.unwrap());
    }

    // Every match should be found exactly once across all clones —
    // no duplicates, nothing missed, regardless of which clone ended
    // up walking vs draining.
    all.sort();
    all.dedup();
    assert_eq!(
        all.len(),
        3,
        "expected exactly 3 unique matches across all worker clones"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn or_matches_any_term() {
    let dir = setup_tree();
    let cq = Query::builder()
        .root(dir.path())
        .and_filter(sx_core::Filter::Or(vec![
            sx_core::Filter::Content(sx_core::MatchExpr::exact("nonexistent_term")),
            sx_core::Filter::Content(sx_core::MatchExpr::exact("readme")),
        ]))
        .build()
        .unwrap()
        .compile();

    let mut stream = cq.run();
    let mut found = vec![];
    while let Some(m) = stream.next().await {
        found.push(m.path);
    }
    assert_eq!(found.len(), 1);
    assert!(found[0].to_string_lossy().contains("readme"));
}

#[tokio::test(flavor = "multi_thread")]
async fn exclude_path_pattern_prunes_subtree() {
    let dir = setup_tree();
    let cq = Query::builder()
        .root(dir.path())
        .content("HashMap")
        .path_not("sub")
        .build()
        .unwrap()
        .compile();

    let mut stream = cq.run();
    let mut found = vec![];
    while let Some(m) = stream.next().await {
        found.push(m.path);
    }
    assert!(
        found.iter().all(|p| !p.to_string_lossy().contains("sub")),
        "sub/ subtree should be excluded"
    );
    assert_eq!(found.len(), 2, "hashmap_notes.md and serde_guide.md remain");
}

/// `path:!src` prunes the `src/` directory before descent, not merely
/// after listing.
#[tokio::test(flavor = "multi_thread")]
async fn path_not_prunes_directory_before_descent() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("readme.md"), b"hello world").unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src").join("main.rs"), b"hello world").unwrap();
    std::fs::create_dir_all(dir.path().join("src").join("deep").join("deeper")).unwrap();
    std::fs::write(
        dir.path()
            .join("src")
            .join("deep")
            .join("deeper")
            .join("leaf.rs"),
        b"hello world",
    )
    .unwrap();

    let cq = Query::builder()
        .root(dir.path())
        .content("hello")
        .path_not("src")
        .build()
        .unwrap()
        .compile();

    let mut stream = cq.run();
    let mut names: Vec<String> = vec![];
    while let Some(m) = stream.next().await {
        names.push(m.path.file_name().unwrap().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(names, ["readme.md"]);
}

// ── CompiledWalkFilter tests ──────────────────────────────────────────────────

#[cfg(test)]
mod compiled_walk_filter_tests {
    use sx_core::{CompiledWalkFilter, FileType, Filter, HiddenMode, MatchExpr, RangeExpr};
    use tempfile::TempDir;
    use walkdir::WalkDir;

    fn entries(dir: &TempDir) -> Vec<walkdir::DirEntry> {
        WalkDir::new(dir.path())
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .collect()
    }

    fn entry_named<'a>(entries: &'a [walkdir::DirEntry], name: &str) -> &'a walkdir::DirEntry {
        entries
            .iter()
            .find(|e| e.file_name().to_str() == Some(name))
            .unwrap_or_else(|| panic!("entry '{name}' not found"))
    }

    #[test]
    fn any_always_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"").unwrap();
        let entries = entries(&dir);
        let cwf = CompiledWalkFilter::from_filter(&Filter::Any);
        assert!(cwf.matches(entry_named(&entries, "file.txt")));
    }

    #[test]
    fn name_exact_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("report.rs"), b"").unwrap();
        std::fs::write(dir.path().join("other.rs"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Name(MatchExpr::exact("report")));
        assert!(cwf.matches(entry_named(&entries, "report.rs")));
        assert!(!cwf.matches(entry_named(&entries, "other.rs")));
    }

    #[test]
    fn name_negated() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("bad.rs"), b"").unwrap();
        std::fs::write(dir.path().join("good.rs"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Name(MatchExpr::exact("bad").negated()));
        assert!(!cwf.matches(entry_named(&entries, "bad.rs")));
        assert!(cwf.matches(entry_named(&entries, "good.rs")));
    }

    #[test]
    fn ext_allow_list() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.go"), b"").unwrap();
        std::fs::write(dir.path().join("c.ts"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Ext(vec![
            Box::from(b"rs" as &[u8]),
            Box::from(b"go" as &[u8]),
        ]));
        assert!(cwf.matches(entry_named(&entries, "a.rs")));
        assert!(cwf.matches(entry_named(&entries, "b.go")));
        assert!(!cwf.matches(entry_named(&entries, "c.ts")));
    }

    #[test]
    fn ext_no_extension_not_matched() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Makefile"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert!(!cwf.matches(entry_named(&entries, "Makefile")));
    }

    #[test]
    fn hidden_exclude() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        std::fs::write(dir.path().join("visible"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Hidden(HiddenMode::Exclude));
        assert!(!cwf.matches(entry_named(&entries, ".hidden")));
        assert!(cwf.matches(entry_named(&entries, "visible")));
    }

    #[test]
    fn hidden_only() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        std::fs::write(dir.path().join("visible"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Hidden(HiddenMode::Only));
        assert!(cwf.matches(entry_named(&entries, ".hidden")));
        assert!(!cwf.matches(entry_named(&entries, "visible")));
    }

    #[test]
    fn hidden_include_passes_both() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        std::fs::write(dir.path().join("visible"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Hidden(HiddenMode::Include));
        assert!(cwf.matches(entry_named(&entries, ".hidden")));
        assert!(cwf.matches(entry_named(&entries, "visible")));
    }

    #[test]
    fn size_gt() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("big"), b"hello world").unwrap();
        std::fs::write(dir.path().join("empty"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Size(RangeExpr::Gt(5)));
        assert!(cwf.matches(entry_named(&entries, "big")));
        assert!(!cwf.matches(entry_named(&entries, "empty")));
    }

    #[test]
    fn size_lt() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("big"), b"hello world").unwrap();
        std::fs::write(dir.path().join("tiny"), b"hi").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Size(RangeExpr::Lt(5)));
        assert!(!cwf.matches(entry_named(&entries, "big")));
        assert!(cwf.matches(entry_named(&entries, "tiny")));
    }

    #[test]
    fn size_eq() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("three"), b"abc").unwrap();
        std::fs::write(dir.path().join("four"), b"abcd").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Size(RangeExpr::Eq(3)));
        assert!(cwf.matches(entry_named(&entries, "three")));
        assert!(!cwf.matches(entry_named(&entries, "four")));
    }

    #[test]
    fn size_between() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a"), b"ab").unwrap(); // 2 bytes
        std::fs::write(dir.path().join("b"), b"abcde").unwrap(); // 5 bytes
        std::fs::write(dir.path().join("c"), b"abcdefgh").unwrap(); // 8 bytes
        let entries = entries(&dir);

        // between(2, 8) → strict: 2 < x < 8
        let cwf = CompiledWalkFilter::from_filter(&Filter::Size(RangeExpr::Between(2, 8)));
        assert!(!cwf.matches(entry_named(&entries, "a"))); // == 2, not >2
        assert!(cwf.matches(entry_named(&entries, "b")));
        assert!(!cwf.matches(entry_named(&entries, "c"))); // == 8, not <8
    }

    #[test]
    fn type_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Type(FileType::File));
        assert!(cwf.matches(entry_named(&entries, "file.txt")));
        assert!(!cwf.matches(entry_named(&entries, "subdir")));
    }

    #[test]
    fn type_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Type(FileType::Dir));
        assert!(!cwf.matches(entry_named(&entries, "file.txt")));
        assert!(cwf.matches(entry_named(&entries, "subdir")));
    }

    #[test]
    fn type_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("empty"), b"").unwrap();
        std::fs::write(dir.path().join("nonempty"), b"data").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Type(FileType::Empty));
        assert!(cwf.matches(entry_named(&entries, "empty")));
        assert!(!cwf.matches(entry_named(&entries, "nonempty")));
    }

    #[test]
    fn and_both_must_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), b"").unwrap();
        std::fs::write(dir.path().join("main.go"), b"").unwrap();
        std::fs::write(dir.path().join("other.rs"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::And(vec![
            Filter::Name(MatchExpr::exact("main")),
            Filter::Ext(vec![Box::from(b"rs" as &[u8])]),
        ]));
        assert!(cwf.matches(entry_named(&entries, "main.rs")));
        assert!(!cwf.matches(entry_named(&entries, "main.go")));
        assert!(!cwf.matches(entry_named(&entries, "other.rs")));
    }

    #[test]
    fn or_either_can_match() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.go"), b"").unwrap();
        std::fs::write(dir.path().join("c.py"), b"").unwrap();
        let entries = entries(&dir);

        let cwf = CompiledWalkFilter::from_filter(&Filter::Or(vec![
            Filter::Ext(vec![Box::from(b"rs" as &[u8])]),
            Filter::Ext(vec![Box::from(b"go" as &[u8])]),
        ]));
        assert!(cwf.matches(entry_named(&entries, "a.rs")));
        assert!(cwf.matches(entry_named(&entries, "b.go")));
        assert!(!cwf.matches(entry_named(&entries, "c.py")));
    }

    #[test]
    fn not_inverts() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.go"), b"").unwrap();
        let entries = entries(&dir);

        let cwf =
            CompiledWalkFilter::from_filter(&Filter::Not(Box::new(Filter::Ext(vec![Box::from(
                b"rs" as &[u8],
            )]))));
        assert!(!cwf.matches(entry_named(&entries, "a.rs")));
        assert!(cwf.matches(entry_named(&entries, "b.go")));
    }
}

// ── CompiledContentFilter tests ───────────────────────────────────────────────

#[cfg(test)]
mod compiled_content_filter_tests {
    use std::path::Path;
    use sx_core::{
        CaseSensitivity, CompiledContentFilter, Filter, MatchExpr, Query, SearchOptions,
    };

    fn fake() -> &'static Path {
        Path::new("/fake/file.txt")
    }

    #[test]
    fn filter_any_is_trivially_true() {
        let ccf = CompiledContentFilter::from_filter(&Filter::Any);
        assert!(ccf.is_trivially_true());
        assert!(ccf.matches(fake(), b"anything", &mut Vec::new()).0);
    }

    #[test]
    fn content_exact_match() {
        let f = Filter::Content(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(!ccf.is_trivially_true());
        assert!(
            ccf.matches(
                fake(),
                b"line one\nTODO: fix this\nline three",
                &mut Vec::new()
            )
            .0
        );
        assert!(
            !ccf.matches(fake(), b"line one\nline two\nline three", &mut Vec::new())
                .0
        );
    }

    #[test]
    fn content_exact_case_sensitive() {
        let f = Filter::Content(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(!ccf.matches(fake(), b"todo: fix this", &mut Vec::new()).0);
    }

    #[test]
    fn content_exact_negated() {
        let f = Filter::Content(MatchExpr::exact("generated").negated());
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(ccf.matches(fake(), b"handwritten code", &mut Vec::new()).0);
        assert!(
            !ccf.matches(fake(), b"// generated by protoc", &mut Vec::new())
                .0
        );
    }

    #[test]
    fn line_exact_matches_anywhere_in_document() {
        let f = Filter::Line(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(ccf.matches(fake(), b"ok\nTODO: fix\nok", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"all good here", &mut Vec::new()).0);
    }

    #[test]
    fn two_content_patterns_both_must_match_in_and() {
        let f = Filter::And(vec![
            Filter::Content(MatchExpr::exact("foo")),
            Filter::Content(MatchExpr::exact("bar")),
        ]);
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(
            ccf.matches(fake(), b"foo and bar together", &mut Vec::new())
                .0
        );
        assert!(!ccf.matches(fake(), b"only foo here", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"only bar here", &mut Vec::new()).0);
    }

    #[test]
    fn two_content_patterns_either_matches_in_or() {
        let f = Filter::Or(vec![
            Filter::Content(MatchExpr::exact("unwrap")),
            Filter::Content(MatchExpr::exact("panic")),
        ]);
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(ccf.matches(fake(), b"x.unwrap()", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"panic!(\"oops\")", &mut Vec::new()).0);
        assert!(
            !ccf.matches(fake(), b"x.expect(\"oops\")", &mut Vec::new())
                .0
        );
    }

    #[test]
    fn not_content_inverts() {
        let f = Filter::Not(Box::new(Filter::Content(MatchExpr::exact("FIXME"))));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(ccf.matches(fake(), b"clean code", &mut Vec::new()).0);
        assert!(
            !ccf.matches(fake(), b"FIXME: address this", &mut Vec::new())
                .0
        );
    }

    #[test]
    fn mime_pdf_magic_bytes() {
        let pdf_bytes = b"%PDF-1.4 fake content";
        let f = Filter::Mime(MatchExpr::exact("application/pdf"));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(ccf.matches(fake(), pdf_bytes, &mut Vec::new()).0);
    }

    #[test]
    fn walk_tier_leaf_in_content_filter_passes_through() {
        // Name filter in content tier matches against the file path.
        let f_match = Filter::Name(MatchExpr::exact("file.txt"));
        let ccf_match = CompiledContentFilter::from_filter(&f_match);
        assert!(
            ccf_match
                .matches(fake(), b"completely unrelated content", &mut Vec::new())
                .0
        );

        let f_no = Filter::Name(MatchExpr::exact("foo"));
        let ccf_no = CompiledContentFilter::from_filter(&f_no);
        assert!(
            !ccf_no
                .matches(fake(), b"completely unrelated content", &mut Vec::new())
                .0
        );

        // Other walk-tier leaves pass through as true.
        let f_size = Filter::Size(sx_core::RangeExpr::Gt(0));
        let ccf_size = CompiledContentFilter::from_filter(&f_size);
        assert!(ccf_size.matches(fake(), b"anything", &mut Vec::new()).0);
    }

    #[test]
    fn from_str_content_filter_compiled() {
        let query = Query::from_str("c:TODO").unwrap();
        let ccf = CompiledContentFilter::from_filter(&query.content_filter);
        assert!(!ccf.is_trivially_true());
        assert!(ccf.matches(fake(), b"// TODO: fix this", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"// all good", &mut Vec::new()).0);
    }

    #[test]
    fn from_str_walk_only_query_content_is_trivially_true() {
        let query = Query::from_str("e:rs").unwrap();
        let ccf = CompiledContentFilter::from_filter(&query.content_filter);
        assert!(ccf.is_trivially_true());
    }

    #[test]
    fn from_str_or_mixed_content_filter() {
        // or(e:rs, c:unwrap) — mixed Or goes entirely to content_filter;
        // the Ext leaf passes through as true, so the whole Or is always true.
        let query = Query::from_str("or(e:rs,c:unwrap)").unwrap();
        let ccf = CompiledContentFilter::from_filter(&query.content_filter);
        assert!(ccf.matches(fake(), b"no match", &mut Vec::new()).0);
    }

    #[test]
    fn from_str_multiple_content_patterns() {
        let query = Query::from_str("c:foo c:bar").unwrap();
        let ccf = CompiledContentFilter::from_filter(&query.content_filter);
        assert!(ccf.matches(fake(), b"foo something bar", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"only foo", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"only bar", &mut Vec::new()).0);
    }

    #[test]
    fn case_sensitive_rejects_wrong_case() {
        let opts = SearchOptions {
            case: CaseSensitivity::Sensitive,
            ..SearchOptions::default()
        };
        let f = Filter::Content(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter_opts(&f, &opts);
        assert!(!ccf.matches(fake(), b"todo: fix this", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"TODO: fix this", &mut Vec::new()).0);
    }

    #[test]
    fn case_insensitive_matches_any_case() {
        let opts = SearchOptions {
            case: CaseSensitivity::Insensitive,
            ..SearchOptions::default()
        };
        let f = Filter::Content(MatchExpr::exact("todo"));
        let ccf = CompiledContentFilter::from_filter_opts(&f, &opts);
        assert!(ccf.matches(fake(), b"TODO: fix this", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"todo: fix this", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"Todo: fix this", &mut Vec::new()).0);
    }

    #[test]
    fn smart_case_lowercase_pattern_is_insensitive() {
        let opts = SearchOptions {
            case: CaseSensitivity::Smart,
            ..SearchOptions::default()
        };
        let f = Filter::Content(MatchExpr::exact("todo"));
        let ccf = CompiledContentFilter::from_filter_opts(&f, &opts);
        assert!(ccf.matches(fake(), b"TODO: fix this", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"Todo: fix this", &mut Vec::new()).0);
    }

    #[test]
    fn smart_case_uppercase_pattern_is_sensitive() {
        let opts = SearchOptions {
            case: CaseSensitivity::Smart,
            ..SearchOptions::default()
        };
        let f = Filter::Content(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter_opts(&f, &opts);
        assert!(ccf.matches(fake(), b"TODO: fix this", &mut Vec::new()).0);
        assert!(!ccf.matches(fake(), b"todo: fix this", &mut Vec::new()).0);
    }

    #[test]
    fn smart_case_multi_pattern_any_upper_forces_sensitive() {
        let opts = SearchOptions {
            case: CaseSensitivity::Smart,
            ..SearchOptions::default()
        };
        let f = Filter::And(vec![
            Filter::Content(MatchExpr::exact("foo")),
            Filter::Content(MatchExpr::exact("Bar")),
        ]);
        let ccf = CompiledContentFilter::from_filter_opts(&f, &opts);
        assert!(!ccf.matches(fake(), b"FOO and Bar", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"foo and Bar", &mut Vec::new()).0);
    }

    #[test]
    fn from_filter_default_is_case_sensitive() {
        // from_filter() uses Smart; "TODO" has uppercase → sensitive.
        let f = Filter::Content(MatchExpr::exact("TODO"));
        let ccf = CompiledContentFilter::from_filter(&f);
        assert!(!ccf.matches(fake(), b"todo: lowercase", &mut Vec::new()).0);
        assert!(ccf.matches(fake(), b"TODO: uppercase", &mut Vec::new()).0);
    }

    #[test]
    fn score_counts_total_ac_hits() {
        let f = Filter::Content(MatchExpr::exact("foo"));
        let ccf = CompiledContentFilter::from_filter(&f);
        let (matched, score) = ccf.matches(fake(), b"foo foo foo", &mut Vec::new());
        assert!(matched);
        assert_eq!(score, 3);
    }

    #[test]
    fn score_zero_when_no_match() {
        let f = Filter::Content(MatchExpr::exact("foo"));
        let ccf = CompiledContentFilter::from_filter(&f);
        let (matched, score) = ccf.matches(fake(), b"nothing here", &mut Vec::new());
        assert!(!matched);
        assert_eq!(score, 0);
    }

    #[test]
    fn score_sums_across_multiple_patterns() {
        let f = Filter::And(vec![
            Filter::Content(MatchExpr::exact("foo")),
            Filter::Content(MatchExpr::exact("bar")),
        ]);
        let ccf = CompiledContentFilter::from_filter(&f);
        let (matched, score) = ccf.matches(fake(), b"foo bar foo", &mut Vec::new());
        assert!(matched);
        assert_eq!(score, 3);
    }

    #[test]
    fn score_zero_for_trivially_true_filter() {
        let ccf = CompiledContentFilter::from_filter(&Filter::Any);
        let (matched, score) = ccf.matches(fake(), b"anything at all", &mut Vec::new());
        assert!(matched);
        assert_eq!(score, 0);
    }
}

// ── HandlerToken + from_str_with_registry ─────────────────────────────────────

#[cfg(test)]
mod handler_token_tests {
    use std::sync::Arc;
    use sx_core::{Filter, HandlerRegistry, HandlerToken, ParseError, Query};
    use sx_plugin_api::{
        stabby, SxBytes, SxHandler, SxHandlerInfo, SxNamespacePolicy, SxQueryContext, SxResultSender,
    };

    struct MockHandler {
        namespace: &'static str,
    }

    impl SxHandler for MockHandler {
        extern "C" fn info(&self) -> SxHandlerInfo {
            SxHandlerInfo {
                handler_version: SxBytes::from_str_bytes("0.1.0"),
                namespace: SxBytes::from_str_bytes(self.namespace),
                aliases: stabby::vec::Vec::new(),
                description: SxBytes::from_str_bytes("test"),
                file_extensions: stabby::vec::Vec::new(),
                file_extensions_optional: stabby::vec::Vec::new(),
                namespace_policy: SxNamespacePolicy::Exclusive,
                global_scope: false,
            }
        }
        extern "C" fn query(
            &self,
            _value: stabby::str::Str<'_>,
            _ctx: &SxQueryContext,
            _sender: SxResultSender,
        ) {
        }
        extern "C" fn prepare_extract(
            &self,
        ) -> stabby::option::Option<sx_plugin_api::SxExtractorHandle> {
            stabby::option::Option::None()
        }
    }

    fn make_registry() -> HandlerRegistry {
        let mut reg = HandlerRegistry::new();
        reg.register(Arc::new(MockHandler { namespace: "calc" }))
            .unwrap();
        reg
    }

    #[test]
    fn registry_aware_parse_splits_file_and_handler_tokens() {
        let reg = make_registry();
        let q = Query::from_str_with_registry("e:rs calc:2+2", vec![], Some(&reg))
            .expect("parse should succeed");

        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert_eq!(q.content_filter, Filter::Any);
        assert_eq!(
            q.handler_tokens,
            vec![HandlerToken {
                namespace: Box::from(b"calc" as &[u8]),
                value: Box::from(b"2+2" as &[u8]),
            }],
        );
    }

    #[test]
    fn file_only_query_unchanged() {
        let q = Query::from_str("e:rs c:hashmap").expect("parse should succeed");
        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert_eq!(
            q.content_filter,
            Filter::Content(sx_core::MatchExpr::exact("hashmap"))
        );
        assert!(q.handler_tokens.is_empty());
    }

    #[test]
    fn unknown_namespace_without_registry_is_error() {
        let err = Query::from_str("calc:2+2").expect_err("should fail");
        assert!(matches!(err, ParseError::UnknownNamespace { ref token } if token == "calc"),);
    }

    #[test]
    fn unknown_namespace_not_in_registry_is_error() {
        let reg = make_registry();
        let err = Query::from_str_with_registry("weather:berlin", vec![], Some(&reg))
            .expect_err("should fail");
        assert!(matches!(err, ParseError::UnknownNamespace { ref token } if token == "weather"),);
    }

    #[test]
    fn multiple_handler_tokens() {
        let mut reg = HandlerRegistry::new();
        reg.register(Arc::new(MockHandler { namespace: "calc" }))
            .unwrap();
        reg.register(Arc::new(MockHandler { namespace: "sys" }))
            .unwrap();

        let q = Query::from_str_with_registry("calc:pi sys:disk", vec![], Some(&reg))
            .expect("parse should succeed");

        assert_eq!(q.handler_tokens.len(), 2);
        assert_eq!(q.handler_tokens[0].namespace.as_ref(), b"calc");
        assert_eq!(q.handler_tokens[0].value.as_ref(), b"pi");
        assert_eq!(q.handler_tokens[1].namespace.as_ref(), b"sys");
        assert_eq!(q.handler_tokens[1].value.as_ref(), b"disk");
    }

    #[test]
    fn handler_token_quoted_value() {
        let reg = make_registry();
        let q = Query::from_str_with_registry(r#"calc:"2 + 2""#, vec![], Some(&reg))
            .expect("parse should succeed");
        assert_eq!(q.handler_tokens[0].value.as_ref(), b"2 + 2");
    }

    #[test]
    fn real_calc_handler_token_with_registry() {
        use sx_plugin_calc::CalcHandler;
        let mut reg = HandlerRegistry::new();
        reg.register(Arc::new(CalcHandler)).unwrap();

        let q = Query::from_str_with_registry("e:rs calc:2^10", vec![], Some(&reg))
            .expect("parse should succeed");

        assert_eq!(q.walk_filter, Filter::Ext(vec![Box::from(b"rs" as &[u8])]));
        assert_eq!(
            q.handler_tokens,
            vec![HandlerToken {
                namespace: Box::from(b"calc" as &[u8]),
                value: Box::from(b"2^10" as &[u8]),
            }]
        );
    }
}

// ── CompiledQuery::dispatch_handler_tokens ────────────────────────────────────

#[cfg(test)]
mod compiled_dispatch_tests {
    use std::sync::Arc;
    use sx_core::{HandlerRegistry, Query};
    use sx_plugin_calc::CalcHandler;

    fn make_registry() -> Arc<HandlerRegistry> {
        let mut reg = HandlerRegistry::new();
        reg.register(Arc::new(CalcHandler)).unwrap();
        Arc::new(reg)
    }

    fn collect(cq: &sx_core::CompiledQuery) -> Vec<sx_plugin_api::SxResult> {
        let registry = match &cq.registry {
            Some(r) => Arc::clone(r),
            None => return vec![],
        };
        if cq.handler_tokens.is_empty() {
            return vec![];
        }
        let (tx, rx) = std::sync::mpsc::channel::<sx_plugin_api::SxResult>();
        for token in &cq.handler_tokens {
            let registry = Arc::clone(&registry);
            let tx = tx.clone();
            let namespace = token.namespace.clone();
            let value = String::from_utf8_lossy(&token.value).into_owned();
            let ctx = sx_plugin_api::SxQueryContext {
                roots: sx_plugin_api::stabby::vec::Vec::new(),
                max_results: 0,
                timeout_ms: 0,
                case_sensitivity: sx_plugin_api::SxCaseSensitivity::Smart,
            };
            std::thread::spawn(move || {
                let sender =
                    sx_plugin_api::SxResultSender::from_fn(move |res| tx.send(res).is_ok());
                let _ = registry.dispatch(&namespace, &value, &ctx, sender);
            });
        }
        drop(tx);
        rx.iter().collect()
    }

    fn first_scalar(results: &[sx_plugin_api::SxResult]) -> String {
        results[0]
            .metadata
            .iter()
            .find(|e| e.key.as_str() == "value")
            .map(|e| e.value.match_ref(|t| t.as_str().to_owned(), |n| n.to_string(), |_| String::new()))
            .expect("expected value metadata entry")
    }

    #[test]
    fn dispatch_calc_token_via_compiled_query() {
        let registry = make_registry();
        let q = Query::from_str_with_registry("calc:2^10", vec![], Some(&registry)).expect("parse");
        let cq = q.compile_with_registry(Some(Arc::clone(&registry)));
        let results = collect(&cq);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_namespace.as_str(), "calc");
        assert_eq!(first_scalar(&results), "1024");
    }

    #[test]
    fn file_only_query_produces_no_handler_results() {
        let registry = make_registry();
        let q = Query::from_str_with_registry("e:rs", vec![], Some(&registry)).expect("parse");
        let cq = q.compile_with_registry(Some(Arc::clone(&registry)));
        assert!(collect(&cq).is_empty());
    }

    #[test]
    fn mixed_query_dispatches_handler_token_only() {
        let registry = make_registry();
        let q = Query::from_str_with_registry("e:rs calc:sqrt(2)", vec![], Some(&registry))
            .expect("parse");
        let cq = q.compile_with_registry(Some(Arc::clone(&registry)));
        let results = collect(&cq);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_namespace.as_str(), "calc");
        let scalar = first_scalar(&results);
        assert!(scalar.starts_with("1.414"), "got: {scalar}");
    }

    #[test]
    fn no_registry_produces_no_handler_results() {
        let registry = make_registry();
        let q = Query::from_str_with_registry("calc:pi", vec![], Some(&registry)).expect("parse");
        let cq = q.compile();
        assert!(collect(&cq).is_empty());
    }

    #[test]
    fn equals_alias_dispatches_as_calc() {
        let registry = make_registry();
        let q =
            Query::from_str_with_registry("=:1920/1080", vec![], Some(&registry)).expect("parse");
        let cq = q.compile_with_registry(Some(Arc::clone(&registry)));
        let results = collect(&cq);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_namespace.as_str(), "calc");
        let scalar = first_scalar(&results);
        assert!(scalar.starts_with("1.777"), "got: {scalar}");
    }
}

mod path_root_promotion_tests {
    use std::path::PathBuf;
    use sx_core::{Filter, MatchExpr, Query};

    #[test]
    fn absolute_p_becomes_root_not_filter() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:/tmp/some_dir", vec![cwd.clone()]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/tmp/some_dir")),
            "absolute p: path must be in roots, got: {:?}",
            q.roots
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn relative_p_stays_as_path_filter() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:src", vec![cwd]).expect("parse");
        assert_eq!(q.roots, vec![PathBuf::from("/home/user/project")]);
        assert_eq!(q.walk_filter, Filter::Path(MatchExpr::exact("src")));
    }

    #[test]
    fn negated_absolute_p_stays_as_filter() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:!/tmp", vec![cwd.clone()]).expect("parse");
        assert_eq!(q.roots, vec![cwd]);
        assert_eq!(
            q.walk_filter,
            Filter::Path(MatchExpr::exact("/tmp").negated())
        );
    }

    #[test]
    fn two_unrelated_p_paths_both_added_alongside_cwd() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:/tmp/path_a p:/var/log", vec![cwd.clone()])
            .expect("parse");

        let mut roots = q.roots.clone();
        roots.sort();
        let mut expected = vec![
            PathBuf::from("/home/user/project"),
            PathBuf::from("/tmp/path_a"),
            PathBuf::from("/var/log"),
        ];
        expected.sort();
        assert_eq!(roots, expected);
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn wider_ancestor_replaces_narrower_existing_root() {
        let q = Query::from_str_with_roots("p:/", vec![PathBuf::from("/tmp")]).expect("parse");
        assert_eq!(q.roots, vec![PathBuf::from("/")]);
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn narrower_path_not_added_when_wider_root_exists() {
        let q = Query::from_str_with_roots("p:/tmp", vec![PathBuf::from("/")]).expect("parse");
        assert_eq!(q.roots, vec![PathBuf::from("/")]);
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn duplicate_p_paths_produce_single_root() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:/tmp/a p:/tmp/a", vec![cwd]).expect("parse");
        assert_eq!(
            q.roots
                .iter()
                .filter(|r| **r == PathBuf::from("/tmp/a"))
                .count(),
            1,
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn nonexistent_absolute_path_kept_verbatim() {
        let cwd = PathBuf::from("/home/user/project");
        let q = Query::from_str_with_roots("p:/tmp/xyzzy_nonexistent", vec![cwd]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/tmp/xyzzy_nonexistent")),
            "non-existent path must be added verbatim, got: {:?}",
            q.roots
        );
        assert!(!q.roots.contains(&PathBuf::from("/tmp")));
        assert_eq!(q.walk_filter, Filter::Any);
    }

    // ── Shell expansion (tilde + $VAR) ────────────────────────────────────────

    #[test]
    fn tilde_slash_path_expands_to_root() {
        std::env::set_var("HOME", "/home/testuser");
        let q = Query::from_str_with_roots("p:~/docs", vec![]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/home/testuser/docs")),
            "p:~/docs must expand and become a root, got: {:?}",
            q.roots,
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn bare_tilde_expands_to_home_root() {
        std::env::set_var("HOME", "/home/testuser");
        let q = Query::from_str_with_roots("p:~", vec![]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/home/testuser")),
            "p:~ must expand to home root, got: {:?}",
            q.roots,
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn negated_tilde_path_expands_in_filter() {
        std::env::set_var("HOME", "/home/testuser");
        let cwd = PathBuf::from("/some/dir");
        let q = Query::from_str_with_roots("p:!~/secrets", vec![cwd.clone()]).expect("parse");
        assert_eq!(q.roots, vec![cwd]);
        assert_eq!(
            q.walk_filter,
            Filter::Path(MatchExpr::exact("/home/testuser/secrets").negated()),
        );
    }

    #[test]
    fn tilde_deep_path_expands_correctly() {
        std::env::set_var("HOME", "/home/testuser");
        let q = Query::from_str_with_roots("p:~/a/b/c", vec![]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/home/testuser/a/b/c")),
            "deep tilde path must expand, got: {:?}",
            q.roots,
        );
    }

    #[test]
    fn env_var_in_path_expands_to_root() {
        std::env::set_var("SXTEST_DIR", "/opt/data");
        let q = Query::from_str_with_roots("p:$SXTEST_DIR", vec![]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/opt/data")),
            "p:$SXTEST_DIR must expand and become a root, got: {:?}",
            q.roots,
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn braced_env_var_in_path_expands_to_root() {
        std::env::set_var("SXTEST_SUBDIR", "/var/app");
        let q = Query::from_str_with_roots("p:${SXTEST_SUBDIR}/logs", vec![]).expect("parse");
        assert!(
            q.roots.contains(&PathBuf::from("/var/app/logs")),
            "p:${{SXTEST_SUBDIR}}/logs must expand, got: {:?}",
            q.roots,
        );
        assert_eq!(q.walk_filter, Filter::Any);
    }

    #[test]
    fn unset_env_var_left_unexpanded() {
        std::env::remove_var("SXTEST_DEFINITELY_UNSET");
        let cwd = PathBuf::from("/some/dir");
        let q = Query::from_str_with_roots("p:$SXTEST_DEFINITELY_UNSET", vec![cwd.clone()])
            .expect("parse");
        assert_eq!(q.roots, vec![cwd]);
        assert_eq!(
            q.walk_filter,
            Filter::Path(MatchExpr::exact("$SXTEST_DEFINITELY_UNSET")),
        );
    }

    #[test]
    fn negated_env_var_path_expands_in_filter() {
        std::env::set_var("SXTEST_DIR", "/opt/data");
        let cwd = PathBuf::from("/some/dir");
        let q = Query::from_str_with_roots("p:!$SXTEST_DIR", vec![cwd.clone()]).expect("parse");
        assert_eq!(q.roots, vec![cwd]);
        assert_eq!(
            q.walk_filter,
            Filter::Path(MatchExpr::exact("/opt/data").negated()),
        );
    }
}
