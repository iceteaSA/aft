#![cfg(unix)]

//! Cross-language parity gate for subc native responses -> agent-facing text.
//!
//! Feeds the golden fixtures captured from the current TypeScript OpenCode tool
//! wrappers (`scripts/capture-subc-parity.ts`) through `aft::subc_format` and
//! asserts the rendered text matches byte-for-byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use serde::Deserialize;
use serde_json::Value;

static PROJECT_FIXTURE: Once = Once::new();
const PROJECT_ROOT_TOKEN: &str = "<PROJECT_ROOT>";

#[derive(Debug, Deserialize)]
struct FormatFixture {
    tool_name: String,
    native_response_json: Value,
    ctx: FormatFixtureContext,
}

#[derive(Debug, Deserialize)]
struct FormatFixtureContext {
    agent_args: Value,
    project_root: String,
}

fn fixtures_root() -> PathBuf {
    crate::helpers::cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join("subc_parity")
        .join("format")
}

fn setup_project_fixture(root: &Path) {
    PROJECT_FIXTURE.call_once(|| {
        fs::create_dir_all(root.join("src")).expect("create src fixture dir");
        fs::write(root.join("src/main.ts"), "const value = 1;\n").expect("write main fixture");
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

fn replace_project_root(text: String, project_root: &Path) -> String {
    text.replace(
        &project_root.to_string_lossy().to_string(),
        PROJECT_ROOT_TOKEN,
    )
}

fn response_from_flattened(value: Value) -> Response {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("native_response_json must be an object"));
    let id = obj
        .get("id")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "fixture".to_string());
    let success = obj.get("success").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut data = serde_json::Map::new();
    for (key, value) in obj {
        if key != "id" && key != "success" {
            data.insert(key.clone(), value.clone());
        }
    }
    Response {
        id,
        success,
        data: Value::Object(data),
    }
}

fn assert_case(dir: &Path) -> Option<String> {
    let case = dir.file_name().unwrap().to_string_lossy().to_string();
    let input: FormatFixture =
        serde_json::from_str(&fs::read_to_string(dir.join("input.json")).expect("read input.json"))
            .expect("parse input.json");
    let project_root = project_root_for_input(&input.ctx.project_root);
    setup_project_fixture(&project_root);

    let response = response_from_flattened(input.native_response_json);
    let ctx = FormatContext::from_tool_call(&input.tool_name, &input.ctx.agent_args, &project_root);
    let actual = replace_project_root(
        format_response_with_context(&input.tool_name, &response, &ctx),
        &project_root,
    );
    let expected = fs::read_to_string(dir.join("expected.txt")).expect("read expected.txt");
    if actual == expected {
        None
    } else {
        Some(format!(
            "case `{case}`:\n  actual:\n{actual}\n  expected:\n{expected}"
        ))
    }
}

#[test]
fn subc_format_matches_typescript_golden_fixtures() {
    let root = fixtures_root();
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", root.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();

    assert!(
        cases.len() >= 20,
        "expected >=20 format parity fixtures, found {}",
        cases.len()
    );

    let failures = cases
        .iter()
        .filter_map(|dir| assert_case(dir))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} format parity mismatch(es):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
