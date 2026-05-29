use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::configure::handle_configure;
use aft::commands::inspect::{handle_inspect, handle_inspect_tier2_run};
use aft::config::Config;
use aft::context::AppContext;
use aft::inspect::{InspectCategory, InspectManager, InspectScanSuccess, InspectSnapshot};
use aft::parser::{SymbolCache, TreeSitterProvider};
use aft::protocol::RawRequest;
use serde_json::{json, Value};

fn fixture_project() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project root");
    (temp_dir, root)
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
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    ctx
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
    let ctx = configured_context(&root);

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
fn inspect_command_dead_code_returns_pending_before_tier2_run() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function used() { return 1; }\nexport function unused() { return 2; }\n",
    );
    let ctx = configured_context(&root);

    // No tier2_run call — inspect should return Pending for dead_code without
    // running the scanner synchronously (which would block for seconds on big
    // projects).
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-dead-code-cold",
            "command": "inspect",
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let pending = response["scanner_state"]["pending_categories"]
        .as_array()
        .expect("pending_categories array");
    assert!(
        pending.iter().any(|category| category == "dead_code"),
        "dead_code should be Pending before tier2_run: {response:#}"
    );
    let count = response["summary"]["dead_code"]["count"]
        .as_u64()
        .expect("dead_code count");
    assert_eq!(
        count, 0,
        "Pending dead_code should report count=0 (no cached aggregate): {response:#}"
    );
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
    let ctx = configured_context(&root);

    let started = Instant::now();
    let response = enqueue_tier2_run(&ctx, &["dead_code"]);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(250),
        "inspect_tier2_run should enqueue without scanning inline; elapsed={elapsed:?} response={response:#}"
    );
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
    assert!(
        response["summary"]["duplicates"]["total_groups"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "stale duplicate cache should still expose cached summary: {response:#}"
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
    let ctx = configured_context(&root);

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
    let ctx = configured_context(&root);

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
    let ctx = configured_context(&root);

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
    let ctx = configured_context(&root);

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
    write_file(&root, "src/foo.ts", duplicate_fixture_source());
    write_file(&root, "src/bar.ts", duplicate_fixture_source());
    let ctx = configured_context(&root);

    tier2_run(&ctx, &["duplicates"]);
    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-duplicates-scoped",
            "command": "inspect",
            "sections": "duplicates",
            "scope": "src/foo.ts",
            "topK": 10,
        }),
    );

    assert_eq!(response["success"], true, "inspect failed: {response:#}");
    let count = response["summary"]["duplicates"]["count"]
        .as_u64()
        .expect("duplicates count");
    assert!(
        count > 0,
        "scoped duplicates should retain matching groups: {response:#}"
    );
    let details = response["details"]["duplicates"]
        .as_array()
        .expect("duplicates details");
    assert!(
        details.iter().any(|group| {
            group["files"]
                .as_array()
                .expect("group files")
                .iter()
                .filter_map(Value::as_str)
                .any(|file| file.starts_with("src/foo.ts:"))
        }),
        "scoped duplicates should include foo occurrence labels: {response:#}"
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

#[test]
fn inspect_command_diagnostics_is_not_active_in_v0_33() {
    let (_temp_dir, root) = fixture_project();
    write_file(&root, "src/app.ts", "export function app() { return 1; }\n");
    let ctx = configured_context(&root);

    let response = inspect(
        &ctx,
        json!({
            "id": "inspect-diagnostics",
            "command": "inspect",
            "sections": ["diagnostics"],
        }),
    );

    assert_eq!(
        response["success"], false,
        "diagnostics should be inactive: {response:#}"
    );
    assert_eq!(response["code"], "invalid_request");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains("registered but disabled in v0.33")),
        "diagnostics should be rejected while deferred: {response:#}"
    );
}
