use std::collections::HashSet;
use std::net::TcpListener as StdTcpListener;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::callgraph::CallGraph;
use aft::config::Config;
use aft::context::AppContext;
use aft::executor::{Executor, ExecutorConfig, Lane};
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use aft::path_identity::ProjectRootId;
use aft::protocol::{PushFrame, RawRequest, Response, StatusChangedFrame};
use aft::subc::run_subc_mode;
use serde_json::{json, Value};
use subc_protocol::manifest::ModuleManifest;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::TcpListener;

static BRIDGE_STATE: OnceLock<Arc<BridgeState>> = OnceLock::new();

struct FakeDaemonInput {
    listener: TcpListener,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    root1: std::path::PathBuf,
    root2: std::path::PathBuf,
    failed_root: std::path::PathBuf,
    push_burst_root: std::path::PathBuf,
    callgraph_root: std::path::PathBuf,
    callgraph_file: std::path::PathBuf,
    state: Arc<BridgeState>,
}

#[derive(Default)]
struct BridgeState {
    inner: Mutex<BridgeInner>,
    cv: Condvar,
}

#[derive(Clone, Debug)]
struct ConfigureEvent {
    request_id: String,
    root: std::path::PathBuf,
    ctx_addr: usize,
    epoch_reads_at_start: usize,
}

#[derive(Default)]
struct BridgeInner {
    overlap_started: usize,
    overlap_current: usize,
    overlap_max: usize,
    overlap_release: bool,
    heavy_started: bool,
    heavy_release: bool,
    epoch_started: usize,
    epoch_current: usize,
    epoch_release: bool,
    configure_events: Vec<ConfigureEvent>,
}

impl BridgeState {
    fn wait_until(&self, label: &str, mut predicate: impl FnMut(&BridgeInner) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut guard = self.inner.lock().expect("bridge state lock");
        while !predicate(&guard) {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for {label}");
            let remaining = deadline.saturating_duration_since(now);
            let (next, result) = self
                .cv
                .wait_timeout(guard, remaining)
                .expect("bridge state condvar");
            guard = next;
            assert!(
                !result.timed_out() || predicate(&guard),
                "timed out waiting for {label}"
            );
        }
    }

    fn release_overlap(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.overlap_release = true;
        self.cv.notify_all();
    }

    fn release_heavy(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.heavy_release = true;
        self.cv.notify_all();
    }

    fn begin_epoch_wave(&self) -> usize {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.epoch_release = false;
        guard.epoch_started
    }

    fn release_epoch_reads(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.epoch_release = true;
        self.cv.notify_all();
    }

    fn assert_overlap(&self) {
        let guard = self.inner.lock().expect("bridge state lock");
        assert!(
            guard.overlap_max >= 2,
            "expected overlapping PureRead jobs, max concurrent reads was {}",
            guard.overlap_max
        );
    }

    fn wait_for_configure(&self, request_id: &str) -> ConfigureEvent {
        let label = format!("configure {request_id}");
        self.wait_until(&label, |inner| {
            inner
                .configure_events
                .iter()
                .any(|event| event.request_id == request_id)
        });
        let guard = self.inner.lock().expect("bridge state lock");
        guard
            .configure_events
            .iter()
            .find(|event| event.request_id == request_id)
            .expect("configure event present")
            .clone()
    }

    fn assert_configure_not_started(&self, request_id: &str) {
        let guard = self.inner.lock().expect("bridge state lock");
        assert!(
            !guard
                .configure_events
                .iter()
                .any(|event| event.request_id == request_id),
            "{request_id} configure started while same-root reads were still in flight"
        );
    }

    fn assert_configure_root(&self, request_id: &str, root: &std::path::Path) {
        let event = self.wait_for_configure(request_id);
        assert_eq!(
            event.root, root,
            "{request_id} configure should target the requested root"
        );
    }

    fn assert_distinct_contexts(&self, first: &str, second: &str) {
        let first = self.wait_for_configure(first);
        let second = self.wait_for_configure(second);
        assert_ne!(
            first.ctx_addr, second.ctx_addr,
            "different roots must configure distinct AppContext actors"
        );
    }

    fn assert_same_context(&self, first: &str, second: &str) {
        let first = self.wait_for_configure(first);
        let second = self.wait_for_configure(second);
        assert_eq!(
            first.ctx_addr, second.ctx_addr,
            "same-root RouteBind must reuse the existing AppContext actor"
        );
    }

    fn overlap_read(&self, id: String) -> Response {
        {
            let mut guard = self.inner.lock().expect("bridge state lock");
            guard.overlap_started += 1;
            guard.overlap_current += 1;
            guard.overlap_max = guard.overlap_max.max(guard.overlap_current);
            self.cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !guard.overlap_release {
                let now = Instant::now();
                assert!(now < deadline, "timed out waiting for overlap release");
                let remaining = deadline.saturating_duration_since(now);
                let (next, result) = self
                    .cv
                    .wait_timeout(guard, remaining)
                    .expect("bridge state condvar");
                guard = next;
                assert!(
                    !result.timed_out() || guard.overlap_release,
                    "timed out waiting for overlap release"
                );
            }
            guard.overlap_current -= 1;
        }
        Response::success(id, json!({ "case": "overlap" }))
    }

