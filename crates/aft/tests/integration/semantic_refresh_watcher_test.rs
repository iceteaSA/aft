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

use aft::context::SemanticIndexStatus;
use aft::search_index::SearchIndex;
use aft::semantic_index::SemanticIndex;
use serde_json::{json, Value};

use crate::helpers::AftProcess;

struct MockEmbeddingServer {
    base_url: String,
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    // Gate for the post-edit refresh embedding request. The refresh worker marks
    // the file "refreshing" BEFORE calling embed and clears it AFTER, so blocking
    // the embed response holds `refreshing_count == 1` open until the test has
    // observed it and flips this flag. This makes the transient refreshing state
    // deterministically observable instead of racing a fixed sleep window — the
    // old 500ms delay could be missed entirely when the test's polling thread is
    // starved under full-suite parallel load.
    release_refresh: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_thread = Arc::clone(&running);
        let release_refresh = Arc::new(AtomicBool::new(false));
        let release_for_thread = Arc::clone(&release_refresh);
        let handle = thread::spawn(move || {
            while running_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = handle_embedding_request(&mut stream, &release_for_thread);
                    }
                    Err(_) => break,
                }
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
    /// test has observed `refreshing_count == 1` so the refresh can complete.
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
        .any(|input| input.to_ascii_lowercase().contains("after edit refreshed"))
    {
        // Hold the refresh open until the test observes `refreshing_count == 1`
        // and releases it (see MockEmbeddingServer::release_refresh). The cap
        // must exceed the test's observe-and-release latency even under heavy
        // load (its status round-trips can be starved), so it's generous; it
        // exists only to avoid wedging if the test panics before releasing.
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
    if lower.contains("alpha_anchor") || lower.contains("alpha anchor") {
        vec![1.0, 0.0, 0.0]
    } else if lower.contains("edited_refresh_marker") || lower.contains("edited refresh marker") {
        vec![0.0, 1.0, 0.0]
    } else {
        vec![0.0, 0.0, 1.0]
    }
}

fn setup_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create project dir");
    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write fixture");
    }
    temp_dir
}

#[cfg(unix)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_dir_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

