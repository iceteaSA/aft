#![cfg(unix)]

//! Cross-language parity gate for subc agent args -> native command translation.
//!
//! Feeds the golden fixtures captured from the current TypeScript OpenCode tool
//! wrappers (`scripts/capture-subc-parity.ts`) through `aft::subc_translate` and
//! asserts the native command payload matches byte-for-byte after deterministic
//! JSON key sorting.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde::Deserialize;
use serde_json::{json, Map, Value};

static PROJECT_FIXTURE: Once = Once::new();
const PROJECT_ROOT_TOKEN: &str = "<PROJECT_ROOT>";

#[derive(Debug, Deserialize)]
struct TranslateInput {
    tool_name: String,
    agent_args: Value,
    project_root: String,
    diagnostics_on_edit: Option<bool>,
}

fn fixtures_root() -> PathBuf {
    crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("subc_parity")
        .join("translate")
}

fn setup_project_fixture(root: &Path) {
    PROJECT_FIXTURE.call_once(|| {
        fs::create_dir_all(root.join("src")).expect("create src fixture dir");
        fs::create_dir_all(root.join("docs")).expect("create docs fixture dir");
        fs::create_dir_all(root.join("packages/app")).expect("create package fixture dir");
        fs::write(root.join("README.md"), "# parity\n").expect("write README fixture");
        fs::write(root.join("src/main.ts"), "const value = 1;\n").expect("write main fixture");
        fs::write(root.join("docs/guide.md"), "# guide\n").expect("write docs fixture");
        fs::write(
            root.join("packages/app/index.tsx"),
            "export const App = () => null;\n",
        )
        .expect("write app fixture");
    });
}

fn fixture_project_root() -> PathBuf {
    std::env::temp_dir().join("aft-subc-parity").join("project")
}

fn project_root_for_input(raw: &str) -> PathBuf {
    if raw == PROJECT_ROOT_TOKEN {
        fixture_project_root()
    } else {
        PathBuf::from(raw)
    }
}

fn expand_project_root_tokens(value: Value, project_root: &Path) -> Value {
    let root = project_root.to_string_lossy();
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| expand_project_root_tokens(item, project_root))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                out.insert(key, expand_project_root_tokens(value, project_root));
            }
            Value::Object(out)
        }
        Value::String(s) => Value::String(s.replace(PROJECT_ROOT_TOKEN, root.as_ref())),
        other => other,
    }
}

fn replace_project_root(value: Value, project_root: &Path) -> Value {
    let root = project_root.to_string_lossy();
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| replace_project_root(item, project_root))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                out.insert(key, replace_project_root(value, project_root));
            }
            Value::Object(out)
        }
        Value::String(s) => Value::String(s.replace(root.as_ref(), PROJECT_ROOT_TOKEN)),
        other => other,
    }
}

fn sort_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_value).collect()),
        Value::Object(map) => {
            let mut sorted = Map::new();
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (key, value) in entries {
                sorted.insert(key, sort_value(value));
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

fn pretty_sorted(value: Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&sort_value(value)).expect("serialize sorted JSON")
    )
}

fn assert_case(dir: &Path) -> Option<String> {
    let case = dir.file_name().unwrap().to_string_lossy().to_string();
    let input: TranslateInput =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).expect("read input.json"))
            .expect("parse input.json");
    let project_root = project_root_for_input(&input.project_root);
    setup_project_fixture(&project_root);

    let ctx = TranslateContext {
        diagnostics_on_edit: input.diagnostics_on_edit.unwrap_or(false),
        preview: false,
    };
    let agent_args = expand_project_root_tokens(input.agent_args, &project_root);
    let actual =
        match subc_translate_with_context(&input.tool_name, &agent_args, &project_root, ctx) {
            Ok(t) => json!({ "command": t.command, "args": t.args }),
            Err(err) => json!({ "error": { "code": err.code, "message": err.message } }),
        };
    let actual = pretty_sorted(replace_project_root(actual, &project_root));
    let expected = fs::read_to_string(dir.join("expected.json")).expect("read expected.json");
    if actual == expected {
        None
    } else {
        Some(format!(
            "case `{case}`:\n  actual:\n{actual}\n  expected:\n{expected}"
        ))
    }
}

#[test]
fn subc_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 19,
        "expected >=19 translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn zoom_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("zoom_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 4,
        "expected >=4 zoom translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} zoom translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn refactor_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("refactor_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 7,
        "expected >=7 refactor translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} refactor translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn safety_translate_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("safety_"))
        })
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 8,
        "expected >=8 safety translate parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} safety translate parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn main_dispatch_has_no_agent_edit_or_search_aliases() {
    let src = include_str!("../../src/main.rs");
    for pat in ["\"edit\" =>", "\"search\" =>"] {
        assert!(
            !src.contains(pat),
            "main::dispatch must not alias agent tool {pat}"
        );
    }
    assert!(src.contains("\"semantic_search\" =>"));
    assert!(src.contains("\"edit_match\" =>"));
    assert!(src.contains("\"outline\" =>"));
}

#[test]
fn file_urls_decode_to_local_paths_in_translate() {
    use aft::subc_translate::resolve_path_from_project_root;
    let root = std::path::Path::new(if cfg!(windows) { "C:\\proj" } else { "/proj" });

    // RFC 8089 spellings of the same local file all resolve identically.
    let plain = resolve_path_from_project_root(root, "/etc/hosts");
    assert_eq!(
        resolve_path_from_project_root(root, "file:///etc/hosts"),
        plain
    );
    assert_eq!(
        resolve_path_from_project_root(root, "file:/etc/hosts"),
        plain
    );
    assert_eq!(
        resolve_path_from_project_root(root, "file://localhost/etc/hosts"),
        plain
    );

    // Percent-encoded characters decode (space in a file name).
    let spaced = resolve_path_from_project_root(root, "file:///tmp/a%20b.md");
    assert_eq!(spaced, resolve_path_from_project_root(root, "/tmp/a b.md"));

    // Malformed-escape traversal: each VALID %HH decodes independently even
    // when a later escape (%ZZ) is malformed. %2e%2e -> .. must escape the
    // project just as a literal ../ would. This is the exact input the TS
    // decoder must agree with (decodeURIComponent would keep the whole string
    // encoded on the malformed %ZZ, diverging from this).
    let malformed = resolve_path_from_project_root(root, "file:///proj/%2e%2e/%ZZ/../secret");
    assert!(
        !malformed.starts_with(std::path::Path::new(if cfg!(windows) {
            "C:\\proj"
        } else {
            "/proj"
        })),
        "malformed-escape traversal must resolve OUT of /proj: {malformed:?}"
    );

    // A non-local authority is not a local path: left as-is on unix (becomes
    // a relative path under the root, matching the old rejected behavior)
    // rather than silently pointing at some other file.
    if cfg!(unix) {
        let unc = resolve_path_from_project_root(root, "file://server/share/x.txt");
        assert!(
            unc.starts_with(root),
            "non-local authority must not decode on unix: {unc:?}"
        );
    }

    // Windows drive-letter form.
    if cfg!(windows) {
        let drive = resolve_path_from_project_root(root, "file:///C:/temp/x.txt");
        assert_eq!(drive, std::path::PathBuf::from("C:\\temp\\x.txt"));
    }
}
