use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness;
use aft::callgraph_store::CallGraphStore;
use aft::commands::configure::handle_configure;
use aft::commands::inspect::{handle_inspect, handle_inspect_tier2_run};
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::inspect::{
    FileContribution, InspectCache, InspectCategory, InspectManager, InspectScanSuccess,
    InspectSnapshot, JobKey,
};
use aft::lsp::registry::ServerKind;
use aft::parser::{SymbolCache, TreeSitterProvider};
use aft::protocol::RawRequest;
use serde_json::{json, Value};

fn fixture_project() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");
    (temp_dir, root)
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

fn write_file(root: &Path, relative_path: &str, contents: &str) -> PathBuf {
    let path = root.join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture parent");
    }
    fs::write(&path, contents).expect("write fixture file");
    path
}

fn request(payload: Value) -> RawRequest {
    serde_json::from_value(payload).expect("request parses")
}

fn configured_context(root: &Path) -> AppContext {
    configured_context_with_callgraph_store(root, false)
}

fn configured_context_with_callgraph_store(root: &Path, callgraph_store: bool) -> AppContext {
    let storage_dir = root.join(".aft-test-storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            ..Config::default()
        },
    );
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "search_index": false,
        "semantic_search": false,
        "callgraph_store": callgraph_store,
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
}

fn drain_callgraph_store_for_test(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };
        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(store) => latest = Some(store),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    if let Some(store) = latest {
        *ctx.callgraph_store().borrow_mut() = Some(store);
        *ctx.callgraph_store_rx().borrow_mut() = None;
    } else if disconnected {
        *ctx.callgraph_store_rx().borrow_mut() = None;
    }
}

fn ensure_callgraph_store_ready(ctx: &AppContext) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match ctx.callgraph_store_for_ops() {
            CallgraphStoreAccess::Ready(_) => return,
            CallgraphStoreAccess::Building => {
                drain_callgraph_store_for_test(ctx);
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for callgraph store cold build"
                );
                thread::sleep(Duration::from_millis(10));
            }
            CallgraphStoreAccess::Unavailable => {
                panic!("callgraph store unexpectedly unavailable in test")
            }
            CallgraphStoreAccess::Error(error) => {
                panic!("callgraph store failed in test: {error}")
            }
        }
    }
}

fn inspect(ctx: &AppContext, payload: Value) -> Value {
    let response = handle_inspect(&request(payload), ctx);
    serde_json::to_value(response).expect("inspect response serializes")
}

fn enqueue_tier2_run(ctx: &AppContext, categories: &[&str]) -> Value {
    let response = handle_inspect_tier2_run(
        &request(json!({
            "id": "tier2-run",
            "command": "inspect_tier2_run",
            "categories": categories,
        })),
        ctx,
    );
    let value = serde_json::to_value(response).expect("tier2_run response serializes");
    assert_eq!(value["success"], true, "tier2_run failed: {value:#}");
    value
}

fn tier2_run(ctx: &AppContext, categories: &[&str]) {
    if categories.contains(&"dead_code") {
        ensure_callgraph_store_ready(ctx);
    }
    enqueue_tier2_run(ctx, categories);
    wait_for_tier2(ctx, categories);
}

fn wait_for_tier2(ctx: &AppContext, categories: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        ctx.inspect_manager().drain_completions();
        let response = inspect(
            ctx,
            json!({
                "id": "inspect-tier2-wait",
                "command": "inspect",
            }),
        );
        assert_eq!(
            response["success"], true,
            "inspect failed while waiting: {response:#}"
        );

        let failed = scanner_state_categories(&response, "failed_categories");
        assert!(
            failed.is_empty(),
            "tier2 failed while waiting: {response:#}"
        );

        let pending = scanner_state_categories(&response, "pending_categories");
        let stale = scanner_state_categories(&response, "stale_categories");
        let still_warming = categories.iter().any(|category| {
            pending.iter().any(|pending| pending == category)
                || stale.iter().any(|stale| stale == category)
        });
        if !still_warming {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for tier2 categories {categories:?}: {response:#}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn scanner_state_categories(response: &Value, key: &str) -> Vec<String> {
    response["scanner_state"][key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if let Some(category) = item.as_str() {
                        Some(category.to_string())
                    } else {
                        item.get("category")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn scanner_state_contains(response: &Value, key: &str, category: &str) -> bool {
    scanner_state_categories(response, key)
        .iter()
        .any(|value| value == category)
}

fn assert_summary_status(response: &Value, category: &str, status: &str) {
    let summary = response["summary"][category]
        .as_object()
        .unwrap_or_else(|| panic!("{category} summary object: {response:#}"));
    assert_eq!(
        summary.get("status").and_then(Value::as_str),
        Some(status),
        "{category} summary should carry status={status}: {response:#}"
    );
    assert!(
        !summary.contains_key("count"),
        "{category} summary status is not a trusted count: {response:#}"
    );
}

/// Stale-WITH-cache contract: the summary surfaces the real last-known count
/// (matching the status bar's `~D…`) flagged `stale: true`, NOT a bare
/// `{status:"stale"}` sentinel. Staleness is still signaled via `stale: true`
/// and `scanner_state.stale_categories` (asserted separately by callers).
fn assert_summary_stale_with_counts(response: &Value, category: &str) {
    let summary = response["summary"][category]
        .as_object()
        .unwrap_or_else(|| panic!("{category} summary object: {response:#}"));
    assert_eq!(
        summary.get("stale").and_then(Value::as_bool),
        Some(true),
        "{category} stale-with-cache summary must be flagged stale: {response:#}"
    );
    assert!(
        summary.get("count").and_then(Value::as_u64).is_some(),
        "{category} stale-with-cache summary must keep its cached count: {response:#}"
    );
    assert!(
        !summary.contains_key("status"),
        "{category} stale-with-cache summary must not be a bare status sentinel: {response:#}"
    );
}

fn assert_summary_count(response: &Value, category: &str, count: u64) {
    let summary = response["summary"][category]
        .as_object()
        .unwrap_or_else(|| panic!("{category} summary object: {response:#}"));
    assert_eq!(
        summary.get("count").and_then(Value::as_u64),
        Some(count),
        "{category} summary should carry count={count}: {response:#}"
    );
    assert!(
        !summary.contains_key("status"),
        "{category} computed summary should not carry a status sentinel: {response:#}"
    );
}

#[test]
fn inspect_command_todos_summary_uses_production_dispatch() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/app.ts",
        "// TODO: assert production dispatch reaches todos scanner\nexport function app() { return 1; }\n",
    );
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-todos",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["todos"]["count"]
        .as_u64()
        .expect("todos count");
    assert!(count > 0, "todos scanner should be reachable: {response:#}");
}

#[test]
fn inspect_command_metrics_summary_uses_production_dispatch() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.rs",
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { alpha() }\n",
    );
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-metrics",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let files = response["summary"]["metrics"]["files"]
        .as_u64()
        .expect("metrics files");
    assert!(
        files > 0,
        "metrics scanner should count files: {response:#}"
    );
    let metrics = response["summary"]["metrics"]
        .as_object()
        .expect("metrics summary object");
    assert!(
        !metrics.contains_key("status"),
        "Tier-1 metrics should be computed, not status-only: {response:#}"
    );
    assert_summary_count(&response, "todos", 0);
}

#[cfg(debug_assertions)]
#[test]
fn inspect_command_tier1_reuses_file_memo_for_unchanged_files() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/app.ts",
        "// TODO: keep cached\nexport function app() { return 1; }\n",
    );
    write_file(&root, "src/lib.ts", "export function lib() { return 2; }\n");
    let ctx = configured_context(&root);

    let first = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-cold",
            "command": "inspect",
        }),
    );
    assert_eq!(first["success"], true, "inspect failed: {first:#}");

    aft::inspect::scanners::metrics::reset_file_read_count_for_debug(&root);
    aft::inspect::scanners::todos::reset_file_read_count_for_debug(&root);

    let second = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-warm",
            "command": "inspect",
        }),
    );

    assert_eq!(second["success"], true, "inspect failed: {second:#}");
    assert_eq!(
        aft::inspect::scanners::metrics::file_read_count_for_debug(&root),
        0,
        "warm metrics scan should reuse unchanged per-file memo entries: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::todos::file_read_count_for_debug(&root),
        0,
        "warm todos scan should reuse unchanged per-file memo entries: {second:#}"
    );
    assert_eq!(first["summary"]["metrics"], second["summary"]["metrics"]);
    assert_eq!(first["summary"]["todos"], second["summary"]["todos"]);
}

