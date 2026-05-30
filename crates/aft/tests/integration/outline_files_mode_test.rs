use super::helpers::AftProcess;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    write_bytes(root, relative, content.as_bytes())
}

fn write_bytes(root: &Path, relative: &str, content: &[u8]) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&request.to_string())
}

fn outline_files(aft: &mut AftProcess, directory: &Path) -> Value {
    send(
        aft,
        json!({
            "id": "outline-files",
            "command": "outline",
            "directory": directory,
            "files": true,
        }),
    )
}

fn files(resp: &Value) -> &Vec<Value> {
    resp["files"].as_array().expect("files array")
}

fn file_entry<'a>(resp: &'a Value, path: &str) -> &'a Value {
    files(resp)
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("missing {path} in files response: {resp:?}"))
}

fn response_paths(resp: &Value) -> Vec<String> {
    files(resp)
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn outline_files_mode_returns_file_metadata_with_language_and_symbol_counts() {
    let dir = TempDir::new().unwrap();
    let ts = write_file(
        dir.path(),
        "src/service.ts",
        "export function greet() { return 1; }\nexport const answer = 42;\n",
    );
    let rs = write_file(
        dir.path(),
        "crates/model.rs",
        "pub struct Config {}\npub fn compute() -> i32 { 1 }\n",
    );
    let md = write_file(dir.path(), "docs/readme.md", "# Title\n\n## Details\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, dir.path());
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );
    assert_eq!(resp["complete"], true);

    let rs_entry = file_entry(&resp, "crates/model.rs");
    assert_eq!(rs_entry["language"], "rust");
    assert_eq!(rs_entry["symbols"], 2);
    assert_eq!(rs_entry["bytes"], fs::metadata(rs).unwrap().len());

    let ts_entry = file_entry(&resp, "src/service.ts");
    assert_eq!(ts_entry["language"], "typescript");
    assert_eq!(ts_entry["symbols"], 2);
    assert_eq!(ts_entry["bytes"], fs::metadata(ts).unwrap().len());

    let md_entry = file_entry(&resp, "docs/readme.md");
    assert_eq!(md_entry["language"], "markdown");
    assert_eq!(md_entry["symbols"], 2);
    assert_eq!(md_entry["bytes"], fs::metadata(md).unwrap().len());

    let text = resp["text"].as_str().unwrap().replace('\\', "/");
    assert!(text.contains("src/service.ts"), "missing TS row: {text}");
    assert!(text.contains("typescript"), "missing language: {text}");
    assert!(text.contains("2 syms"), "missing symbol count: {text}");

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_false_keeps_symbol_focused_directory_output() {
    let dir = TempDir::new().unwrap();
    write_file(
        dir.path(),
        "src/service.ts",
        "export function greet() { return 1; }\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-directory",
            "command": "outline",
            "directory": dir.path(),
        }),
    );

    assert_eq!(
        resp["success"], true,
        "directory outline should succeed: {resp:?}"
    );
    assert!(
        resp.get("files").is_none(),
        "default outline must not return files data: {resp:?}"
    );
    let text = resp["text"].as_str().unwrap().replace('\\', "/");
    assert!(
        text.contains("src/\n") || text.contains("src/service.ts"),
        "missing directory tree: {text}"
    );
    assert!(text.contains("service.ts"), "missing file in tree: {text}");
    assert!(text.contains("greet"), "missing symbol in tree: {text}");

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_mode_rejects_file_targets() {
    let dir = TempDir::new().unwrap();
    let file = write_file(dir.path(), "single.ts", "export function single() {}\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-files-file-target",
            "command": "outline",
            "file": file,
            "files": true,
        }),
    );

    assert_eq!(resp["success"], false);
    assert_eq!(resp["code"], "invalid_request");
    assert_eq!(resp["message"], "files mode requires a directory target");

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_mode_lists_binary_unknown_extension_files() {
    let dir = TempDir::new().unwrap();
    write_bytes(dir.path(), "assets/blob.dat", &[0, 159, 146, 150, 0, 1]);

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, dir.path());
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );

    let entry = file_entry(&resp, "assets/blob.dat");
    assert_eq!(entry["language"], "binary");
    assert_eq!(entry["symbols"], 0);
    assert_eq!(entry["bytes"], 6);

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_mode_excludes_gitignored_paths_when_matcher_is_active() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), ".gitignore", "ignored.ts\nignored-dir/\n");
    write_file(dir.path(), "visible.ts", "export function visible() {}\n");
    write_file(dir.path(), "ignored.ts", "export function ignored() {}\n");
    write_file(
        dir.path(),
        "ignored-dir/nested.ts",
        "export function nested() {}\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, dir.path());
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );
    let paths = response_paths(&resp);

    assert!(
        paths.contains(&"visible.ts".to_string()),
        "visible file missing: {paths:?}"
    );
    assert!(
        !paths.contains(&"ignored.ts".to_string()),
        "ignored file should be excluded: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with("ignored-dir/")),
        "ignored directory should be excluded: {paths:?}"
    );

    assert!(aft.shutdown().success());
}