fn send(aft: &mut AftProcess, request: Value) -> Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
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
            "id": "cfg-semantic-refresh",
            "command": "configure",
            "harness": "opencode",
            "project_root": root.display().to_string(),
            "search_index": false,
            "semantic_search": true,
            "storage_dir": storage_dir.display().to_string(),
            "semantic": {
                "backend": "openai_compatible",
                "model": "test-embedding",
                "base_url": base_url,
                "timeout_ms": 30_000,
                "max_batch_size": 64,
            },
        }),
    )
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
    // Generous budget: this e2e test depends on an OS file-watcher event being
    // delivered to a spawned aft process, then a refresh worker reacting. Under
    // full-suite parallelism (dozens of concurrent aft processes saturating the
    // CPU) that pipeline can be starved for several seconds. The loop returns
    // immediately on match, so a high cap costs nothing on the happy path and
    // only buys headroom under load. The barrier in the mock embedding server
    // holds the refreshing window open once the refresh fires, so the only thing
    // this budget needs to absorb is watcher/worker scheduling latency.
    let mut last_response = None;
    for _ in 0..400 {
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

/// Like `wait_for_semantic_status`, but re-writes `contents` to `file` on every
/// poll until `predicate` holds. This defeats FSEvents watcher attach latency:
/// the recursive watcher comes up asynchronously after configure, so a single
/// pre-attach write can be missed entirely. Re-emitting the modify event each
/// iteration guarantees the watcher eventually observes a change once it is
/// live, without depending on one perfectly-timed write.
fn wait_for_semantic_status_with_retouch<F>(
    aft: &mut AftProcess,
    label: &str,
    file: &Path,
    contents: &str,
    predicate: F,
) -> Value
where
    F: Fn(&Value) -> bool,
{
    let mut last_response = None;
    for i in 0..400 {
        let response = status(aft);
        assert_eq!(
            response["success"], true,
            "status should succeed while waiting for {label}: {response:?}"
        );
        if predicate(&response) {
            return response;
        }
        // Re-touch periodically (not every poll) so we keep emitting modify
        // events until the watcher attaches, without hammering the filesystem.
        if i % 3 == 0 {
            let _ = fs::write(file, contents);
        }
        last_response = Some(response);
        thread::sleep(Duration::from_millis(100));
    }

    panic!(
        "semantic status did not become {label} in time (with retouch); last response: {:?}",
        last_response
    );
}

#[test]
fn refreshing_status_keeps_repeated_same_file_invalidations_until_last_completion() {
    let file = Path::new("src/repeated.rs").to_path_buf();
    let mut status = SemanticIndexStatus::ready();

    status.add_refreshing_file(file.clone());
    status.start_refreshing_file(file.clone());
    status.add_refreshing_file(file.clone());

    assert_eq!(status.refreshing_count(), 1);
    let SemanticIndexStatus::Ready { refreshing } = &status else {
        panic!("semantic status should stay ready");
    };
    assert_eq!(refreshing.as_slice(), std::slice::from_ref(&file));

    status.complete_refreshing_file(&file);

    assert_eq!(
        status.refreshing_count(),
        1,
        "first refresh completion must not clear a queued refresh for the same file"
    );
    let SemanticIndexStatus::Ready { refreshing } = &status else {
        panic!("semantic status should stay ready");
    };
    assert_eq!(refreshing.as_slice(), std::slice::from_ref(&file));

    status.start_refreshing_file(file.clone());
    status.complete_refreshing_file(&file);

    assert_eq!(status.refreshing_count(), 0);
    let SemanticIndexStatus::Ready { refreshing } = &status else {
        panic!("semantic status should stay ready");
    };
    assert!(refreshing.is_empty());
}

#[test]
fn semantic_refresh_watcher_reindexes_modified_file_and_clears_refreshing() {
    let project = setup_project(&[
        (
            "src/a.rs",
            "pub fn alpha_anchor() -> &'static str {\n    \"alpha anchor\"\n}\n",
        ),
        (
            "src/b.rs",
            "pub fn edited_refresh_marker() -> &'static str {\n    \"before edit\"\n}\n",
        ),
        (
            "src/c.rs",
            "pub fn gamma_helper() -> &'static str {\n    \"gamma\"\n}\n",
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

    let ready = wait_for_semantic_status(&mut aft, "initial ready", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["entries"].as_u64().unwrap_or(0) >= 3
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(ready["semantic_index"]["refreshing_count"], 0);

    let edited_file = project.path().join("src/b.rs");
    let edited_contents =
        "pub fn edited_refresh_marker() -> &'static str {\n    \"after edit refreshed content\"\n}\n";
    fs::write(&edited_file, edited_contents).expect("edit watched file");

    // The mock holds the refresh embedding request open, so the file stays in
    // the "refreshing" set until we release it below — making this transient
    // state deterministically observable.
    //
    // Re-touch on every poll: the recursive FSEvents watcher attaches
    // asynchronously after configure, so under full-suite parallelism the very
    // first write can land before the watch is live (or its initial event can
    // be coalesced/dropped) and the refresh would never fire. Re-writing the
    // same content each iteration keeps emitting modify events until the watcher
    // is attached and reacts; once `refreshing_count == 1` the barrier holds it
    // open so we reliably observe it. This makes the test robust to watcher
    // attach latency instead of depending on a single well-timed event.
    let refreshing = wait_for_semantic_status_with_retouch(
        &mut aft,
        "watcher refreshing",
        &edited_file,
        edited_contents,
        |response| {
            response["semantic_index"]["status"] == "ready"
                && response["semantic_index"]["refreshing_count"] == 1
        },
    );
    assert_eq!(refreshing["semantic_index"]["refreshing_count"], 1);

    // Observed the refreshing state; let the refresh complete.
    server.release_refresh();

    let refreshed = wait_for_semantic_status(&mut aft, "refresh completed", |response| {
        response["semantic_index"]["status"] == "ready"
            && response["semantic_index"]["refreshing_count"] == 0
    });
    assert_eq!(refreshed["semantic_index"]["refreshing_count"], 0);

    let search = send(
        &mut aft,
        json!({
            "id": "semantic-refresh-search",
            "command": "semantic_search",
            "query": "edited refresh marker",
            "hint": "semantic",
            "top_k": 5,
        }),
    );
    assert_eq!(
        search["success"], true,
        "semantic search should succeed: {search:?}"
    );
    assert_eq!(search["status"], "ready");
    let results = search["results"].as_array().expect("results array");
    let edited_result = results
        .iter()
        .find(|result| {
            result["file"]
                .as_str()
                .is_some_and(|file| file.replace('\\', "/").ends_with("src/b.rs"))
        })
        .unwrap_or_else(|| panic!("expected refreshed src/b.rs result, got {results:?}"));
    assert!(
        edited_result["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("after edit refreshed content")),
        "expected refreshed snippet, got {edited_result:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn watcher_deleted_alias_path_invalidates_canonical_search_and_semantic_entries() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let real_root = temp_dir.path().join("real-project");
    let source_file = real_root.join("src/lib.rs");
    fs::create_dir_all(source_file.parent().expect("source parent")).expect("create src dir");
    fs::write(
        &source_file,
        "pub fn alias_delete_anchor() -> usize {
    42
}
",
    )
    .expect("write indexed source");

    let alias_root = temp_dir.path().join("alias-project");
    if let Err(error) = create_dir_symlink(&real_root, &alias_root) {
        eprintln!("skipping symlink canonicalization test: {error}");
        return;
    }

    let canonical_root = fs::canonicalize(&real_root).expect("canonicalize real root");
    let canonical_file = fs::canonicalize(&source_file).expect("canonicalize source file");
    let alias_file = alias_root.join("src/lib.rs");

    let mut search_index = SearchIndex::build(&canonical_root);
    assert!(
        search_index.path_to_id.contains_key(&canonical_file),
        "search index should store the canonical file key"
    );

    let mut embed = |texts: Vec<String>| {
        Ok::<Vec<Vec<f32>>, String>(texts.into_iter().map(|text| embedding_for(&text)).collect())
    };
    let mut semantic_index = SemanticIndex::build(
        &canonical_root,
        std::slice::from_ref(&canonical_file),
        &mut embed,
        64,
    )
    .expect("build semantic index");
    assert!(
        semantic_index.len() > 0,
        "semantic index should contain the canonical file entry"
    );

    fs::remove_file(&canonical_file).expect("delete canonical source file");
    assert!(
        !alias_file.exists(),
        "alias path should be missing after canonical delete"
    );

    search_index.remove_file(&alias_file);
    semantic_index.invalidate_file(&alias_file);

    assert!(
        !search_index.path_to_id.contains_key(&canonical_file),
        "deleted alias path should invalidate the canonical search-index key"
    );
    assert_eq!(
        semantic_index.len(),
        0,
        "deleted alias path should invalidate canonical semantic entries"
    );
}