#[cfg(debug_assertions)]
#[test]
fn inspect_command_tier1_changed_file_invalidates_only_that_file() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/unchanged.ts",
        "// TODO: already counted\nexport function unchanged() { return 1; }\n",
    );
    write_file(
        &root,
        "src/changed.ts",
        "export function changed() { return 2; }\n",
    );
    let ctx = configured_context(&root);

    let first = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-before-change",
            "command": "inspect",
        }),
    );
    assert_eq!(first["success"], true, "inspect failed: {first:#}");
    assert_eq!(first["summary"]["todos"]["count"], 1);

    aft::inspect::scanners::metrics::reset_file_read_count_for_debug(&root);
    aft::inspect::scanners::todos::reset_file_read_count_for_debug(&root);

    write_file(
        &root,
        "src/changed.ts",
        "// TODO: newly counted after memo invalidation\nexport function changed() { return 2; }\n",
    );

    let second = inspect(
        &ctx,
        json!({
            "id": "inspect-tier1-after-change",
            "command": "inspect",
        }),
    );

    assert_eq!(second["success"], true, "inspect failed: {second:#}");
    assert_eq!(
        second["summary"]["todos"]["count"], 2,
        "changed file's TODO should update the Tier 1 summary: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::metrics::file_read_count_for_debug(&root),
        1,
        "metrics should rescan only the changed file: {second:#}"
    );
    assert_eq!(
        aft::inspect::scanners::todos::file_read_count_for_debug(&root),
        1,
        "todos should rescan only the changed file: {second:#}"
    );
}

#[test]
fn inspect_command_dead_code_uses_callgraph_snapshot_and_details() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "import { used } from './lib';\nused();\n",
    );
    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    // aft_inspect never scans Tier 2 categories synchronously. Tier 2 scans run
    // via aft_inspect_tier2_run on session.idle in production. Simulate that
    // here so the cached aggregate is populated before the read-only inspect
    // call.
    tier2_run(&ctx, &["dead_code"]);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["dead_code"]["count"]
        .as_u64()
        .expect("dead_code count");
    assert!(
        count > 0,
        "dead_code should report fixture's intentionally dead export: {response:#}"
    );

    let details = response["details"]["dead_code"]
        .as_array()
        .expect("dead_code details array");
    assert!(
        details.iter().any(|item| item["symbol"] == "unused"),
        "dead_code details should include unused export: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_returns_pending_status_before_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context(&root);

    // No tier2_run call — inspect should return Pending for Tier 2 without
    // running scanners synchronously (which would block for seconds on big
    // projects). The summary entry itself must be status-only so agents do not
    // read an uncomputed category as clean.
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-tier2-cold",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    for category in ["dead_code", "unused_exports", "duplicates"] {
        assert!(
            scanner_state_contains(&response, "pending_categories", category),
            "{category} should be Pending before tier2_run: {response:#}"
        );
        assert_summary_status(&response, category, "pending");
    }
}

#[test]
fn inspect_tier2_run_returns_promptly_with_background_in_flight() {
    let (_temp_dir, root) = fixture_project();
    for index in 0..40 {
        write_file(
            &root,
            &format!("src/file_{index:03}.ts"),
            &format!(
                "export function unused_{index}() {{ return {index}; }}
"
            ),
        );
    }
    let ctx = configured_context_with_callgraph_store(&root, true);
    ensure_callgraph_store_ready(&ctx);

    let response = enqueue_tier2_run(&ctx, &["dead_code"]);

    // Queue/in-flight state is the load-resistant promptness contract: if the
    // command scanned inline, the category would not still be marked in flight.
    // A wall-clock bound here flaked under shared CPU contention.
    assert!(
        response["queued_categories"]
            .as_array()
            .expect("queued_categories array")
            .iter()
            .any(|category| category.as_str() == Some("dead_code")),
        "dead_code should be queued: {response:#}"
    );
    assert!(
        response["in_flight_categories"]
            .as_array()
            .expect("in_flight_categories array")
            .iter()
            .any(|category| category.as_str() == Some("dead_code")),
        "dead_code should be marked in-flight: {response:#}"
    );

    wait_for_tier2(&ctx, &["dead_code"]);
}

fn duplicate_fixture_source() -> &'static str {
    r#"
export function calculate(input: number) {
  const first = input + 1;
  const second = first + 2;
  const third = second + first;
  const fourth = third + 3;
  const fifth = fourth + third;
  return fifth + second;
}
"#
}

fn tier2_snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

