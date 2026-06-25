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

        pub fn artifact_cache_key(project_root: &Path) -> String {
            let canonical_root =
                fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
            let mut hasher = Sha256::new();
            hasher.update(canonical_root.to_string_lossy().as_bytes());
            let digest = format!("{:x}", hasher.finalize());
            digest[..16].to_string()
        }
    }
}

use aft::search_index::artifact_cache_key;
use serde_json::{json, Value};

use crate::helpers::{user_config, AftProcess};

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
            "storage_dir": storage_dir.display().to_string(),
            "config": user_config(serde_json::json!({ "semantic_search": enabled })),
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
            "storage_dir": storage_dir.display().to_string(),
            "config": user_config(serde_json::json!({
                "semantic_search": true,
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "test-embedding",
                    "base_url": base_url,
                    "timeout_ms": 5_000,
                    "max_batch_size": 64
                }
            })),
        }),
    )
}

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    // Gate for the post-edit refresh embedding request. The refresh worker marks
    // the file "refreshing" BEFORE calling embed and clears it AFTER, so blocking
    // the embed response for the edited file's content holds `refreshing_count == 1`
    // open until the test has observed it and queried, then flips this flag. This
    // makes the transient refreshing state deterministically observable instead of
    // racing the refresh-completion window, which is lost under full-suite parallel
    // load (the test's status/search round-trips get starved). The query embed
    // ("unchanged semantic target") is NOT gated, so the search still proceeds.
    release_refresh: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        // The LISTENER is nonblocking so the accept loop can poll `running` and
        // exit promptly on Drop. Each ACCEPTED stream is explicitly set back to
        // blocking before it is handled (below) — accepted sockets inherit the
        // listener's nonblocking flag on some platforms (notably Windows), and a
        // nonblocking `read()` returns `WouldBlock`, which `handle_embedding_request`
        // propagates via `?` and drops the connection mid-request. reqwest then
        // reports a generic "error sending request" that is NOT classified
        // connect/timeout transient, so the semantic build parks in terminal
        // `Failed` instead of riding it out — the root cause of the Windows-CI
        // flake (note #351). `set_read_timeout` alone does not reliably override
        // an inherited nonblocking socket on Windows, so we clear the flag
        // explicitly.
        listener
            .set_nonblocking(true)
            .expect("set embedding server nonblocking");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let release_refresh = Arc::new(AtomicBool::new(false));
        let release_for_thread = Arc::clone(&release_refresh);
        let handle = thread::spawn(move || {
            // Each connection is served on its own thread so a slow or gated
            // request (the held post-edit embed) never blocks accepting the
            // concurrent cold-build batch or the mid-refresh query. Single-threaded
            // inline handling head-of-line-blocked those concurrent embeds under
            // loaded CI, starving readiness round-trips.
            let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Restore blocking mode so the handler's read()/write()
                        // block (honoring the per-request read timeout) instead of
                        // returning WouldBlock and dropping the connection.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let release = Arc::clone(&release_for_thread);
                        workers.push(thread::spawn(move || {
                            let mut stream = stream;
                            let _ = handle_embedding_request(&mut stream, &release);
                        }));
                        // Reap finished handlers so the vec stays bounded.
                        workers.retain(|worker| !worker.is_finished());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            addr,
            running,
            release_refresh,
            handle: Some(handle),
        }
    }

    /// Release the held post-edit refresh embedding request. Call this once the
    /// test has observed `refreshing_count == 1` and run its mid-refresh query so
    /// the refresh can complete and the server thread can drain.
    fn release_refresh(&self) {
        self.release_refresh.store(true, Ordering::SeqCst);
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

fn handle_embedding_request(
    stream: &mut TcpStream,
    release_refresh: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
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

    if inputs
        .iter()
        .any(|input| input.to_ascii_lowercase().contains("after edit"))
    {
        // Hold the post-edit refresh embed open until the test observes
        // `refreshing_count == 1`, runs its mid-refresh query, and releases it
        // (see MockEmbeddingServer::release_refresh). The 30s cap exceeds the
        // test's observe-query-release latency even under heavy parallel load
        // (status/search round-trips can be starved); it only avoids wedging if
        // the test panics before releasing. The query embed ("unchanged
        // semantic target") never contains "after edit", so search proceeds.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !release_refresh.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

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
    // 300 x 100ms = 30s. A contended Windows CI runner executes the integration
    // binary several times slower than local (note: memory 6987), so a 10s
    // budget can expire before a cold semantic build + first embed round-trip
    // completes even when nothing is actually wrong. 30s comfortably covers it
    // while still failing fast on a genuine wedge.
    for _ in 0..300 {
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
fn semantic_search_falls_back_to_lexical_when_disabled_without_index() {
    // When semantic search is disabled, a natural-language query degrades to a
    // lexical grep fallback (council #5 design) so the agent is not stranded with
    // zero results. The fallback is honest: it reports semantic_status "disabled"
    // and interpreted_as "literal" alongside whatever lexical results it finds.
    // Use an empty project directory so the path is deterministic regardless of cwd.
    let project = setup_project(&[]);
    let previous_cwd = std::env::current_dir().expect("read cwd");
    std::env::set_current_dir(project.path()).expect("set cwd to empty project");

    let mut aft = AftProcess::spawn();

    let response = send(
        &mut aft,
        json!({
            "id": "semantic-disabled-fallback",
            "command": "semantic_search",
            // Natural-language phrasing routes to the degraded lexical fallback
            // when semantic is disabled.
            "query": "how does request handling work",
        }),
    );

    std::env::set_current_dir(&previous_cwd).expect("restore cwd");

    assert_eq!(
        response["success"], true,
        "search should succeed: {response:?}"
    );
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["lexical_only_fallback"], true);

    let status = aft.shutdown();
    assert!(status.success());
}
#[test]
fn semantic_search_falls_back_to_lexical_when_feature_is_off() {
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

    // semantic_search: false -> natural-language query degrades to the honest
    // lexical-only grep fallback (council #5), not a bare "not enabled" error.
    assert_eq!(
        response["success"], true,
        "search should succeed: {response:?}"
    );
    assert_eq!(response["semantic_status"], "disabled");
    assert_eq!(response["interpreted_as"], "literal");
    assert_eq!(response["lexical_only_fallback"], true);

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
    // Watcher-dependent: edits src/b.rs after configure and waits for the
    // watcher-driven refresh. The default `spawn()` disables the OS watcher
    // (see AftProcess::spawn_with_real_watcher); this test must opt in. Runs in
    // its own standalone `semantic_test` binary (sequential, no concurrent load).
    //
    // Shrink the build-level retry backoff so that if the very first embed
    // request transiently connect-fails (e.g. the mock server thread has not
    // started accepting yet), the build self-heals within the readiness poll
    // window instead of waiting the real 15s first backoff (which exceeds the
    // 10s poll and would flake). This is the documented test seam, not a
    // user-facing knob.
    let mut aft = AftProcess::spawn_with_real_watcher_env(&[(
        "AFT_SEMANTIC_RETRY_BACKOFF_MS",
        std::ffi::OsStr::new("200"),
    )]);

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
                    .is_some_and(|file| file.replace('\\', "/").ends_with("src/a.rs"))
        }),
        "expected semantic result from unchanged file, got {results:?}"
    );

    // Release the held post-edit refresh embed now that the mid-refresh state
    // has been observed and queried, so the refresh completes and the mock
    // server thread can drain cleanly on shutdown.
    server.release_refresh();

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
    let project_key = artifact_cache_key(project.path());
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
                "config": user_config(serde_json::json!({
                    "semantic_search": true,
                    "semantic": {
                        "backend": "ollama",
                        "model": "nomic-embed-text",
                        "base_url": base_url
                    }
                })),
            }),
        );
        assert_eq!(
            response["success"], true,
            "configure should accept loopback base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}

/// User-tier semantic endpoints are trusted user config, so self-hosted
/// private-network embedding backends are accepted through the tier resolver.
#[test]
fn configure_accepts_user_private_base_url() {
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
                "config": user_config(serde_json::json!({
                    "semantic_search": true,
                    "semantic": {
                        "backend": "openai_compatible",
                        "model": "text-embedding-3-small",
                        "base_url": base_url,
                        "api_key_env": "FAKE_KEY"
                    }
                })),
            }),
        );
        assert_eq!(
            response["success"], true,
            "configure should accept user private base_url {base_url}, got: {response:?}"
        );
        let _ = aft.shutdown();
    }
}
