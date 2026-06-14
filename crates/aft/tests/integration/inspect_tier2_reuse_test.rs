use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aft::callgraph_store::CallGraphStore;
use aft::config::Config;
use aft::inspect::{InspectCategory, InspectManager, InspectScanSuccess, InspectSnapshot};
use aft::parser::SymbolCache;
use serde_json::Value;

fn write_file(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture file has parent")).expect("create parent");
    fs::write(&path, contents).expect("write fixture");
    path
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

fn duplicate_source() -> String {
    r#"
export function calculate(input: number) {
  const first = input + 1;
  const second = first + 2;
  const third = second + first;
  const fourth = third + 3;
  const fifth = fourth + third;
  const sixth = fifth + second;
  return sixth + fourth;
}
"#
    .to_string()
}

fn changed_source() -> String {
    r#"
export function calculate(input: number) {
  const first = input + 10;
  const second = first + 20;
  const third = second + first;
  const fourth = third + 30;
  const fifth = fourth + third;
  const sixth = fifth + second;
  const seventh = sixth + fifth;
  return seventh + fourth;
}
"#
    .to_string()
}

fn build_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project");
    let source = duplicate_source();
    let mut mutated_file = PathBuf::new();
    for index in 0..32 {
        let relative = format!("src/file_{index:02}.ts");
        let path = write_file(&root, &relative, &source);
        if index == 7 {
            mutated_file = path;
        }
    }
    (temp_dir, root, mutated_file)
}

fn run_reuse(
    manager: &InspectManager,
    snapshot: InspectSnapshot,
) -> (InspectScanSuccess, Duration) {
    run_reuse_category(manager, snapshot, InspectCategory::Duplicates)
}

fn run_reuse_category(
    manager: &InspectManager,
    snapshot: InspectSnapshot,
    category: InspectCategory,
) -> (InspectScanSuccess, Duration) {
    let started = Instant::now();
    let result = manager.tier2_run_with_reuse_result(snapshot, category, None);
    let elapsed = started.elapsed();
    (result.outcome.expect("tier2 reuse run succeeds"), elapsed)
}

fn relative_paths(project_root: &Path, files: &[PathBuf]) -> Vec<String> {
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

fn project_source_files(project_root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("."))
            {
                continue;
            }
            if path.is_dir() {
                visit(&path, files);
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(
                        ext,
                        "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" | "rs"
                    )
                })
            {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(project_root, &mut files);
    files.sort();
    files
}

fn rebuild_dead_code_callgraph_store(project_root: &Path, inspect_dir: &Path) {
    let store_dir = inspect_dir
        .parent()
        .expect("inspect dir has parent")
        .join("callgraph");
    let store = CallGraphStore::open(store_dir, project_root.to_path_buf()).expect("open store");
    let files = project_source_files(project_root);
    store
        .cold_build(&files)
        .expect("cold build callgraph store");
}

fn contribution_payloads(
    project_root: &Path,
    success: &InspectScanSuccess,
) -> Vec<(String, Value)> {
    let mut payloads = success
        .contributions
        .iter()
        .map(|contribution| {
            let relative = contribution
                .file_path
                .strip_prefix(project_root)
                .unwrap_or(&contribution.file_path)
                .to_string_lossy()
                .replace('\\', "/");
            (relative, contribution.contribution.clone())
        })
        .collect::<Vec<_>>();
    payloads.sort_by(|left, right| left.0.cmp(&right.0));
    payloads
}

fn aggregate_item_symbols(success: &InspectScanSuccess) -> Vec<(String, String)> {
    let mut symbols = success.aggregate["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item.get("file")?.as_str()?.to_string(),
                item.get("symbol")?.as_str()?.to_string(),
            ))
        })
        .collect::<Vec<_>>();
    symbols.sort();
    symbols
}

