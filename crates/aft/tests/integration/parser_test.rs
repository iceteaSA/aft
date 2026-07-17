use aft::parser::FileParser;

#[test]
fn python_decorated_function_range_includes_decorators() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("decorated.py");
    std::fs::write(&file, "@cache\n@profile\ndef f():\n    pass\n").expect("write python file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let symbol = symbols
        .iter()
        .find(|sym| sym.name == "f")
        .expect("find decorated function");

    assert_eq!(symbol.range.start_line, 0, "range should start at @cache");
    assert_eq!(symbol.range.start_col, 0);
}

#[test]
fn symbol_cache_trusts_matching_size_and_mtime_until_invalidated() {
    // Freshness policy: a warm cache hit trusts matching size + mtime without
    // re-reading or hashing the file (the same trust the trigram index's
    // stat-first warm verification applies). A same-size edit that also
    // preserves mtime is therefore NOT detected by the freshness check alone —
    // only mtime-preserving tools (rsync --times, explicit utimes) produce
    // that shape, and the file watcher's cache reset is the invalidation
    // path that covers them.
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("cached.rs");
    std::fs::write(&file, "pub fn alpha() {}\n").expect("write rust file");
    let original_mtime = std::fs::metadata(&file)
        .expect("stat rust file")
        .modified()
        .expect("mtime");

    let mut parser = FileParser::new();
    let first = parser
        .extract_symbols(&file)
        .expect("extract first symbols");
    assert!(first.iter().any(|symbol| symbol.name == "alpha"));

    std::fs::write(&file, "pub fn bravo() {}\n").expect("rewrite rust file same size");
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(original_mtime))
        .expect("restore original mtime");

    let second = parser
        .extract_symbols(&file)
        .expect("extract second symbols");
    assert!(
        second.iter().any(|symbol| symbol.name == "alpha"),
        "matching size+mtime is trusted: the cached symbols are served"
    );

    // Watcher-driven invalidation is the correctness path for
    // mtime-preserving edits: after it, the new content must be parsed.
    parser.invalidate_symbols(&file);
    let third = parser
        .extract_symbols(&file)
        .expect("extract third symbols");
    assert!(third.iter().any(|symbol| symbol.name == "bravo"));
    assert!(!third.iter().any(|symbol| symbol.name == "alpha"));
}

#[test]
fn symbol_cache_detects_mtime_moved_content_edit_of_same_size() {
    // The other direction stays guarded: when mtime moves, same-size content
    // changes must be detected (the content hash runs on the mtime-moved
    // path and catches them).
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("cached.rs");
    std::fs::write(&file, "pub fn alpha() {}\n").expect("write rust file");

    let mut parser = FileParser::new();
    let first = parser
        .extract_symbols(&file)
        .expect("extract first symbols");
    assert!(first.iter().any(|symbol| symbol.name == "alpha"));

    std::fs::write(&file, "pub fn bravo() {}\n").expect("rewrite rust file same size");
    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(4_102_444_800, 0))
        .expect("move mtime");

    let second = parser
        .extract_symbols(&file)
        .expect("extract second symbols");
    assert!(second.iter().any(|symbol| symbol.name == "bravo"));
    assert!(!second.iter().any(|symbol| symbol.name == "alpha"));
}

#[test]
fn ts_export_clause_marks_local_symbol_exported() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("exports.ts");
    std::fs::write(&file, "function foo() { return 1; }\nexport { foo };\n")
        .expect("write ts file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let foo = symbols
        .iter()
        .find(|symbol| symbol.name == "foo")
        .expect("find foo");
    assert!(foo.exported, "foo should be exported via export clause");
}

#[test]
fn ts_export_default_identifier_marks_local_symbol_exported() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file = tmp.path().join("default.ts");
    std::fs::write(&file, "function foo() { return 1; }\nexport default foo;\n")
        .expect("write ts file");

    let mut parser = FileParser::new();
    let symbols = parser.extract_symbols(&file).expect("extract symbols");
    let foo = symbols
        .iter()
        .find(|symbol| symbol.name == "foo")
        .expect("find foo");
    assert!(
        foo.exported,
        "foo should be exported via default identifier"
    );
}
