#![cfg(debug_assertions)]

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness::{
    reset_verify_file_strict_count_for_debug, verify_file_strict_count_for_debug,
};
use aft::search_index::SearchIndex;
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

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_search_index(aft: &mut AftProcess, root: &Path, id: &str) -> Value {
    send(
        aft,
        json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "config": user_config(serde_json::json!({
                "search_index": true,
                "semantic_search": false
            })),
        }),
    )
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status-search-index-warm-restart",
            "command": "status",
        }),
    )
}

fn wait_for_search_index_ready(aft: &mut AftProcess, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    let mut last_response = None;

    while Instant::now() < deadline {
        let response = status(aft);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if response["search_index"]["status"] == "ready" {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(50));
    }

    panic!(
        "search index should become ready within {:?}; last response: {:?}",
        timeout, last_response
    );
}

#[test]
fn unchanged_head_warm_configure_reuses_verified_cache_without_rebuild_thread() {
    let project = setup_project(&[("src/lib.rs", "pub fn needle() -> usize { 1 }\n")]);
    let marker_dir = tempfile::tempdir().expect("create marker dir");
    let marker = marker_dir.path().join("rebuild-spawned");
    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_SEARCH_REBUILD_THREAD_MARKER",
        marker.as_os_str(),
    )]);

    let first = configure_search_index(&mut aft, project.path(), "cfg-first");
    assert_eq!(first["success"], true, "configure failed: {first:?}");
    assert!(
        marker.exists(),
        "cold configure should exercise the marker hook"
    );
    wait_for_search_index_ready(&mut aft, Duration::from_secs(5));
    fs::remove_file(&marker).expect("remove cold-build marker");

    let second = configure_search_index(&mut aft, project.path(), "cfg-second");
    assert_eq!(second["success"], true, "configure failed: {second:?}");
    assert_eq!(second["search_index_cache_reused"], true);
    // Warm reuse now VERIFIES the cached index on a background thread instead of
    // inline on the dispatch thread (verify_against_disk content-hashes every
    // cached file, O(repo) — blocking configure past the 30s transport timeout
    // on a large repo). So the unchanged-HEAD warm path DOES spawn the
    // background marker now, and configure returns without doing the hash-all
    // work synchronously. (Previously this asserted `!marker.exists()`, which
    // encoded the buggy inline-verify behavior.)
    assert!(
        marker.exists(),
        "warm reuse should verify the cached index on a background thread"
    );

    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(5));
    assert_eq!(ready["search_index"]["status"], "ready");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn search_index_ready_status_does_not_wait_for_symbol_prewarm() {
    let project = setup_project(&[("src/lib.rs", "pub fn searchable_symbol() -> usize { 7 }\n")]);
    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_TEST_SYMBOL_PREWARM_DELAY_MS", OsStr::new("5000"))]);

    let configure = configure_search_index(&mut aft, project.path(), "cfg-prewarm-delay");
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let ready = wait_for_search_index_ready(&mut aft, Duration::from_secs(3));
    assert_eq!(ready["search_index"]["status"], "ready");
    // Polling readiness while a 5s symbol prewarm delay is configured proves the
    // ready status is not gated on prewarm, without comparing wall-clock elapsed
    // time against the delay under shared CPU contention.

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn verify_file_mtimes_checks_each_cached_file_once() {
    let project = setup_project(&[
        ("src/lib.rs", "pub fn alpha() {}\n"),
        ("src/main.rs", "fn main() { alpha(); }\n"),
    ]);
    let mut index = SearchIndex::build(project.path());
    let cached_files = index
        .files
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .count();

    reset_verify_file_strict_count_for_debug();
    index.verify_against_disk_for_debug(None);

    assert_eq!(verify_file_strict_count_for_debug(), cached_files);
}
