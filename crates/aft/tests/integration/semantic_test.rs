use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

mod aft {
    pub mod search_index {
        use std::fs;
        use std::path::Path;

        use sha2::{Digest, Sha256};

        pub fn project_cache_key(project_root: &Path) -> String {
            let canonical_root =
                fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
            let mut hasher = Sha256::new();
            hasher.update(canonical_root.to_string_lossy().as_bytes());
            let digest = format!("{:x}", hasher.finalize());
            digest[..16].to_string()
        }
    }
}

use aft::search_index::project_cache_key;
use serde_json::{json, Value};

use crate::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

fn configure_semantic(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    enabled: bool,
) -> Value {
    send(
        aft,
        json!({
            "id": "cfg-semantic",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "semantic_search": enabled,
            "storage_dir": storage_dir.display().to_string(),
        }),
    )
}

fn configure_semantic_openai(
    aft: &mut AftProcess,
    root: &Path,
    storage_dir: &Path,
    base_url: &str,
) -> Value {
    send(
        aft,
        json!({
            "id": "cfg-semantic-openai",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "semantic_search": true,
            "storage_dir": storage_dir.display().to_string(),
            "semantic": {
                "backend": "openai_compatible",
                "model": "test-embedding",
                "base_url": base_url,
                "timeout_ms": 5_000,
                "max_batch_size": 64,
            },
        }),
    )
}

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        listener
            .set_nonblocking(true)
            .expect("set embedding server nonblocking");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = handle_embedding_request(&mut stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            handle: Some(handle),
        }
    }
}

impl Drop for MockEmbeddingServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            handle.join().expect("embedding server thread");
        }
    }
}

fn handle_embedding_request(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;

    loop {
        let n = stream.read(&mut chunk)?;
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

    let body = header_end
        .and_then(|end| buf.get(end..end + content_length))
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .unwrap_or_else(|| json!({ "input": [] }));
    let inputs = match &body["input"] {
        Value::Array(values) => values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        Value::String(value) => vec![value.clone()],
        _ => Vec::new(),
    };
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| json!({ "embedding": embedding_for(input), "index": index }))
        .collect::<Vec<_>>();
    let body = json!({ "data": data }).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())
}

