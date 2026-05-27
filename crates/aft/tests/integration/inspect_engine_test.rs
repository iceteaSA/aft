use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::cache_freshness;
use aft::config::Config;
use aft::inspect::{
    contribution_is_fresh, verify_contribution_file, ContributionFreshness, FileContribution,
    InspectCache, InspectCategory, InspectManager, InspectResult, InspectScanSuccess,
    InspectSnapshot, InspectWorker, JobKey, JobOutcome, JobScope,
};
use aft::parser::SymbolCache;
use serde_json::json;

use super::helpers::AftProcess;

fn fixture_project() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    let src = root.join("src");
    fs::create_dir_all(&src).expect("create src");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"inspect-fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    let file = src.join("lib.rs");
    fs::write(&file, "pub fn alive() {}\n").expect("write source");
    (temp_dir, root, file)
}

fn snapshot(project_root: &Path, inspect_dir: &Path) -> InspectSnapshot {
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

fn test_worker(worker_count: Arc<AtomicUsize>, sleep_for: Duration, count: u64) -> InspectWorker {
    Arc::new(move |job| {
        let started = Instant::now();
        worker_count.fetch_add(1, Ordering::SeqCst);
        thread::sleep(sleep_for);
        let aggregate = json!({
            "count": count,
            "items": [{"file": "src/lib.rs", "line": 1}],
        });
        InspectResult::success(
            &job,
            InspectScanSuccess {
                scanned_files: job.scope_files.clone(),
                contributions: Vec::new(),
                aggregate,
            },
            started.elapsed(),
        )
    })
}

#[test]
fn inspect_engine_cache_persists_tier2_contributions_and_aggregate() {
    let (_temp_dir, root, file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
    let freshness = cache_freshness::collect(&file).expect("collect freshness");
    let key = JobKey::for_project_category(InspectCategory::DeadCode);
    let contribution = FileContribution::new(
        InspectCategory::DeadCode,
        file.clone(),
        freshness,
        json!({"file": "src/lib.rs", "exported_symbols": [], "outbound_calls": []}),
    );

    cache
        .store_tier2_result(
            key.clone(),
            std::slice::from_ref(&file),
            &[contribution],
            json!({"count": 1, "items": [{"file": "src/lib.rs", "symbol": "alive"}]}),
        )
        .expect("store result");

    assert!(cache.sqlite_path().starts_with(&inspect_dir));
    assert!(
        cache
            .contribution_set_hash(InspectCategory::DeadCode)
            .unwrap()
            .len()
            >= 32
    );

    let reopened = InspectCache::open(inspect_dir, root).expect("reopen cache");
    let aggregate = reopened
        .get_aggregated(&key)
        .expect("read aggregate")
        .expect("aggregate present");
    assert_eq!(aggregate["count"], 1);

    let contributions = reopened
        .load_tier2_contributions(InspectCategory::DeadCode)
        .expect("load contributions");
    assert_eq!(contributions.len(), 1);
    assert_eq!(contributions[0].file_path, PathBuf::from("src/lib.rs"));
    assert_eq!(contributions[0].contribution["file"], "src/lib.rs");
}

#[test]
fn inspect_engine_freshness_treats_hot_and_content_fresh_as_fresh() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let file = temp_dir.path().join("a.rs");
    fs::write(&file, "alpha").expect("write file");
    let freshness = cache_freshness::collect(&file).expect("collect freshness");

    assert!(contribution_is_fresh(&file, &freshness));

    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).expect("touch mtime");
    match verify_contribution_file(&file, &freshness) {
        ContributionFreshness::Fresh {
            metadata_changed, ..
        } => assert!(metadata_changed),
        other => panic!("expected content-fresh contribution, got {other:?}"),
    }

    fs::write(&file, "bravo").expect("write changed same-size file");
    assert_eq!(
        verify_contribution_file(&file, &freshness),
        ContributionFreshness::Stale
    );

    fs::remove_file(&file).expect("delete file");
    assert_eq!(
        verify_contribution_file(&file, &freshness),
        ContributionFreshness::Deleted
    );
}

#[test]
fn inspect_engine_deduplicates_in_flight_waiters() {
    let (_temp_dir, root, _file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let worker_count = Arc::new(AtomicUsize::new(0));
    let manager = Arc::new(InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(150), 7),
        Duration::from_secs(2),
    ));
    let snapshot = snapshot(&root, &inspect_dir);
    let scope = JobScope::for_project(root.clone());

    let first_manager = Arc::clone(&manager);
    let first_snapshot = snapshot.clone();
    let first_scope = scope.clone();
    let first = thread::spawn(move || {
        first_manager.submit_category(first_snapshot, InspectCategory::DeadCode, first_scope)
    });

    thread::sleep(Duration::from_millis(25));

    let second_manager = Arc::clone(&manager);
    let second = thread::spawn(move || {
        second_manager.submit_category(snapshot, InspectCategory::DeadCode, scope)
    });

    let first = first.join().expect("first waiter");
    let second = second.join().expect("second waiter");

    assert_eq!(
        worker_count.load(Ordering::SeqCst),
        1,
        "one worker job should serve both waiters"
    );
    assert!(matches!(first, JobOutcome::Fresh { .. }));
    assert!(matches!(second, JobOutcome::Fresh { .. }));
    assert_eq!(first.payload().unwrap()["count"], 7);
    assert_eq!(second.payload().unwrap()["count"], 7);
}

#[test]
fn inspect_engine_drain_routes_idle_scan_to_cache() {
    let (_temp_dir, root, _file) = fixture_project();
    let inspect_dir = root.join(".aft-cache").join("inspect");
    let worker_count = Arc::new(AtomicUsize::new(0));
    let manager = InspectManager::with_worker(
        test_worker(Arc::clone(&worker_count), Duration::from_millis(25), 3),
        Duration::from_secs(1),
    );
    let snapshot = snapshot(&root, &inspect_dir);
    let scope = JobScope::for_project(root.clone());
    let key = manager
        .submit_background(snapshot.clone(), InspectCategory::Duplicates, scope)
        .expect("queue background scan");

    let mut drained = 0usize;
    for _ in 0..20 {
        drained += manager.drain_completions();
        if drained > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    assert_eq!(worker_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        drained, 1,
        "background completion should drain exactly once"
    );
    let cache = manager.cache_for_snapshot(&snapshot).expect("cache");
    let aggregate = cache
        .get_aggregated(&key)
        .expect("aggregate read")
        .expect("aggregate present");
    assert_eq!(aggregate["count"], 3);
}

#[test]
fn inspect_engine_command_returns_lane_a_shape() {
    let (_temp_dir, root, _file) = fixture_project();
    let mut aft = AftProcess::spawn();
    let configure = aft.configure(&root);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = aft.send(
        &json!({
            "id": "inspect-engine",
            "command": "inspect",
            "sections": "all",
            "topK": 5,
        })
        .to_string(),
    );

    assert_eq!(
        response["success"], true,
        "inspect should succeed: {response:?}"
    );
    assert!(response["summary"]["diagnostics"].is_object());
    assert!(response["summary"]["metrics"].is_object());
    assert!(response["summary"]["todos"].is_object());
    assert!(response["details"]["dead_code"].is_array());
    assert!(response["scanner_state"]["disabled_categories"]
        .as_array()
        .expect("disabled categories")
        .iter()
        .any(|category| category == "vulnerabilities"));

    assert!(aft.shutdown().success());
}
