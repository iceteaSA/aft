use aft::callgraph::walk_project_files;
use aft::callgraph_store::{live_callgraph_edge_snapshot, CallGraphStore, StoredEdge};
use filetime::FileTime;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tempfile::tempdir;

static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_800_000_000);

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