fn dead_code_tier2_snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
    let config = Config {
        project_root: Some(project_root.to_path_buf()),
        callgraph_store: true,
        ..Config::default()
    };
    InspectSnapshot::new(
        project_root.to_path_buf(),
        inspect_dir.to_path_buf(),
        Arc::new(config),
        Arc::new(RwLock::new(SymbolCache::new())),
    )
}

#[test]
fn inspect_dead_code_reuse_reports_unavailable_when_store_not_ready() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let callgraph_dir = inspect_dir.parent().expect("harness dir").join("callgraph");
    let _not_ready_store =
        CallGraphStore::open(callgraph_dir, root.clone()).expect("open non-ready callgraph store");

    let manager = InspectManager::new();
    let success = manager
        .tier2_run_with_reuse_result(
            dead_code_tier2_snapshot(&root, &inspect_dir),
            InspectCategory::DeadCode,
            None,
        )
        .outcome
        .expect("dead_code unavailable aggregate succeeds");

    assert!(
        success.contributions.is_empty(),
        "unavailable callgraph must not fabricate per-file dead_code contributions"
    );
    assert_eq!(success.aggregate["callgraph_available"], false);
    assert_eq!(success.aggregate["notes"], json!(["callgraph_unavailable"]));
    assert_eq!(success.aggregate["count"], 0);
}

#[test]
fn inspect_dead_code_unavailable_renders_honestly_and_preserves_status_bar_count() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }
",
    );
    let ctx = configured_context(&root);
    ctx.update_status_bar_tier2(Some(7), Some(2), Some(3), Some(0), false);

    let inspect_dir = ctx.inspect_dir();
    let success = ctx
        .inspect_manager()
        .tier2_run_with_reuse_result(
            dead_code_tier2_snapshot(&root, &inspect_dir),
            InspectCategory::DeadCode,
            None,
        )
        .outcome
        .expect("dead_code unavailable aggregate succeeds");
    assert_eq!(success.aggregate["callgraph_available"], false);

    let (dead_code, _, _) = ctx
        .inspect_manager()
        .latest_tier2_counts(inspect_dir, root.clone());
    assert_eq!(dead_code, None, "unavailable dead_code must not publish D0");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-unavailable-text",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["status"], "unavailable");
    assert_eq!(
        response["summary"]["dead_code"]["reason"],
        "call graph building/retrying"
    );
    let text = response["text"].as_str().expect("inspect text");
    assert!(
        text.contains("Dead code: unavailable (call graph building/retrying)"),
        "unavailable callgraph should not render as zero: {text}"
    );
    assert!(!text.contains("Dead code: 0"));

    let status_bar = ctx
        .status_bar_counts()
        .expect("seeded status bar remains visible");
    assert_eq!(status_bar.dead_code, 7);
}

fn run_duplicates_reuse(
    manager: &InspectManager,
    project_root: &Path,
    inspect_dir: &Path,
) -> InspectScanSuccess {
    manager
        .tier2_run_with_reuse_result(
            tier2_snapshot(project_root, inspect_dir),
            InspectCategory::Duplicates,
            None,
        )
        .outcome
        .expect("duplicates tier2 reuse run succeeds")
}

fn relative_scanned_paths(project_root: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|file| {
            file.strip_prefix(project_root)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn duplicate_aggregate_mentions_file(success: &InspectScanSuccess, file_prefix: &str) -> bool {
    success.aggregate["items"].as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            group["files"].as_array().is_some_and(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|file| file.starts_with(file_prefix))
            })
        })
    })
}

#[test]
fn inspect_command_tier2_quick_reuse_is_path_aware_after_rename() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let first = run_duplicates_reuse(&first_manager, &root, &inspect_dir);
    assert_eq!(first.scanned_files.len(), 2);
    assert!(duplicate_aggregate_mentions_file(&first, "src/foo.ts:"));
    assert!(duplicate_aggregate_mentions_file(&first, "src/bar.ts:"));

    fs::rename(root.join("src/foo.ts"), root.join("src/baz.ts")).expect("rename fixture file");

    let second_manager = InspectManager::new();
    let second = run_duplicates_reuse(&second_manager, &root, &inspect_dir);

    assert_eq!(
        relative_scanned_paths(&root, &second.scanned_files),
        vec!["src/baz.ts"],
        "rename should invalidate quick reuse and rescan the new path"
    );
    assert!(duplicate_aggregate_mentions_file(&second, "src/baz.ts:"));
    assert!(duplicate_aggregate_mentions_file(&second, "src/bar.ts:"));
    assert!(
        !duplicate_aggregate_mentions_file(&second, "src/foo.ts:"),
        "renamed path must not leak from the stale aggregate"
    );
}

#[test]
fn inspect_command_tier2_quick_reuse_skips_rescan_for_unchanged_file_set() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let manager = Arc::new(InspectManager::new());

    let first = run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir);
    assert_eq!(first.scanned_files.len(), 2);

    let second = run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir);
    assert!(
        second.scanned_files.is_empty(),
        "unchanged file identity set should use quick reuse without rescanning"
    );
    assert_eq!(second.aggregate, first.aggregate);

    let handles = (0..4)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let root = root.clone();
            let inspect_dir = inspect_dir.clone();
            thread::spawn(move || run_duplicates_reuse(manager.as_ref(), &root, &inspect_dir))
        })
        .collect::<Vec<_>>();

    for handle in handles {
        let success = handle.join().expect("concurrent quick reuse joins");
        assert!(
            success.scanned_files.is_empty(),
            "concurrent freshness/fingerprint reads should reuse without rescanning"
        );
        assert_eq!(success.aggregate, first.aggregate);
    }
}

#[test]
fn inspect_command_computed_tier2_zero_count_stays_count_zero() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/unique.ts",
        "export function unique(input: number) { return input + 1; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-zero",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        !scanner_state_contains(&response, "pending_categories", "duplicates"),
        "computed duplicate cache hit must not be pending: {response:#}"
    );
    assert!(
        !scanner_state_contains(&response, "stale_categories", "duplicates"),
        "computed duplicate cache hit must not be stale: {response:#}"
    );
    assert_summary_count(&response, "duplicates", 0);
    assert_eq!(
        response["summary"]["duplicates"]["total_groups"].as_u64(),
        Some(0),
        "computed zero duplicate summary should keep total_groups=0: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_warm_cache_hit_is_not_stale() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-warm-cache",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        !scanner_state_contains(&response, "stale_categories", "duplicates"),
        "warm duplicate cache hit must not be marked stale: {response:#}"
    );
    assert!(
        !scanner_state_contains(&response, "pending_categories", "duplicates"),
        "warm duplicate cache hit must not be marked pending: {response:#}"
    );
    assert!(
        response["summary"]["duplicates"]["total_groups"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "duplicates aggregate should be available from cache: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_changed_file_surfaces_stale_category() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    write_file(
        &root,
        "src/foo.ts",
        "export function changed(input: number) { return input + 42; }\n",
    );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-stale-cache",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert!(
        scanner_state_contains(&response, "stale_categories", "duplicates"),
        "changed duplicate source should mark cached aggregate stale: {response:#}"
    );
    assert_summary_stale_with_counts(&response, "duplicates");
}