// NOTE: A previous test here (`outline_files_mode_uses_fresh_symbol_cache_without_reparsing`)
// tried to verify that the cache fast-path returns the stored symbol count when
// `(mtime, size)` match, even if the file's current bytes are unparseable. The
// scenario only succeeded when the filesystem watcher's invalidation happened to
// be slow enough that the read landed before invalidation. On Linux with a
// responsive inotify watcher (and on GH Actions specifically) the watcher
// catches the corruption write and correctly invalidates the cache — which is
// the desired behavior in real usage. The cache fast-path itself is covered by
// unit tests in `crates/aft/src/parser.rs` against `SymbolCache` directly,
// without the racy protocol-level setup.

#[test]
fn outline_files_mode_truncates_text_and_reports_unchecked_files() {
    let dir = TempDir::new().unwrap();
    let long_dir = format!("src/{}", "very-long-segment-".repeat(8));
    for index in 0..200 {
        write_file(
            dir.path(),
            &format!("{long_dir}/file-{index:03}-with-extra-name.txt"),
            "x\n",
        );
    }

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, dir.path());
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );
    assert_eq!(resp["complete"], false);
    assert_eq!(resp["walk_truncated"], false);
    assert_eq!(files(&resp).len(), 200);
    assert!(
        !resp["unchecked_files"].as_array().unwrap().is_empty(),
        "unchecked_files should list rows omitted by the text cap: {resp:?}"
    );
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("... truncated ("),
        "missing truncation marker: {text}"
    );
    assert!(
        text.contains("30KB limit"),
        "missing cap in truncation marker: {text}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn files_mode_multi_target_uses_project_root_relative_paths() {
    let dir = TempDir::new().unwrap();
    write_file(
        dir.path(),
        "src/shared/index.ts",
        "export function fromSrc() {}\n",
    );
    write_file(
        dir.path(),
        "tests/shared/index.ts",
        "export function fromTests() {}\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-files-multi-target",
            "command": "outline",
            "target": [dir.path().join("src"), dir.path().join("tests")],
            "files": true,
        }),
    );
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );

    let paths = response_paths(&resp);
    assert_eq!(paths, vec!["src/shared/index.ts", "tests/shared/index.ts"]);
    assert!(
        !paths.iter().any(|path| path == "shared/index.ts"),
        "multi-target paths must not be stripped to ambiguous target-relative names: {paths:?}"
    );

    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("src/shared/index.ts"),
        "missing src row: {text}"
    );
    assert!(
        text.contains("tests/shared/index.ts"),
        "missing tests row: {text}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn files_mode_single_target_keeps_target_relative_behavior() {
    let dir = TempDir::new().unwrap();
    write_file(
        dir.path(),
        "src/shared/index.ts",
        "export function singleTarget() {}\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, &dir.path().join("src"));
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );

    let paths = response_paths(&resp);
    assert_eq!(paths, vec!["shared/index.ts"]);
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("shared/index.ts"),
        "missing target-relative row: {text}"
    );
    assert!(
        !text.contains("src/shared/index.ts"),
        "single-target mode should remain target-relative: {text}"
    );

    assert!(aft.shutdown().success());
}

#[test]
fn files_mode_walk_cap_is_subset_stable_under_lexical_sort() {
    let dir = TempDir::new().unwrap();
    for index in (0..260).rev() {
        write_file(dir.path(), &format!("files/file-{index:03}.txt"), "x\n");
    }
    let expected = (0..200)
        .map(|index| format!("files/file-{index:03}.txt"))
        .collect::<Vec<_>>();

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    for _ in 0..3 {
        let resp = outline_files(&mut aft, dir.path());
        assert_eq!(
            resp["success"], true,
            "outline files should succeed: {resp:?}"
        );
        assert_eq!(resp["complete"], false);
        assert_eq!(resp["walk_truncated"], true);
        assert_eq!(resp["collection_truncated"], false);
        assert_eq!(response_paths(&resp), expected);
    }

    assert!(aft.shutdown().success());
}

#[test]
fn outline_files_mode_sorts_entries_by_path() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "zeta.ts", "export function zeta() {}\n");
    write_file(dir.path(), "alpha.ts", "export function alpha() {}\n");
    write_file(dir.path(), "nested/beta.ts", "export function beta() {}\n");

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = outline_files(&mut aft, dir.path());
    assert_eq!(
        resp["success"], true,
        "outline files should succeed: {resp:?}"
    );
    assert_eq!(
        response_paths(&resp),
        vec!["alpha.ts", "nested/beta.ts", "zeta.ts"]
    );

    assert!(aft.shutdown().success());
}
