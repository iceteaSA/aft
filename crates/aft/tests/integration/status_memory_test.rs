use std::sync::Arc;

use aft::commands::status::handle_status;
use aft::config::Config;
use aft::context::{App, AppContext};
use aft::executor::Executor;
use aft::protocol::RawRequest;
use cortexkit_paths::ProjectRootId;
use serde_json::json;

fn status_request() -> RawRequest {
    RawRequest {
        id: "memory-status".to_string(),
        command: "status".to_string(),
        lsp_hints: None,
        session_id: None,
        params: json!({}),
    }
}

#[test]
fn status_memory_attributes_every_registered_root_and_exposes_residual() {
    let first = tempfile::tempdir().expect("first root");
    let second = tempfile::tempdir().expect("second root");
    let first_root = std::fs::canonicalize(first.path()).expect("canonical first root");
    let second_root = std::fs::canonicalize(second.path()).expect("canonical second root");
    let app = App::default_shared();
    let first_ctx = Arc::new(AppContext::from_app(
        Arc::clone(&app),
        Config {
            project_root: Some(first_root.clone()),
            ..Config::default()
        },
    ));
    let second_ctx = Arc::new(AppContext::from_app(
        app,
        Config {
            project_root: Some(second_root.clone()),
            ..Config::default()
        },
    ));
    let executor = Executor::new();
    assert!(executor.register_actor(
        ProjectRootId::from_path(&first_root).expect("first root id"),
        Arc::clone(&first_ctx),
    ));
    assert!(executor.register_actor(
        ProjectRootId::from_path(&second_root).expect("second root id"),
        second_ctx,
    ));

    let response = handle_status(&status_request(), &first_ctx);
    let memory = &response.data["memory"];
    assert_eq!(memory["roots_status"], "ready");
    let roots = memory["roots"].as_object().expect("memory roots object");
    assert_eq!(roots.len(), 2);
    for root in [first_root, second_root] {
        let estimate = roots
            .get(&root.display().to_string())
            .expect("registered root estimate");
        for subsystem in [
            "semantic",
            "trigram",
            "symbols",
            "callgraph",
            "inspect",
            "bash",
            "lsp",
            "parser_pool",
        ] {
            assert!(estimate.get(subsystem).is_some(), "missing {subsystem}");
        }
    }
    let attributed = memory["process"]["total_attributed_bytes"]
        .as_u64()
        .expect("attributed bytes");
    assert!(attributed < 16 * 1024 * 1024 * 1024);
    assert!(memory["process"].get("unattributed_bytes").is_some());
    assert!(memory["process"].get("rss_status").is_some());
}
