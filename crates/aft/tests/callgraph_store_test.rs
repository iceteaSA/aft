use aft::callgraph::{walk_project_files, CallGraph};
use aft::callgraph_store::{live_callgraph_edge_snapshot, CallGraphStore, StoredEdge};
use aft::commands::callgraph_store_adapter;
use aft::config::Config;
use aft::context::AppContext;
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use filetime::FileTime;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tempfile::tempdir;

static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_800_000_000);

#[test]
fn store_op_outputs_match_legacy_for_tier1_languages() {
    let dir = tempdir().unwrap();
    write_parity_project(dir.path());
    let root = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let files = project_files(&root);
    let store = CallGraphStore::open(root.join(".store-op-parity"), root.clone()).unwrap();
    store.cold_build(&files).unwrap();

    assert_op_parity(
        &root,
        &store,
        "typescript callers",
        |graph| {
            serde_json::to_value(
                graph
                    .callers_of(&root.join("src/foo.ts"), "foo", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::callers_result(&store, &root.join("src/foo.ts"), "foo", 2)
                    .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "typescript call_tree",
        |graph| {
            serde_json::to_value(
                graph
                    .forward_tree(&root.join("src/main.ts"), "main", 2)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::call_tree_result(
                    &store,
                    &root.join("src/main.ts"),
                    "main",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "typescript impact",
        |graph| {
            serde_json::to_value(
                graph
                    .impact(&root.join("src/foo.ts"), "foo", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::impact_result(&store, &root.join("src/foo.ts"), "foo", 2)
                    .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "typescript trace_to",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to(&root.join("src/foo.ts"), "foo", 4, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_result(
                    &store,
                    &root.join("src/foo.ts"),
                    "foo",
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "typescript trace_to_symbol",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to_symbol(
                        &root.join("src/main.ts"),
                        "main",
                        "foo",
                        Some(&root.join("src/foo.ts")),
                        4,
                        usize::MAX,
                    )
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_symbol_result(
                    &store,
                    &root.join("src/main.ts"),
                    "main",
                    "foo",
                    Some(&root.join("src/foo.ts")),
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );

    assert_op_parity(
        &root,
        &store,
        "javascript callers",
        |graph| {
            serde_json::to_value(
                graph
                    .callers_of(&root.join("src/js_helper.js"), "jsHelper", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::callers_result(
                    &store,
                    &root.join("src/js_helper.js"),
                    "jsHelper",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "javascript call_tree",
        |graph| {
            serde_json::to_value(
                graph
                    .forward_tree(&root.join("src/app.js"), "jsEntry", 2)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::call_tree_result(
                    &store,
                    &root.join("src/app.js"),
                    "jsEntry",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "javascript impact",
        |graph| {
            serde_json::to_value(
                graph
                    .impact(&root.join("src/js_helper.js"), "jsHelper", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::impact_result(
                    &store,
                    &root.join("src/js_helper.js"),
                    "jsHelper",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "javascript trace_to",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to(&root.join("src/js_helper.js"), "jsHelper", 4, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_result(
                    &store,
                    &root.join("src/js_helper.js"),
                    "jsHelper",
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "javascript trace_to_symbol",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to_symbol(
                        &root.join("src/app.js"),
                        "jsEntry",
                        "jsHelper",
                        Some(&root.join("src/js_helper.js")),
                        4,
                        usize::MAX,
                    )
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_symbol_result(
                    &store,
                    &root.join("src/app.js"),
                    "jsEntry",
                    "jsHelper",
                    Some(&root.join("src/js_helper.js")),
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );

    assert_op_parity(
        &root,
        &store,
        "rust callers",
        |graph| {
            serde_json::to_value(
                graph
                    .callers_of(&root.join("src/util.rs"), "rust_helper", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::callers_result(
                    &store,
                    &root.join("src/util.rs"),
                    "rust_helper",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "rust call_tree",
        |graph| {
            serde_json::to_value(
                graph
                    .forward_tree(&root.join("src/lib.rs"), "rust_entry", 2)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::call_tree_result(
                    &store,
                    &root.join("src/lib.rs"),
                    "rust_entry",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "rust impact",
        |graph| {
            serde_json::to_value(
                graph
                    .impact(&root.join("src/util.rs"), "rust_helper", 2, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::impact_result(
                    &store,
                    &root.join("src/util.rs"),
                    "rust_helper",
                    2,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "rust trace_to",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to(&root.join("src/util.rs"), "rust_helper", 4, usize::MAX)
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_result(
                    &store,
                    &root.join("src/util.rs"),
                    "rust_helper",
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
    assert_op_parity(
        &root,
        &store,
        "rust trace_to_symbol",
        |graph| {
            serde_json::to_value(
                graph
                    .trace_to_symbol(
                        &root.join("src/lib.rs"),
                        "rust_entry",
                        "rust_helper",
                        Some(&root.join("src/util.rs")),
                        4,
                        usize::MAX,
                    )
                    .unwrap(),
            )
            .unwrap()
        },
        || {
            serde_json::to_value(
                callgraph_store_adapter::trace_to_symbol_result(
                    &store,
                    &root.join("src/lib.rs"),
                    "rust_entry",
                    "rust_helper",
                    Some(&root.join("src/util.rs")),
                    4,
                )
                .unwrap(),
            )
            .unwrap()
        },
    );
}

#[test]
fn store_edges_match_live_callgraph_for_tier1_languages() {
    let dir = tempdir().unwrap();
    write_parity_project(dir.path());
    let files = project_files(dir.path());
    let store_dir = dir.path().join(".store");
    let store = CallGraphStore::open(store_dir, dir.path().to_path_buf()).unwrap();

    let stats = store.cold_build(&files).unwrap();
    assert!(
        stats.files >= 6,
        "expected mixed TS/JS/Rust files: {stats:?}"
    );

    let store_edges = store.edge_snapshot().unwrap();
    let live_edges = live_callgraph_edge_snapshot(dir.path(), &files).unwrap();
    assert_eq!(store_edges, live_edges);
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("main.ts")),
        "parity fixture should exercise TypeScript edges: {store_edges:#?}"
    );
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("app.js")),
        "parity fixture should exercise JavaScript edges: {store_edges:#?}"
    );
    assert!(
        store_edges
            .iter()
            .any(|edge| edge.source_file.ends_with("lib.rs")),
        "parity fixture should exercise Rust edges: {store_edges:#?}"
    );
}

#[test]
fn scenario_matrix_incremental_matches_cold_rebuild() {
    run_scenario(
        "rename symbol",
        setup_rename_symbol,
        edit_rename_symbol,
        None,
    );
    run_scenario("delete file", setup_delete_file, edit_delete_file, None);
    run_scenario(
        "delete reexport-only barrel",
        setup_barrel,
        edit_delete_barrel,
        None,
    );
    run_scenario(
        "add file satisfying prior unresolved import",
        setup_unresolved_import,
        edit_add_late_file,
        None,
    );
    run_scenario(
        "move symbol via reexport topology",
        setup_barrel_move,
        edit_move_reexport,
        None,
    );
    run_scenario(
        "barrel retarget while old target exists",
        setup_barrel,
        edit_retarget_barrel,
        None,
    );
    run_scenario(
        "file that both defines and calls",
        setup_defines_and_calls,
        edit_defines_and_calls,
        None,
    );
    run_scenario(
        "body-only edit does not invalidate fan-in",
        setup_body_only,
        edit_body_only,
        Some(|stats| {
            assert!(
                stats.surface_changed.is_empty(),
                "body-only edit should not change surface: {stats:?}"
            );
            assert_eq!(
                stats.dependency_selected_refs, 0,
                "body-only edit must not select fan-in refs: {stats:?}"
            );
        }),
    );
}

#[test]
fn scenario_matrix_op_outputs_incremental_match_cold_rebuild() {
    run_op_scenario(
        "rename symbol",
        setup_rename_symbol,
        edit_rename_symbol,
        ScenarioQuery::new("a.ts", "renamed", "a.ts", "outer", "renamed", Some("a.ts")),
    );
    run_op_scenario(
        "delete file",
        setup_delete_file,
        edit_delete_file,
        ScenarioQuery::new(
            "main.ts",
            "main",
            "main.ts",
            "main",
            "main",
            Some("main.ts"),
        ),
    );
    run_op_scenario(
        "delete reexport-only barrel",
        setup_barrel,
        edit_delete_barrel,
        ScenarioQuery::new(
            "main.ts",
            "main",
            "main.ts",
            "main",
            "main",
            Some("main.ts"),
        ),
    );
    run_op_scenario(
        "add file satisfying prior unresolved import",
        setup_unresolved_import,
        edit_add_late_file,
        ScenarioQuery::new(
            "late.ts",
            "late",
            "main.ts",
            "main",
            "late",
            Some("late.ts"),
        ),
    );
    run_op_scenario(
        "move symbol via reexport topology",
        setup_barrel_move,
        edit_move_reexport,
        ScenarioQuery::new("alt.ts", "foo", "main.ts", "main", "foo", Some("alt.ts")),
    );
    run_op_scenario(
        "barrel retarget while old target exists",
        setup_barrel,
        edit_retarget_barrel,
        ScenarioQuery::new("alt.ts", "foo", "main.ts", "main", "foo", Some("alt.ts")),
    );
    run_op_scenario(
        "file that both defines and calls",
        setup_defines_and_calls,
        edit_defines_and_calls,
        ScenarioQuery::new(
            "combo.ts",
            "next",
            "combo.ts",
            "caller",
            "next",
            Some("combo.ts"),
        ),
    );
    run_op_scenario(
        "body-only edit does not invalidate fan-in",
        setup_body_only,
        edit_body_only,
        ScenarioQuery::new("foo.ts", "foo", "main.ts", "main", "foo", Some("foo.ts")),
    );
}

#[test]
fn query_api_matches_hand_built_ts_expectations() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        r#"export function entry() {
  second();
  missing();
}

function second() {
  leaf();
}

function leaf() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-query"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let entry = store.node_for(Path::new("main.ts"), "entry").unwrap();
    assert_eq!(entry.symbol, "entry");
    assert!(entry.is_entry_point);

    let unresolved = store.unresolved_calls_of(&entry).unwrap();
    assert_eq!(
        unresolved
            .iter()
            .map(|call| call.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["missing"]
    );

    let tree = store.call_tree(Path::new("main.ts"), "entry", 2).unwrap();
    assert_eq!(tree.name, "entry");
    assert_eq!(
        tree.children
            .iter()
            .map(|child| child.name.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "missing"]
    );
    assert!(tree.children[0].resolved);
    assert!(!tree.children[1].resolved);
    assert_eq!(tree.children[0].children[0].name, "leaf");

    let callers = store.callers_of(Path::new("main.ts"), "leaf", 2).unwrap();
    assert_eq!(callers.target.symbol, "leaf");
    assert!(callers
        .callers
        .iter()
        .any(|site| site.caller.symbol == "second"));
    assert!(callers
        .callers
        .iter()
        .any(|site| site.caller.symbol == "entry"));

    let impact = store.impact_of(Path::new("main.ts"), "leaf", 1).unwrap();
    assert_eq!(impact.target.symbol, "leaf");
    assert_eq!(impact.callers[0].site.caller.symbol, "second");
    assert_eq!(
        impact.callers[0].call_expression.as_deref(),
        Some("leaf();")
    );

    let trace = store.trace_to(Path::new("main.ts"), "leaf", 4).unwrap();
    assert_eq!(trace.entry_points_found, 1);
    assert_eq!(
        trace.paths[0]
            .hops
            .iter()
            .map(|hop| hop.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["entry", "second", "leaf"]
    );

    let candidates = store.trace_to_symbol_candidates("leaf").unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].file, "main.ts");

    let path = store
        .trace_to_symbol(Path::new("main.ts"), "entry", "leaf", None, 4)
        .unwrap()
        .path
        .unwrap();
    assert_eq!(
        path.iter()
            .map(|hop| hop.symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["entry", "second", "leaf"]
    );
}

#[test]
fn reverse_lookup_includes_orphan_target_edges() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        r#"export function orphanCaller() {}
export function leaf() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-orphan"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let conn = rusqlite::Connection::open(store.sqlite_path()).unwrap();
    let source_node: String = conn
        .query_row(
            "SELECT id FROM nodes WHERE file_path = 'main.ts' AND scoped_name = 'orphanCaller'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO refs(ref_id, caller_node, caller_file, kind, short_name, full_ref, line, byte_start, byte_end, status, target_file, target_symbol, provenance)
         VALUES('orphan-ref', ?1, 'main.ts', 'call', 'leaf', 'leaf', 1, 1, 5, 'resolved', 'main.ts', 'leaf', 'test')",
        rusqlite::params![source_node],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges(edge_id, ref_id, source_node, target_node, target_file, target_symbol, kind, line, provenance)
         VALUES('orphan-edge', 'orphan-ref', ?1, NULL, 'main.ts', 'leaf', 'call', 1, 'test')",
        rusqlite::params![source_node],
    )
    .unwrap();
    drop(conn);

    let callers = store.callers_of(Path::new("main.ts"), "leaf", 1).unwrap();
    assert!(
        callers
            .callers
            .iter()
            .any(|site| site.caller.symbol == "orphanCaller" && site.target.is_none()),
        "target_node=NULL edge should still be found by target file/symbol: {callers:#?}"
    );
}

#[test]
fn cold_build_skips_failed_extracts_and_marks_them_stale() {
    let dir = tempdir().unwrap();
    let good = dir.path().join("good.ts");
    let bad = dir.path().join("bad.txt");
    write_file(&good, "export function good() {}\n");
    write_file(&bad, "not a callgraph language\n");

    let store = CallGraphStore::open(
        dir.path().join(".store-best-effort"),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let stats = store.cold_build(&[good, bad.clone()]).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.failed_files, vec!["bad.txt"]);
    assert_eq!(
        store.backend_status_for_file(&bad).unwrap().as_deref(),
        Some("stale")
    );
    assert!(store.node_for(Path::new("good.ts"), "good").is_ok());
}

#[test]
fn scoped_entry_point_flag_matches_legacy_scoped_input() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("suite.ts"),
        r#"class Suite {
  setup() {}
}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-entry"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let setup = store
        .node_for(Path::new("suite.ts"), "Suite::setup")
        .unwrap();
    assert!(
        !setup.is_entry_point,
        "scoped Suite::setup must not be treated like bare setup"
    );
}

#[test]
fn outgoing_calls_are_returned_in_source_order() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("order.ts"),
        r#"export function entry() { beta(); alpha(); }
function alpha() {}
function beta() {}
"#,
    );
    let store =
        CallGraphStore::open(dir.path().join(".store-order"), dir.path().to_path_buf()).unwrap();
    store.cold_build(&project_files(dir.path())).unwrap();

    let entry = store.node_for(Path::new("order.ts"), "entry").unwrap();
    let calls = store.outgoing_calls_of(&entry).unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call.target_symbol.as_str())
            .collect::<Vec<_>>(),
        vec!["beta", "alpha"]
    );
}

#[test]
fn cold_build_with_lease_swaps_atomically_over_old_ready_db() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        "export function entry() { oldLeaf(); }\nfunction oldLeaf() {}\n",
    );
    let files = project_files(dir.path());
    let store_dir = dir.path().join(".store-atomic");
    let (store, _) =
        CallGraphStore::cold_build_with_lease(store_dir.clone(), dir.path().to_path_buf(), &files)
            .unwrap();
    let old_edges = store.edge_snapshot().unwrap();
    assert!(!old_edges.is_empty());

    write_file(
        &dir.path().join("main.ts"),
        "export function entry() { newLeaf(); }\nfunction newLeaf() {}\n",
    );
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_for_hook = observed.clone();
    let root = dir.path().to_path_buf();
    let store_dir_for_hook = store_dir.clone();
    let old_edges_for_hook = old_edges.clone();
    aft::callgraph_store::set_cold_build_swap_observer(Some(std::sync::Arc::new(
        move |_tmp, _target| {
            let reader = CallGraphStore::open_readonly(store_dir_for_hook.clone(), root.clone())
                .unwrap()
                .expect("old ready DB should remain visible before swap");
            assert_eq!(reader.edge_snapshot().unwrap(), old_edges_for_hook);
            observed_for_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        },
    )));
    let rebuilt = CallGraphStore::cold_build_with_lease(
        store_dir,
        dir.path().to_path_buf(),
        &project_files(dir.path()),
    );
    aft::callgraph_store::set_cold_build_swap_observer(None);
    let (store, _) = rebuilt.unwrap();
    assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
    let tree = store.call_tree(Path::new("main.ts"), "entry", 1).unwrap();
    assert_eq!(tree.children[0].name, "newLeaf");
}

#[test]
fn app_context_demand_builds_once_and_worktree_reads_readonly() {
    let dir = tempdir().unwrap();
    write_file(
        &dir.path().join("main.ts"),
        "export function entry() { leaf(); }\nfunction leaf() {}\n",
    );
    let storage = dir.path().join("storage");
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(storage.clone()),
            callgraph_store: true,
            ..Config::default()
        },
    );
    ctx.set_harness(Harness::Opencode);
    ctx.set_canonical_cache_root(dir.path().to_path_buf());
    ctx.set_cache_role(false, None);

    {
        let store = ctx
            .ensure_callgraph_store()
            .unwrap()
            .expect("main checkout builds store");
        assert!(store.sqlite_path().is_file());
    }
    let sqlite_path = ctx
        .callgraph_store()
        .borrow()
        .as_ref()
        .unwrap()
        .sqlite_path()
        .to_path_buf();
    let first_mtime = fs::metadata(&sqlite_path).unwrap().modified().unwrap();
    {
        let store = ctx
            .ensure_callgraph_store()
            .unwrap()
            .expect("second ensure reuses open store");
        let tree = store.call_tree(Path::new("main.ts"), "entry", 1).unwrap();
        assert_eq!(tree.children[0].name, "leaf");
    }
    assert_eq!(
        fs::metadata(&sqlite_path).unwrap().modified().unwrap(),
        first_mtime
    );

    let worktree_ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(storage.clone()),
            callgraph_store: true,
            ..Config::default()
        },
    );
    worktree_ctx.set_harness(Harness::Opencode);
    worktree_ctx.set_canonical_cache_root(dir.path().to_path_buf());
    worktree_ctx.set_cache_role(true, None);
    assert!(worktree_ctx.ensure_callgraph_store().unwrap().is_some());

    let unavailable_dir = tempdir().unwrap();
    let unavailable_ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(unavailable_dir.path().to_path_buf()),
            storage_dir: Some(unavailable_dir.path().join("storage")),
            callgraph_store: true,
            ..Config::default()
        },
    );
    unavailable_ctx.set_harness(Harness::Opencode);
    unavailable_ctx.set_canonical_cache_root(unavailable_dir.path().to_path_buf());
    unavailable_ctx.set_cache_role(true, None);
    assert!(unavailable_ctx.ensure_callgraph_store().unwrap().is_none());
}

#[test]
#[ignore]
fn measure_current_worktree_cold_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf();
    let files = project_files(&root);
    let dir = tempdir().unwrap();
    let store = CallGraphStore::open(dir.path().join("callgraph"), root.clone()).unwrap();
    let stats = store.cold_build(&files).unwrap();
    let rss = peak_rss_bytes();
    eprintln!(
        "callgraph_store_measure root={} files={} nodes={} refs={} edges={} elapsed_ms={} peak_rss_bytes={}",
        root.display(),
        stats.files,
        stats.nodes,
        stats.refs,
        stats.edges,
        stats.elapsed_ms,
        rss.unwrap_or(0)
    );
}

#[derive(Clone, Copy)]
struct ScenarioQuery {
    target_file: &'static str,
    target_symbol: &'static str,
    tree_file: &'static str,
    tree_symbol: &'static str,
    to_symbol: &'static str,
    to_file: Option<&'static str>,
}

impl ScenarioQuery {
    fn new(
        target_file: &'static str,
        target_symbol: &'static str,
        tree_file: &'static str,
        tree_symbol: &'static str,
        to_symbol: &'static str,
        to_file: Option<&'static str>,
    ) -> Self {
        Self {
            target_file,
            target_symbol,
            tree_file,
            tree_symbol,
            to_symbol,
            to_file,
        }
    }
}

fn run_op_scenario(
    name: &str,
    setup: fn(&Path),
    edit: fn(&Path) -> Vec<PathBuf>,
    query: ScenarioQuery,
) {
    let dir = tempdir().unwrap();
    setup(dir.path());

    let root = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let files_before = project_files(&root);
    let incremental_store =
        CallGraphStore::open(root.join(".store-op-incremental"), root.clone()).unwrap();
    incremental_store.cold_build(&files_before).unwrap();

    let changed = edit(&root);
    incremental_store.refresh_files(&changed).unwrap();
    let incremental = scenario_op_snapshot(&incremental_store, &root, query);

    let cold_store = CallGraphStore::open(root.join(".store-op-cold"), root.clone()).unwrap();
    cold_store.cold_build(&project_files(&root)).unwrap();
    let cold = scenario_op_snapshot(&cold_store, &root, query);

    assert_eq!(
        incremental, cold,
        "scenario {name} op-layer output after incremental refresh must match cold rebuild"
    );
}

fn scenario_op_snapshot(store: &CallGraphStore, root: &Path, query: ScenarioQuery) -> Value {
    let target_file = root.join(query.target_file);
    let tree_file = root.join(query.tree_file);
    let to_file = query.to_file.map(|file| root.join(file));
    json!({
        "callers": serde_json::to_value(
            callgraph_store_adapter::callers_result(store, &target_file, query.target_symbol, 2)
                .unwrap()
        )
        .unwrap(),
        "call_tree": serde_json::to_value(
            callgraph_store_adapter::call_tree_result(store, &tree_file, query.tree_symbol, 2)
                .unwrap()
        )
        .unwrap(),
        "impact": serde_json::to_value(
            callgraph_store_adapter::impact_result(store, &target_file, query.target_symbol, 2)
                .unwrap()
        )
        .unwrap(),
        "trace_to": serde_json::to_value(
            callgraph_store_adapter::trace_to_result(store, &target_file, query.target_symbol, 4)
                .unwrap()
        )
        .unwrap(),
        "trace_to_symbol": serde_json::to_value(
            callgraph_store_adapter::trace_to_symbol_result(
                store,
                &tree_file,
                query.tree_symbol,
                query.to_symbol,
                to_file.as_deref(),
                4,
            )
            .unwrap()
        )
        .unwrap(),
    })
}

fn assert_op_parity<L, S>(root: &Path, _store: &CallGraphStore, label: &str, legacy: L, store: S)
where
    L: FnOnce(&mut CallGraph) -> serde_json::Value,
    S: FnOnce() -> serde_json::Value,
{
    let mut graph = CallGraph::new(root.to_path_buf());
    let legacy_json = legacy(&mut graph);
    let store_json = store();
    let legacy_bytes = serde_json::to_string(&legacy_json).unwrap();
    let store_bytes = serde_json::to_string(&store_json).unwrap();
    assert_eq!(
        store_bytes, legacy_bytes,
        "{label} store output must be byte-identical to legacy output\nlegacy: {legacy_json:#}\nstore: {store_json:#}"
    );
}

fn run_scenario(
    name: &str,
    setup: fn(&Path),
    edit: fn(&Path) -> Vec<PathBuf>,
    extra_assert: Option<fn(&aft::callgraph_store::IncrementalStats)>,
) {
    let dir = tempdir().unwrap();
    setup(dir.path());

    let files_before = project_files(dir.path());
    let store = CallGraphStore::open(
        dir.path().join(".store-incremental"),
        dir.path().to_path_buf(),
    )
    .unwrap();
    store.cold_build(&files_before).unwrap();

    let changed = edit(dir.path());
    let stats = store.refresh_files(&changed).unwrap();
    if let Some(assertion) = extra_assert {
        assertion(&stats);
    }
    let incremental = store.edge_snapshot().unwrap();

    let cold = cold_edges(dir.path());
    assert_eq!(
        incremental, cold,
        "scenario {name} incremental graph must match cold rebuild"
    );
}

fn cold_edges(root: &Path) -> BTreeSet<StoredEdge> {
    let store = CallGraphStore::open(root.join(".store-cold"), root.to_path_buf()).unwrap();
    let files = project_files(root);
    store.cold_build(&files).unwrap();
    store.edge_snapshot().unwrap()
}

fn project_files(root: &Path) -> Vec<PathBuf> {
    walk_project_files(root).collect()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    bump_mtime(path);
}

fn bump_mtime(path: &Path) {
    let secs = NEXT_MTIME.fetch_add(1, Ordering::SeqCst);
    filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).unwrap();
}

fn remove_file(path: &Path) {
    fs::remove_file(path).unwrap();
}

fn write_parity_project(root: &Path) {
    write_file(
        &root.join("package.json"),
        r#"{"name":"callgraph-store-fixture","type":"module"}"#,
    );
    write_file(
        &root.join("Cargo.toml"),
        r#"[package]
name = "callgraph_store_fixture"
version = "0.1.0"
edition = "2021"
"#,
    );
    write_file(
        &root.join("src/main.ts"),
        r#"import { foo as renamed } from "./foo";
import runDefault from "./def";
import * as ns from "./ns";

export function main() {
  renamed();
  runDefault();
  ns.member();
  localOnly();
}

function localOnly() {}
"#,
    );
    write_file(
        &root.join("src/foo.ts"),
        r#"export function foo() {}
"#,
    );
    write_file(
        &root.join("src/def.ts"),
        r#"export default function runDefault() {}
"#,
    );
    write_file(
        &root.join("src/ns.ts"),
        r#"export function member() {}
"#,
    );
    write_file(
        &root.join("src/app.js"),
        r#"import { jsHelper } from "./js_helper.js";

export function jsEntry() {
  jsHelper();
}
"#,
    );
    write_file(
        &root.join("src/js_helper.js"),
        r#"export function jsHelper() {}
"#,
    );
    write_file(
        &root.join("src/lib.rs"),
        r#"mod util;
use crate::util::rust_helper;

pub fn rust_entry() {
    rust_helper();
}
"#,
    );
    write_file(
        &root.join("src/util.rs"),
        r#"pub fn rust_helper() {}
"#,
    );
}

fn setup_rename_symbol(root: &Path) {
    write_file(
        &root.join("a.ts"),
        r#"export function outer() {
  inner();
}

export function inner() {}
"#,
    );
}

fn edit_rename_symbol(root: &Path) -> Vec<PathBuf> {
    let path = root.join("a.ts");
    write_file(
        &path,
        r#"export function outer() {
  renamed();
}

export function renamed() {}
"#,
    );
    vec![path]
}

fn setup_delete_file(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
}

fn edit_delete_file(root: &Path) -> Vec<PathBuf> {
    let path = root.join("foo.ts");
    remove_file(&path);
    vec![path]
}

fn setup_barrel(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("barrel.ts"),
        r#"export { foo } from "./foo";
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
    write_file(&root.join("alt.ts"), "export function foo() {}\n");
}

fn edit_delete_barrel(root: &Path) -> Vec<PathBuf> {
    let path = root.join("barrel.ts");
    remove_file(&path);
    vec![path]
}

fn setup_unresolved_import(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { late } from "./late";
export function main() { late(); }
"#,
    );
}

fn edit_add_late_file(root: &Path) -> Vec<PathBuf> {
    let path = root.join("late.ts");
    write_file(&path, "export function late() {}\n");
    vec![path]
}

fn setup_barrel_move(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("barrel.ts"),
        r#"export { foo } from "./foo";
"#,
    );
    write_file(&root.join("foo.ts"), "export function foo() {}\n");
    write_file(&root.join("alt.ts"), "export function foo() {}\n");
}

fn edit_move_reexport(root: &Path) -> Vec<PathBuf> {
    let barrel = root.join("barrel.ts");
    let source_file = root.join("foo.ts");
    write_file(
        &barrel,
        r#"export { foo } from "./alt";
"#,
    );
    write_file(&source_file, "export function oldFoo() {}\n");
    vec![barrel, source_file]
}

fn edit_retarget_barrel(root: &Path) -> Vec<PathBuf> {
    let path = root.join("barrel.ts");
    write_file(
        &path,
        r#"export { foo } from "./alt";
"#,
    );
    vec![path]
}

fn setup_defines_and_calls(root: &Path) {
    write_file(
        &root.join("combo.ts"),
        r#"export function caller() {
  callee();
}

export function callee() {}
"#,
    );
}

fn edit_defines_and_calls(root: &Path) -> Vec<PathBuf> {
    let path = root.join("combo.ts");
    write_file(
        &path,
        r#"export function caller() {
  next();
}

export function next() {}
"#,
    );
    vec![path]
}

fn setup_body_only(root: &Path) {
    write_file(
        &root.join("main.ts"),
        r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
    );
    write_file(
        &root.join("foo.ts"),
        r#"export function foo() {
  return 1;
}
"#,
    );
}

fn edit_body_only(root: &Path) -> Vec<PathBuf> {
    let path = root.join("foo.ts");
    write_file(
        &path,
        r#"export function foo() {
  return 2;
}
"#,
    );
    vec![path]
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    unsafe {
        let mut usage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            Some(usage.ru_maxrss as u64)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Some(usage.ru_maxrss as u64 * 1024)
        }
    }
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}