#[test]
fn inspect_command_tier2_hash_miss_after_restart_serves_stale_dead_code_results() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "import { used } from './lib';\nused();\n",
    );
    let lib = write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let before = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-before-hash-miss",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );
    assert_eq!(before["success"], true, "inspect failed: {before:#}");
    assert!(
        !scanner_state_contains(&before, "stale_categories", "dead_code"),
        "freshly computed dead_code aggregate should not be stale: {before:#}"
    );
    assert!(
        !scanner_state_contains(&before, "pending_categories", "dead_code"),
        "freshly computed dead_code aggregate should not be pending: {before:#}"
    );
    let before_count = before["summary"]["dead_code"]["count"]
        .as_u64()
        .expect("dead_code count");
    assert!(
        before_count > 0,
        "dead_code should have cached results: {before:#}"
    );
    assert!(
        dead_code_items(&before).contains(&("src/lib.ts".to_string(), "unused".to_string())),
        "dead_code fixture should report the intentionally unused export: {before:#}"
    );

    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 10; }\nexport function unused() { return 20; }\n",
    );

    // Simulate the restarted-process hash-miss case: a changed source file has
    // fresh per-file contribution metadata in SQLite, while the aggregate row is
    // still the previous contribution_set_hash. Old behavior returned Pending
    // here because get_aggregated() misses the exact hash and ignored the
    // persisted aggregate row.
    let cache = InspectCache::open(ctx.inspect_dir(), root.clone()).expect("open inspect cache");
    let changed_freshness = cache_freshness::collect(&lib).expect("collect changed freshness");
    cache
        .update_content_fresh_metadata(
            InspectCategory::DeadCode,
            Path::new("src").join("lib.ts").as_path(),
            &changed_freshness,
        )
        .expect("update contribution metadata to force aggregate hash miss");
    assert!(
        cache
            .get_aggregated(&JobKey::for_project_category(InspectCategory::DeadCode))
            .expect("hash-aware aggregate lookup")
            .is_none(),
        "test setup must force the exact-hash aggregate lookup to miss"
    );

    let restarted_ctx = configured_context_with_callgraph_store(&root, true);
    let after = inspect(
        &restarted_ctx,
        json!({
            "id": "inspect-dead-code-after-restart-hash-miss",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(after["success"], true, "inspect failed: {after:#}");
    assert!(
        scanner_state_contains(&after, "stale_categories", "dead_code"),
        "hash-miss fallback should surface the last-known aggregate as stale: {after:#}"
    );
    assert!(
        !scanner_state_contains(&after, "pending_categories", "dead_code"),
        "hash-miss fallback should not drop to pending when an aggregate exists: {after:#}"
    );
    assert_summary_stale_with_counts(&after, "dead_code");
    assert!(
        dead_code_items(&after).contains(&("src/lib.ts".to_string(), "unused".to_string())),
        "stale hash-miss response should retain previous details: {after:#}"
    );
}

#[test]
fn inspect_command_tier2_aggregate_hash_mismatch_is_cache_miss() {
    let (_temp_dir, root) = fixture_project();
    let file = write_file(&root, "src/foo.ts", duplicate_fixture_source());
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
    let key = JobKey::for_project_category(InspectCategory::Duplicates);
    let freshness = cache_freshness::collect(&file).expect("collect freshness");
    let contribution = FileContribution::new(
        InspectCategory::Duplicates,
        file.clone(),
        freshness,
        json!({"file": "src/foo.ts", "fragments": []}),
    );

    cache
        .store_tier2_result(
            key.clone(),
            std::slice::from_ref(&file),
            &[contribution],
            json!({"count": 1, "items": [{"file": "src/foo.ts"}]}),
        )
        .expect("store tier2 aggregate");
    assert!(
        cache
            .get_aggregated(&key)
            .expect("warm aggregate")
            .is_some(),
        "freshly stored aggregate should be readable"
    );

    write_file(
        &root,
        "src/foo.ts",
        "export function changed(input: number) { return input + 42; }\n",
    );
    let changed_freshness = cache_freshness::collect(&file).expect("collect changed freshness");
    cache
        .update_content_fresh_metadata(
            InspectCategory::Duplicates,
            Path::new("src/foo.ts"),
            &changed_freshness,
        )
        .expect("update cached contribution metadata without aggregate recompute");

    assert!(
        cache
            .get_aggregated(&key)
            .expect("hash-aware memory aggregate read")
            .is_none(),
        "in-memory aggregate must miss after contribution_set_hash changes"
    );
    let reopened = InspectCache::open(inspect_dir, root).expect("reopen cache");
    assert!(
        reopened
            .get_aggregated(&key)
            .expect("hash-aware sqlite aggregate read")
            .is_none(),
        "persisted aggregate must miss when its stored contribution_set_hash is stale"
    );
}

fn dead_code_items(response: &Value) -> Vec<(String, String)> {
    response["details"]["dead_code"]
        .as_array()
        .expect("dead_code details array")
        .iter()
        .map(|item| {
            (
                item["file"].as_str().expect("dead file").to_string(),
                item["symbol"].as_str().expect("dead symbol").to_string(),
            )
        })
        .collect()
}

fn unused_export_items(response: &Value) -> Vec<(String, String)> {
    response["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details array")
        .iter()
        .map(|item| {
            (
                item["file"]
                    .as_str()
                    .expect("unused export file")
                    .to_string(),
                item["symbol"]
                    .as_str()
                    .expect("unused export symbol")
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn inspect_command_dead_code_uses_cargo_manifest_targets_not_nested_main_files() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
autobins = false

[[bin]]
name = "fixture-cli"
path = "src/bin/app.rs"
"#,
    );
    write_file(
        &root,
        "src/bin/app.rs",
        "pub fn declared_bin_entry() -> u32 { 1 }\npub fn unused_bin_helper() -> u32 { 0 }\nfn main() { declared_bin_entry(); }\n",
    );
    write_file(
        &root,
        "tools/main.rs",
        "pub fn nested_only() -> u32 { 2 }\npub fn nested_main() -> u32 { nested_only() }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-cargo-manifest-entry-points",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let items = dead_code_items(&response);
    assert!(
        items.contains(&("tools/main.rs".to_string(), "nested_only".to_string())),
        "nested main.rs must not be treated as a Cargo entry point: {response:#}"
    );
    assert!(
        !items.contains(&(
            "src/bin/app.rs".to_string(),
            "declared_bin_entry".to_string()
        )),
        "declared Cargo bin export called from main should be live: {response:#}"
    );
    assert!(
        items.contains(&(
            "src/bin/app.rs".to_string(),
            "unused_bin_helper".to_string()
        )),
        "binary exports are not public API and should remain eligible: {response:#}"
    );
}

#[test]
fn inspect_command_unused_exports_uses_package_exports_as_public_api_but_not_bin() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        r#"{
  "name": "fixture",
  "exports": {
    ".": "./src/index.ts",
    "./feature": { "import": "./src/feature.ts" }
  },
  "bin": { "fixture": "./src/cli.ts" }
}
"#,
    );
    write_file(
        &root,
        "src/index.ts",
        "export function publicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/feature.ts",
        "export function publicFeature() { return 2; }\n",
    );
    write_file(
        &root,
        "src/cli.ts",
        "export function cliEntry() { return 3; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function nonPublicUncalled() { return 4; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-package-public-api",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        unused_export_items(&response),
        vec![
            ("src/cli.ts".to_string(), "cliEntry".to_string()),
            (
                "src/internal.ts".to_string(),
                "nonPublicUncalled".to_string()
            ),
        ],
        "package exports should be public API while bin/non-public exports are reported: {response:#}"
    );
}

#[test]
fn inspect_command_dead_code_and_unused_exports_share_workspace_public_api_resolution() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "package.json",
        r#"{"private":true,"workspaces":["apps/*"]}"#,
    );
    write_file(
        &root,
        "apps/service/package.json",
        r#"{"name":"service","exports":"./src/index.ts"}"#,
    );
    write_file(
        &root,
        "apps/service/src/index.ts",
        "export function serviceApi() { return 1; }\n",
    );
    write_file(
        &root,
        "apps/service/src/internal.ts",
        "export function serviceInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-shared-public-api-resolution",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "apps/service/src/internal.ts".to_string(),
            "serviceInternal".to_string()
        )],
        "dead_code should use the workspace package public API: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "apps/service/src/internal.ts".to_string(),
            "serviceInternal".to_string()
        )],
        "unused_exports should match dead_code without a packages/* assumption: {response:#}"
    );
}