#[test]
fn inspect_tier2_reuse_skips_fresh_files_and_rescans_stale_file() {
    let (_temp_dir, root, mutated_file) = build_fixture();
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let (first, _t1) = run_reuse(&first_manager, snapshot(&root, &inspect_dir));
    assert_eq!(first.scanned_files.len(), 32);
    assert!(first.aggregate["groups_count"].as_u64().unwrap_or(0) > 0);

    let second_manager = InspectManager::new();
    let (second, _t2) = run_reuse(&second_manager, snapshot(&root, &inspect_dir));
    // Cache reuse is proven behaviorally: a fully-fresh second run rescans
    // zero files and returns the identical aggregate. (A wall-clock "faster"
    // assertion was removed — it flaked under parallel test load while adding
    // no signal beyond the scanned_files/aggregate checks below.)
    assert!(second.scanned_files.is_empty());
    assert_eq!(second.aggregate, first.aggregate);

    fs::write(&mutated_file, changed_source()).expect("mutate one fixture file");

    let third_manager = InspectManager::new();
    let (third, _t3) = run_reuse(&third_manager, snapshot(&root, &inspect_dir));
    assert_eq!(
        relative_paths(&root, &third.scanned_files),
        vec!["src/file_07.ts"]
    );
    assert_ne!(third.aggregate, first.aggregate);
}

#[test]
fn inspect_tier2_reuse_rescans_same_size_content_change_with_restored_mtime() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).expect("create project");
    let source = write_file(&root, "src/export.ts", "export function one() {}\n");
    let fixed_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&source, fixed_mtime).expect("set fixed mtime");
    let inspect_dir = root.join(".aft-cache").join("inspect");

    let first_manager = InspectManager::new();
    let (first, _t1) = run_reuse_category(
        &first_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert_eq!(first.scanned_files.len(), 1);
    assert_eq!(first.aggregate["items"][0]["symbol"], "one");

    fs::write(&source, "export function two() {}\n").expect("same-size mutate");
    filetime::set_file_mtime(&source, fixed_mtime).expect("restore mtime");

    let second_manager = InspectManager::new();
    let (second, _t2) = run_reuse_category(
        &second_manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(
        relative_paths(&root, &second.scanned_files),
        vec!["src/export.ts"]
    );
    assert_eq!(second.aggregate["items"][0]["symbol"], "two");
    assert_ne!(second.aggregate, first.aggregate);
}

fn unused_contribution_payloads(
    project_root: &Path,
    success: &InspectScanSuccess,
) -> Vec<(String, Value)> {
    let mut payloads = success
        .contributions
        .iter()
        .map(|contribution| {
            let relative = contribution
                .file_path
                .strip_prefix(project_root)
                .unwrap_or(&contribution.file_path)
                .to_string_lossy()
                .replace('\\', "/");
            (relative, contribution.contribution.clone())
        })
        .collect::<Vec<_>>();
    payloads.sort_by(|left, right| left.0.cmp(&right.0));
    payloads
}

fn assert_unused_exports_incremental_matches_cold<S, E>(name: &str, setup: S, edit: E)
where
    S: FnOnce(&Path),
    E: FnOnce(&Path),
{
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join(format!("project-{name}"));
    fs::create_dir_all(&root).expect("create project");
    setup(&root);

    let warm_inspect_dir = temp_dir.path().join(format!("inspect-warm-{name}"));
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    assert!(
        !first.contributions.is_empty(),
        "{name}: initial cold scan should populate contributions"
    );

    edit(&root);

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::UnusedExports,
    );
    let cold_inspect_dir = temp_dir.path().join(format!("inspect-cold-{name}"));
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::UnusedExports,
    );

    assert_eq!(warm.aggregate, cold.aggregate, "{name}: aggregate mismatch");
    assert_eq!(
        unused_contribution_payloads(&root, &warm),
        unused_contribution_payloads(&root, &cold),
        "{name}: per-file contribution payload mismatch"
    );
}

fn assert_dead_code_incremental_matches_cold<S, E>(
    name: &str,
    setup: S,
    edit: E,
    expected_scanned_after_edit: &[&str],
) where
    S: FnOnce(&Path),
    E: FnOnce(&Path),
{
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join(format!("dead-code-project-{name}"));
    fs::create_dir_all(&root).expect("create project");
    setup(&root);

    let warm_inspect_dir = temp_dir.path().join(format!("warm-{name}/inspect"));
    rebuild_dead_code_callgraph_store(&root, &warm_inspect_dir);
    let warm_manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::DeadCode,
    );
    assert_eq!(
        first.aggregate["callgraph_available"], true,
        "{name}: callgraph store should be available"
    );
    assert!(
        !first.contributions.is_empty(),
        "{name}: initial cold scan should populate contributions"
    );

    edit(&root);
    rebuild_dead_code_callgraph_store(&root, &warm_inspect_dir);

    let (warm, _warm_elapsed) = run_reuse_category(
        &warm_manager,
        snapshot(&root, &warm_inspect_dir),
        InspectCategory::DeadCode,
    );
    let cold_inspect_dir = temp_dir.path().join(format!("cold-{name}/inspect"));
    rebuild_dead_code_callgraph_store(&root, &cold_inspect_dir);
    let cold_manager = InspectManager::new();
    let (cold, _cold_elapsed) = run_reuse_category(
        &cold_manager,
        snapshot(&root, &cold_inspect_dir),
        InspectCategory::DeadCode,
    );

    assert_eq!(
        relative_paths(&root, &warm.scanned_files),
        expected_scanned_after_edit,
        "{name}: edited source reparses should stay minimal"
    );
    assert_eq!(warm.aggregate, cold.aggregate, "{name}: aggregate mismatch");
    assert_eq!(
        contribution_payloads(&root, &warm),
        contribution_payloads(&root, &cold),
        "{name}: raw per-file contribution payload mismatch"
    );
}

