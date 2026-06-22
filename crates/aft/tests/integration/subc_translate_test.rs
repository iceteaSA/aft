//! Golden parity tests: agent-facing args → native (command, args).

use std::path::PathBuf;

use aft::subc_translate::subc_translate;
use serde_json::{json, Value};

fn root() -> PathBuf {
    PathBuf::from("/tmp/subc-parity-root")
}

/// Expected resolved path for a project-root-relative arg, built with the same
/// `Path::join` the translator uses so the separator matches the host OS
/// (forward slash on Unix, backslash on Windows).
fn expected_resolved(rel: &str) -> String {
    root().join(rel).to_string_lossy().into_owned()
}

fn assert_edit_error(args: Value, fragment: &str) {
    let err = subc_translate("edit", &args, &root()).unwrap_err();
    assert_eq!(err.code, "invalid_request");
    assert!(
        err.message.contains(fragment),
        "expected {fragment:?} in {}",
        err.message
    );
}

#[test]
fn status_empty_args() {
    let t = subc_translate("status", &json!({}), &root()).unwrap();
    assert_eq!(t.command, "status");
    assert!(t.args.is_empty());
}

#[test]
fn read_offset_limit_normalizes_to_start_end_line() {
    let t = subc_translate(
        "read",
        &json!({ "filePath": "a.rs", "offset": 10, "limit": 5 }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "read");
    assert_eq!(
        t.args.get("file").and_then(Value::as_str).unwrap(),
        expected_resolved("a.rs")
    );
    assert_eq!(t.args.get("start_line").and_then(Value::as_u64), Some(10));
    assert_eq!(t.args.get("end_line").and_then(Value::as_u64), Some(14));
    assert!(t.args.get("limit").is_none());
}

#[test]
fn read_limit_without_offset_forwards_limit() {
    let t = subc_translate("read", &json!({ "filePath": "a.rs", "limit": 20 }), &root()).unwrap();
    assert_eq!(t.args.get("limit").and_then(Value::as_u64), Some(20));
    assert!(t.args.get("start_line").is_none());
}

#[test]
fn edit_top_level_start_line_errors() {
    assert_edit_error(
        json!({ "filePath": "f", "startLine": 1, "oldString": "x" }),
        "startLine",
    );
}

#[test]
fn edit_missing_file_path_errors() {
    assert_edit_error(json!({ "oldString": "x" }), "'filePath' is required");
}

#[test]
fn edit_append_content_routes_edit_match_append() {
    let t = subc_translate(
        "edit",
        &json!({ "filePath": "n.txt", "appendContent": "line\n" }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "edit_match");
    assert_eq!(t.args.get("op").and_then(Value::as_str), Some("append"));
    assert_eq!(
        t.args.get("append_content").and_then(Value::as_str),
        Some("line\n")
    );
    assert_eq!(
        t.args.get("create_dirs").and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn edit_edits_array_routes_batch_with_key_translation() {
    let t = subc_translate(
        "edit",
        &json!({
            "filePath": "f.ts",
            "edits": [{ "oldString": "a", "newString": "b", "startLine": 1, "endLine": 2 }]
        }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "batch");
    let edits = t.args.get("edits").and_then(Value::as_array).unwrap();
    let first = edits[0].as_object().unwrap();
    assert_eq!(first.get("match").and_then(Value::as_str), Some("a"));
    assert_eq!(first.get("replacement").and_then(Value::as_str), Some("b"));
    assert_eq!(first.get("line_start").and_then(Value::as_u64), Some(1));
    assert_eq!(first.get("line_end").and_then(Value::as_u64), Some(2));
}

#[test]
fn edit_symbol_and_content_without_old_string() {
    let t = subc_translate(
        "edit",
        &json!({ "filePath": "f.ts", "symbol": "foo", "content": "fn foo() {}" }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "edit_symbol");
    assert_eq!(
        t.args.get("operation").and_then(Value::as_str),
        Some("replace")
    );
}

#[test]
fn edit_symbol_with_old_string_wins_edit_match() {
    let t = subc_translate(
        "edit",
        &json!({
            "filePath": "f.ts",
            "symbol": "foo",
            "content": "ignored",
            "oldString": "needle"
        }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "edit_match");
    assert_eq!(t.args.get("match").and_then(Value::as_str), Some("needle"));
    assert!(!t.args.contains_key("symbol"));
}

#[test]
fn edit_old_string_only_default_replacement_empty() {
    let t = subc_translate(
        "edit",
        &json!({ "filePath": "f.ts", "oldString": "x" }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.command, "edit_match");
    assert_eq!(t.args.get("replacement").and_then(Value::as_str), Some(""));
}

#[test]
fn edit_occurrence_zero_is_present() {
    let t = subc_translate(
        "edit",
        &json!({ "filePath": "f.ts", "oldString": "x", "newString": "y", "occurrence": 0 }),
        &root(),
    )
    .unwrap();
    assert_eq!(t.args.get("occurrence").and_then(Value::as_u64), Some(0));
}

#[test]
fn edit_no_mode_errors() {
    assert_edit_error(
        json!({ "filePath": "f.ts", "content": "whole file" }),
        "no edit mode resolved",
    );
}

#[test]
fn search_top_k_maps_to_top_k_default_10() {
    let t = subc_translate("search", &json!({ "query": "foo" }), &root()).unwrap();
    assert_eq!(t.command, "semantic_search");
    assert_eq!(t.args.get("top_k").and_then(Value::as_u64), Some(10));
    let t2 = subc_translate("search", &json!({ "query": "foo", "topK": 25 }), &root()).unwrap();
    assert_eq!(t2.args.get("top_k").and_then(Value::as_u64), Some(25));
}

#[test]
fn grep_path_resolves_relative() {
    let t = subc_translate(
        "grep",
        &json!({ "pattern": "fn main", "path": "src" }),
        &root(),
    )
    .unwrap();
    assert_eq!(
        t.args.get("path").and_then(Value::as_str).unwrap(),
        expected_resolved("src")
    );
}

#[test]
fn outline_url_passthrough() {
    let t = subc_translate(
        "outline",
        &json!({ "target": "https://example.com/doc" }),
        &root(),
    )
    .unwrap();
    assert_eq!(
        t.args.get("file").and_then(Value::as_str),
        Some("https://example.com/doc")
    );
}

#[test]
fn outline_string_file_vs_directory_uses_stat() {
    let dir = std::env::temp_dir().join(format!("subc_outline_dir_{}", std::process::id()));
    let file = dir.join("one.rs");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&file, "fn one() {}\n").unwrap();

    let t_dir = subc_translate("outline", &json!({ "target": "." }), &dir).unwrap();
    assert!(t_dir.args.contains_key("directory"));

    let t_file = subc_translate("outline", &json!({ "target": "one.rs" }), &dir).unwrap();
    assert!(t_file.args.contains_key("file"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn main_dispatch_has_no_agent_edit_or_search_aliases() {
    let src = include_str!("../../src/main.rs");
    for pat in ["\"edit\" =>", "\"search\" =>"] {
        assert!(
            !src.contains(pat),
            "main::dispatch must not alias agent tool {pat}"
        );
    }
    assert!(src.contains("\"semantic_search\" =>"));
    assert!(src.contains("\"edit_match\" =>"));
    assert!(src.contains("\"outline\" =>"));
}
