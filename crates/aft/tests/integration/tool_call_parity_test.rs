use std::fs;
use std::path::Path;

use aft::protocol::Response;
use aft::subc_format::{format_response_with_context, FormatContext};
use aft::subc_translate::{subc_translate_with_context, TranslateContext};
use serde_json::{json, Map, Value};

use super::helpers::AftProcess;

const SESSION_ID: &str = "tool-call-parity-session";

struct ParityCase {
    label: &'static str,
    tool: &'static str,
    arguments: Value,
}

#[test]
fn tool_call_matches_direct_spine_envelopes() {
    let direct_project = tempfile::tempdir().expect("direct temp project");
    let tool_call_project = tempfile::tempdir().expect("tool_call temp project");
    create_fixture_project(direct_project.path());
    create_fixture_project(tool_call_project.path());

    let mut direct_aft = AftProcess::spawn();
    let mut tool_call_aft = AftProcess::spawn();
    configure_project(&mut direct_aft, direct_project.path(), "cfg-direct");
    configure_project(
        &mut tool_call_aft,
        tool_call_project.path(),
        "cfg-tool-call",
    );

    for case in parity_cases() {
        let request_id = format!("tool-call-parity-{}", case.label);
        let direct_request = direct_request(
            &request_id,
            case.tool,
            &case.arguments,
            direct_project.path(),
        );
        let tool_call_request = json!({
            "id": request_id,
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": case.tool,
            "arguments": case.arguments,
        });

        let direct_response = send_json(&mut direct_aft, direct_request);
        let tool_call_response = send_json(&mut tool_call_aft, tool_call_request);

        assert_eq!(
            direct_response["success"], tool_call_response["success"],
            "success mismatch for {}: direct={direct_response:#} tool_call={tool_call_response:#}",
            case.label
        );
        assert_eq!(
            direct_response["success"].as_bool().map(|success| !success),
            tool_call_response["success"]
                .as_bool()
                .map(|success| !success),
            "derived is_error mismatch for {}",
            case.label
        );
        assert_eq!(
            direct_response.get("code"),
            tool_call_response.get("code"),
            "error code mismatch for {}",
            case.label
        );

        let expected_text = formatted_text_from_direct_response(
            case.tool,
            &case.arguments,
            direct_project.path(),
            &direct_response,
        );
        let actual_text = tool_call_response["text"]
            .as_str()
            .unwrap_or_else(|| panic!("tool_call response missing text for {}", case.label));
        assert_eq!(
            normalize_text(&expected_text, direct_project.path()),
            normalize_text(actual_text, tool_call_project.path()),
            "formatted text mismatch for {}",
            case.label
        );

        let mut expected_envelope = direct_response;
        expected_envelope
            .as_object_mut()
            .expect("direct response is object")
            .insert("text".to_string(), Value::String(expected_text));

        let expected_envelope = normalized_envelope(expected_envelope, direct_project.path());
        let actual_envelope = normalized_envelope(tool_call_response, tool_call_project.path());
        assert_eq!(
            expected_envelope, actual_envelope,
            "full envelope mismatch for {}",
            case.label
        );
    }

    assert!(direct_aft.shutdown().success());
    assert!(tool_call_aft.shutdown().success());
}