#[test]
fn inspect_dead_code_incremental_facts_invariants_match_cold() {
    assert_dead_code_incremental_matches_cold(
        "ts_last_importer_removed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "src/exported.ts",
                "export const used = 1;\nexport const dead = 2;\n",
            );
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nexport function main() { console.log(used); }\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "export function main() { console.log('removed'); }\n",
            );
        },
        &["src/main.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_importer_deleted",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(root, "src/exported.ts", "export const used = 1;\n");
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nconsole.log(used);\n",
            );
        },
        |root| {
            fs::remove_file(root.join("src/main.ts")).expect("delete importer");
        },
        &[],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_file_renamed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(root, "src/exported.ts", "export const used = 1;\n");
            write_file(
                root,
                "src/main.ts",
                "import { used } from './exported';\nconsole.log(used);\n",
            );
        },
        |root| {
            fs::create_dir_all(root.join("src/moved")).expect("create moved dir");
            fs::rename(
                root.join("src/exported.ts"),
                root.join("src/moved/exported.ts"),
            )
            .expect("rename exported file");
        },
        &["src/moved/exported.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "tsconfig_alias_change",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/main.ts"}"#);
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(root, "src/a.ts", "export const x = 'a';\n");
            write_file(root, "src/b.ts", "export const x = 'b';\n");
            write_file(
                root,
                "src/main.ts",
                "import { x } from '@lib';\nexport function main() { console.log(x); }\n",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
        &["tsconfig.json"],
    );

    assert_dead_code_incremental_matches_cold(
        "ts_barrel_target_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/barrel.ts"}"#);
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;\nexport default function def() { return named; }\n",
            );
            write_file(root, "src/extra.ts", "export const star = 1;\n");
            write_file(
                root,
                "src/barrel.ts",
                "export { named } from './target';\nexport { default } from './target';\nexport * from './extra';\nexport * as ns from './target';\n",
            );
        },
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;\nexport const added = 2;\nexport default function def() { return named + added; }\n",
            );
        },
        &["src/target.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_last_importer_removed",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(
                root,
                "src/main.rs",
                "mod helper;\nfn main() { helper::used(); }\n",
            );
            write_file(
                root,
                "src/helper.rs",
                "pub fn used() {}\npub fn dead() {}\n",
            );
        },
        |root| {
            write_file(root, "src/main.rs", "mod helper;\nfn main() {}\n");
        },
        &["src/main.rs"],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_importer_deleted",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(
                root,
                "src/main.rs",
                "mod helper;\nfn main() { helper::used(); }\n",
            );
            write_file(root, "src/helper.rs", "pub fn used() {}\n");
        },
        |root| {
            fs::remove_file(root.join("src/main.rs")).expect("delete rust importer");
        },
        &[],
    );

    assert_dead_code_incremental_matches_cold(
        "rust_pub_use_target_changed",
        |root| {
            write_file(
                root,
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            );
            write_file(root, "src/lib.rs", "pub use foo::Foo;\nmod foo;\n");
            write_file(root, "src/foo.rs", "pub struct Foo;\npub struct Dead;\n");
        },
        |root| {
            write_file(
                root,
                "src/foo.rs",
                "pub struct Foo;\npub struct Dead;\npub struct Added;\n",
            );
        },
        &["src/foo.rs"],
    );

    assert_dead_code_incremental_matches_cold(
        "cross_file_type_ref_removed",
        |root| {
            write_file(
                root,
                "src/types.ts",
                "export interface Widget { id: string }\n",
            );
            write_file(
                root,
                "src/use.ts",
                "import type { Widget } from './types';\ntype Box = Widget;\n",
            );
        },
        |root| {
            write_file(root, "src/use.ts", "type Box = string;\n");
        },
        &["src/use.ts"],
    );

    assert_dead_code_incremental_matches_cold(
        "manifest_entry_point_changed",
        |root| {
            write_file(root, "package.json", r#"{"main":"src/a.ts"}"#);
            write_file(root, "src/a.ts", "export function a() {}\n");
            write_file(root, "src/b.ts", "export function b() {}\n");
        },
        |root| {
            write_file(root, "package.json", r#"{"main":"src/b.ts"}"#);
        },
        &["package.json"],
    );
}

#[test]
fn inspect_dead_code_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("dead-code-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(root.as_path(), "package.json", r#"{"main":"src/main.ts"}"#);
    write_file(
        root.as_path(),
        "src/main.ts",
        "import { live } from './live';\nexport function main() { live(); }\n",
    );
    write_file(
        root.as_path(),
        "src/live.ts",
        "export function live() {}\nexport function unused() {}\n",
    );

    let inspect_a = temp_dir.path().join("dead-cold-a/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_a);
    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &inspect_a),
        InspectCategory::DeadCode,
    );

    let inspect_b = temp_dir.path().join("dead-cold-b/inspect");
    rebuild_dead_code_callgraph_store(&root, &inspect_b);
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &inspect_b),
        InspectCategory::DeadCode,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        contribution_payloads(&root, &cold_a),
        contribution_payloads(&root, &cold_b)
    );
    assert_eq!(
        aggregate_item_symbols(&cold_a),
        aggregate_item_symbols(&cold_b)
    );
}