#[test]
fn inspect_command_manifestless_projects_keep_conventional_entry_point_fallback() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/index.ts",
        "export function fallbackPublicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function fallbackInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-manifestless-entry-point-fallback",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest-less conventional index.ts should remain an entry/public API file: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest-less fallback should be shared by unused_exports: {response:#}"
    );
}

#[test]
fn inspect_command_manifest_without_declared_entries_uses_conventional_fallback() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"private":true}"#);
    write_file(
        &root,
        "src/index.ts",
        "export function fallbackPublicApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function fallbackInternal() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code", "unused_exports"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-manifest-no-entry-fallback",
            "command": "inspect",
            "sections": ["dead_code", "unused_exports"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        dead_code_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest presence without declared roots should still use conventional index.ts fallback for dead_code: {response:#}"
    );
    assert_eq!(
        unused_export_items(&response),
        vec![(
            "src/internal.ts".to_string(),
            "fallbackInternal".to_string()
        )],
        "manifest presence without declared roots should still use conventional index.ts fallback for unused_exports: {response:#}"
    );
}

#[test]
fn inspect_command_manifest_edit_invalidates_unused_exports_aggregate() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", r#"{"main":"./src/index.ts"}"#);
    write_file(
        &root,
        "src/index.ts",
        "export function indexApi() { return 1; }\n",
    );
    write_file(
        &root,
        "src/public.ts",
        "export function publicApi() { return 2; }\n",
    );
    write_file(
        &root,
        "src/internal.ts",
        "export function internalOnly() { return 3; }\n",
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let before = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-before-manifest-edit",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );
    assert_eq!(before["success"], true, "inspect failed: {before:#}");
    assert_eq!(
        unused_export_items(&before),
        vec![
            ("src/internal.ts".to_string(), "internalOnly".to_string()),
            ("src/public.ts".to_string(), "publicApi".to_string()),
        ],
        "initial package main should suppress only index.ts: {before:#}"
    );

    write_file(&root, "package.json", r#"{"main":"./src/public.ts"}"#);
    tier2_run(&ctx, &["unused_exports"]);
    let after = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-after-manifest-edit",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 10,
        }),
    );

    assert_eq!(after["success"], true, "inspect failed: {after:#}");
    assert_eq!(
        unused_export_items(&after),
        vec![
            ("src/index.ts".to_string(), "indexApi".to_string()),
            ("src/internal.ts".to_string(), "internalOnly".to_string()),
        ],
        "manifest edit should change the contribution_set_hash and force aggregate recompute: {after:#}"
    );
}

#[test]
fn inspect_command_dead_code_keeps_same_name_exports_distinct_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/main.ts",
        "import { foo } from './alive';\nexport function main() { return foo(); }\n",
    );
    write_file(
        &root,
        "src/alive.ts",
        "export function foo() { return 1; }\n",
    );
    write_file(
        &root,
        "src/dead.ts",
        "export function foo() { return 2; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-same-name",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["count"], 1);
    assert_eq!(
        dead_code_items(&response),
        vec![("src/dead.ts".to_string(), "foo".to_string())]
    );
}

#[test]
fn inspect_command_dead_code_reports_unreachable_cycle_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/a.ts",
        "import { b } from './b';\nexport function a() { return b(); }\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { a } from './a';\nexport function b() { return a(); }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-cycle",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let mut items = dead_code_items(&response);
    items.sort();
    assert_eq!(response["summary"]["dead_code"]["count"], 2);
    assert_eq!(
        items,
        vec![
            ("src/a.ts".to_string(), "a".to_string()),
            ("src/b.ts".to_string(), "b".to_string()),
        ]
    );
}

#[test]
fn inspect_command_dead_code_keeps_multi_hop_entry_reachability_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/main.ts",
        "import { b } from './b';\nexport function main() { return b(); }\n",
    );
    write_file(
        &root,
        "src/b.ts",
        "import { c } from './c';\nexport function b() { return c(); }\n",
    );
    write_file(&root, "src/c.ts", "export function c() { return 3; }\n");
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-multihop",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["dead_code"]["count"], 0);
    assert!(
        dead_code_items(&response).is_empty(),
        "response: {response:#}"
    );
}