    fn heavy(&self, id: String) -> Response {
        {
            let mut guard = self.inner.lock().expect("bridge state lock");
            guard.heavy_started = true;
            self.cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !guard.heavy_release {
                let now = Instant::now();
                assert!(now < deadline, "timed out waiting for heavy release");
                let remaining = deadline.saturating_duration_since(now);
                let (next, result) = self
                    .cv
                    .wait_timeout(guard, remaining)
                    .expect("bridge state condvar");
                guard = next;
                assert!(
                    !result.timed_out() || guard.heavy_release,
                    "timed out waiting for heavy release"
                );
            }
        }
        Response::success(id, json!({ "case": "heavy" }))
    }

    fn epoch_read(&self, id: String) -> Response {
        {
            let mut guard = self.inner.lock().expect("bridge state lock");
            guard.epoch_started += 1;
            guard.epoch_current += 1;
            self.cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !guard.epoch_release {
                let now = Instant::now();
                assert!(now < deadline, "timed out waiting for epoch release");
                let remaining = deadline.saturating_duration_since(now);
                let (next, result) = self
                    .cv
                    .wait_timeout(guard, remaining)
                    .expect("bridge state condvar");
                guard = next;
                assert!(
                    !result.timed_out() || guard.epoch_release,
                    "timed out waiting for epoch release"
                );
            }
            guard.epoch_current -= 1;
        }
        Response::success(id, json!({ "case": "epoch" }))
    }

    fn configure(&self, req: &RawRequest, ctx: &AppContext) {
        let root = req
            .params
            .get("project_root")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let ctx_addr = ctx as *const AppContext as usize;
        let mut guard = self.inner.lock().expect("bridge state lock");
        let epoch_reads_at_start = guard.epoch_current;
        guard.configure_events.push(ConfigureEvent {
            request_id: req.id.clone(),
            root,
            ctx_addr,
            epoch_reads_at_start,
        });
        self.cv.notify_all();
    }
}

fn configure_bridge_context(req: &RawRequest, ctx: &AppContext) -> Response {
    let root = match req.params.get("project_root").and_then(Value::as_str) {
        Some(root) => std::path::PathBuf::from(root),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: missing required param 'project_root'",
            );
        }
    };
    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());

    ctx.update_config(|config| {
        config.project_root = Some(root.clone());
        config.harness = Some(Harness::Opencode);
        config.callgraph_store = false;
        config.search_index = false;
        config.semantic_search = false;
        config.experimental_bash_background = true;
    });
    ctx.set_harness(Harness::Opencode);
    ctx.set_canonical_cache_root(canonical_root);
    ctx.set_cache_role(false, None);
    *ctx.callgraph().lock() = Some(CallGraph::new(root.clone()));
    *ctx.callgraph_store()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    *ctx.callgraph_store_rx().lock() = None;

    Response::success(
        &req.id,
        json!({
            "configured": true,
            "project_root": root.to_string_lossy(),
        }),
    )
}