#[test]
fn known_tool_translate_errors_surface_as_invalid_request() {
    let mut aft = AftProcess::spawn();
    for (label, name, arguments, expected_message) in [
        (
            "callgraph-missing-op",
            "callgraph",
            json!({}),
            "'op' is required",
        ),
        (
            "zoom-mutually-exclusive-targets",
            "zoom",
            json!({"filePath": "src/main.ts", "url": "https://example.com/doc", "symbols": "run"}),
            "Provide exactly ONE of 'filePath' or 'url'",
        ),
    ] {
        let response = send_json(
            &mut aft,
            json!({
                "id": format!("tool-call-{label}"),
                "command": "tool_call",
                "session_id": SESSION_ID,
                "name": name,
                "arguments": arguments,
            }),
        );
        assert_eq!(response["success"], false, "expected failure: {response:#}");
        assert_eq!(
            response["code"], "invalid_request",
            "translation errors for known tools must not fall through to raw dispatch: {response:#}"
        );
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_message),
            "message should include {expected_message:?}: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn unsupported_translate_tools_still_raw_dispatch_native_commands() {
    let project = tempfile::tempdir().expect("tool_call configure temp project");
    let mut aft = AftProcess::spawn();
    let response = send_json(
        &mut aft,
        json!({
            "id": "tool-call-native-configure",
            "command": "tool_call",
            "session_id": SESSION_ID,
            "name": "configure",
            "arguments": {
                "project_root": project.path().to_string_lossy(),
                "harness": "opencode",
                "config": crate::helpers::user_config(json!({
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false
                }))
            }
        }),
    );
    assert_eq!(
        response["success"], true,
        "configure raw dispatch failed: {response:#}"
    );
    assert!(
        response["text"].is_string(),
        "raw-dispatched native tool_call should still carry rendered text: {response:#}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn tool_call_rejects_missing_or_invalid_name() {
    let mut aft = AftProcess::spawn();
    for request in [
        json!({"id": "tool-call-missing-name", "command": "tool_call", "arguments": {}}),
        json!({"id": "tool-call-invalid-name", "command": "tool_call", "name": 7, "arguments": {}}),
    ] {
        let response = send_json(&mut aft, request);
        assert_eq!(response["success"], false, "expected failure: {response:#}");
        assert_eq!(
            response["code"], "invalid_request",
            "wrong code: {response:#}"
        );
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains("name"),
            "message should mention the invalid name field: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}

fn parity_cases() -> Vec<ParityCase> {
    vec![
        ParityCase {
            label: "read_text_file",
            tool: "read",
            arguments: json!({"filePath": "src/read.txt"}),
        },
        ParityCase {
            label: "grep_matches",
            tool: "grep",
            arguments: json!({"pattern": "needle", "path": "src"}),
        },
        ParityCase {
            label: "search_literal",
            tool: "search",
            arguments: json!({"query": "needle", "hint": "literal", "topK": 5}),
        },
        ParityCase {
            label: "inspect_todos",
            tool: "inspect",
            arguments: json!({"sections": "todos", "scope": "src", "topK": 5}),
        },
        ParityCase {
            label: "status_snapshot",
            tool: "status",
            arguments: json!({}),
        },
        ParityCase {
            label: "write_create_file",
            tool: "write",
            arguments: json!({"filePath": "src/new.txt", "content": "created by tool_call parity\n"}),
        },
        ParityCase {
            label: "edit_replace_string",
            tool: "edit",
            arguments: json!({"filePath": "src/edit.txt", "oldString": "old", "newString": "new"}),
        },
        ParityCase {
            label: "read_missing_file_error",
            tool: "read",
            arguments: json!({"filePath": "src/missing.txt"}),
        },
        ParityCase {
            label: "zoom_single_symbol",
            tool: "zoom",
            arguments: json!({"filePath": "src/zoom.ts", "symbols": "helper", "contextLines": 1}),
        },
        ParityCase {
            label: "zoom_multi_symbol_partial",
            tool: "zoom",
            arguments: json!({"filePath": "src/zoom.ts", "symbols": ["helper", "missingSymbol"]}),
        },
        ParityCase {
            label: "zoom_line_range",
            tool: "zoom",
            arguments: json!({"filePath": "docs/zoom.md", "startLine": 1, "endLine": 3}),
        },
        ParityCase {
            label: "zoom_multi_target_all_success",
            tool: "zoom",
            arguments: json!({
                "targets": [
                    {"filePath": "src/zoom.ts", "symbol": "helper"},
                    {"filePath": "src/zoom_other.ts", "symbol": "otherHelper"}
                ],
                "callgraph": true
            }),
        },
    ]
}

fn create_fixture_project(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("create src dir");
    fs::create_dir_all(root.join("docs")).expect("create docs dir");
    fs::write(
        root.join("src/read.txt"),
        "alpha\nneedle in a haystack\nomega\n",
    )
    .expect("write read fixture");
    fs::write(root.join("src/search.txt"), "another needle\n").expect("write search fixture");
    fs::write(root.join("src/edit.txt"), "replace old value\n").expect("write edit fixture");
    fs::write(
        root.join("src/todos.rs"),
        "// TODO: keep the parity fixture visible to inspect\nfn main() {}\n",
    )
    .expect("write todo fixture");
    fs::write(
        root.join("src/zoom.ts"),
        "export function helper(): string {\n  return 'ok';\n}\n\nexport function caller(): string {\n  return helper();\n}\n",
    )
    .expect("write zoom fixture");
    fs::write(
        root.join("src/zoom_other.ts"),
        "export function otherHelper(): string {\n  return 'other';\n}\n",
    )
    .expect("write zoom multi-target fixture");
    fs::write(root.join("docs/zoom.md"), "# Zoom Doc\n\nIntro line\n")
        .expect("write zoom docs fixture");
}

fn configure_project(aft: &mut AftProcess, root: &Path, id: &str) {
    let response = send_json(
        aft,
        json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": root.to_string_lossy(),
            "config": crate::helpers::user_config(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            })),
        }),
    );
    assert_eq!(response["success"], true, "configure failed: {response:#}");
}