fn embedding_for(text: &str) -> Vec<f32> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("unchanged_semantic_target")
        || lower.contains("stable retrieval")
        || lower.contains("unchanged semantic target")
    {
        vec![1.0, 0.0, 0.0]
    } else if lower.contains("edited_refresh_marker") || lower.contains("edited refresh marker") {
        vec![0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

fn status(aft: &mut AftProcess) -> Value {
    send(
        aft,
        json!({
            "id": "status",
            "command": "status",
        }),
    )
}

fn wait_for_semantic_status<F>(aft: &mut AftProcess, label: &str, predicate: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let mut last_response = None;
    for _ in 0..100 {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed while waiting for {label}: {response:?}"
        );
        if predicate(&response) {
            return response;
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!(
        "semantic status did not become {label} in time; last response: {:?}",
        last_response
    );
}

fn wait_for_ready_search(aft: &mut AftProcess, query: &str) -> Value {
    for _ in 0..180 {
        let response = send(
            aft,
            json!({
                "id": "semantic-search",
                "command": "semantic_search",
                "query": query,
                "top_k": 5,
            }),
        );

        assert_eq!(
            response["success"], true,
            "semantic_search should succeed while polling: {response:?}"
        );

        if response["status"] == "ready" {
            return response;
        }

        thread::sleep(Duration::from_millis(250));
    }

    panic!("semantic index did not become ready in time");
}

#[test]
fn semantic_search_returns_not_ready_without_an_index() {
    // Without configure, project_root defaults to the process cwd. In CI that
    // is the full aft repo, which can trigger lexical grep fallback and report
    // status "ready" even though semantic search is disabled. Use an empty
    // project directory so the test exercises the disabled path deterministically.
    let project = setup_project(&[]);
    let previous_cwd = std::env::current_dir().expect("read cwd");
    std::env::set_current_dir(project.path()).expect("set cwd to empty project");

    let mut aft = AftProcess::spawn();

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-not-ready",
            "command": "semantic_search",
            // Natural-language phrasing so auto mode does not classify this as an
            // Identifier (which triggers lexical grep fallback with status "ready"
            // when semantic search is disabled).
            "query": "how does request handling work",
        }),
    );

    std::env::set_current_dir(&previous_cwd).expect("restore cwd");

    assert_eq!(
        response["success"], true,
        "search should succeed: {response:?}"
    );
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["status"], "disabled");
    assert_eq!(response["text"], "Semantic search is not enabled.");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn semantic_search_returns_disabled_when_feature_is_off() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() -> bool { true }\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let mut aft = AftProcess::spawn();

    let configure = configure_semantic(&mut aft, project.path(), storage.path(), false);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-disabled",
            "command": "semantic_search",
            "query": "how does request handling work",
        }),
    );

    assert_eq!(
        response["success"], true,
        "search should succeed: {response:?}"
    );
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["status"], "disabled");
    assert_eq!(response["text"], "Semantic search is not enabled.");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn semantic_search_stays_queryable_while_file_refreshes_after_watcher_invalidation() {
    let project = setup_project(&[
        (
            "src/a.rs",
            "pub fn unchanged_semantic_target() -> &'static str {\n    \"stable retrieval\"\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn edited_refresh_marker() -> &'static str {\n    \"before edit\"\n}\n",
        ),
        (
            "src/c.rs",
            "pub fn unrelated_helper() -> &'static str {\n    \"other\"\n}\n",
        ),
    ]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let server = MockEmbeddingServer::start();
    let mut aft = AftProcess::spawn();

    let configure =
        configure_semantic_openai(&mut aft, project.path(), storage.path(), &server.base_url);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let ready = wait_for_semantic_status(&mut aft, "ready", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(ready["semantic_index"]["status"], "ready");
    assert_eq!(ready["semantic_index"]["refreshing_count"], 0);

    let edited_file = project.path().join("src/b.rs");
    fs::write(
        &edited_file,
        "pub fn edited_refresh_marker() -> &'static str {\n    \"after edit\"\n}\n",
    )
    .expect("edit file");

    let refreshing =
        wait_for_semantic_status(&mut aft, "ready with one refreshing file", |response| {
            response["semantic_index"]["status"] == "ready"
                && response["semantic_index"]["refreshing_count"] == 1
        });
    assert_eq!(refreshing["semantic_index"]["status"], "ready");
    assert_eq!(refreshing["semantic_index"]["refreshing_count"], 1);

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-refreshing-search",
            "command": "semantic_search",
            "query": "unchanged semantic target",
            "hint": "semantic",
            "top_k": 5,
        }),
    );

    assert_eq!(
        response["success"], true,
        "semantic search should succeed: {response:?}"
    );
    assert_eq!(response["status"], "ready");
    assert_eq!(response["semantic_status"], "ready");
    assert_eq!(response["interpreted_as"], "semantic");
    assert_ne!(response["status"], "building");
    let warnings = response["warnings"].as_array().expect("warnings array");
    assert!(
        warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("1 file(s) refreshing"))),
        "expected refreshing warning, got {warnings:?}"
    );
    let results = response["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|result| {
            result["source"] == "semantic"
                && result["file"]
                    .as_str()
                    .is_some_and(|file| file.ends_with("src/a.rs"))
        }),
        "expected semantic result from unchanged file, got {results:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