fn ctx_project_root(ctx: &AppContext) -> String {
    ctx.config()
        .project_root
        .as_ref()
        .map(|root| root.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unset>".to_string())
}

fn emit_test_status(ctx: &AppContext, marker: &str, seq: u64) {
    if let Some(sender) = ctx.progress_sender_handle() {
        sender(PushFrame::StatusChanged(StatusChangedFrame {
            frame_type: "status_changed",
            session_id: Some("session-1".to_string()),
            snapshot: json!({
                "marker": marker,
                "seq": seq,
                "project_root": ctx_project_root(ctx),
            }),
        }));
    }
}

fn emit_test_status_burst(ctx: &AppContext, marker: &str, count: u64) {
    for seq in 0..count {
        emit_test_status(ctx, marker, seq);
    }
}

fn configure_status_burst_spec(req: &RawRequest) -> Option<(String, u64)> {
    let tiers = req.params.get("config")?.as_array()?;
    for tier in tiers {
        let Some(doc) = tier.get("doc").and_then(Value::as_str) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(doc) else {
            continue;
        };
        let Some(spec) = doc.get("subc_test_configure_status_burst") else {
            continue;
        };
        let marker = spec
            .get("marker")
            .and_then(Value::as_str)
            .unwrap_or("configure-status-burst")
            .to_string();
        let count = spec.get("count").and_then(Value::as_u64).unwrap_or(1);
        return Some((marker, count));
    }
    None
}

fn emit_configure_status_burst_if_requested(req: &RawRequest, ctx: &AppContext) {
    if let Some((marker, count)) = configure_status_burst_spec(req) {
        emit_test_status_burst(ctx, &marker, count);
    }
}

fn bridge_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    let state = Arc::clone(BRIDGE_STATE.get().expect("bridge state installed"));
    match req.command.as_str() {
        "configure" => {
            state.configure(&req, ctx);
            if req.id == "subc-bind-5" {
                emit_configure_status_burst_if_requested(&req, ctx);
                return Response::error(
                    &req.id,
                    "config_divergence",
                    "intentional test configure failure",
                );
            }
            let response = configure_bridge_context(&req, ctx);
            emit_configure_status_burst_if_requested(&req, ctx);
            response
        }
        "read" => match req.params.get("case").and_then(Value::as_str) {
            Some("overlap") => state.overlap_read(req.id),
            Some("fast") => Response::success(
                req.id,
                json!({ "case": "fast", "project_root": ctx_project_root(ctx) }),
            ),
            Some("status_bar") => {
                let dead_code = req
                    .params
                    .get("dead_code")
                    .and_then(Value::as_u64)
                    .unwrap_or(11) as usize;
                ctx.update_status_bar_tier2(Some(dead_code), Some(12), Some(13), Some(14), false);
                Response::success(req.id, json!({ "case": "status_bar" }))
            }
            Some("epoch") => state.epoch_read(req.id),
            other => Response::error(
                req.id,
                "unexpected_read_case",
                format!("unexpected read case: {other:?}"),
            ),
        },
        "bash" => aft::commands::bash::handle(&req, ctx),
        "bash_status" => aft::commands::bash_status::handle(&req, ctx),
        "semantic_search" => state.heavy(req.id),
        "subc_test_emit_status" => {
            let marker = req
                .params
                .get("marker")
                .and_then(Value::as_str)
                .unwrap_or("tool-status");
            let seq = req.params.get("seq").and_then(Value::as_u64).unwrap_or(0);
            emit_test_status(ctx, marker, seq);
            Response::success(
                req.id,
                json!({ "emitted": true, "marker": marker, "seq": seq }),
            )
        }
        "subc_test_emit_status_burst" => {
            let marker = req
                .params
                .get("marker")
                .and_then(Value::as_str)
                .unwrap_or("tool-status-burst");
            let count = req.params.get("count").and_then(Value::as_u64).unwrap_or(1);
            emit_test_status_burst(ctx, marker, count);
            Response::success(req.id, json!({ "emitted": count, "marker": marker }))
        }
        "enable_callgraph_store_for_test" => {
            ctx.update_config(|config| config.callgraph_store = true);
            Response::success(req.id, json!({ "callgraph_store": true }))
        }
        "callers" => aft::commands::callers::handle_callers(&req, ctx),
        other => Response::error(
            req.id,
            "unexpected_command",
            format!("unexpected test command: {other}"),
        ),
    }
}

#[test]
fn subc_bridge_routes_multiple_roots_and_reuses_same_root_actor() {
    let state = Arc::new(BridgeState::default());
    let _ = BRIDGE_STATE.set(Arc::clone(&state));

    let root1 = tempfile::tempdir().expect("root1 tempdir");
    let root2 = tempfile::tempdir().expect("root2 tempdir");
    let failed_root = tempfile::tempdir().expect("failed root tempdir");
    let push_burst_root = tempfile::tempdir().expect("push burst root tempdir");
    let callgraph_root = tempfile::tempdir().expect("callgraph root tempdir");
    let callgraph_src = callgraph_root.path().join("src");
    std::fs::create_dir_all(&callgraph_src).expect("callgraph src dir");
    let callgraph_file = callgraph_src.join("lib.rs");
    std::fs::write(
        &callgraph_file,
        "pub fn caller() { callee(); }\npub fn callee() {}\n",
    )
    .expect("callgraph source file");
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn_dir = tempfile::tempdir().expect("connection tempdir");
    let conn_path = conn_dir.path().join("subc-connection.json");

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    std_listener
        .set_nonblocking(true)
        .expect("set fake daemon nonblocking");
    let port = std_listener.local_addr().expect("fake daemon addr").port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port,
        }],
        key: key.clone(),
        daemon_id,
        pid: std::process::id(),
        daemon_ver: "subc-test".to_string(),
    };
    connection_file::write_atomic(&conn_path, &conn).expect("write connection file");

    let daemon_state = Arc::clone(&state);
    let root1_path = root1.path().to_path_buf();
    let root2_path = root2.path().to_path_buf();
    let failed_root_path = failed_root.path().to_path_buf();
    let push_burst_root_path = push_burst_root.path().to_path_buf();
    let callgraph_root_path = callgraph_root.path().to_path_buf();
    let callgraph_file_path = callgraph_file.clone();
    let daemon = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake daemon runtime");
        runtime.block_on(async move {
            let listener = TcpListener::from_std(std_listener).expect("tokio listener");
            tokio::time::timeout(
                Duration::from_secs(10),
                drive_fake_daemon(FakeDaemonInput {
                    listener,
                    key,
                    daemon_id,
                    root1: root1_path,
                    root2: root2_path,
                    failed_root: failed_root_path,
                    push_burst_root: push_burst_root_path,
                    callgraph_root: callgraph_root_path,
                    callgraph_file: callgraph_file_path,
                    state: daemon_state,
                }),
            )
            .await
            .expect("subc bridge test watchdog")
        });
    });

    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(storage.path().to_path_buf()),
            ..Config::default()
        },
    ));
    let executor = Arc::new(Executor::with_config(ExecutorConfig {
        pool_size: 4,
        read_cap: 3,
        actor_cap: 3,
        heavy_permits: 2,
        drr_quantum: 1,
    }));

    let executor_for_check = Arc::clone(&executor);
    run_subc_mode(&conn_path, ctx, executor, bridge_dispatch).expect("subc mode exits cleanly");
    daemon.join().expect("fake daemon joins");

    state.assert_overlap();
    state.assert_configure_root("subc-bind-1", root1.path());
    state.assert_configure_root("subc-bind-2", root2.path());
    state.assert_configure_root("subc-bind-4", root1.path());
    state.assert_distinct_contexts("subc-bind-1", "subc-bind-2");
    state.assert_same_context("subc-bind-1", "subc-bind-4");
    assert!(
        state.wait_for_configure("subc-bind-2").epoch_reads_at_start >= 2,
        "different-root configure should run while route 1 reads are in flight"
    );
    assert_eq!(
        state.wait_for_configure("subc-bind-4").epoch_reads_at_start,
        0,
        "same-root reconfigure must wait for route 1 reads to drain"
    );

    let failed_root_id = ProjectRootId::from_path(failed_root.path()).expect("failed root id");
    let actor_check = executor_for_check.submit(
        failed_root_id,
        Lane::PureRead,
        "b3-actor-check".to_string(),
        Box::new(|_| Response::success("b3-actor-check", json!({ "unexpected": true }))),
    );
    let actor_check_response = actor_check
        .recv_timeout(Duration::from_secs(1))
        .expect("B3 actor check response");
    assert!(!actor_check_response.success);
    assert_eq!(
        actor_check_response
            .data
            .get("code")
            .and_then(Value::as_str),
        Some("actor_not_registered"),
        "failed new-root bind must remove its just-registered actor"
    );

    drop(root1);
    drop(root2);
    drop(failed_root);
    drop(push_burst_root);
    drop(callgraph_root);
    drop(storage);
}