fn direct_request(id: &str, tool: &str, arguments: &Value, project_root: &Path) -> Value {
    let translated = subc_translate_with_context(
        tool,
        arguments,
        project_root,
        TranslateContext {
            diagnostics_on_edit: false,
            preview: false,
        },
    )
    .unwrap_or_else(|error| panic!("translate {tool} failed: {}", error.message));
    let mut request = translated.args;
    request.insert("id".to_string(), Value::String(id.to_string()));
    request.insert("command".to_string(), Value::String(translated.command));
    request.insert(
        "session_id".to_string(),
        Value::String(SESSION_ID.to_string()),
    );
    Value::Object(request)
}

fn send_json(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn formatted_text_from_direct_response(
    tool: &str,
    arguments: &Value,
    project_root: &Path,
    direct_response: &Value,
) -> String {
    let response = response_from_wire(direct_response);
    let context = FormatContext::from_tool_call(tool, arguments, project_root);
    format_response_with_context(tool, &response, &context)
}

fn response_from_wire(value: &Value) -> Response {
    let object = value.as_object().expect("response is object");
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let success = object
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut data = Map::new();
    for (key, value) in object {
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

fn normalized_envelope(mut value: Value, project_root: &Path) -> Value {
    normalize_value(&mut value, project_root);
    value
}

fn normalize_value(value: &mut Value, project_root: &Path) {
    match value {
        Value::String(text) => *text = normalize_text(text, project_root),
        Value::Array(items) => {
            for item in items {
                normalize_value(item, project_root);
            }
        }
        Value::Object(map) => {
            // These fields are intentionally volatile: grep reports wall-clock timing,
            // backup ids are per-operation identifiers, and cache keys derive from the
            // temporary root path. The parity assertion keeps every stable field intact.
            for key in ["search_ms", "backup_id", "project_cache_key"] {
                if map.contains_key(key) {
                    map.insert(key.to_string(), Value::String(format!("<{key}>")));
                }
            }
            for value in map.values_mut() {
                normalize_value(value, project_root);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn normalize_text(text: &str, project_root: &Path) -> String {
    // Base root forms: the raw path plus its canonicalized form (macOS /var ->
    // /private/var, Windows verbatim prefixes, etc.).
    let mut base_roots = vec![project_root.to_string_lossy().to_string()];
    if let Ok(canonical) = fs::canonicalize(project_root) {
        base_roots.push(canonical.to_string_lossy().to_string());
    }

    // The `status` tool is the only spine case rendered via serde_json
    // `to_string_pretty`, which JSON-ESCAPES backslashes — so a Windows root
    // embedded in that blob appears as `C:\\Users\\...` (doubled), not the raw
    // `C:\Users\...` the path yields. Forward-slash `display()` variants also
    // appear in some fields. Mask every form so the two temp-project processes'
    // differing roots all collapse to the same token; otherwise this parity
    // assertion fails Windows-only on `status_snapshot`.
    let mut roots = Vec::new();
    for base in base_roots {
        let escaped = base.replace('\\', "\\\\");
        let slashed = base.replace('\\', "/");
        roots.push(escaped);
        roots.push(slashed);
        roots.push(base);
    }
    // Replace longer forms first so a shorter prefix never shadows a longer
    // match (e.g. the escaped form is longer than the raw form).
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    roots.dedup();

    let mut normalized = text.to_string();
    for root in roots {
        normalized = normalized.replace(&root, "<PROJECT_ROOT>");
    }
    normalized
}