#[ignore = "requires fastembed model download (~22MB) and a full semantic index build"]
fn semantic_index_persists_across_configure_build_search_roundtrip() {
    let project = setup_project(&[
        (
            "src/lib.rs",
            "pub fn handle_request(token: &str) -> bool {\n    !token.is_empty()\n}\n\npub struct AuthService;\n",
        ),
        (
            "src/utils.rs",
            "pub fn normalize_user_id(input: &str) -> String {\n    input.trim().to_lowercase()\n}\n",
        ),
    ]);
    let storage = tempfile::tempdir().expect("create storage dir");
    let project_key = project_cache_key(project.path());
    let semantic_file = storage
        .path()
        .join("semantic")
        .join(&project_key)
        .join("semantic.bin");

    // Slow by design: this may download the embedding model on first use.
    let mut first = AftProcess::spawn();
    let configure = configure_semantic(&mut first, project.path(), storage.path(), true);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let first_response = wait_for_ready_search(&mut first, "request authentication handler");
    assert_eq!(first_response["status"], "ready");
    assert!(
        semantic_file.is_file(),
        "semantic index should persist to disk"
    );

    let first_results = first_response["results"]
        .as_array()
        .expect("semantic results array");
    assert!(
        !first_results.is_empty(),
        "expected at least one semantic result"
    );
    assert_eq!(first_results[0]["name"], "handle_request");
    assert_eq!(first_results[0]["source"], "semantic");

    let status = first.shutdown();
    assert!(status.success());

    let mut second = AftProcess::spawn();
    let configure = configure_semantic(&mut second, project.path(), storage.path(), true);
    assert_eq!(
        configure["success"], true,
        "configure should succeed: {configure:?}"
    );

    let second_response = wait_for_ready_search(&mut second, "request authentication handler");
    assert_eq!(second_response["status"], "ready");
    assert_eq!(second_response["text"], first_response["text"]);
    assert_eq!(second_response["results"], first_response["results"]);

    let status = second.shutdown();
    assert!(status.success());
}

/// Regression for the v0.19.5 fix: Ollama's default `base_url`
/// (`http://127.0.0.1:11434`) and `http://localhost:11434` must be accepted at
/// configure time. Earlier versions rejected all loopback as an SSRF guard,
/// which made the Ollama backend unusable at its default config.
#[test]
fn configure_accepts_loopback_base_url_for_self_hosted_backends() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() {}\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");

    for base_url in &[
        "http://127.0.0.1:11434", // Ollama default
        "http://localhost:11434",
        "http://127.0.0.1:8080",
    ] {
        let mut aft = AftProcess::spawn();
        let response = send(
            &mut aft,
            json!({
                "id": "cfg-ollama",
                "command": "configure",
            "harness": "opencode",
                "project_root": project.path().display().to_string(),
                "storage_dir": storage.path().display().to_string(),
                "semantic_search": true,
                "semantic": {
                    "backend": "ollama",
                    "model": "nomic-embed-text",
                    "base_url": base_url,
                },
            }),
        );
        assert_eq!(
            response["success"], true,
            "configure should accept loopback base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}

/// Non-loopback private IPs (LAN/intranet ranges) must still be rejected at
/// configure time. SSRF guard remains meaningful for homelab/corporate
/// network targets even though the user is the trust boundary.
#[test]
fn configure_rejects_non_loopback_private_base_url() {
    let project = setup_project(&[("src/lib.rs", "pub fn handle_request() {}\n")]);
    let storage = tempfile::tempdir().expect("create storage dir");

    for base_url in &[
        "http://192.168.1.50:8080",
        "http://10.0.0.5:11434",
        "http://172.16.0.10:8080",
    ] {
        let mut aft = AftProcess::spawn();
        let response = send(
            &mut aft,
            json!({
                "id": "cfg-private",
                "command": "configure",
            "harness": "opencode",
                "project_root": project.path().display().to_string(),
                "storage_dir": storage.path().display().to_string(),
                "semantic_search": true,
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "text-embedding-3-small",
                    "base_url": base_url,
                    "api_key_env": "FAKE_KEY",
                },
            }),
        );
        assert_eq!(
            response["success"], false,
            "configure should reject non-loopback private base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}