async fn drive_fake_daemon(input: FakeDaemonInput) {
    let FakeDaemonInput {
        listener,
        key,
        daemon_id,
        root1,
        root2,
        failed_root,
        push_burst_root,
        callgraph_root,
        callgraph_file,
        state,
    } = input;
    let (mut stream, _) = listener.accept().await.expect("accept aft client");
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "subc-test",
        Duration::from_secs(5),
    )
    .await
    .expect("authenticate aft client");

    let hello = read_any_frame_timeout(&mut stream, "ModuleHello").await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let hello_body: ModuleHelloBody = serde_json::from_slice(&hello.body).expect("hello body");
    let _: ModuleManifest = hello_body.manifest;
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: Vec::new(),
                subc_capabilities: Vec::new(),
            })
            .expect("hello ack body"),
        )
        .expect("hello ack frame"),
    )
    .await;

    send_route_bind(&mut stream, 1, 10, &root1).await;
    expect_route_bind_ack(&mut stream, 10).await;

    // L3 PUSH fan-out: a root-1 actor emit is serialized as a server-initiated
    // Push frame on route 1 with corr=0.
    send_tool_call(
        &mut stream,
        1,
        70,
        "subc_test_emit_status",
        json!({ "marker": "root1-fanout", "seq": 1 }),
    )
    .await;
    let fanout_pushes =
        expect_status_pushes_for_tool(&mut stream, 70, "root1-fanout", HashSet::from([1])).await;
    assert_eq!(push_seq(&fanout_pushes[0]), Some(1));

    // L3 coalescing integration: configure emits a burst while run_module_loop is
    // awaiting the Mutating configure job, so the queued burst drains in one pass.
    send_route_bind_with_doc(
        &mut stream,
        6,
        16,
        &push_burst_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_configure_status_burst": {
                "marker": "configure-burst",
                "count": 16,
            },
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 16).await;
    let configure_burst_pushes =
        expect_status_pushes(&mut stream, "configure-burst", HashSet::from([6])).await;
    assert_eq!(configure_burst_pushes.len(), 1);
    assert_eq!(push_seq(&configure_burst_pushes[0]), Some(15));
    assert_no_status_push_for_marker(&mut stream, "configure-burst", Duration::from_millis(150))
        .await;

    // L2 response finalizer: a normal route-channel read gets status_bar once
    // the actor has real Tier-2 counts, matching standalone response shape.
    send_tool_call(
        &mut stream,
        1,
        80,
        "read",
        json!({ "case": "status_bar", "dead_code": 21 }),
    )
    .await;
    let status_bar_read = read_frame_timeout(&mut stream, "status-bar read response").await;
    assert_eq!(status_bar_read.header.corr, 80);
    let status_bar_response = tool_response_json(&status_bar_read);
    assert_eq!(status_bar_response["status_bar"]["dead_code"], 21);
    assert_eq!(status_bar_response["status_bar"]["unused_exports"], 12);
    assert_eq!(status_bar_response["status_bar"]["duplicates"], 13);
    assert_eq!(status_bar_response["status_bar"]["todos"], 14);

    // A completed bg-bash task for the bound route's BindIdentity.session is
    // attached to subsequent non-skip responses. The bash_status poll itself is
    // a skip-list command, so it must not carry status_bar/bg_completions.
    send_tool_call(
        &mut stream,
        1,
        81,
        "bash",
        json!({
            "session_id": "session-1",
            "params": {
                "command": "echo subc-bg-completion",
                "background": true,
                "timeout": 5000,
            },
        }),
    )
    .await;
    let bash_started = read_frame_timeout(&mut stream, "bash start response").await;
    assert_eq!(bash_started.header.corr, 81);
    let bash_started_response = tool_response_json(&bash_started);
    assert!(
        bash_started_response["success"].as_bool().unwrap_or(false),
        "background bash should start: {bash_started_response:?}"
    );
    let task_id = bash_started_response["task_id"]
        .as_str()
        .expect("bash start task_id")
        .to_string();
    wait_for_bash_completion(&mut stream, 1, 82, &task_id).await;

    send_tool_call(&mut stream, 1, 120, "read", json!({ "case": "fast" })).await;
    let first_after_completion = read_frame_timeout(&mut stream, "first completion read").await;
    assert_eq!(first_after_completion.header.corr, 120);
    let first_after_completion_response = tool_response_json(&first_after_completion);
    assert_bg_completion(&first_after_completion_response, &task_id);

    send_tool_call(&mut stream, 1, 121, "read", json!({ "case": "fast" })).await;
    let second_after_completion = read_frame_timeout(&mut stream, "second completion read").await;
    assert_eq!(second_after_completion.header.corr, 121);
    let second_after_completion_response = tool_response_json(&second_after_completion);
    assert_bg_completion(&second_after_completion_response, &task_id);

    // Two same-actor PureRead jobs finish/finalize concurrently. Both should
    // clone the pending bg completion safely; the status_bar dedup lock is also
    // exercised because counts were populated above.
    let finalizer_epoch_base = state.begin_epoch_wave();
    for corr in 122..124 {
        send_tool_call(&mut stream, 1, corr, "read", json!({ "case": "epoch" })).await;
    }
    state.wait_until("finalizer epoch reads started", |inner| {
        inner.epoch_started == finalizer_epoch_base + 2
    });
    state.release_epoch_reads();
    let mut finalizer_corrs = HashSet::new();
    for _ in 0..2 {
        let frame = read_frame_timeout(&mut stream, "concurrent finalizer response").await;
        assert_eq!(frame.header.ty, FrameType::Response);
        finalizer_corrs.insert(frame.header.corr);
        let response = tool_response_json(&frame);
        assert!(response["success"].as_bool().unwrap_or(false));
        assert_bg_completion(&response, &task_id);
    }
    assert_eq!(finalizer_corrs, HashSet::from([122, 123]));

    // 1. Overlap: three PureRead calls on one route must all reach dispatch
    // before any is released.
    for corr in 100..103 {
        send_tool_call(&mut stream, 1, corr, "read", json!({ "case": "overlap" })).await;
    }
    state.wait_until("overlap reads started", |inner| inner.overlap_started == 3);
    state.assert_overlap();
    state.release_overlap();
    let overlap_corrs = collect_response_corrs(&mut stream, 3).await;
    assert_eq!(overlap_corrs, HashSet::from([100, 101, 102]));

    // 2. Slow HeavyInit + fast PureRead: a read submitted after the heavy job has
    // started returns before the heavy response.
    send_tool_call(
        &mut stream,
        1,
        200,
        "semantic_search",
        json!({ "case": "heavy" }),
    )
    .await;
    state.wait_until("heavy started", |inner| inner.heavy_started);
    send_tool_call(&mut stream, 1, 201, "read", json!({ "case": "fast" })).await;
    let fast = read_frame_timeout(&mut stream, "fast read response").await;
    assert_eq!(fast.header.ty, FrameType::Response);
    assert_eq!(fast.header.channel, 1);
    assert_eq!(
        fast.header.corr, 201,
        "fast read should beat heavy response"
    );
    assert_eq!(tool_response_json(&fast)["id"], "subc-1-201");
    state.release_heavy();
    let heavy = read_frame_timeout(&mut stream, "heavy response").await;
    assert_eq!(heavy.header.corr, 200);

    // 3. Different roots: route 2 binds while route 1 reads are in flight.
    // It must get its own actor, so its configure can start and ack before
    // route 1's read epoch is released.
    let epoch_base = state.begin_epoch_wave();
    for corr in 300..302 {
        send_tool_call(&mut stream, 1, corr, "read", json!({ "case": "epoch" })).await;
    }
    state.wait_until("epoch reads started", |inner| {
        inner.epoch_started == epoch_base + 2
    });
    send_route_bind(&mut stream, 2, 30, &root2).await;
    let route2_configure = state.wait_for_configure("subc-bind-2");
    assert!(
        route2_configure.epoch_reads_at_start >= 2,
        "different-root configure should not be blocked by route 1 reads"
    );
    expect_route_bind_ack(&mut stream, 30).await;
    state.release_epoch_reads();
    let epoch_corrs = collect_response_corrs(&mut stream, 2).await;
    assert_eq!(epoch_corrs, HashSet::from([300, 301]));

    send_tool_call(&mut stream, 1, 400, "read", json!({ "case": "fast" })).await;
    let route1_read = read_frame_timeout(&mut stream, "route 1 read response").await;
    assert_eq!(route1_read.header.channel, 1);
    assert_eq!(route1_read.header.corr, 400);
    assert_tool_project_root(&route1_read, &root1);
    send_tool_call(&mut stream, 2, 401, "read", json!({ "case": "fast" })).await;
    let route2_read = read_frame_timeout(&mut stream, "route 2 read response").await;
    assert_eq!(route2_read.header.channel, 2);
    assert_eq!(route2_read.header.corr, 401);
    assert_tool_project_root(&route2_read, &root2);

    // L3 PUSH isolation: root-1 emits do not leak to root-2's bound channel.
    send_tool_call(
        &mut stream,
        1,
        402,
        "subc_test_emit_status",
        json!({ "marker": "root1-isolation", "seq": 2 }),
    )
    .await;
    let isolation_pushes =
        expect_status_pushes_for_tool(&mut stream, 402, "root1-isolation", HashSet::from([1]))
            .await;
    assert_eq!(isolation_pushes.len(), 1);
    assert_eq!(push_seq(&isolation_pushes[0]), Some(2));

    // 4. Same root: route 4 binds to root 1 while route 1 reads are in flight.
    // It must reuse the existing actor, so configure cannot start until those
    // reads leave the shared per-root epoch.
    let same_root_epoch_base = state.begin_epoch_wave();
    for corr in 410..412 {
        send_tool_call(&mut stream, 1, corr, "read", json!({ "case": "epoch" })).await;
    }
    state.wait_until("same-root epoch reads started", |inner| {
        inner.epoch_started == same_root_epoch_base + 2
    });
    send_route_bind(&mut stream, 4, 44, &root1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    state.assert_configure_not_started("subc-bind-4");
    state.release_epoch_reads();
    let same_root_corrs = collect_response_corrs(&mut stream, 2).await;
    assert_eq!(same_root_corrs, HashSet::from([410, 411]));
    expect_route_bind_ack(&mut stream, 44).await;
    assert_eq!(
        state.wait_for_configure("subc-bind-4").epoch_reads_at_start,
        0,
        "same-root configure should start only after route 1 reads drain"
    );
    send_tool_call(&mut stream, 4, 420, "read", json!({ "case": "fast" })).await;
    let route4_read = read_frame_timeout(&mut stream, "route 4 read response").await;
    assert_eq!(route4_read.header.channel, 4);
    assert_eq!(route4_read.header.corr, 420);
    assert_tool_project_root(&route4_read, &root1);

    // L3 PUSH multi-channel fan-out: root 1 is bound on routes 1 and 4.
    send_tool_call(
        &mut stream,
        1,
        421,
        "subc_test_emit_status",
        json!({ "marker": "root1-multichannel", "seq": 4 }),
    )
    .await;
    let multichannel_pushes = expect_status_pushes_for_tool(
        &mut stream,
        421,
        "root1-multichannel",
        HashSet::from([1, 4]),
    )
    .await;
    assert_eq!(multichannel_pushes.len(), 2);
    assert!(multichannel_pushes
        .iter()
        .all(|push| push_seq(push) == Some(4)));

    // 5. B3: a new-root configure failure must not install a route and must be
    // removed from the executor before the connection exits. Any Push emitted by
    // that actor before the failed bind is dropped because no channel is bound.
    send_route_bind_with_doc(
        &mut stream,
        5,
        50,
        &failed_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_configure_status_burst": {
                "marker": "failed-no-channel",
                "count": 4,
            },
        }),
    )
    .await;
    expect_route_bind_error(&mut stream, 50, "config_divergence").await;
    assert_no_status_push_for_marker(&mut stream, "failed-no-channel", Duration::from_millis(150))
        .await;
    send_tool_call(&mut stream, 5, 550, "read", json!({ "case": "fast" })).await;
    expect_error_frame(&mut stream, 5, 550, "route_not_bound").await;

    // 6. Per-root maintenance: route 3 triggers a callgraph build and the
    // coalesced maintenance tick drains route 3's actor without collapsing route
    // 2's independent project_root state.
    send_route_bind(&mut stream, 3, 60, &callgraph_root).await;
    expect_route_bind_ack(&mut stream, 60).await;
    send_tool_call(
        &mut stream,
        3,
        500,
        "enable_callgraph_store_for_test",
        json!({}),
    )
    .await;
    let enabled = read_frame_timeout(&mut stream, "enable callgraph response").await;
    assert_eq!(enabled.header.corr, 500);
    assert!(tool_response_json(&enabled)["success"]
        .as_bool()
        .unwrap_or(false));

    aft::context::reset_callgraph_cold_build_spawn_count_for_test();
    let callers_args = json!({
        "file": callgraph_file.to_string_lossy(),
        "symbol": "callee",
        "depth": 1,
    });
    send_tool_call(&mut stream, 3, 501, "callers", callers_args.clone()).await;
    send_tool_call(&mut stream, 3, 502, "callers", callers_args.clone()).await;
    let cold_one = read_frame_timeout(&mut stream, "cold callers response 1").await;
    let cold_two = read_frame_timeout(&mut stream, "cold callers response 2").await;
    let cold_responses = [tool_response_json(&cold_one), tool_response_json(&cold_two)];
    assert_eq!(
        aft::context::callgraph_cold_build_spawn_count_for_test(),
        1,
        "concurrent cold callers through subc must share one background build"
    );
    assert!(
        cold_responses.iter().any(is_callgraph_building),
        "at least one cold callers response should report callgraph_building: {cold_responses:?}"
    );

    let ready = poll_callers_until_ready(&mut stream, 3, 510, callers_args).await;
    assert!(ready["success"].as_bool().unwrap_or(false));
    assert_ne!(
        ready.get("code").and_then(Value::as_str),
        Some("callgraph_building")
    );
    assert!(
        ready["total_callers"].as_u64().unwrap_or(0) >= 1,
        "ready callers response should contain the built result: {ready:?}"
    );
    send_tool_call(&mut stream, 2, 700, "read", json!({ "case": "fast" })).await;
    let route2_after_maintenance =
        read_frame_timeout(&mut stream, "route 2 read after route 3 maintenance").await;
    assert_eq!(route2_after_maintenance.header.channel, 2);
    assert_tool_project_root(&route2_after_maintenance, &root2);

    send_frame(
        &mut stream,
        Frame::build(FrameType::Goodbye, control_flags(), 0, 99, Vec::new())
            .expect("goodbye frame"),
    )
    .await;
}

