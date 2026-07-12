use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use aft::callgraph_store::CallGraphStore;
use aft::commands::configure::handle_configure;
use aft::commands::inspect::{handle_inspect, handle_inspect_tier2_run};
use aft::config::Config;
use aft::context::{AppContext, CallgraphStoreAccess};
use aft::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY;
use aft::inspect::{InspectCache, InspectCategory, InspectSnapshot, Tier2TriggerReason};
use aft::parser::TreeSitterProvider;
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
    configured_context_with_storage(root, &root.join(".aft-test-storage"), true)
}

fn configured_context_with_storage(
    root: &Path,
    storage_dir: &Path,
    callgraph_store: bool,
) -> AppContext {
    crate::helpers::disable_in_process_file_watcher();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage_dir.to_path_buf()),
            ..Config::default()
        },
    );
    let configure = request(json!({
        "id": "configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": root.to_string_lossy(),
        "storage_dir": storage_dir.to_string_lossy(),
        "config": crate::helpers::user_config(serde_json::json!({
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": callgraph_store
        })),
    }));
    let response = serde_json::to_value(handle_configure(&configure, &ctx))
        .expect("configure response serializes");
    assert_eq!(response["success"], true, "configure failed: {response:#}");
    if callgraph_store {
        ensure_callgraph_store_ready(&ctx);
    }
    ctx
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn linked_worktree_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let worktree_root = temp_dir.path().join("linked-worktree");
    let storage_dir = temp_dir.path().join("storage");
    fs::create_dir_all(&root).expect("create project root");
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    write_file(
        &root,
        "src/copy.ts",
        "export function unusedCopy() { return 1; }\n",
    );
    git(&root, &["init"]);
    git(&root, &["config", "user.name", "AFT Test"]);
    git(&root, &["config", "user.email", "aft-test@example.com"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "fixture"]);
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "linked-inspect-test",
            worktree_root.to_str().expect("UTF-8 worktree path"),
        ],
    );
    (temp_dir, root, worktree_root, storage_dir)
}

fn tier2_aggregate_bytes(ctx: &AppContext, root: &Path) -> BTreeMap<String, Vec<u8>> {
    let cache = InspectCache::open_readonly(ctx.inspect_dir(), root.to_path_buf())
        .expect("open inspect cache")
        .expect("inspect cache exists");
    [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
        InspectCategory::Cycles,
    ]
    .into_iter()
    .map(|category| {
        let aggregate = cache
            .latest_aggregate_any_hash(category)
            .expect("read Tier-2 aggregate")
            .expect("Tier-2 aggregate exists");
        (
            category.as_str().to_string(),
            serde_json::to_vec(&aggregate).expect("serialize aggregate"),
        )
    })
    .collect()
}

fn drain_callgraph_store_for_test(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().lock();
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
        drop(store);
        if let Some(project_root) = ctx.callgraph_project_root() {
            let store = CallGraphStore::open_readonly(ctx.callgraph_store_dir(), project_root)
                .expect("open read-only callgraph store")
                .expect("ready callgraph store");
            *ctx.callgraph_store()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(std::sync::Arc::new(store));
        }
        *ctx.callgraph_store_rx().lock() = None;
    } else if disconnected {
        *ctx.callgraph_store_rx().lock() = None;
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

fn inspect(ctx: &AppContext) -> Value {
    let response = handle_inspect(
        &request(json!({
            "id": "inspect",
            "command": "inspect",
        })),
        ctx,
    );
    serde_json::to_value(response).expect("inspect response serializes")
}

fn scanner_state_categories(response: &Value, key: &str) -> Vec<String> {
    response["scanner_state"][key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str().map(str::to_string).or_else(|| {
                        item.get("category")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
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

fn wait_for_tier2(ctx: &AppContext, categories: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = inspect(ctx);
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
            return response;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for tier2 categories {categories:?}: {response:#}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn watcher_tick_after_quiet_gap_triggers_tier2_refresh() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);

    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 1),
        None
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + TIER2_REFRESH_COLD_CACHE_DELAY, 0),
        Some(Tier2TriggerReason::Debounce)
    );

    let response = wait_for_tier2(
        &ctx,
        &["dead_code", "unused_exports", "duplicates", "cycles"],
    );
    assert_eq!(
        response["scanner_state"]["tier2_trigger_reason"].as_str(),
        Some("debounce"),
        "inspect should expose the watcher debounce trigger reason: {response:#}"
    );
}

#[test]
fn direct_inspect_mid_watcher_quiet_window_computes_immediately() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    let change = base + TIER2_REFRESH_COLD_CACHE_DELAY;
    ctx.reset_tier2_refresh_scheduler_at(base);
    assert_eq!(ctx.tick_tier2_refresh_scheduler_at(change, 1), None);

    let response = inspect(&ctx);

    assert!(
        !scanner_state_contains(&response, "pending_categories", "dead_code"),
        "direct inspect should compute Tier-2 during the watcher quiet window: {response:#}"
    );
    assert_eq!(
        ctx.inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        0,
        "the direct inspect path must not wait for or enqueue an automatic refresh"
    );
}

#[test]
fn direct_inspect_cold_tier2_computes_without_scheduler_pull() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);

    let response = inspect(&ctx);

    assert!(
        !scanner_state_contains(&response, "pending_categories", "dead_code"),
        "direct inspect should wait for the cold Tier-2 result when it finishes before the deadline: {response:#}"
    );
    assert!(
        !ctx.tier2_pull_demand_pending(),
        "fresh direct inspect should not leave a scheduler pull demand"
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 0),
        None
    );
}