#[test]
fn inspect_command_dead_code_resolves_extensionless_package_module_entry_after_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "package.json", "{\"module\":\"src/index\"}\n");
    write_file(
        &root,
        "src/index.mts",
        "export function publicApi() { return 1; }\n",
    );
    let ctx = configured_context_with_callgraph_store(&root, true);

    tier2_run(&ctx, &["dead_code"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-package-entry",
            "command": "inspect",
            "sections": "dead_code",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(
        response["summary"]["dead_code"]["count"], 0,
        "extensionless package module entry should be public API: {response:#}"
    );
}

#[test]
fn inspect_command_duplicates_summary_count_uses_production_payload() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-count",
            "command": "inspect",
            "sections": "duplicates",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    let total_groups = response["summary"]["duplicates"]["total_groups"]
        .as_u64()
        .expect("duplicates total_groups");
    assert!(
        count > 0,
        "duplicates count should be non-zero: {response:#}"
    );
    assert_eq!(
        count, total_groups,
        "summary should mirror scanner contract: {response:#}"
    );
    assert!(
        !response["details"]["duplicates"]
            .as_array()
            .expect("duplicates details")
            .is_empty(),
        "duplicates details should include groups: {response:#}"
    );
}

#[test]
fn inspect_command_duplicates_file_scope_matches_occurrence_labels() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/scoped/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/scoped/baz.ts", duplicate_fixture_source());
    write_file(&root, "src/outside/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-scoped",
            "command": "inspect",
            "sections": "duplicates",
            "scope": "src/scoped",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    assert!(
        count > 0,
        "scoped duplicates should retain groups duplicated within scope: {response:#}"
    );
    let details = response["details"]["duplicates"]
        .as_array()
        .expect("duplicates details");
    assert!(
        !details.is_empty(),
        "expected scoped duplicate details: {response:#}"
    );
    for group in details {
        let files = group["files"].as_array().expect("group files");
        assert!(
            files.len() >= 2,
            "duplicate groups with fewer than two in-scope files should be dropped: {response:#}"
        );
        assert!(
            files
                .iter()
                .filter_map(Value::as_str)
                .all(|file| file.starts_with("src/scoped/")),
            "scoped duplicate groups must prune out-of-scope files: {response:#}"
        );
    }
}

#[test]
fn inspect_command_unused_exports_scope_filters_full_contributions_before_cap() {
    let (_temp_dir, root) = fixture_project();
    for index in 0..120 {
        write_file(
            &root,
            &format!("aaa_global/file_{index:03}.ts"),
            &format!("export function global_{index:03}() {{ return {index}; }}\n"),
        );
    }
    for index in 0..3 {
        write_file(
            &root,
            &format!("zzz_scoped/file_{index:03}.ts"),
            &format!("export function scoped_{index:03}() {{ return {index}; }}\n"),
        );
    }
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["unused_exports"]);
    let scoped = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-scoped-after-cap",
            "command": "inspect",
            "sections": "unused_exports",
            "scope": "zzz_scoped",
            "topK": 100,
        }),
    );

    assert_eq!(scoped["success"], true, "inspect failed: {scoped:#}");
    assert_eq!(
        scoped["summary"]["unused_exports"]["count"], 3,
        "scoped count should come from full contributions, not the capped project aggregate: {scoped:#}"
    );
    let scoped_details = scoped["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details");
    assert_eq!(
        scoped_details.len(),
        3,
        "scoped details should include all scoped items beyond the project cap: {scoped:#}"
    );
    assert!(
        scoped_details.iter().all(|item| item["file"]
            .as_str()
            .is_some_and(|file| file.starts_with("zzz_scoped/"))),
        "scoped details should only include scoped files: {scoped:#}"
    );

    let project = inspect(
        &ctx,
        json!({
            "id": "inspect-unused-exports-project-cap",
            "command": "inspect",
            "sections": "unused_exports",
            "topK": 100,
        }),
    );

    assert_eq!(project["success"], true, "inspect failed: {project:#}");
    assert_eq!(
        project["summary"]["unused_exports"]["count"], 123,
        "project-wide count should keep the full aggregate count: {project:#}"
    );
    let project_details = project["details"]["unused_exports"]
        .as_array()
        .expect("unused_exports details");
    assert_eq!(
        project_details.len(),
        100,
        "project-wide details should still be capped at 100: {project:#}"
    );
    assert!(
        project_details
            .iter()
            .all(|item| item["file"].as_str().is_some_and(|file| file.starts_with("aaa_global/"))),
        "project-wide cap should be applied before later zzz_scoped files appear in details: {project:#}"
    );
}

#[test]
fn inspect_command_duplicates_scope_filters_full_contributions_before_cap() {
    let (_temp_dir, root) = fixture_project();
    // Distinct per-file markers so the whole-file (program) node is not itself a
    // cross-file duplicate (which would correctly subsume the 130 inner groups);
    // the 130 functions remain byte-identical across files and so stay duplicated.
    write_file(
        &root,
        "aaa_global/left.ts",
        &many_duplicate_groups_source(2),
    );
    write_file(
        &root,
        "aaa_global/right.ts",
        &many_duplicate_groups_source(3),
    );
    write_file(
        &root,
        "zzz_scoped/left.ts",
        &many_duplicate_groups_source(4),
    );
    write_file(
        &root,
        "zzz_scoped/right.ts",
        &many_duplicate_groups_source(5),
    );
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-scoped-after-cap",
            "command": "inspect",
            "sections": "duplicates",
            "scope": "zzz_scoped",
            "topK": 100,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    assert!(
        count > 100,
        "scoped duplicate count should come from full contributions, not the capped project aggregate: {response:#}"
    );
    let details = response["details"]["duplicates"]
        .as_array()
        .expect("duplicates details");
    assert_eq!(
        details.len(),
        100,
        "scoped duplicate details should be capped after filtering the full rollup: {response:#}"
    );
    assert!(
        details.iter().all(|group| group["files"]
            .as_array()
            .expect("group files")
            .iter()
            .filter_map(Value::as_str)
            .all(|file| file.starts_with("zzz_scoped/"))),
        "scoped duplicate details should only include scoped files: {response:#}"
    );
}

