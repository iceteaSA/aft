use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn configure_with_index(aft: &mut AftProcess, root: &Path) {
    let resp = send(
        aft,
        json!({
            "id": "cfg-index",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "config": user_config(serde_json::json!({ "search_index": true }))
        }),
    );
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn fake_server_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn wait_for_index_ready<F>(aft: &mut AftProcess, mut request: F) -> Value
where
    F: FnMut() -> Value,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;

    while Instant::now() < deadline {
        let response = send(aft, request());
        if response["index_status"] == "Ready" {
            return response;
        }

        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!(
        "search index should become ready within 10s; last response: {:?}",
        last_response
    );
}

#[test]
fn grep_rejects_invalid_regex_with_pattern_data() {
    let project = setup_project(&[("src/main.rs", "fn main() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-invalid-regex",
            "command": "grep",
            "pattern": "[",
        }),
    );

    assert_eq!(response["success"], false, "grep should fail: {response:?}");
    assert_eq!(response["code"], "invalid_pattern");
    assert_eq!(response["pattern"], "[");
    assert!(response["message"]
        .as_str()
        .expect("message")
        .contains("invalid regex"));

    let status = aft.shutdown();
    assert!(status.success());
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .expect("canonicalize path")
        .display()
        .to_string()
}

#[test]
fn glob_fallback_small_tree_has_no_walk_truncated() {
    let project = setup_project(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-no-walk-trunc",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert!(response.get("walk_truncated").is_none() || response["walk_truncated"] == false);

    let status = aft.shutdown();
    assert!(status.success());
}

fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn grep_fallback_returns_relative_paths_and_counts() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
        ("notes.txt", "alpha beta gamma\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-fallback",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(response["total_matches"], 2);
    assert_eq!(response["files_with_matches"], 2);
    assert_eq!(response["files_searched"], 2);
    assert_eq!(response["complete"], true);
    assert!(response.get("walk_truncated").is_none() || response["walk_truncated"] == false);

    let matches = response["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0]["line"], 1);
    assert!(matches[0]["column"].as_u64().unwrap_or(0) >= 1);
    // Files are returned as absolute paths
    let file_path = matches[0]["file"].as_str().expect("file path");
    let file_path = file_path.replace('\\', "/");
    assert!(file_path.contains("src/one.rs") || file_path.contains("src/two.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_regex_over_em_dash_does_not_panic() {
    let project = setup_project(&[(
        "src/notes.md",
        "Intro — the em dash here can confuse byte offsets\n",
    )]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-em-dash",
            "command": "grep",
            "pattern": "—",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert!(
        response["total_matches"].as_u64().unwrap_or(0) >= 1,
        "expected at least one match: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-empty-scope",
            "command": "grep",
            "pattern": "anything",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_fallback_respects_gitignore_and_returns_absolute_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/skip.ts", "const skip = true;\n"),
        ("ignored.log", "secret\n"),
        (".gitignore", "*.log\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-fallback",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 1);
    let files = response["files"].as_array().expect("files array");
    assert_eq!(
        files,
        &vec![Value::String(canonical_path_string(
            &project.path().join("src/keep.rs")
        ))]
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_reports_empty_scope_separately() {
    let project = setup_project(&[]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-empty-scope",
            "command": "glob",
            "pattern": "**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["no_files_matched_scope"], true);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_uses_index_when_configured() {
    let project = setup_project(&[
        ("src/search.rs", "fn search() { println!(\"needle\"); }\n"),
        ("src/other.rs", "fn other() { println!(\"haystack\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-indexed",
            "command": "grep",
            "pattern": "needle",
            "include": ["src/**/*.rs"],
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 1);
    assert_eq!(response["files_with_matches"], 1);
    assert_eq!(response["files_searched"], 1);
    // Files are returned as absolute paths
    let expected_path = canonical_path_string(&project.path().join("src/search.rs"));
    assert_eq!(response["matches"][0]["file"], expected_path);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_text_uses_relative_paths_and_compact_format() {
    let project = setup_project(&[
        ("src/one.rs", "fn alpha() { println!(\"alpha\"); }\n"),
        ("src/two.rs", "fn beta() { println!(\"alpha\"); }\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-format",
            "command": "grep",
            "pattern": r#""alpha""#,
            "include": ["src/**/*.rs"],
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    let text = response["text"]
        .as_str()
        .expect("grep text")
        .replace('\\', "/");
    // New format: relative paths, no decorators, line:text format
    assert!(text.contains("src/one.rs\n"));
    assert!(text.contains("src/two.rs\n"));
    // No decorators
    assert!(!text.contains("──"));
    // No "Line" prefix, no indentation
    assert!(text.contains("1: fn alpha()"));
    assert!(text.contains("1: fn beta()"));
    assert!(text.ends_with("Found 2 match across 2 file [index: fallback]"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_text_uses_relative_paths() {
    let project = setup_project(&[
        ("src/keep.rs", "fn keep() {}\n"),
        ("src/other.rs", "fn other() {}\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-format",
            "command": "glob",
            "pattern": "src/**/*.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    let text = response["text"]
        .as_str()
        .expect("glob text")
        .replace('\\', "/");
    // Relative paths in text
    assert!(text.contains("src/keep.rs") || text.contains("src/other.rs"));
    // No absolute paths
    assert!(!text.contains("/private/"));
    assert!(text.starts_with("2 files matching src/**/*.rs"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_line_anchors() {
    let project = setup_project(&[
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-anchor-fallback",
            "command": "grep",
            "pattern": "^## ",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    // README has two `## ` headings at line start; `src/lib.rs` has one but
    // on a non-first line, so multi_line anchors must match it too.
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_indexed_supports_line_anchors() {
    let project = setup_project(&[
        (".fixture-id", "grep_indexed_supports_line_anchors\n"),
        (
            "README.md",
            "# Title\n\n## Section One\nbody\n\n## Section Two\nbody\n",
        ),
        (
            "src/lib.rs",
            "// not a heading\n## actually also not\nfn main() {}\n",
        ),
    ]);
    let mut aft = AftProcess::spawn();
    configure_with_index(&mut aft, project.path());

    let response = wait_for_index_ready(&mut aft, || {
        json!({
            "id": "grep-anchor-indexed",
            "command": "grep",
            "pattern": "^## ",
        })
    });
    assert_eq!(
        response["success"], true,
        "indexed grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Ready");
    assert_eq!(response["total_matches"], 3);
    assert_eq!(response["files_with_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_treats_leading_dash_pattern_as_literal() {
    let project = setup_project(&[("notes.txt", "-foo\nbar\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-leading-dash",
            "command": "grep",
            "pattern": "-foo",
            "path": project.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1, "response: {response:?}");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_fallback_supports_end_of_line_anchor() {
    let project = setup_project(&[("src/a.rs", "fn foo() {}\nfn bar() {}\nlet x = 1;\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // Match lines ending with `{}` — requires `$` to act as line-end anchor.
    let response = send(
        &mut aft,
        json!({
            "id": "grep-eol",
            "command": "grep",
            "pattern": r"\{\}$",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 2);

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_gitignored_file_is_searched() {
    // ripgrep parity: naming a gitignored file explicitly searches it anyway.
    let project = setup_project(&[
        ("captures/log.txt", "needle in a gitignored capture\n"),
        (".gitignore", "captures/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-gitignored",
            "command": "grep",
            "pattern": "needle",
            "path": "captures/log.txt",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        "explicitly-named gitignored file must be searched: {response:?}"
    );
    assert_eq!(
        response["no_files_matched_scope"], false,
        "explicit existing file must not report empty scope: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_aftignore_excludes_from_directory_search() {
    // .aftignore excludes paths from AFT's directory walk, layered on .gitignore.
    let project = setup_project(&[
        ("keep.rs", "findme here\n"),
        ("vendored/sub.rs", "findme in vendored\n"),
        (".aftignore", "vendored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-aftignore-dir",
            "command": "grep",
            "pattern": "findme",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        ".aftignored dir must be excluded from directory search: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_aftignored_file_is_searched() {
    // Explicitly naming an .aftignored file still searches it (ripgrep parity).
    let project = setup_project(&[
        ("vendored/sub.rs", "needle in aftignored vendored\n"),
        (".aftignore", "vendored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-aftignored",
            "command": "grep",
            "pattern": "needle",
            "path": "vendored/sub.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(
        response["total_matches"], 1,
        "explicitly-named .aftignored file must be searched: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_external_ripgrep_fallback_respects_search_root_aftignore() {
    if !rg_available() {
        return;
    }

    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "external-needle here\n"),
        ("ignored/skip.rs", "external-needle ignored\n"),
        (".aftignore", "ignored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-external-rg-aftignore",
            "command": "grep",
            "pattern": "external-needle",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(
        response["total_matches"], 1,
        "external ripgrep fallback must honor search-root .aftignore: {response:?}"
    );
    assert_eq!(response["files_with_matches"], 1);
    assert_eq!(
        response["matches"][0]["file"],
        canonical_path_string(&external.path().join("keep.rs"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_external_fallback_respects_nested_aftignore() {
    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "nested-external-needle here\n"),
        ("sub/skip.rs", "nested-external-needle ignored\n"),
        ("sub/.aftignore", "skip.rs\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-external-nested-aftignore",
            "command": "grep",
            "pattern": "nested-external-needle",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["index_status"], "Fallback");
    assert_eq!(
        response["total_matches"], 1,
        "external grep must honor nested .aftignore files: {response:?}"
    );
    assert_eq!(
        response["matches"][0]["file"],
        canonical_path_string(&external.path().join("keep.rs"))
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn glob_external_fallback_respects_nested_aftignore() {
    let project = setup_project(&[]);
    let external = setup_project(&[
        ("keep.rs", "fn keep() {}\n"),
        ("sub/skip.rs", "fn skip() {}\n"),
        ("sub/.aftignore", "skip.rs\n"),
    ]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "glob-external-nested-aftignore",
            "command": "glob",
            "pattern": "**/*.rs",
            "path": external.path(),
        }),
    );

    assert_eq!(
        response["success"], true,
        "glob should succeed: {response:?}"
    );
    assert_eq!(response["total"], 1, "glob response: {response:?}");
    assert_eq!(
        response["files"].as_array().expect("files"),
        &vec![Value::String(canonical_path_string(
            &external.path().join("keep.rs")
        ))]
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn grep_explicit_file_reports_runtime_fallback_index_status_when_index_disabled() {
    let project = setup_project(&[("src/main.rs", "fn main() { println!(\"needle\"); }\n")]);
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let response = send(
        &mut aft,
        json!({
            "id": "grep-explicit-index-status",
            "command": "grep",
            "pattern": "needle",
            "path": "src/main.rs",
        }),
    );

    assert_eq!(
        response["success"], true,
        "grep should succeed: {response:?}"
    );
    assert_eq!(response["total_matches"], 1, "response: {response:?}");
    assert_eq!(
        response["index_status"], "Fallback",
        "explicit-file grep must report actual runtime index state, not requested scope indexability: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn semantic_degraded_grep_fallback_respects_aftignore() {
    let project = setup_project(&[
        (
            "src/lib.rs",
            "pub fn retry() { /* how retry logic works */ }\n",
        ),
        (
            "ignored/skip.rs",
            "pub fn retry() { /* how retry logic works */ }\n",
        ),
        (".aftignore", "ignored/\n"),
    ]);
    let mut aft = AftProcess::spawn();
    let configure = send(
        &mut aft,
        json!({
            "id": "cfg-semantic-disabled",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(serde_json::json!({
                "semantic_search": false,
                "search_index": false
            })),
        }),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-degraded-aftignore",
            "command": "semantic_search",
            "query": "how retry logic works",
            "top_k": 10,
        }),
    );

    assert_eq!(
        response["success"], true,
        "semantic_search should succeed: {response:?}"
    );
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["lexical_only_fallback"], true);
    let files = response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|result| {
            result["file"]
                .as_str()
                .expect("result file")
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        files,
        vec![canonical_path_string(&project.path().join("src/lib.rs")).replace('\\', "/")],
        ".aftignored file must be skipped by degraded semantic grep fallback: {response:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn inspect_scoped_diagnostics_respects_aftignore() {
    let fake_server = fake_server_path();
    let project = setup_project(&[
        ("fake.toml", "[project]\n"),
        ("src/main.fake", "hello\n"),
        ("ignored/skip.fake", "hello\n"),
        (".aftignore", "ignored/\n"),
    ]);
    let fake_bin_dir = project.path().join("bin");
    fs::create_dir_all(&fake_bin_dir).expect("create fake server bin dir");
    let fake_binary_name = fake_server
        .file_name()
        .expect("fake server file name")
        .to_string_lossy()
        .to_string();
    fs::copy(&fake_server, fake_bin_dir.join(&fake_binary_name)).expect("copy fake server");

    let mut aft = AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1"))]);
    let configure = send(
        &mut aft,
        json!({
            "id": "cfg-inspect-aftignore",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "lsp_paths_extra": [fake_bin_dir],
            "config": user_config(serde_json::json!({
                "lsp": {
                    "servers": {
                        "fake": {
                            "extensions": ["fake"],
                            "binary": fake_binary_name,
                            "args": [],
                            "root_markers": ["fake.toml"]
                        }
                    }
                }
            }))
        }),
    );
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = send(
        &mut aft,
        json!({
            "id": "inspect-diagnostics-aftignore",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": ".",
            "topK": 10,
        }),
    );

    assert_eq!(
        response["success"], true,
        "inspect should succeed: {response:?}"
    );
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert_eq!(
        details.len(),
        1,
        ".aftignored file must be skipped by scoped inspect diagnostics: {response:?}"
    );
    assert_eq!(details[0]["file"], "src/main.fake");
    assert_eq!(details[0]["message"], "test pull diagnostic");

    let status = aft.shutdown();
    assert!(status.success());
}