async fn send_route_bind(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
) {
    send_route_bind_with_doc(
        stream,
        route_channel,
        corr,
        root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
        }),
    )
    .await;
}

async fn send_route_bind_with_doc(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
    doc: Value,
) {
    let request = ModuleControlRequest::RouteBind {
        route_channel,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.to_path_buf(),
            harness: "opencode".to_string(),
            session: format!("session-{route_channel}"),
        },
        config: vec![user_config_tier(doc)],
    };
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            corr,
            serde_json::to_vec(&request).expect("route bind body"),
        )
        .expect("route bind frame"),
    )
    .await;
}

async fn send_tool_call(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
) {
    let body = json!({ "name": name, "arguments": arguments });
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            serde_json::to_vec(&body).expect("tool call body"),
        )
        .expect("tool call frame"),
    )
    .await;
}

async fn send_frame(stream: &mut tokio::net::TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.expect("write frame");
}

async fn read_any_frame_timeout(stream: &mut tokio::net::TcpStream, label: &str) -> Frame {
    tokio::time::timeout(Duration::from_secs(3), read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .expect("read frame")
        .unwrap_or_else(|| panic!("EOF waiting for {label}"))
}

async fn read_frame_timeout(stream: &mut tokio::net::TcpStream, label: &str) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for {label}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for {label}"));
        if frame.header.ty != FrameType::Push {
            return frame;
        }
    }
}