#[test]
fn inspect_unused_exports_incremental_oxc_invariants_match_cold() {
    assert_unused_exports_incremental_matches_cold(
        "last_importer_removed",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
export const y = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/use.ts",
                "console.log('import removed');
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "importer_deleted",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            fs::remove_file(root.join("src/use.ts")).expect("delete importer");
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "file_renamed",
        |root| {
            write_file(
                root,
                "src/exported.ts",
                "export const x = 1;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './exported';
console.log(x);
",
            );
        },
        |root| {
            fs::create_dir_all(root.join("src/moved")).expect("create moved dir");
            fs::rename(
                root.join("src/exported.ts"),
                root.join("src/moved/exported.ts"),
            )
            .expect("rename exported file");
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "tsconfig_alias_change",
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(
                root,
                "src/a.ts",
                "export const x = 'a';
",
            );
            write_file(
                root,
                "src/b.ts",
                "export const x = 'b';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from '@lib';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "tsconfig_base_alias_change",
        |root| {
            write_file(
                root,
                "tsconfig.json",
                r#"{"extends":"./tsconfig.base.json"}"#,
            );
            write_file(
                root,
                "tsconfig.base.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/a.ts"]}}}"#,
            );
            write_file(
                root,
                "src/a.ts",
                "export const x = 'a';
",
            );
            write_file(
                root,
                "src/b.ts",
                "export const x = 'b';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from '@lib';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "tsconfig.base.json",
                r#"{"compilerOptions":{"baseUrl":".","paths":{"@lib":["src/b.ts"]}}}"#,
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "barrel_target_changed",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;
export default function def() { return named; }
",
            );
            write_file(
                root,
                "src/extra.ts",
                "export const star = 1;
",
            );
            write_file(
                root,
                "src/barrel.ts",
                "export { named } from './target';
export { default } from './target';
export * from './extra';
export * as ns from './target';
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { named, default as def, star, ns } from './barrel';
console.log(named, def, star, ns);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const named = 1;
export const added = 2;
export default function def() { return named + added; }
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "namespace_import_uncertain",
        |root| {
            write_file(
                root,
                "src/target.ts",
                "export const a = 1;
export const b = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { a } from './target';
console.log(a);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/use.ts",
                "import * as target from './target';
console.log(target);
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "dynamic_import_added",
        |root| {
            write_file(
                root,
                "src/lazy.ts",
                "export const lazy = 1;
",
            );
            write_file(
                root,
                "src/main.ts",
                "console.log('main');
",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "import('./lazy').then((module) => console.log(module));
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "dynamic_import_removed",
        |root| {
            write_file(
                root,
                "src/lazy.ts",
                "export const lazy = 1;
",
            );
            write_file(
                root,
                "src/main.ts",
                "import('./lazy').then((module) => console.log(module));
",
            );
        },
        |root| {
            write_file(
                root,
                "src/main.ts",
                "console.log('main');
",
            );
        },
    );

    assert_unused_exports_incremental_matches_cold(
        "new_sibling_resolution_candidate",
        |root| {
            write_file(
                root,
                "src/foo/index.ts",
                "export const x = 1;
export const oldOnly = 2;
",
            );
            write_file(
                root,
                "src/use.ts",
                "import { x } from './foo';
console.log(x);
",
            );
        },
        |root| {
            write_file(
                root,
                "src/foo.ts",
                "export const x = 1;
export const newOnly = 3;
",
            );
        },
    );
}

#[cfg(unix)]
#[test]
fn inspect_unused_exports_oxc_read_error_surfaces_after_cached_rollup() {
    use std::os::unix::fs::PermissionsExt;

    fn assert_read_error(success: &InspectScanSuccess, relative_file: &str) {
        assert_eq!(
            success.aggregate["complete"].as_bool(),
            Some(false),
            "read error should make aggregate incomplete: {:#}",
            success.aggregate
        );
        let parse_errors = success.aggregate["parse_errors"]
            .as_array()
            .expect("parse_errors array");
        assert!(
            parse_errors.iter().any(|error| {
                error.get("file").and_then(Value::as_str) == Some(relative_file)
                    && error
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("read:"))
            }),
            "expected read error for {relative_file}: {:#}",
            success.aggregate
        );
    }

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-unreadable-unused-exports");
    fs::create_dir_all(&root).expect("create project");
    write_file(&root, "package.json", r#"{}"#);
    write_file(
        &root,
        "src/good.ts",
        "export const good = 1;
",
    );
    let unreadable = write_file(
        &root,
        "src/unreadable.ts",
        "export const hidden = 1;
",
    );
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
        .expect("make fixture unreadable");

    let inspect_dir = temp_dir.path().join("inspect-unreadable");
    let manager = InspectManager::new();
    let (first, _first_elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );
    if first.aggregate["complete"].as_bool() != Some(false) {
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))
            .expect("restore readable fixture");
        eprintln!("skipping unreadable-file assertion because this process can read chmod 000");
        return;
    }
    assert_read_error(&first, "src/unreadable.ts");

    write_file(&root, "package.json", r#"{"main":"src/good.ts"}"#);
    let (second, _second_elapsed) = run_reuse_category(
        &manager,
        snapshot(&root, &inspect_dir),
        InspectCategory::UnusedExports,
    );

    let second_scanned = relative_paths(&root, &second.scanned_files);
    assert!(
        !second_scanned.iter().any(|path| path == "src/good.ts"),
        "error-only contributions should not force a full JS/TS facts refresh: {second_scanned:?}"
    );
    assert_read_error(&second, "src/unreadable.ts");
    assert!(
        second.contributions.iter().any(|contribution| {
            contribution.contribution["file"].as_str() == Some("src/unreadable.ts")
                && contribution.contribution["parse_errors"]
                    .as_array()
                    .is_some_and(|errors| !errors.is_empty())
        }),
        "cached roll-up should load an error contribution for unreadable.ts"
    );

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))
        .expect("restore readable fixture");
}

#[test]
fn inspect_unused_exports_twice_cold_is_deterministic() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().join("project-twice-cold");
    fs::create_dir_all(&root).expect("create project");
    write_file(
        root.as_path(),
        "src/a.ts",
        "export const a = 1;
export const unused = 2;
",
    );
    write_file(
        root.as_path(),
        "src/b.ts",
        "import { a } from './a';
console.log(a);
",
    );

    let manager_a = InspectManager::new();
    let (cold_a, _elapsed_a) = run_reuse_category(
        &manager_a,
        snapshot(&root, &temp_dir.path().join("inspect-cold-a")),
        InspectCategory::UnusedExports,
    );
    let manager_b = InspectManager::new();
    let (cold_b, _elapsed_b) = run_reuse_category(
        &manager_b,
        snapshot(&root, &temp_dir.path().join("inspect-cold-b")),
        InspectCategory::UnusedExports,
    );

    assert_eq!(cold_a.aggregate, cold_b.aggregate);
    assert_eq!(
        unused_contribution_payloads(&root, &cold_a),
        unused_contribution_payloads(&root, &cold_b)
    );
}