/// 130 literal-distinct functions shared across files (the real cross-file
/// duplicate groups) plus a trailing marker function whose statement count is
/// unique per `unique_stmts`. The unique marker makes each file's top-level
/// (program) node structurally distinct, so the WHOLE FILE is not itself a
/// cross-file duplicate that would (correctly) subsume the 130 inner groups
/// under the nested-overlap collapse. Each caller passes a distinct
/// `unique_stmts` so every file's program node appears exactly once.
fn many_duplicate_groups_source(unique_stmts: usize) -> String {
    let mut source = String::new();
    for index in 0..130 {
        source.push_str(&format!(
            r#"export function duplicate_group_{index:03}(input: number) {{
  const first = input + {index};
  const second = first * {};
  const third = second - {};
  const label = "group_{index:03}";
  if (third > {}) {{
    return label + third.toString();
  }}
  return label + first.toString();
}}
"#,
            index + 3,
            index + 7,
            index + 11
        ));
    }
    source.push_str("function file_unique_marker() {\n");
    for n in 0..unique_stmts {
        source.push_str(&format!("  const marker_{n} = {n} * 2 + 1;\n"));
    }
    source.push_str("  return 0;\n}\n");
    source
}

#[test]
fn inspect_command_duplicates_project_wide_cap_preserves_total_groups() {
    let (_temp_dir, root) = fixture_project();
    // Distinct per-file markers (see scope test): keep the 130 functions
    // duplicated across files without the whole file becoming one big duplicate.
    write_file(&root, "src/left.ts", &many_duplicate_groups_source(2));
    write_file(&root, "src/right.ts", &many_duplicate_groups_source(3));
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-project-cap",
            "command": "inspect",
            "sections": "duplicates",
            "topK": 100,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    let total_groups = response["summary"]["duplicates"]["total_groups"]
        .as_u64()
        .expect("duplicates total_groups");
    assert!(
        count > 100,
        "fixture should produce more groups than the drill-down cap: {response:#}"
    );
    assert_eq!(
        total_groups, count,
        "project-wide total_groups should retain the full group count: {response:#}"
    );
    assert_eq!(
        response["details"]["duplicates"]
            .as_array()
            .expect("duplicates details")
            .len(),
        100,
        "project-wide duplicate details should still be capped at 100: {response:#}"
    );
}

#[test]
fn inspect_command_tier2_last_run_updates_on_hash_match_reuse() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    let cold = inspect(
        &ctx,
        json!({
            "id": "inspect-last-run-cold",
            "command": "inspect",
        }),
    );
    assert_eq!(cold["success"], true, "inspect failed: {cold:#}");
    assert!(
        cold["scanner_state"]["tier2_last_run"].is_null(),
        "cold Tier 2 state should not have a last run: {cold:#}"
    );

    tier2_run(&ctx, &["duplicates"]);
    let first = inspect(
        &ctx,
        json!({
            "id": "inspect-last-run-first",
            "command": "inspect",
        }),
    );
    let first_last_run = first["scanner_state"]["tier2_last_run"]
        .as_i64()
        .expect("first tier2_last_run");

    tier2_run(&ctx, &["duplicates"]);
    let second = inspect(
        &ctx,
        json!({
            "id": "inspect-last-run-second",
            "command": "inspect",
        }),
    );
    let second_last_run = second["scanner_state"]["tier2_last_run"]
        .as_i64()
        .expect("second tier2_last_run");

    assert!(
        second_last_run > first_last_run,
        "hash-match reuse should refresh tier2_last_run: first={first_last_run} second={second_last_run} response={second:#}"
    );
}

fn configure_fake_rust_lsp(ctx: &AppContext) {
    ctx.lsp()
        .override_binary(ServerKind::Rust, fake_server_path());
}

fn open_with_lsp(ctx: &AppContext, file: &Path, content: &str) {
    let config = ctx.config().clone();
    ctx.lsp()
        .notify_file_changed(file, content, &config)
        .expect("notify file changed");
    let diagnostics = ctx
        .lsp()
        .wait_for_diagnostics(file, &config, Duration::from_secs(2));
    assert!(
        !diagnostics.is_empty(),
        "fake LSP should publish diagnostics for {file:?}"
    );
}

fn close_with_lsp(ctx: &AppContext, file: &Path) {
    let config = ctx.config().clone();
    ctx.lsp().notify_file_closed(file).expect("close file");
    let diagnostics = ctx
        .lsp()
        .wait_for_diagnostics(file, &config, Duration::from_secs(2));
    assert!(
        diagnostics.is_empty(),
        "fake LSP close should publish checked-clean diagnostics"
    );
    assert!(
        ctx.lsp().has_diagnostic_report_for_file(file),
        "empty publish should remain as checked-clean proof"
    );
}

#[test]
fn inspect_command_diagnostics_default_reports_warm_counts_and_details() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-warm\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &file, "fn main() {}\n");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-warm",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(1));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(1));
    assert_eq!(summary.get("info").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("hints").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "warm diagnostics should be computed, not pending: {response:#}"
    );

    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert_eq!(details.len(), 2, "response: {response:#}");
    assert!(details.iter().all(|item| item["file"] == "src/main.rs"));
    assert!(details.iter().any(|item| item["severity"] == "error"));
    assert!(details.iter().any(|item| item["severity"] == "warning"));
}

#[test]
fn inspect_command_diagnostics_pending_when_no_server_ran() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-pending\"\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-no-server-ran",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    // New contract: counts-so-far ARE present alongside the pending status.
    // Honesty comes from the `status` field (not the absence of counts): an
    // agent seeing `status: pending` knows the counts are not the final picture,
    // so a 0 here is never misread as "clean".
    assert_eq!(
        summary.get("status").and_then(Value::as_str),
        Some("pending"),
        "pending status must be present so counts aren't read as final: {response:#}"
    );
    assert_eq!(
        summary.get("errors").and_then(Value::as_u64),
        Some(0),
        "counts-so-far should be present (0 found yet) alongside pending: {response:#}"
    );
    assert!(
        scanner_state_contains(&response, "pending_categories", "diagnostics"),
        "pending diagnostics should appear in scanner_state: {response:#}"
    );
}

#[test]
fn inspect_command_diagnostics_clean_zero_after_empty_publish() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-clean\"\n");
    let file = write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &file, "fn main() {}\n");
    close_with_lsp(&ctx, &file);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-clean",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(summary.get("errors").and_then(Value::as_u64), Some(0));
    assert_eq!(summary.get("warnings").and_then(Value::as_u64), Some(0));
    assert!(
        !summary.contains_key("status"),
        "checked-clean diagnostics should be distinct from pending: {response:#}"
    );
    assert!(response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details")
        .is_empty());
}

#[test]
fn inspect_command_diagnostics_scope_actively_pulls_cold_file_and_narrows() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-scope\"\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    ctx.lsp().set_extra_env("AFT_FAKE_LSP_PULL", "1");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-scoped-pull",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/main.rs",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 1);
    assert_eq!(response["summary"]["diagnostics"]["warnings"], 0);
    let details = response["details"]["diagnostics"]
        .as_array()
        .expect("diagnostics details");
    assert_eq!(details.len(), 1, "response: {response:#}");
    assert_eq!(details[0]["file"], "src/main.rs");
    assert_eq!(details[0]["message"], "test pull diagnostic");
}

