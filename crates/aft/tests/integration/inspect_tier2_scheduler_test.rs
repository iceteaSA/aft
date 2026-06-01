use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use aft::commands::configure::handle_configure;
use aft::commands::inspect::handle_inspect;
use aft::config::Config;
use aft::context::AppContext;
use aft::inspect::tier2_scheduler::TIER2_REFRESH_COLD_CACHE_DELAY;
use aft::inspect::Tier2TriggerReason;
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

    let response = wait_for_tier2(&ctx, &["dead_code", "unused_exports", "duplicates"]);
    assert_eq!(
        response["scanner_state"]["tier2_trigger_reason"].as_str(),
        Some("debounce"),
        "inspect should expose the watcher debounce trigger reason: {response:#}"
    );
}

#[test]
fn inspect_stale_tier2_sets_pull_demand_without_blocking() {
    let (_temp_dir, root) = fixture_project();
    write_file(
        &root,
        "src/lib.ts",
        "export function unused() { return 1; }\n",
    );
    let ctx = configured_context(&root);
    let base = Instant::now();
    ctx.reset_tier2_refresh_scheduler_at(base);

    let started = Instant::now();
    let response = inspect(&ctx);
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "inspect should return promptly without running tier2 inline; elapsed={elapsed:?} response={response:#}"
    );
    assert!(
        scanner_state_contains(&response, "pending_categories", "dead_code"),
        "cold tier2 should be pending before the pull-triggered refresh: {response:#}"
    );
    assert!(
        ctx.tier2_pull_demand_pending(),
        "inspect should leave a scheduler pull demand for stale/pending tier2"
    );
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(base + Duration::from_secs(1), 0),
        Some(Tier2TriggerReason::Pull)
    );

    let response = wait_for_tier2(&ctx, &["dead_code", "unused_exports", "duplicates"]);
    assert_eq!(
        response["scanner_state"]["tier2_trigger_reason"].as_str(),
        Some("pull"),
        "inspect should expose the pull trigger reason after the demanded refresh: {response:#}"
    );
}
