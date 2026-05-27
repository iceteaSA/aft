use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use aft::commands::semantic_search::handle_semantic_search;
use aft::config::{Config, SemanticBackend, SemanticBackendConfig};
use aft::context::{AppContext, SemanticIndexStatus};
use aft::parser::TreeSitterProvider;
use aft::protocol::{RawRequest, Response};
use aft::search_index::SearchIndex;
use aft::semantic_index::SemanticIndex;
use serde_json::Value;

fn request(query: &str) -> RawRequest {
    request_with(query, None)
}

fn request_with(query: &str, hint: Option<&str>) -> RawRequest {
    let mut value = serde_json::json!({
        "id": "aft-search-contract",
        "command": "semantic_search",
        "query": query,
        "top_k": 5,
    });
    if let Some(hint) = hint {
        value["hint"] = serde_json::json!(hint);
    }
    serde_json::from_value(value).expect("build semantic search request")
}

fn response_value(response: Response) -> Value {
    serde_json::to_value(response).expect("serialize response")
}

fn test_context(project_root: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        },
    )
}

fn openai_context(project_root: &Path, base_url: String) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project_root.to_path_buf()),
            semantic: SemanticBackendConfig {
                backend: SemanticBackend::OpenAiCompatible,
                model: "test-embedding".to_string(),
                base_url: Some(base_url),
                api_key_env: None,
                timeout_ms: 5_000,
                max_batch_size: 64,
            },
            ..Config::default()
        },
    )
}

fn project_with_needle() -> (tempfile::TempDir, std::path::PathBuf, &'static str) {
    let project = tempfile::tempdir().expect("create project dir");
    let source_file = project.path().join("src/lib.rs");
    std::fs::create_dir_all(source_file.parent().expect("source parent"))
        .expect("create source dir");
    let source = "pub fn needle_symbol() -> bool { true }\npub fn exported() {}\n";
    std::fs::write(&source_file, source).expect("write source file");
    (project, source_file, source)
}

fn install_lexical_index(ctx: &AppContext, source_file: &Path, source: &str) {
    let mut index = SearchIndex::new();
    index.index_file(source_file, source.as_bytes());
    index.ready = true;
    *ctx.search_index().borrow_mut() = Some(index);
}

fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let addr = listener.local_addr().expect("embedding server addr");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept embedding request");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut header_end = None;
        let mut content_length = 0usize;

        loop {
            let n = stream.read(&mut chunk).expect("read embedding request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if header_end.is_none() {
                if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                        let Some((name, value)) = line.split_once(':') else {
                            continue;
                        };
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
            }
            if let Some(end) = header_end {
                if buf.len() >= end + content_length {
                    break;
                }
            }
        }

        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    });

    (format!("http://{addr}"), handle)
}

fn assert_lexical_fallback(response: &Value, semantic_status: &str) {
    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], false);
    assert_eq!(response["semantic_unavailable"], true);
    assert_eq!(response["lexical_only_fallback"], true);
    assert_eq!(response["semantic_status"], semantic_status);
    assert_eq!(response["interpreted_as"], "hybrid");
    assert_eq!(response["status"], "ready");
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| result["source"] == "lexical"
            && result["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("src/lib.rs"))),
        "expected lexical fallback result, got {results:?}"
    );
    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("lexical-only fallback"))),
        "expected lexical fallback warning, got {warnings:?}"
    );
}

#[test]
fn blank_queries_are_rejected_before_routing() {
    let project = tempfile::tempdir().expect("create project dir");
    let ctx = test_context(project.path());

    for query in ["", "  "] {
        let response = response_value(handle_semantic_search(&request(query), &ctx));
        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "invalid_request");
        assert_eq!(response["message"], "query must be non-empty");
    }
}

#[test]
fn hybrid_disabled_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "disabled");
}

#[test]
fn hybrid_failed_semantic_uses_lexical_only_fallback() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() =
        SemanticIndexStatus::Failed("ONNX Runtime unavailable".to_string());

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_lexical_fallback(&response, "unavailable");
}

#[test]
fn explicit_semantic_hint_fails_when_semantic_is_unavailable() {
    let (project, source_file, source) = project_with_needle();
    let ctx = test_context(project.path());
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("needle_symbol", Some("semantic")),
        &ctx,
    ));

    assert_eq!(response["success"], false);
    assert_eq!(response["code"], "semantic_unavailable");
}

#[test]
fn regex_grep_success_reports_ready_status_not_semantic_backend_status() {
    let (project, _source_file, _source) = project_with_needle();
    let ctx = test_context(project.path());
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;

    let response = response_value(handle_semantic_search(
        &request_with("^pub fn exported", Some("regex")),
        &ctx,
    ));

    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["complete"], true);
    assert_eq!(response["results"][0]["kind"], "GrepLine");
}

#[test]
fn hybrid_ready_semantic_reports_complete_success() {
    let (project, source_file, source) = project_with_needle();
    let (base_url, handle) = start_mock_embedding_server();
    let ctx = openai_context(project.path(), base_url);
    install_lexical_index(&ctx, &source_file, source);
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
    *ctx.semantic_index().borrow_mut() = Some(SemanticIndex::new(project.path().to_path_buf(), 3));

    let response = response_value(handle_semantic_search(&request("needle_symbol"), &ctx));

    assert_eq!(
        response["success"], true,
        "response should succeed: {response:?}"
    );
    assert_eq!(response["complete"], true);
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "ready");
    assert_eq!(response["interpreted_as"], "hybrid");
    handle.join().expect("embedding server thread");
}