#[test]
fn inspect_command_diagnostics_missing_server_is_incomplete_not_zero() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-missing\"\n");
    write_file(&root, "src/main.rs", "fn main() {}\n");
    let ctx = configured_context(&root);
    ctx.lsp().override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server"),
    );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-missing-server",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/main.rs",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(
        summary.get("status").and_then(Value::as_str),
        Some("incomplete")
    );
    assert!(
        summary["servers_not_installed"]
            .as_array()
            .is_some_and(|servers| servers.iter().any(|server| server == "rust")),
        "missing server should be named: {response:#}"
    );
    // Counts-so-far present alongside the incomplete status (the status flags
    // that more may exist behind the missing server, so 0 isn't "clean").
    assert_eq!(
        summary.get("errors").and_then(Value::as_u64),
        Some(0),
        "counts-so-far should accompany incomplete status: {response:#}"
    );
}

#[test]
fn inspect_command_diagnostics_no_server_for_filetype_reports_no_server_not_pending() {
    // Regression: scoping diagnostics at a file type that has NO registered LSP
    // server (here a Markdown file in a Rust project) used to report
    // status: "pending" forever — implying results were still coming when none
    // ever would. It must report a terminal "no_server" status, carry a
    // files_without_server count, and NOT be listed in pending_categories.
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "Cargo.toml",
        "[package]\nname = \"diag-no-server\"\n",
    );
    write_file(&root, "docs/readme.md", "# Title\n\nsome prose\n");
    let ctx = configured_context(&root);
    // No LSP server configured for Markdown — ensure_server_for_file returns
    // no_server_registered for the scoped .md file.

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-no-server",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "docs/readme.md",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(
        summary.get("status").and_then(Value::as_str),
        Some("no_server"),
        "no registered server must report terminal no_server, not pending: {response:#}"
    );
    assert!(
        summary
            .get("files_without_server")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 1),
        "files_without_server count must be surfaced: {response:#}"
    );
    assert!(
        summary["servers_pending"]
            .as_array()
            .is_some_and(|servers| servers.is_empty()),
        "no server is pending — nothing is coming: {response:#}"
    );
    // A terminal no_server state must NOT keep the category in pending_categories
    // (which would tell the agent to keep waiting for a Tier-2 refresh).
    assert!(
        !scanner_state_contains(&response, "pending_categories", "diagnostics"),
        "no_server diagnostics must not be reported as pending: {response:#}"
    );
}

#[test]
fn inspect_command_diagnostics_inapplicable_root_marker_reports_no_server_not_pending() {
    // Regression: a server registered for the file's extension whose root
    // markers are ABSENT from the project (e.g. oxlint registered for `.ts`
    // with no `.oxlintrc.json`) returned ServerAttemptResult::NoRootMarker,
    // which was bucketed into servers_pending. Because a missing root marker is
    // a filesystem fact that never changes mid-scan, scoped diagnostics then
    // reported status: "pending" forever — even after every truly-applicable
    // server answered — telling the agent to re-run indefinitely. Such a file
    // has no applicable server: it must report terminal "no_server", carry a
    // files_without_server count, and NOT appear pending.
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let storage_dir = root.join(".aft-test-storage");
    fs::create_dir_all(&root).expect("create project root");
    // Source matches the custom server's extension, but the required root
    // marker (`needs-this-marker.json`) is deliberately NOT written.
    write_file(&root, "src/app.customts", "export const value = 1;\n");

    let server_id = "needs-marker-ls";
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.clone()),
            lsp_servers: vec![aft::config::UserServerDef {
                id: server_id.to_string(),
                extensions: vec!["customts".to_string()],
                binary: "needs-marker-ls".to_string(),
                args: Vec::new(),
                root_markers: vec!["needs-this-marker.json".to_string()],
                env: Default::default(),
                initialization_options: None,
                disabled: false,
            }],
            ..Config::default()
        },
    );
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "search_index": false,
        "semantic_search": false,
    }));
    let configure_response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(
        configure_response["success"], true,
        "configure failed: {configure_response:#}"
    );
    // Mark the binary as installed so the attempt reaches the root-marker check
    // (NoRootMarker), not BinaryNotInstalled.
    ctx.lsp().override_binary(
        ServerKind::Custom(std::sync::Arc::from(server_id)),
        fake_server_path(),
    );

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-inapplicable-marker",
            "command": "inspect",
            "sections": ["diagnostics"],
            "scope": "src/app.customts",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let summary = response["summary"]["diagnostics"].as_object().unwrap();
    assert_eq!(
        summary.get("status").and_then(Value::as_str),
        Some("no_server"),
        "an inapplicable root-marker server must report terminal no_server, not pending: {response:#}"
    );
    assert!(
        summary
            .get("files_without_server")
            .and_then(Value::as_u64)
            .is_some_and(|count| count >= 1),
        "files_without_server must count the inapplicable file: {response:#}"
    );
    assert!(
        summary["servers_pending"]
            .as_array()
            .is_some_and(|servers| servers.is_empty()),
        "the inapplicable server must NOT be pending — nothing is coming: {response:#}"
    );
    assert!(
        !scanner_state_contains(&response, "pending_categories", "diagnostics"),
        "inapplicable-marker diagnostics must not be reported as pending: {response:#}"
    );
}

#[test]
fn inspect_command_diagnostics_details_honor_top_k() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "Cargo.toml", "[package]\nname = \"diag-top-k\"\n");
    let main_rs = write_file(&root, "src/main.rs", "fn main() {}\n");
    let lib_rs = write_file(&root, "src/lib.rs", "pub fn lib() {}\n");
    let ctx = configured_context(&root);
    configure_fake_rust_lsp(&ctx);
    open_with_lsp(&ctx, &main_rs, "fn main() {}\n");
    open_with_lsp(&ctx, &lib_rs, "pub fn lib() {}\n");

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics-top-k",
            "command": "inspect",
            "sections": ["diagnostics"],
            "topK": 3,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    assert_eq!(response["summary"]["diagnostics"]["errors"], 2);
    assert_eq!(response["summary"]["diagnostics"]["warnings"], 2);
    assert_eq!(
        response["details"]["diagnostics"]
            .as_array()
            .expect("diagnostics details")
            .len(),
        3,
        "diagnostics details should honor topK: {response:#}"
    );
}