async fn expect_route_bind_ack(stream: &mut tokio::net::TcpStream, corr: u64) {
    let frame = read_frame_timeout(stream, "RouteBindAck").await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    let ack: ModuleControlResponse = serde_json::from_slice(&frame.body).expect("ack body");
    assert_eq!(ack, ModuleControlResponse::RouteBindAck {});
}

async fn expect_route_bind_error(stream: &mut tokio::net::TcpStream, corr: u64, code: &str) {
    expect_error_frame(stream, 0, corr, code).await;
}

async fn expect_error_frame(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    code: &str,
) {
    let frame = read_frame_timeout(stream, "Error frame").await;
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(frame.header.channel, channel);
    assert_eq!(frame.header.corr, corr);
    let body: Value = serde_json::from_slice(&frame.body).expect("error body");
    assert_eq!(body.get("code").and_then(Value::as_str), Some(code));
}

fn push_marker(body: &Value) -> Option<&str> {
    body.get("snapshot")
        .and_then(|snapshot| snapshot.get("marker"))
        .and_then(Value::as_str)
}

fn push_seq(body: &Value) -> Option<u64> {
    body.get("snapshot")
        .and_then(|snapshot| snapshot.get("seq"))
        .and_then(Value::as_u64)
}

async fn expect_status_pushes_for_tool(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    marker: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut response_seen = false;
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while !response_seen || channels != expected_channels {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for push marker {marker}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for push marker {marker}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for push marker {marker}"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_marker(&body) == Some(marker) {
                    channels.insert(frame.header.channel);
                    pushes.push(body);
                }
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "emit tool response should succeed: {response:?}"
                );
                response_seen = true;
            }
            other => panic!("unexpected frame while waiting for push marker {marker}: {other:?}"),
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_status_pushes(
    stream: &mut tokio::net::TcpStream,
    marker: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while channels != expected_channels {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for push marker {marker}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for push marker {marker}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for push marker {marker}"));
        if frame.header.ty != FrameType::Push {
            panic!(
                "unexpected non-Push frame while waiting for marker {marker}: {:?}",
                frame.header.ty
            );
        }
        assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
        let body: Value = serde_json::from_slice(&frame.body).expect("push body");
        if push_marker(&body) == Some(marker) {
            channels.insert(frame.header.channel);
            pushes.push(body);
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn assert_no_status_push_for_marker(
    stream: &mut tokio::net::TcpStream,
    marker: &str,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        match tokio::time::timeout(remaining, read_frame(stream)).await {
            Err(_) => return,
            Ok(Ok(Some(frame))) => {
                if frame.header.ty == FrameType::Push {
                    let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                    assert_ne!(
                        push_marker(&body),
                        Some(marker),
                        "unexpected push marker {marker}: {body:?}"
                    );
                }
            }
            Ok(Ok(None)) => panic!("EOF while checking absence of push marker {marker}"),
            Ok(Err(error)) => {
                panic!("error while checking absence of push marker {marker}: {error}")
            }
        }
    }
}

fn assert_tool_project_root(frame: &Frame, root: &std::path::Path) {
    let response = tool_response_json(frame);
    assert!(
        response["success"].as_bool().unwrap_or(false),
        "expected successful tool response: {response:?}"
    );
    let expected = root.to_string_lossy().into_owned();
    assert_eq!(
        response.get("project_root").and_then(Value::as_str),
        Some(expected.as_str()),
        "tool call should target its bound project root"
    );
}

async fn collect_response_corrs(stream: &mut tokio::net::TcpStream, count: usize) -> HashSet<u64> {
    let mut corrs = HashSet::new();
    for _ in 0..count {
        let frame = read_frame_timeout(stream, "tool response").await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert!(tool_response_json(&frame)["success"]
            .as_bool()
            .unwrap_or(false));
        corrs.insert(frame.header.corr);
    }
    corrs
}

fn tool_response_json(frame: &Frame) -> Value {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let text = body["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    serde_json::from_str(text).expect("embedded AFT response JSON")
}

async fn wait_for_bash_completion(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    task_id: &str,
) -> Value {
    for attempt in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let corr = first_corr + attempt;
        send_tool_call(
            stream,
            channel,
            corr,
            "bash_status",
            json!({ "session_id": "session-1", "params": { "task_id": task_id } }),
        )
        .await;
        let frame = read_frame_timeout(stream, "bash status response").await;
        assert_eq!(frame.header.corr, corr);
        let response = tool_response_json(&frame);
        assert_no_finalizer_fields(&response);
        if response["success"].as_bool() == Some(true)
            && response
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(is_terminal_status)
        {
            return response;
        }
    }
    panic!("background bash task did not complete: {task_id}");
}

fn assert_no_finalizer_fields(response: &Value) {
    assert!(
        response.get("status_bar").is_none(),
        "skip-list response should not carry status_bar: {response:?}"
    );
    assert!(
        response.get("bg_completions").is_none(),
        "skip-list response should not carry bg_completions: {response:?}"
    );
}

fn assert_bg_completion(response: &Value, task_id: &str) {
    let completions = response["bg_completions"]
        .as_array()
        .unwrap_or_else(|| panic!("expected bg_completions on response: {response:?}"));
    assert!(
        completions.iter().any(|completion| {
            completion.get("task_id").and_then(Value::as_str) == Some(task_id)
                && completion.get("status").and_then(Value::as_str) == Some("completed")
                && completion
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains("subc-bg-completion"))
        }),
        "expected completion for task {task_id}, got {completions:?}"
    );
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "timed_out")
}

async fn poll_callers_until_ready(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    arguments: Value,
) -> Value {
    for attempt in 0..30 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let corr = first_corr + attempt;
        send_tool_call(stream, channel, corr, "callers", arguments.clone()).await;
        let frame = read_frame_timeout(stream, "poll callers response").await;
        assert_eq!(frame.header.corr, corr);
        let response = tool_response_json(&frame);
        if response["success"].as_bool() == Some(true) {
            return response;
        }
        assert_eq!(
            response.get("code").and_then(Value::as_str),
            Some("callgraph_building"),
            "callers should build or become ready, got {response:?}"
        );
    }
    panic!("callgraph store did not become ready after maintenance drain ticks");
}

fn is_callgraph_building(response: &Value) -> bool {
    response.get("code").and_then(Value::as_str) == Some("callgraph_building")
}

fn user_config_tier(doc: Value) -> subc_protocol::session::ConfigTier {
    subc_protocol::session::ConfigTier {
        tier: "user".to_string(),
        source: "/tmp/aft-subc-bridge-test-user.jsonc".to_string(),
        doc: doc.to_string(),
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}