#[test]
fn linked_worktree_skips_automatic_tier2_and_leaves_parent_gate_open() {
    let (_temp_dir, root, worktree_root, storage_dir) = linked_worktree_fixture();
    let parent_ctx = configured_context_with_storage(&root, &storage_dir, false);
    let worktree_ctx = configured_context_with_storage(&worktree_root, &storage_dir, false);

    assert!(!parent_ctx.is_worktree_bridge());
    assert!(worktree_ctx.is_worktree_bridge());
    assert!(
        worktree_ctx.build_status_snapshot()["status_bar"].is_null(),
        "an unscanned worktree must not fabricate Tier-2 status counts"
    );

    let worktree_base = Instant::now();
    worktree_ctx.reset_tier2_refresh_scheduler_at(worktree_base);
    assert!(!worktree_ctx.request_tier2_refresh_pull());
    assert_eq!(
        worktree_ctx
            .tick_tier2_refresh_scheduler_at(worktree_base + TIER2_REFRESH_COLD_CACHE_DELAY, 0,),
        None
    );
    let manager_submission = worktree_ctx
        .inspect_manager()
        .submit_tier2_run_with_reuse_background(
            InspectSnapshot::new_with_capabilities(
                worktree_root.clone(),
                worktree_ctx.inspect_dir(),
                worktree_ctx.config(),
                worktree_ctx.symbol_cache(),
                true,
                false,
            ),
            InspectCategory::Duplicates,
        )
        .expect("manager worktree gate is not an error");
    assert!(manager_submission.is_none());

    let warm_response = serde_json::to_value(handle_inspect_tier2_run(
        &request(json!({
            "id": "worktree-tier2-warm",
            "command": "inspect_tier2_run",
            "categories": ["dead_code", "unused_exports", "duplicates", "cycles"],
        })),
        &worktree_ctx,
    ))
    .expect("Tier-2 warm response serializes");
    assert_eq!(warm_response["success"], true);
    assert_eq!(warm_response["queued_categories"], json!([]));
    assert_eq!(
        worktree_ctx
            .inspect_manager()
            .automatic_tier2_schedule_count_for_test(),
        0,
        "no automatic Tier-2 scheduling path may reach the manager for a linked worktree"
    );

    assert!(
        parent_ctx
            .inspect_manager()
            .automatic_tier2_refresh_allowed(),
        "linked-worktree detection must not close the parent root's scheduling gate"
    );
}

#[test]
fn linked_worktree_explicit_inspect_keeps_parent_aggregates_byte_identical() {
    let (_temp_dir, root, worktree_root, storage_dir) = linked_worktree_fixture();
    let parent_ctx = configured_context_with_storage(&root, &storage_dir, false);
    let worktree_ctx = configured_context_with_storage(&worktree_root, &storage_dir, false);

    let parent_response = inspect(&parent_ctx);
    assert_eq!(
        parent_response["success"], true,
        "parent inspect failed: {parent_response:#}"
    );
    let parent_before = tier2_aggregate_bytes(&parent_ctx, &root);
    assert!(
        worktree_ctx.build_status_snapshot()["status_bar"].is_null(),
        "worktree status must remain absent until explicit demand produces real counts"
    );

    let worktree_response = inspect(&worktree_ctx);
    assert_eq!(
        worktree_response["success"], true,
        "worktree inspect failed: {worktree_response:#}"
    );
    let pending = scanner_state_categories(&worktree_response, "pending_categories");
    assert!(
        ["dead_code", "unused_exports", "duplicates", "cycles"]
            .iter()
            .all(|category| !pending.iter().any(|pending| pending == category)),
        "explicit worktree inspect should complete its Tier-2 demand scan: {worktree_response:#}"
    );
    assert!(
        worktree_response["summary"]["unused_exports"]["count"].is_number(),
        "explicit worktree inspect should return computed Tier-2 data: {worktree_response:#}"
    );

    let parent_after = tier2_aggregate_bytes(&parent_ctx, &root);
    assert_eq!(
        parent_after, parent_before,
        "a worktree demand scan must not alter any parent Tier-2 aggregate bytes"
    );
    assert_ne!(
        parent_ctx.inspect_dir(),
        worktree_ctx.inspect_dir(),
        "absolute root identity must keep parent and worktree inspect generations separate"
    );
    let parent_cache = InspectCache::open_readonly(parent_ctx.inspect_dir(), root)
        .expect("open parent inspect cache")
        .expect("parent inspect cache exists");
    let worktree_cache = InspectCache::open_readonly(worktree_ctx.inspect_dir(), worktree_root)
        .expect("open worktree inspect cache")
        .expect("worktree inspect cache exists");
    assert_ne!(parent_cache.project_key(), worktree_cache.project_key());
}
