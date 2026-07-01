use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use aft::bash_background::BgTaskStatus;
use aft::config::Config;
use aft::context::{
    AppContext, SemanticIndexStatus, SemanticRefreshEvent, SemanticRefreshRequest,
    SemanticRefreshWorkerSlot,
};
use aft::executor::{Executor, ExecutorConfig, Lane};
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use aft::path_identity::ProjectRootId;
use aft::protocol::{
    BashCompletedFrame, BashLongRunningFrame, ConfigureWarningsFrame, PushFrame, RawRequest,
    Response, StatusChangedFrame,
};
use aft::subc::{run_subc_mode, run_subc_mode_for_test};
use aft::watcher_filter::WatcherDispatchEvent;
use serde_json::{json, Value};
use subc_protocol::manifest::ModuleManifest;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::TcpListener;

static BRIDGE_STATE: OnceLock<Mutex<Option<Arc<BridgeState>>>> = OnceLock::new();
static BRIDGE_TEST_SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

struct FakeDaemonInput {
    listener: TcpListener,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    root1: std::path::PathBuf,
    root2: std::path::PathBuf,
    failed_root: std::path::PathBuf,
    push_burst_root: std::path::PathBuf,
    slow_root: std::path::PathBuf,
    callgraph_root: std::path::PathBuf,
    callgraph_file: std::path::PathBuf,
    state: Arc<BridgeState>,
    executor: Arc<Executor>,
}

struct FakeDaemonSession {
    stream: tokio::net::TcpStream,
    root1: std::path::PathBuf,
    root2: std::path::PathBuf,
    failed_root: std::path::PathBuf,
    push_burst_root: std::path::PathBuf,
    slow_root: std::path::PathBuf,
    callgraph_root: std::path::PathBuf,
    callgraph_file: std::path::PathBuf,
    state: Arc<BridgeState>,
    executor: Arc<Executor>,
}

struct SubcBridgeTestRoots {
    root1: tempfile::TempDir,
    root2: tempfile::TempDir,
    failed_root: tempfile::TempDir,
    push_burst_root: tempfile::TempDir,
    slow_root: tempfile::TempDir,
    callgraph_root: tempfile::TempDir,
    callgraph_file: std::path::PathBuf,
    storage: tempfile::TempDir,
    conn_dir: tempfile::TempDir,
}

impl SubcBridgeTestRoots {
    fn new() -> Self {
        let root1 = tempfile::tempdir().expect("root1 tempdir");
        let root2 = tempfile::tempdir().expect("root2 tempdir");
        let failed_root = tempfile::tempdir().expect("failed root tempdir");
        let push_burst_root = tempfile::tempdir().expect("push burst root tempdir");
        let slow_root = tempfile::tempdir().expect("slow root tempdir");
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

        Self {
            root1,
            root2,
            failed_root,
            push_burst_root,
            slow_root,
            callgraph_root,
            callgraph_file,
            storage,
            conn_dir,
        }
    }
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
    slow_configure_started: usize,
    slow_configure_finished: usize,
    slow_configure_release: bool,
    epoch_started: usize,
    epoch_current: usize,
    epoch_release: bool,
    deferred_push_started: usize,
    deferred_push_release: bool,
    configure_events: Vec<ConfigureEvent>,
    watcher_senders: Vec<crossbeam_channel::Sender<WatcherDispatchEvent>>,
    semantic_refresh_event_senders: Vec<crossbeam_channel::Sender<SemanticRefreshEvent>>,
}

fn bridge_state_slot() -> &'static Mutex<Option<Arc<BridgeState>>> {
    BRIDGE_STATE.get_or_init(|| Mutex::new(None))
}

fn bridge_test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    BRIDGE_TEST_SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn set_test_foreground_wait_ms(ms: u64) -> EnvVarGuard {
    let key = "AFT_TEST_FOREGROUND_WAIT_MS";
    let previous = std::env::var_os(key);
    std::env::set_var(key, ms.to_string());
    EnvVarGuard { key, previous }
}

fn set_test_force_bash_promote_error() -> EnvVarGuard {
    let key = "AFT_TEST_FORCE_SUBC_BASH_PROMOTE_ERROR";
    let previous = std::env::var_os(key);
    std::env::set_var(key, "1");
    EnvVarGuard { key, previous }
}

fn set_test_force_bash_promote_panic() -> EnvVarGuard {
    let key = "AFT_TEST_FORCE_SUBC_BASH_PROMOTE_ERROR";
    let previous = std::env::var_os(key);
    std::env::set_var(key, "panic");
    EnvVarGuard { key, previous }
}

fn install_bridge_state(state: Arc<BridgeState>) {
    let mut guard = bridge_state_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(state);
}

fn clear_bridge_state() {
    let mut guard = bridge_state_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

fn current_bridge_state() -> Arc<BridgeState> {
    bridge_state_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .expect("bridge state installed")
        .clone()
}

impl BridgeState {
    fn wait_until(&self, label: &str, mut predicate: impl FnMut(&BridgeInner) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
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

    fn begin_slow_configure_wave(&self) -> usize {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.slow_configure_release = false;
        guard.slow_configure_started
    }

    fn release_slow_configures(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.slow_configure_release = true;
        self.cv.notify_all();
    }

    fn wait_for_slow_configure_finished(&self, expected: usize) {
        self.wait_until("slow configure finished", |inner| {
            inner.slow_configure_finished >= expected
        });
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

    fn begin_deferred_push_wave(&self) -> usize {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.deferred_push_release = false;
        guard.deferred_push_started
    }

    fn release_deferred_pushes(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.deferred_push_release = true;
        self.cv.notify_all();
    }

    fn wait_for_deferred_push_release(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.deferred_push_started += 1;
        self.cv.notify_all();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !guard.deferred_push_release {
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for deferred push release"
            );
            let remaining = deadline.saturating_duration_since(now);
            let (next, result) = self
                .cv
                .wait_timeout(guard, remaining)
                .expect("bridge state condvar");
            guard = next;
            assert!(
                !result.timed_out() || guard.deferred_push_release,
                "timed out waiting for deferred push release"
            );
        }
    }

    fn retain_watcher_sender(&self, sender: crossbeam_channel::Sender<WatcherDispatchEvent>) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.watcher_senders.push(sender);
    }

    fn retain_semantic_refresh_event_sender(
        &self,
        sender: crossbeam_channel::Sender<SemanticRefreshEvent>,
    ) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.semantic_refresh_event_senders.push(sender);
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
            let deadline = Instant::now() + Duration::from_secs(30);
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
            let deadline = Instant::now() + Duration::from_secs(30);
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

    fn slow_configure(&self) {
        let mut guard = self.inner.lock().expect("bridge state lock");
        guard.slow_configure_started += 1;
        self.cv.notify_all();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !guard.slow_configure_release {
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for slow configure release"
            );
            let remaining = deadline.saturating_duration_since(now);
            let (next, result) = self
                .cv
                .wait_timeout(guard, remaining)
                .expect("bridge state condvar");
            guard = next;
            assert!(
                !result.timed_out() || guard.slow_configure_release,
                "timed out waiting for slow configure release"
            );
        }
        guard.slow_configure_finished += 1;
        self.cv.notify_all();
    }

    fn epoch_read(&self, id: String) -> Response {
        {
            let mut guard = self.inner.lock().expect("bridge state lock");
            guard.epoch_started += 1;
            guard.epoch_current += 1;
            self.cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(30);
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
            session_id: None,
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

fn bash_completed_push(task_id: &str, session_id: &str) -> PushFrame {
    PushFrame::BashCompleted(BashCompletedFrame {
        frame_type: "bash_completed",
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        status: BgTaskStatus::Completed,
        exit_code: Some(0),
        command: format!("echo {task_id}"),
        output_preview: String::new(),
        output_truncated: false,
        original_tokens: None,
        compressed_tokens: None,
        tokens_skipped: false,
    })
}

fn bash_long_running_push(task_id: &str, session_id: &str) -> PushFrame {
    PushFrame::BashLongRunning(BashLongRunningFrame {
        frame_type: "bash_long_running",
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        command: format!("sleep {task_id}"),
        elapsed_ms: 1_000,
    })
}

fn emit_push_frame(ctx: &AppContext, frame: PushFrame) -> bool {
    if let Some(sender) = ctx.progress_sender_handle() {
        sender(frame);
        true
    } else {
        false
    }
}

fn defer_push_frame(state: Arc<BridgeState>, ctx: &AppContext, frame: PushFrame) -> bool {
    let Some(sender) = ctx.progress_sender_handle() else {
        return false;
    };
    thread::spawn(move || {
        state.wait_for_deferred_push_release();
        sender(frame);
    });
    true
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

fn configure_bash_completed_task(req: &RawRequest) -> Option<String> {
    let tiers = req.params.get("config")?.as_array()?;
    for tier in tiers {
        let Some(doc) = tier.get("doc").and_then(Value::as_str) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(doc) else {
            continue;
        };
        let Some(spec) = doc.get("subc_test_configure_bash_completed") else {
            continue;
        };
        return Some(
            spec.get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("subc-configure-completed")
                .to_string(),
        );
    }
    None
}

fn emit_configure_bash_completed_if_requested(req: &RawRequest, ctx: &AppContext) {
    if let Some(task_id) = configure_bash_completed_task(req) {
        let session_id = req.session().to_string();
        emit_push_frame(ctx, bash_completed_push(&task_id, &session_id));
    }
}

fn configure_warning_message(req: &RawRequest) -> Option<String> {
    let tiers = req.params.get("config")?.as_array()?;
    for tier in tiers {
        let Some(doc) = tier.get("doc").and_then(Value::as_str) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(doc) else {
            continue;
        };
        let Some(spec) = doc.get("subc_test_configure_warning") else {
            continue;
        };
        return Some(
            spec.get("message")
                .and_then(Value::as_str)
                .unwrap_or("subc maintenance configure warning")
                .to_string(),
        );
    }
    None
}

fn slow_configure_requested(req: &RawRequest) -> bool {
    let Some(tiers) = req.params.get("config").and_then(Value::as_array) else {
        return false;
    };
    tiers.iter().any(|tier| {
        let Some(doc) = tier.get("doc").and_then(Value::as_str) else {
            return false;
        };
        let Ok(doc) = serde_json::from_str::<Value>(doc) else {
            return false;
        };
        doc.get("subc_test_slow_configure")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
}

fn enqueue_configure_warning_if_requested(req: &RawRequest, ctx: &AppContext) {
    let Some(message) = configure_warning_message(req) else {
        return;
    };
    let project_root = ctx
        .config()
        .project_root
        .as_ref()
        .map(|root| root.to_string_lossy().into_owned())
        .unwrap_or_default();
    let frame = ConfigureWarningsFrame::new_with_session_id(
        aft::log_ctx::current_session(),
        project_root,
        1,
        vec![json!({
            "code": "subc_test_configure_warning",
            "message": message,
        })],
    );
    ctx.configure_warnings_sender()
        .send((ctx.configure_generation(), frame))
        .expect("queue configure warning");
}

fn enqueue_watcher_event_for_test(
    req: &RawRequest,
    ctx: &AppContext,
    state: &BridgeState,
) -> Response {
    let Some(root) = ctx.config().project_root.clone() else {
        return Response::error(
            &req.id,
            "missing_project_root",
            "subc watcher test requires a configured project root",
        );
    };
    let path = root.join("subc_watcher_tick.rs");
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return Response::error(
                &req.id,
                "watcher_test_setup_failed",
                format!("failed to create watcher test dir: {error}"),
            );
        }
    }
    if let Err(error) = std::fs::write(&path, "pub fn subc_watcher_tick() {}\n") {
        return Response::error(
            &req.id,
            "watcher_test_setup_failed",
            format!("failed to write watcher test file: {error}"),
        );
    }

    // Seed visible Tier-2 counts so the watcher drain has an observable stale-bit
    // transition to emit as a StatusChanged Push.
    ctx.update_status_bar_tier2(Some(31), Some(32), Some(33), Some(34), false);
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    tx.send(WatcherDispatchEvent::Paths(vec![path.clone()]))
        .expect("queue watcher event");
    state.retain_watcher_sender(tx);

    Response::success(
        &req.id,
        json!({ "queued": true, "path": path.to_string_lossy() }),
    )
}

fn enqueue_semantic_refresh_event_for_test(
    req: &RawRequest,
    ctx: &AppContext,
    state: &BridgeState,
) -> Response {
    let Some(root) = ctx.config().project_root.clone() else {
        return Response::error(
            &req.id,
            "missing_project_root",
            "subc semantic refresh test requires a configured project root",
        );
    };
    let path = root.join("subc_semantic_refresh_tick.rs");
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return Response::error(
                &req.id,
                "semantic_refresh_test_setup_failed",
                format!("failed to create semantic refresh test dir: {error}"),
            );
        }
    }
    if let Err(error) = std::fs::write(&path, "pub fn subc_semantic_refresh_tick() {}\n") {
        return Response::error(
            &req.id,
            "semantic_refresh_test_setup_failed",
            format!("failed to write semantic refresh test file: {error}"),
        );
    }

    let (request_tx, _request_rx) = crossbeam_channel::unbounded::<SemanticRefreshRequest>();
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<SemanticRefreshEvent>();
    let worker_slot: SemanticRefreshWorkerSlot = Arc::new(Mutex::new(None));
    ctx.install_semantic_refresh_worker(request_tx, event_rx, worker_slot);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
    event_tx
        .send(SemanticRefreshEvent::Started {
            paths: vec![path.clone()],
        })
        .expect("queue semantic refresh event");
    state.retain_semantic_refresh_event_sender(event_tx);

    Response::success(
        &req.id,
        json!({ "queued": true, "path": path.to_string_lossy() }),
    )
}

fn bridge_dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    let state = current_bridge_state();
    match req.command.as_str() {
        "configure" => {
            state.configure(&req, ctx);
            if slow_configure_requested(&req) {
                state.slow_configure();
            }
            if req.id == "subc-bind-5" {
                emit_configure_status_burst_if_requested(&req, ctx);
                return Response::error(
                    &req.id,
                    "config_divergence",
                    "intentional test configure failure",
                );
            }
            let response = configure_bridge_context(&req, ctx);
            enqueue_configure_warning_if_requested(&req, ctx);
            emit_configure_status_burst_if_requested(&req, ctx);
            emit_configure_bash_completed_if_requested(&req, ctx);
            response
        }
        // `echo` is the executor-mechanics vehicle: a transport primitive that is
        // PureRead-lane (so reader-concurrency scenarios get the reader pool) and
        // has NO subc translate arm, so the translator passes it through verbatim
        // (falling through `unsupported_tool` to this stub dispatch with raw args).
        // The `case` arg drives which synthetic scenario the fake daemon plays.
        // We deliberately use a transport primitive, NOT an agent tool (read/glob/
        // grep/…): agent tools go through arg translation + formatting (tested in
        // isolation in subc_translate_test.rs), and — as the B-track cutover of
        // `glob` to a translated tool showed — squatting on a real agent-tool name
        // breaks the moment that tool is cut over. `echo` can never be an agent
        // tool, so this vehicle is immune to future cutovers.
        "echo" => match req.params.get("case").and_then(Value::as_str) {
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
        "bash_drain_completions" => aft::commands::bash_drain_completions::handle(&req, ctx),
        "bash_ack_completions" => aft::commands::bash_drain_completions::handle_ack(&req, ctx),
        "read" => aft::commands::read::handle_read(&req, ctx),
        "write" => aft::commands::write::handle_write(&req, ctx),
        "apply_patch" => aft::commands::apply_patch::handle_apply_patch(&req, ctx),
        "delete_file" => aft::commands::delete_file::handle_delete_file(&req, ctx),
        "move_file" => aft::commands::move_file::handle_move_file(&req, ctx),
        "add_import" => aft::commands::add_import::handle_add_import(&req, ctx),
        "remove_import" => aft::commands::remove_import::handle_remove_import(&req, ctx),
        "organize_imports" => aft::commands::organize_imports::handle_organize_imports(&req, ctx),
        "glob" => aft::commands::glob::handle_glob(&req, ctx),
        "grep" => aft::commands::grep::handle_grep(&req, ctx),
        "outline" => aft::commands::outline::handle_outline(&req, ctx),
        "zoom" => aft::commands::zoom::handle_zoom(&req, ctx),
        "call_tree" => aft::commands::call_tree::handle_call_tree(&req, ctx),
        "trace_to" => aft::commands::trace_to::handle_trace_to(&req, ctx),
        "trace_to_symbol" => aft::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        "trace_data" => aft::commands::trace_data::handle_trace_data(&req, ctx),
        "git_conflicts" => aft::commands::conflicts::handle_git_conflicts(ctx, &req),
        "ast_search" => aft::commands::ast_search::handle_ast_search(&req, ctx),
        "ast_replace" => aft::commands::ast_replace::handle_ast_replace(&req, ctx),
        "move_symbol" => aft::commands::move_symbol::handle_move_symbol(&req, ctx),
        "extract_function" => aft::commands::extract_function::handle_extract_function(&req, ctx),
        "inline_symbol" => aft::commands::inline_symbol::handle_inline_symbol(&req, ctx),
        "undo" => aft::commands::undo::handle_undo(&req, ctx),
        "edit_history" => aft::commands::edit_history::handle_edit_history(&req, ctx),
        "checkpoint" => aft::commands::checkpoint::handle_checkpoint(&req, ctx),
        "restore_checkpoint" => {
            aft::commands::restore_checkpoint::handle_restore_checkpoint(&req, ctx)
        }
        "list_checkpoints" => aft::commands::list_checkpoints::handle_list_checkpoints(&req, ctx),
        "semantic_search" => state.heavy(req.id),
        "subc_test_mutating_internal_error" => Response::error(
            req.id,
            "internal_error",
            "intentional mutating internal error",
        ),
        "subc_test_mutating_panic" => panic!("intentional mutating panic"),
        "subc_test_echo_session" => {
            let session = req.session().to_string();
            Response::success(req.id, json!({ "transport_session": session }))
        }
        "subc_test_validate_path" => {
            let Some(path) = req.params.get("path").and_then(Value::as_str) else {
                return Response::error(req.id, "invalid_request", "missing path");
            };
            match ctx.validate_path(&req.id, std::path::Path::new(path)) {
                Ok(validated) => {
                    Response::success(req.id, json!({ "path": validated.to_string_lossy() }))
                }
                Err(response) => response,
            }
        }
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
        "subc_test_emit_bash_completed" => {
            let task_id = req
                .params
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("subc-test-completed")
                .to_string();
            let session_id = req
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| req.session())
                .to_string();
            let emitted = emit_push_frame(ctx, bash_completed_push(&task_id, &session_id));
            Response::success(
                req.id,
                json!({ "emitted": emitted, "task_id": task_id, "session_id": session_id }),
            )
        }
        "subc_test_emit_bash_completed_then_long_running" => {
            let task_id = req
                .params
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("subc-test-stale-long-running")
                .to_string();
            let session_id = req
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| req.session())
                .to_string();
            let completed = emit_push_frame(ctx, bash_completed_push(&task_id, &session_id));
            let long_running = emit_push_frame(ctx, bash_long_running_push(&task_id, &session_id));
            Response::success(
                req.id,
                json!({
                    "completed_emitted": completed,
                    "long_running_emitted": long_running,
                    "task_id": task_id,
                    "session_id": session_id,
                }),
            )
        }
        "subc_test_defer_bash_completed" => {
            let task_id = req
                .params
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("subc-test-deferred-completed")
                .to_string();
            let session_id = req
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| req.session())
                .to_string();
            let emitted = defer_push_frame(
                Arc::clone(&state),
                ctx,
                bash_completed_push(&task_id, &session_id),
            );
            Response::success(
                req.id,
                json!({ "deferred": emitted, "task_id": task_id, "session_id": session_id }),
            )
        }
        "subc_test_defer_bash_long_running" => {
            let task_id = req
                .params
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("subc-test-deferred-long-running")
                .to_string();
            let session_id = req
                .params
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| req.session())
                .to_string();
            let emitted = defer_push_frame(
                Arc::clone(&state),
                ctx,
                bash_long_running_push(&task_id, &session_id),
            );
            Response::success(
                req.id,
                json!({ "deferred": emitted, "task_id": task_id, "session_id": session_id }),
            )
        }
        "subc_test_enqueue_watcher_event" => enqueue_watcher_event_for_test(&req, ctx, &state),
        "subc_test_enqueue_semantic_refresh_event" => {
            enqueue_semantic_refresh_event_for_test(&req, ctx, &state)
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

fn run_subc_bridge_test<F, Fut, A>(name: &'static str, watchdog: Duration, driver: F, after: A)
where
    F: FnOnce(FakeDaemonInput) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
    A: FnOnce(&Arc<BridgeState>, &Arc<Executor>, &SubcBridgeTestRoots),
{
    run_subc_bridge_test_with_env(name, watchdog, Vec::new, driver, after);
}

/// Like [`run_subc_bridge_test`], but installs the process-global test env vars
/// that control foreground wait windows and forced promote errors or panics AFTER
/// acquiring the serial guard, so the returned [`EnvVarGuard`]s live entirely
/// inside the single-test critical section. Setting these env guards in the test body
/// (before this fn takes the lock) races other bash tests under parallel CI:
/// two tests can clobber each other's process-global wait window in the gap
/// between guard creation and lock acquisition, which intermittently flipped
/// `subc_bridge_bash_promote_failure_is_normal_tool_error` to success.
fn run_subc_bridge_test_with_env<E, F, Fut, A>(
    name: &'static str,
    watchdog: Duration,
    env_setup: E,
    driver: F,
    after: A,
) where
    E: FnOnce() -> Vec<EnvVarGuard>,
    F: FnOnce(FakeDaemonInput) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
    A: FnOnce(&Arc<BridgeState>, &Arc<Executor>, &SubcBridgeTestRoots),
{
    run_subc_bridge_test_inner(name, watchdog, env_setup, driver, after, true);
}

fn run_subc_bridge_production_test<F, Fut, A>(
    name: &'static str,
    watchdog: Duration,
    driver: F,
    after: A,
) where
    F: FnOnce(FakeDaemonInput) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
    A: FnOnce(&Arc<BridgeState>, &Arc<Executor>, &SubcBridgeTestRoots),
{
    run_subc_bridge_test_inner(name, watchdog, Vec::new, driver, after, false);
}

fn run_subc_bridge_test_inner<E, F, Fut, A>(
    name: &'static str,
    watchdog: Duration,
    env_setup: E,
    driver: F,
    after: A,
    allow_native_passthrough: bool,
) where
    E: FnOnce() -> Vec<EnvVarGuard>,
    F: FnOnce(FakeDaemonInput) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
    A: FnOnce(&Arc<BridgeState>, &Arc<Executor>, &SubcBridgeTestRoots),
{
    let _serial = bridge_test_serial_guard();
    let _env_guards = env_setup();
    let state = Arc::new(BridgeState::default());
    install_bridge_state(Arc::clone(&state));

    let roots = SubcBridgeTestRoots::new();
    let conn_path = roots.conn_dir.path().join("subc-connection.json");

    let ctx = Arc::new(AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            storage_dir: Some(roots.storage.path().to_path_buf()),
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
    let executor_for_daemon = Arc::clone(&executor);
    let executor_for_check = Arc::clone(&executor);

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
    let root1_path = roots.root1.path().to_path_buf();
    let root2_path = roots.root2.path().to_path_buf();
    let failed_root_path = roots.failed_root.path().to_path_buf();
    let push_burst_root_path = roots.push_burst_root.path().to_path_buf();
    let slow_root_path = roots.slow_root.path().to_path_buf();
    let callgraph_root_path = roots.callgraph_root.path().to_path_buf();
    let callgraph_file_path = roots.callgraph_file.clone();
    let daemon = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake daemon runtime");
        runtime.block_on(async move {
            let listener = TcpListener::from_std(std_listener).expect("tokio listener");
            tokio::time::timeout(
                watchdog,
                driver(FakeDaemonInput {
                    listener,
                    key,
                    daemon_id,
                    root1: root1_path,
                    root2: root2_path,
                    failed_root: failed_root_path,
                    push_burst_root: push_burst_root_path,
                    slow_root: slow_root_path,
                    callgraph_root: callgraph_root_path,
                    callgraph_file: callgraph_file_path,
                    state: daemon_state,
                    executor: executor_for_daemon,
                }),
            )
            .await
            .unwrap_or_else(|_| panic!("{name} fake daemon watchdog"));
        });
    });

    // Inject a hermetic (nonexistent) user config path so the W5 local read
    // never touches a real ~/.config/cortexkit/aft.jsonc on the dev/CI machine.
    let user_config_path = roots.storage.path().join("nonexistent-user-aft.jsonc");
    let run_result = if allow_native_passthrough {
        run_subc_mode_for_test(
            &conn_path,
            ctx,
            executor,
            bridge_dispatch,
            Some(user_config_path),
        )
    } else {
        run_subc_mode(
            &conn_path,
            ctx,
            executor,
            bridge_dispatch,
            Some(user_config_path),
        )
    };
    let join_result = daemon.join();
    clear_bridge_state();

    run_result.expect("subc mode exits cleanly");
    join_result.expect("fake daemon joins");
    after(&state, &executor_for_check, &roots);
}

#[test]
fn subc_bridge_core_routing_reuses_same_root_actor_and_allows_different_roots() {
    run_subc_bridge_test(
        "subc_bridge_core_routing_reuses_same_root_actor_and_allows_different_roots",
        Duration::from_secs(45),
        drive_core_routing_daemon,
        |state, _, roots| {
            state.assert_overlap();
            state.assert_configure_root("subc-bind-1", roots.root1.path());
            state.assert_configure_root("subc-bind-2", roots.root2.path());
            state.assert_configure_root("subc-bind-4", roots.root1.path());
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
        },
    );
}

#[test]
fn subc_bridge_configure_warning_pushes_are_session_scoped() {
    run_subc_bridge_test(
        "subc_bridge_configure_warning_pushes_are_session_scoped",
        Duration::from_secs(30),
        drive_configure_warning_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_semantic_refresh_maintenance_push() {
    run_subc_bridge_test(
        "subc_bridge_semantic_refresh_maintenance_push",
        Duration::from_secs(30),
        drive_semantic_refresh_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_watcher_stale_maintenance_push() {
    run_subc_bridge_test(
        "subc_bridge_watcher_stale_maintenance_push",
        Duration::from_secs(30),
        drive_watcher_stale_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_tool_calls_carry_route_bind_session() {
    run_subc_bridge_test(
        "subc_bridge_tool_calls_carry_route_bind_session",
        Duration::from_secs(30),
        drive_route_bind_session_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_l3_fanout_seq_ordering() {
    run_subc_bridge_test(
        "subc_bridge_l3_fanout_seq_ordering",
        Duration::from_secs(30),
        drive_l3_fanout_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_routebind_nonblocking_slow_configure() {
    run_subc_bridge_test(
        "subc_bridge_routebind_nonblocking_slow_configure",
        Duration::from_secs(30),
        drive_routebind_nonblocking_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_duplicate_routebind_rejects_in_flight_bind() {
    run_subc_bridge_test(
        "subc_bridge_duplicate_routebind_rejects_in_flight_bind",
        Duration::from_secs(30),
        drive_duplicate_routebind_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_tool_call_before_bind_ack_is_route_not_bound() {
    run_subc_bridge_test(
        "subc_bridge_tool_call_before_bind_ack_is_route_not_bound",
        Duration::from_secs(30),
        drive_pending_bind_tool_call_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_goodbye_cancels_pending_bind() {
    run_subc_bridge_test(
        "subc_bridge_goodbye_cancels_pending_bind",
        Duration::from_secs(30),
        drive_goodbye_cancels_pending_bind_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_l3_coalesces_already_bound_route_burst() {
    run_subc_bridge_test(
        "subc_bridge_l3_coalesces_already_bound_route_burst",
        Duration::from_secs(30),
        drive_l3_coalescing_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_lossy_pressure_reliable_completion_still_delivers() {
    run_subc_bridge_test(
        "subc_bridge_lossy_pressure_reliable_completion_still_delivers",
        // This test drives a 2048-frame status BURST (2x the lossy channel
        // capacity) to exercise the pressure path, then waits through it for the
        // coalesced latest. Generating + coalescing 2048 frames is genuinely
        // load-proportional, so the watchdog (and the inner push-wait deadlines
        // below) carry extra headroom for contended CI runners where the suite
        // runs ~5x slower (memory: loaded macOS/Windows runner). The inner
        // expect_*_pushes deadlines are 90s, so the outer watchdog must exceed
        // them.
        Duration::from_secs(150),
        drive_lossy_pressure_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_response_finalizer_status_bar_and_bg_completion_once_per_epoch() {
    run_subc_bridge_test(
        "subc_bridge_response_finalizer_status_bar_and_bg_completion_once_per_epoch",
        Duration::from_secs(60),
        drive_response_finalizer_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_session_scoped_bg_completion_and_push_isolation() {
    run_subc_bridge_test(
        "subc_bridge_session_scoped_bg_completion_and_push_isolation",
        Duration::from_secs(60),
        drive_session_scoped_bg_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bg_events_idle_completion_wake_lane() {
    run_subc_bridge_test(
        "subc_bridge_bg_events_idle_completion_wake_lane",
        Duration::from_secs(90),
        drive_bg_events_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_principal_trust_enforces_restrict_and_bash_deny() {
    run_subc_bridge_test(
        "subc_bridge_principal_trust_enforces_restrict_and_bash_deny",
        Duration::from_secs(60),
        drive_principal_trust_enforcement_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_untrusted_bind_cannot_observe_trusted_bash() {
    run_subc_bridge_test(
        "subc_bridge_untrusted_bind_cannot_observe_trusted_bash",
        Duration::from_secs(90),
        drive_cross_bind_trust_isolation_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_reliable_replay_and_lossy_drop_for_detached_session() {
    run_subc_bridge_test(
        "subc_bridge_reliable_replay_and_lossy_drop_for_detached_session",
        Duration::from_secs(45),
        drive_detached_session_replay_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_failed_new_root_bind_rolls_back_actor_and_drops_pre_ack_push() {
    run_subc_bridge_test(
        "subc_bridge_failed_new_root_bind_rolls_back_actor_and_drops_pre_ack_push",
        Duration::from_secs(30),
        drive_failed_new_root_daemon,
        |_, executor, roots| {
            let failed_root_id =
                ProjectRootId::from_path(roots.failed_root.path()).expect("failed root id");
            let actor_check = executor.submit(
                failed_root_id,
                Lane::PureRead,
                "b3-actor-check".to_string(),
                Box::new(|_| Response::success("b3-actor-check", json!({ "unexpected": true }))),
            );
            let actor_check_response = actor_check
                .recv_timeout(Duration::from_secs(30))
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
        },
    );
}

#[test]
fn subc_bridge_callgraph_maintenance_is_per_root() {
    run_subc_bridge_test(
        "subc_bridge_callgraph_maintenance_is_per_root",
        Duration::from_secs(60),
        drive_callgraph_maintenance_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_new_manifest_tools_route_in_production() {
    run_subc_bridge_production_test(
        "subc_bridge_new_manifest_tools_route_in_production",
        Duration::from_secs(90),
        drive_manifest_reachability_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_mutating_internal_error_is_not_fatal_teardown() {
    run_subc_bridge_test(
        "subc_bridge_mutating_internal_error_is_not_fatal_teardown",
        Duration::from_secs(30),
        drive_mutating_internal_error_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_mutating_panic_triggers_fatal_teardown() {
    run_subc_bridge_test(
        "subc_bridge_mutating_panic_triggers_fatal_teardown",
        Duration::from_secs(30),
        drive_mutating_panic_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_pure_read_actor_fatal_response_triggers_fatal_teardown() {
    run_subc_bridge_test(
        "subc_bridge_pure_read_actor_fatal_response_triggers_fatal_teardown",
        Duration::from_secs(30),
        drive_pure_read_actor_fatal_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_fast_foreground_returns_terminal_response() {
    run_subc_bridge_test(
        "subc_bridge_bash_fast_foreground_returns_terminal_response",
        Duration::from_secs(30),
        drive_bash_fast_foreground_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_promotes_after_wait_window_and_remains_tracked() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_promotes_after_wait_window_and_remains_tracked",
        Duration::from_secs(30),
        || vec![set_test_foreground_wait_ms(200)],
        drive_bash_promotion_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_block_to_completion_waits_for_terminal() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_block_to_completion_waits_for_terminal",
        Duration::from_secs(30),
        || vec![set_test_foreground_wait_ms(100)],
        drive_bash_block_to_completion_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_background_returns_launch_text() {
    run_subc_bridge_test(
        "subc_bridge_bash_background_returns_launch_text",
        Duration::from_secs(30),
        drive_bash_background_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_nonzero_exit_renders_exit_code() {
    run_subc_bridge_test(
        "subc_bridge_bash_nonzero_exit_renders_exit_code",
        Duration::from_secs(30),
        drive_bash_nonzero_exit_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_wait_holds_no_executor_lane() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_wait_holds_no_executor_lane",
        Duration::from_secs(45),
        || vec![set_test_foreground_wait_ms(5_000)],
        drive_bash_lane_nonoccupancy_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_route_close_cancels_deferred_wait() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_route_close_cancels_deferred_wait",
        Duration::from_secs(45),
        || vec![set_test_foreground_wait_ms(5_000)],
        drive_bash_route_close_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_promote_failure_is_normal_tool_error() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_promote_failure_is_normal_tool_error",
        Duration::from_secs(45),
        || {
            vec![
                set_test_foreground_wait_ms(200),
                set_test_force_bash_promote_error(),
            ]
        },
        drive_bash_promote_failure_daemon,
        |_, _, _| {},
    );
}

#[test]
fn subc_bridge_bash_promote_panic_triggers_fatal_teardown() {
    run_subc_bridge_test_with_env(
        "subc_bridge_bash_promote_panic_triggers_fatal_teardown",
        Duration::from_secs(45),
        || {
            vec![
                set_test_foreground_wait_ms(200),
                set_test_force_bash_promote_panic(),
            ]
        },
        drive_bash_promote_panic_daemon,
        |_, _, _| {},
    );
}

/// Flips true if the S1 attacker's `configure` payload ever reaches `dispatch`.
/// It must stay false: a forwarded non-manifest `configure` must be rejected by
/// the fail-closed gate BEFORE building a RawRequest, so the attacker's tiers
/// never reach `handle_configure` (which would bypass the W5 RouteBind cap).
static S1_ATTACKER_CONFIG_REACHED_DISPATCH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
const S1_ATTACKER_MARKER: &str = "S1_ATTACKER_SECRET_ENV";

fn s1_guard_dispatch(req: RawRequest, _ctx: &AppContext) -> Response {
    // The RouteBind reconcile dispatches a clean `configure` (no marker); only a
    // forwarded tool-call `configure` would carry the marker. If the marker ever
    // arrives here, the fail-closed gate failed and the cap was bypassed.
    if req.command == "configure" {
        let raw = serde_json::to_string(&req.params).unwrap_or_default();
        if raw.contains(S1_ATTACKER_MARKER) {
            S1_ATTACKER_CONFIG_REACHED_DISPATCH.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    Response::success(req.id, json!({}))
}

/// Security regression for audit finding S1: in production mode an `mcp:*` front
/// that gets subc to forward a non-manifest tool call named `configure` must be
/// REJECTED with `unknown_tool` before reaching dispatch — never allowed to
/// reconcile attacker-controlled config tiers (which would bypass the W5
/// RouteBind config-trust cap). This drives the PRODUCTION `run_subc_mode`
/// (passthrough off), unlike the mega-test which uses `run_subc_mode_for_test`.
#[test]
fn subc_rejects_forwarded_configure_tool_call_in_production() {
    S1_ATTACKER_CONFIG_REACHED_DISPATCH.store(false, std::sync::atomic::Ordering::SeqCst);

    let root = tempfile::tempdir().expect("root tempdir");
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

    let root_path = root.path().to_path_buf();
    let daemon = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fake daemon runtime");
        runtime.block_on(async move {
            tokio::time::timeout(
                Duration::from_secs(120),
                drive_s1_rejection_daemon(std_listener, key, daemon_id, root_path),
            )
            .await
            .expect("s1 rejection daemon watchdog");
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
        pool_size: 2,
        read_cap: 2,
        actor_cap: 2,
        heavy_permits: 1,
        drr_quantum: 1,
    }));
    let user_config_path = storage.path().join("nonexistent-user-aft.jsonc");

    // PRODUCTION entry — passthrough is OFF.
    run_subc_mode(
        &conn_path,
        ctx,
        executor,
        s1_guard_dispatch,
        Some(user_config_path),
    )
    .expect("subc mode exits cleanly");
    daemon.join().expect("s1 rejection daemon joins");

    assert!(
        !S1_ATTACKER_CONFIG_REACHED_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
        "forwarded `configure` tool call reached dispatch — the fail-closed gate did not reject it"
    );

    drop(root);
    drop(storage);
}

async fn drive_s1_rejection_daemon(
    std_listener: StdTcpListener,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    root: std::path::PathBuf,
) {
    let listener = TcpListener::from_std(std_listener).expect("tokio listener");
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
                storage: None,
            })
            .expect("hello ack body"),
        )
        .expect("hello ack frame"),
    )
    .await;

    // An mcp:* front binds a route with a CLEAN config (no marker).
    let bind = ModuleControlRequest::RouteBind {
        route_channel: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.clone(),
            harness: "mcp:generic".to_string(),
            session: "s1-session".to_string(),
        },
        principal: Some(Principal::Direct),
    };
    send_frame(
        &mut stream,
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            1,
            serde_json::to_vec(&bind).expect("route bind body"),
        )
        .expect("route bind frame"),
    )
    .await;
    expect_route_bind_ack(&mut stream, 1).await;

    // The attack: forward a non-manifest `configure` tool call carrying an inline
    // user-tier privileged field. If dispatched, it would reach handle_configure
    // and bypass the W5 cap entirely.
    send_tool_call(
        &mut stream,
        1,
        100,
        "configure",
        json!({
            "config": [{
                "tier": "user",
                "source": "wire",
                "doc": format!("{{ \"semantic\": {{ \"api_key_env\": \"{S1_ATTACKER_MARKER}\" }} }}"),
            }],
        }),
    )
    .await;

    // The fail-closed gate must answer with an `unknown_tool` error, NOT execute.
    let response = read_frame_timeout(&mut stream, "rejected configure response").await;
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "must be a tool Response frame"
    );
    assert_eq!(response.header.channel, 1);
    assert_eq!(response.header.corr, 100);
    let body: Value = serde_json::from_slice(&response.body).expect("tool result body");
    assert_eq!(
        body["isError"].as_bool(),
        Some(true),
        "rejected tool call must be isError: {body:?}"
    );
    let inner = tool_response_json(&response);
    assert_eq!(
        inner.get("code").and_then(Value::as_str),
        Some("unknown_tool"),
        "rejection must carry unknown_tool: {inner:?}"
    );

    // Close the stream so the production run_subc_mode reader hits EOF and exits.
    drop(stream);
}

async fn open_fake_daemon_session(input: FakeDaemonInput) -> FakeDaemonSession {
    let FakeDaemonInput {
        listener,
        key,
        daemon_id,
        root1,
        root2,
        failed_root,
        push_burst_root,
        slow_root,
        callgraph_root,
        callgraph_file,
        state,
        executor,
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
                storage: None,
            })
            .expect("hello ack body"),
        )
        .expect("hello ack frame"),
    )
    .await;

    FakeDaemonSession {
        stream,
        root1,
        root2,
        failed_root,
        push_burst_root,
        slow_root,
        callgraph_root,
        callgraph_file,
        state,
        executor,
    }
}

async fn bind_route1(stream: &mut tokio::net::TcpStream, root1: &std::path::Path) {
    send_route_bind(stream, 1, 10, root1).await;
    expect_route_bind_ack(stream, 10).await;
}

async fn bind_routes_1_and_4(stream: &mut tokio::net::TcpStream, root1: &std::path::Path) {
    bind_route1(stream, root1).await;
    send_route_bind(stream, 4, 44, root1).await;
    expect_route_bind_ack(stream, 44).await;
}

async fn send_connection_goodbye(stream: &mut tokio::net::TcpStream) {
    send_frame(
        stream,
        Frame::build(FrameType::Goodbye, control_flags(), 0, 99, Vec::new())
            .expect("goodbye frame"),
    )
    .await;
}

async fn drive_mutating_internal_error_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_routes_1_and_4(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        100,
        "subc_test_mutating_internal_error",
        json!({}),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "mutating internal_error response").await;
    assert_eq!(frame.header.channel, 1);
    assert_eq!(frame.header.corr, 100);
    assert!(tool_result_is_error(&frame));
    let response = tool_response_json(&frame);
    assert_eq!(response["code"].as_str(), Some("internal_error"));
    assert_no_response_frame_within(
        &mut stream,
        Duration::from_millis(500),
        "fatal teardown after non-panic internal_error",
    )
    .await;
    send_tool_call(&mut stream, 4, 101, "echo", json!({ "case": "fast" })).await;
    let alive = read_frame_timeout(&mut stream, "post-internal-error response").await;
    assert_eq!(alive.header.channel, 4);
    assert_eq!(alive.header.corr, 101);
    assert_eq!(tool_response_json(&alive)["success"].as_bool(), Some(true));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_mutating_panic_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(&mut stream, 1, 102, "subc_test_mutating_panic", json!({})).await;
    let frame = read_frame_timeout(&mut stream, "mutating panic response").await;
    assert_eq!(frame.header.channel, 1);
    assert_eq!(frame.header.corr, 102);
    assert!(tool_result_is_error(&frame));
    let response = tool_response_json(&frame);
    assert_eq!(response["code"].as_str(), Some("actor_fatal"));

    let route_goodbye = read_any_frame_timeout(&mut stream, "fatal route goodbye").await;
    assert_eq!(route_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(route_goodbye.header.channel, 1);
    assert_eq!(route_goodbye.header.corr, 102);

    let connection_goodbye = read_any_frame_timeout(&mut stream, "fatal connection goodbye").await;
    assert_eq!(connection_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(connection_goodbye.header.channel, 0);
}

async fn drive_pure_read_actor_fatal_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        executor,
        ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    let root_id = ProjectRootId::from_path(&root1).expect("root1 id");
    let fatal_response = executor.submit(
        root_id.clone(),
        Lane::Mutating,
        "subc-test-mark-fatal".to_string(),
        Box::new(|_| panic!("intentional direct mutating panic")),
    );
    let fatal_response = fatal_response
        .recv_timeout(Duration::from_secs(30))
        .expect("direct mutating panic response");
    assert_eq!(fatal_response.data["code"].as_str(), Some("actor_fatal"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !executor.actor_is_fatal(&root_id) {
        assert!(
            Instant::now() < deadline,
            "direct mutating panic should mark the actor fatal"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    send_tool_call(&mut stream, 1, 103, "echo", json!({ "case": "fast" })).await;
    let frame = read_frame_timeout(&mut stream, "pure-read actor_fatal response").await;
    assert_eq!(frame.header.channel, 1);
    assert_eq!(frame.header.corr, 103);
    assert!(tool_result_is_error(&frame));
    let response = tool_response_json(&frame);
    assert_eq!(response["code"].as_str(), Some("actor_fatal"));

    let route_goodbye = read_any_frame_timeout(&mut stream, "pure-read fatal route goodbye").await;
    assert_eq!(route_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(route_goodbye.header.channel, 1);
    assert_eq!(route_goodbye.header.corr, 103);

    let connection_goodbye =
        read_any_frame_timeout(&mut stream, "pure-read fatal connection goodbye").await;
    assert_eq!(connection_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(connection_goodbye.header.channel, 0);
}

async fn drive_bash_fast_foreground_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        100,
        "bash",
        json!({
            "command": "printf 'subc-fast\\n'",
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "fast bash response").await;
    assert_eq!(frame.header.channel, 1);
    assert_eq!(frame.header.corr, 100);
    assert!(!tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(text.contains("subc-fast"), "unexpected bash text: {text:?}");
    assert_no_response_frame_within(
        &mut stream,
        Duration::from_millis(250),
        "extra fast bash response",
    )
    .await;
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_promotion_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        101,
        "bash",
        json!({
            "command": "sleep 2; printf 'promoted-done\\n'",
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "promoted bash response").await;
    assert_eq!(frame.header.corr, 101);
    assert!(!tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(
        text.contains("promoted to background"),
        "expected promotion text, got {text:?}"
    );
    let task_id = extract_bash_task_id(&text);
    send_tool_call(
        &mut stream,
        1,
        102,
        "bash_status",
        json!({ "params": { "task_id": task_id } }),
    )
    .await;
    let status = read_frame_timeout(&mut stream, "promoted bash status").await;
    assert_eq!(status.header.corr, 102);
    let response = tool_response_json(&status);
    assert_eq!(response["success"].as_bool(), Some(true));
    assert_eq!(response["task_id"].as_str(), Some(task_id.as_str()));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_block_to_completion_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    let started = Instant::now();
    send_tool_call(
        &mut stream,
        1,
        103,
        "bash",
        json!({
            "command": "sleep 1; printf 'block-done\\n'",
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "timeout": 5_000,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "block-to-completion bash response").await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(800),
        "block_to_completion returned too early after {elapsed:?}"
    );
    assert_eq!(frame.header.corr, 103);
    assert!(!tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(
        text.contains("block-done"),
        "unexpected bash text: {text:?}"
    );
    assert!(!text.contains("promoted to background"));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_background_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        104,
        "bash",
        json!({
            "command": "sleep 1; printf 'background-done\\n'",
            "background": true,
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "background bash response").await;
    assert_eq!(frame.header.corr, 104);
    assert!(!tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(
        text.contains("Background task started"),
        "unexpected background launch text: {text:?}"
    );
    let task_id = extract_bash_task_id(&text);
    send_tool_call(
        &mut stream,
        1,
        105,
        "bash_status",
        json!({ "params": { "task_id": task_id } }),
    )
    .await;
    let status = read_frame_timeout(&mut stream, "background bash status").await;
    let response = tool_response_json(&status);
    assert_eq!(response["success"].as_bool(), Some(true));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_nonzero_exit_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    // Run the nonzero exit in a CHILD process per platform. A bare `exit 7`
    // shell builtin would terminate the Windows PowerShell bg-bash wrapper
    // (which runs the user command via Invoke-Expression in its own scope)
    // BEFORE it persists the exit marker, leaving exit_code=null. A child
    // `cmd /c "...& exit 7"` exits 7 as a native process whose code the
    // wrapper captures via $LASTEXITCODE — matching the product's own
    // windows_spawn_writes_exit_marker_for_nonzero_exit (`cmd /c exit N`) and
    // the cross-platform command helpers in bash_background_test.rs. On Unix
    // the wrapper already subshells via `{shell} -c`, so the POSIX form is safe.
    let nonzero_command = if cfg!(windows) {
        "cmd /c \"echo subc-fail&exit 7\""
    } else {
        "printf 'subc-fail\\n'; exit 7"
    };
    send_tool_call(
        &mut stream,
        1,
        106,
        "bash",
        json!({
            "command": nonzero_command,
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "nonzero bash response").await;
    assert_eq!(frame.header.corr, 106);
    assert!(!tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(text.contains("subc-fail"), "unexpected bash text: {text:?}");
    assert!(
        text.contains("[exit code: 7]"),
        "missing exit code in {text:?}"
    );
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_lane_nonoccupancy_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        107,
        "bash",
        json!({
            "command": "sleep 2; printf 'lane-done\\n'",
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "timeout": 5_000,
            "compressed": false,
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    send_tool_call(&mut stream, 1, 108, "echo", json!({ "case": "fast" })).await;
    send_tool_call(
        &mut stream,
        1,
        109,
        "subc_test_emit_status",
        json!({ "marker": "lane-mutating", "seq": 1 }),
    )
    .await;
    let first = read_frame_within(
        &mut stream,
        Duration::from_secs(1),
        "lane nonoccupancy response",
    )
    .await
    .expect("first lane nonoccupancy response");
    let second = read_frame_within(
        &mut stream,
        Duration::from_secs(1),
        "lane nonoccupancy response",
    )
    .await
    .expect("second lane nonoccupancy response");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "lane work was blocked for {elapsed:?}"
    );
    assert_eq!(
        HashSet::from([first.header.corr, second.header.corr]),
        HashSet::from([108, 109]),
        "pure-read and mutating tool calls should both beat the pending bash final response"
    );
    assert!(
        !tool_result_is_error(&first),
        "first lane response corr {} failed: {}",
        first.header.corr,
        tool_result_text(&first)
    );
    assert!(
        !tool_result_is_error(&second),
        "second lane response corr {} failed: {}",
        second.header.corr,
        tool_result_text(&second)
    );
    let bash = read_frame_timeout(&mut stream, "lane bash final response").await;
    assert_eq!(bash.header.corr, 107);
    let text = tool_result_text(&bash);
    assert!(text.contains("lane-done"), "unexpected bash text: {text:?}");
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_route_close_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        110,
        "bash",
        json!({
            "command": "sleep 5; printf 'closed-route-done\\n'",
            "foreground_orchestrate": true,
            "block_to_completion": true,
            "timeout": 10_000,
            "compressed": false,
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_frame(
        &mut stream,
        Frame::build(FrameType::Goodbye, control_flags(), 1, 111, Vec::new())
            .expect("route goodbye frame"),
    )
    .await;
    assert_no_response_frame_within(
        &mut stream,
        Duration::from_millis(500),
        "closed route deferred bash response",
    )
    .await;
    send_route_bind(&mut stream, 1, 112, &root1).await;
    expect_route_bind_ack(&mut stream, 112).await;
    send_tool_call(&mut stream, 1, 113, "echo", json!({ "case": "fast" })).await;
    let frame = read_frame_timeout(&mut stream, "post-route-close response").await;
    assert_eq!(frame.header.corr, 113);
    assert_eq!(tool_response_json(&frame)["success"].as_bool(), Some(true));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_promote_failure_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_routes_1_and_4(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        114,
        "bash",
        json!({
            "command": "sleep 2; printf 'promote-failure-done\\n'",
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "promote failure bash response").await;
    assert_eq!(frame.header.corr, 114);
    assert!(tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(
        text.contains("forced subc bash promote failure"),
        "expected promote failure text, got {text:?}"
    );
    send_tool_call(&mut stream, 4, 115, "echo", json!({ "case": "fast" })).await;
    let alive = read_frame_timeout(&mut stream, "post-promote-failure response").await;
    assert_eq!(alive.header.channel, 4);
    assert_eq!(alive.header.corr, 115);
    assert_eq!(tool_response_json(&alive)["success"].as_bool(), Some(true));
    send_connection_goodbye(&mut stream).await;
}

async fn drive_bash_promote_panic_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_tool_call(
        &mut stream,
        1,
        116,
        "bash",
        json!({
            "command": "sleep 2; printf 'promote-panic-done\\n'",
            "foreground_orchestrate": true,
            "compressed": false,
        }),
    )
    .await;
    let frame = read_frame_timeout(&mut stream, "promote panic bash response").await;
    assert_eq!(frame.header.channel, 1);
    assert_eq!(frame.header.corr, 116);
    assert!(tool_result_is_error(&frame));
    let text = tool_result_text(&frame);
    assert!(
        text.contains("forced subc bash promote panic"),
        "expected promote panic text, got {text:?}"
    );

    let route_goodbye = read_any_frame_timeout(&mut stream, "promote panic route goodbye").await;
    assert_eq!(route_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(route_goodbye.header.channel, 1);
    assert_eq!(route_goodbye.header.corr, 116);

    let connection_goodbye =
        read_any_frame_timeout(&mut stream, "promote panic connection goodbye").await;
    assert_eq!(connection_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(connection_goodbye.header.channel, 0);
}

async fn drive_core_routing_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        root2,
        state,
        ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    // 1. Overlap: three PureRead calls on one route must all reach dispatch
    // before any is released.
    for corr in 100..103 {
        send_tool_call(&mut stream, 1, corr, "echo", json!({ "case": "overlap" })).await;
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
    send_tool_call(&mut stream, 1, 201, "echo", json!({ "case": "fast" })).await;
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
        send_tool_call(&mut stream, 1, corr, "echo", json!({ "case": "epoch" })).await;
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

    send_tool_call(&mut stream, 1, 400, "echo", json!({ "case": "fast" })).await;
    let route1_read = read_frame_timeout(&mut stream, "route 1 read response").await;
    assert_eq!(route1_read.header.channel, 1);
    assert_eq!(route1_read.header.corr, 400);
    assert_tool_project_root(&route1_read, &root1);
    send_tool_call(&mut stream, 2, 401, "echo", json!({ "case": "fast" })).await;
    let route2_read = read_frame_timeout(&mut stream, "route 2 read response").await;
    assert_eq!(route2_read.header.channel, 2);
    assert_eq!(route2_read.header.corr, 401);
    assert_tool_project_root(&route2_read, &root2);

    // PUSH isolation: root-1 emits do not leak to root-2's bound channel.
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
        send_tool_call(&mut stream, 1, corr, "echo", json!({ "case": "epoch" })).await;
    }
    state.wait_until("same-root epoch reads started", |inner| {
        inner.epoch_started == same_root_epoch_base + 2
    });
    send_route_bind(&mut stream, 4, 44, &root1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    state.assert_configure_not_started("subc-bind-4");
    state.release_epoch_reads();
    // The same-root configure for route 4 starts only after these reads drain,
    // so RouteBindAck(44) (channel 0) and the read responses (channel 1) are
    // emitted concurrently and can reach the stream in any order. Demux by
    // channel rather than assuming the reads land before the ack.
    let same_root_corrs =
        collect_tool_responses_and_route_bind_ack(&mut stream, &[410, 411], 44).await;
    assert_eq!(same_root_corrs, HashSet::from([410, 411]));
    assert_eq!(
        state.wait_for_configure("subc-bind-4").epoch_reads_at_start,
        0,
        "same-root configure should start only after route 1 reads drain"
    );
    send_tool_call(&mut stream, 4, 420, "echo", json!({ "case": "fast" })).await;
    let route4_read = read_frame_timeout(&mut stream, "route 4 read response").await;
    assert_eq!(route4_read.header.channel, 4);
    assert_eq!(route4_read.header.corr, 420);
    assert_tool_project_root(&route4_read, &root1);

    send_connection_goodbye(&mut stream).await;
}

async fn drive_configure_warning_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;

    send_route_bind_with_doc(
        &mut stream,
        1,
        10,
        &root1,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_configure_warning": {
                "message": "route-1-maintenance-warning",
            },
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 10).await;
    let configure_warning_pushes = expect_configure_warning_pushes(
        &mut stream,
        HashSet::from([1]),
        &root1,
        "route-1-maintenance-warning",
        "session-1",
    )
    .await;
    assert_eq!(configure_warning_pushes.len(), 1);

    send_route_bind(&mut stream, 4, 44, &root1).await;
    expect_route_bind_ack(&mut stream, 44).await;

    // Configure warnings for one session on a shared root must carry that
    // session id and not leak to sibling sessions.
    send_route_bind_with_session_and_doc(
        &mut stream,
        1,
        49,
        &root1,
        "session-1",
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_configure_warning": {
                "message": "session-1-reconfigure-warning",
            },
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 49).await;
    let session_configure_warning_pushes = expect_configure_warning_pushes(
        &mut stream,
        HashSet::from([1]),
        &root1,
        "session-1-reconfigure-warning",
        "session-1",
    )
    .await;
    assert_eq!(session_configure_warning_pushes.len(), 1);
    assert_eq!(
        session_configure_warning_pushes[0]
            .get("session_id")
            .and_then(Value::as_str),
        Some("session-1")
    );
    send_tool_call(&mut stream, 4, 50, "echo", json!({ "case": "fast" })).await;
    expect_tool_response_without_configure_warning_for_message(
        &mut stream,
        50,
        &root1,
        "session-1-reconfigure-warning",
    )
    .await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_semantic_refresh_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    // Semantic-refresh maintenance injects a SemanticRefreshEvent through the
    // same event_rx seam used by standalone tests. The full subc maintenance
    // tick must run the semantic refresh drain and emit the refreshing status.
    send_tool_call(
        &mut stream,
        1,
        66,
        "subc_test_enqueue_semantic_refresh_event",
        json!({}),
    )
    .await;
    let semantic_refresh_pushes =
        expect_semantic_refresh_status_pushes_for_tool(&mut stream, 66, HashSet::from([1]), &root1)
            .await;
    assert_eq!(semantic_refresh_pushes.len(), 1);

    send_connection_goodbye(&mut stream).await;
}

async fn drive_watcher_stale_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    // Watcher maintenance injects a compact watcher event through the same
    // watcher_rx seam standalone tests use, then lets the subc maintenance tick
    // drain it on the Mutating lane and emit the stale-status Push.
    send_tool_call(
        &mut stream,
        1,
        65,
        "subc_test_enqueue_watcher_event",
        json!({}),
    )
    .await;
    let watcher_pushes =
        expect_watcher_stale_status_pushes_for_tool(&mut stream, 65, HashSet::from([1]), &root1)
            .await;
    assert_eq!(watcher_pushes.len(), 1);

    send_connection_goodbye(&mut stream).await;
}

async fn drive_route_bind_session_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_routes_1_and_4(&mut stream, &root1).await;

    // Subc tool calls carry the RouteBind session on the RawRequest.
    send_tool_call(&mut stream, 1, 11, "subc_test_echo_session", json!({})).await;
    let echo_s1 = read_frame_timeout(&mut stream, "echo session route 1").await;
    assert_eq!(echo_s1.header.corr, 11);
    let echo_s1_body = tool_response_json(&echo_s1);
    assert_eq!(
        echo_s1_body["transport_session"].as_str(),
        Some("session-1"),
        "route 1 bind identity session: {echo_s1_body:?}"
    );

    send_tool_call(&mut stream, 4, 422, "subc_test_echo_session", json!({})).await;
    let echo_s4 = read_frame_timeout(&mut stream, "echo session route 4").await;
    assert_eq!(echo_s4.header.corr, 422);
    assert_eq!(
        tool_response_json(&echo_s4)["transport_session"].as_str(),
        Some("session-4"),
    );

    send_connection_goodbye(&mut stream).await;
}

async fn drive_l3_fanout_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

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

    send_connection_goodbye(&mut stream).await;
}

async fn drive_routebind_nonblocking_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        slow_root,
        state,
        ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    // P5b B2 #4: a slow RouteBind configure must not block the subc loop.
    // While route 9 is pending, route 1 is already bound and must still service
    // a tool call + Push before route 9's RouteBindAck can be produced.
    let slow_bind_base = state.begin_slow_configure_wave();
    send_route_bind_with_doc(
        &mut stream,
        9,
        19,
        &slow_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_slow_configure": true,
        }),
    )
    .await;
    state.wait_until("slow route 9 configure started", |inner| {
        inner.slow_configure_started > slow_bind_base
    });
    send_tool_call(
        &mut stream,
        1,
        90,
        "subc_test_emit_status",
        json!({ "marker": "routebind-nonblocking", "seq": 9 }),
    )
    .await;
    let nonblocking_pushes =
        expect_status_pushes_for_tool(&mut stream, 90, "routebind-nonblocking", HashSet::from([1]))
            .await;
    assert_eq!(nonblocking_pushes.len(), 1);
    assert_eq!(push_seq(&nonblocking_pushes[0]), Some(9));
    state.release_slow_configures();
    state.wait_for_slow_configure_finished(slow_bind_base + 1);
    expect_route_bind_ack(&mut stream, 19).await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_duplicate_routebind_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        slow_root,
        state,
        ..
    } = open_fake_daemon_session(input).await;

    // P5b B2 amend 5: duplicate RouteBind on a channel with an in-flight bind
    // is rejected immediately and does not submit a second configure.
    let duplicate_bind_base = state.begin_slow_configure_wave();
    send_route_bind_with_doc(
        &mut stream,
        11,
        111,
        &slow_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_slow_configure": true,
        }),
    )
    .await;
    state.wait_until("slow route 11 configure started", |inner| {
        inner.slow_configure_started > duplicate_bind_base
    });
    send_route_bind(&mut stream, 11, 112, &slow_root).await;
    expect_route_bind_error(&mut stream, 112, "config_divergence").await;
    state.release_slow_configures();
    state.wait_for_slow_configure_finished(duplicate_bind_base + 1);
    expect_route_bind_ack(&mut stream, 111).await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_pending_bind_tool_call_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        slow_root,
        state,
        ..
    } = open_fake_daemon_session(input).await;

    // P5b B2 finding (d): a route-channel tool call sent before its bind ack is
    // a protocol error and remains route_not_bound while the bind is pending.
    let pending_tool_base = state.begin_slow_configure_wave();
    send_route_bind_with_doc(
        &mut stream,
        12,
        1200,
        &slow_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_slow_configure": true,
        }),
    )
    .await;
    state.wait_until("slow route 12 configure started", |inner| {
        inner.slow_configure_started > pending_tool_base
    });
    send_tool_call(&mut stream, 12, 1201, "echo", json!({ "case": "fast" })).await;
    expect_error_frame(&mut stream, 12, 1201, "route_not_bound").await;
    state.release_slow_configures();
    state.wait_for_slow_configure_finished(pending_tool_base + 1);
    expect_route_bind_ack(&mut stream, 1200).await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_goodbye_cancels_pending_bind_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        slow_root,
        state,
        ..
    } = open_fake_daemon_session(input).await;

    // P5b B2 amend 5: Goodbye during a pending bind cancels completion so it
    // cannot install a ghost route or send an ack for the closed channel.
    let goodbye_bind_base = state.begin_slow_configure_wave();
    send_route_bind_with_doc(
        &mut stream,
        10,
        110,
        &slow_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_slow_configure": true,
        }),
    )
    .await;
    state.wait_until("slow route 10 configure started", |inner| {
        inner.slow_configure_started > goodbye_bind_base
    });
    send_frame(
        &mut stream,
        Frame::build(FrameType::Goodbye, control_flags(), 10, 1110, Vec::new())
            .expect("route 10 goodbye"),
    )
    .await;
    state.release_slow_configures();
    state.wait_for_slow_configure_finished(goodbye_bind_base + 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    send_tool_call(&mut stream, 10, 1111, "echo", json!({ "case": "fast" })).await;
    expect_error_frame_skipping_optional_ack(&mut stream, 10, 1111, "route_not_bound", 110).await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_l3_coalescing_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        push_burst_root,
        ..
    } = open_fake_daemon_session(input).await;

    // Bind a single-channel root for the configure-emitted push scenario. The
    // follow-up RouteBind exercises already-live reconfigure behavior while
    // keeping project-scoped status fan-out unambiguous.
    send_route_bind(&mut stream, 6, 15, &push_burst_root).await;
    expect_route_bind_ack(&mut stream, 15).await;

    // L3 coalescing integration: configure emits a burst for an already-bound
    // route. The non-blocking RouteBind path may deliver the coalesced Push
    // before or after the bind ack, but it must still collapse the burst.
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
    let configure_burst_pushes = expect_route_bind_ack_and_status_pushes(
        &mut stream,
        16,
        "configure-burst",
        HashSet::from([6]),
    )
    .await;
    assert!(
        !configure_burst_pushes.is_empty(),
        "configure burst should deliver at least one coalesced status frame"
    );
    assert!(
        configure_burst_pushes
            .iter()
            .any(|push| push_seq(push) == Some(15)),
        "configure burst should include the final status snapshot"
    );
    send_tool_call(
        &mut stream,
        6,
        160,
        "subc_test_emit_status",
        json!({ "marker": "configure-burst-sentinel", "seq": 16 }),
    )
    .await;
    let sentinel_pushes = expect_status_sentinel_without_marker_before(
        &mut stream,
        160,
        "configure-burst-sentinel",
        "configure-burst",
        HashSet::from([6]),
    )
    .await;
    assert_eq!(sentinel_pushes.len(), 1);
    assert_eq!(push_seq(&sentinel_pushes[0]), Some(16));

    send_connection_goodbye(&mut stream).await;
}

async fn drive_lossy_pressure_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        push_burst_root,
        ..
    } = open_fake_daemon_session(input).await;

    send_route_bind(&mut stream, 6, 15, &push_burst_root).await;
    expect_route_bind_ack(&mut stream, 15).await;

    // P5b B1: reliable Push frames bypass the bounded lossy funnel. This
    // configure emits enough lossy status frames to fill lossy_tx, then emits a
    // reliable BashCompleted. The completion must still arrive.
    let pressure_task = "reliable-after-lossy-pressure";
    send_route_bind_with_doc(
        &mut stream,
        6,
        17,
        &push_burst_root,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
            "subc_test_configure_status_burst": {
                "marker": "lossy-pressure",
                "count": 2048,
            },
            "subc_test_configure_bash_completed": {
                "task_id": pressure_task,
            },
        }),
    )
    .await;
    let (pressure_statuses, pressure_completions) =
        expect_route_bind_ack_status_and_bash_completed_pushes(
            &mut stream,
            17,
            "lossy-pressure",
            pressure_task,
            HashSet::from([6]),
        )
        .await;
    assert!(
        !pressure_statuses.is_empty(),
        "lossy pressure should still deliver at least one coalesced status frame"
    );
    assert_eq!(pressure_completions.len(), 1);
    if !pressure_statuses
        .iter()
        .any(|push| push_seq(push) == Some(2047))
    {
        let final_pressure_pushes = expect_status_pushes_for_marker_seq(
            &mut stream,
            "lossy-pressure",
            2047,
            HashSet::from([6]),
        )
        .await;
        assert_eq!(final_pressure_pushes.len(), 1);
    }
    send_tool_call(
        &mut stream,
        6,
        170,
        "subc_test_emit_status",
        json!({ "marker": "lossy-pressure-sentinel", "seq": 2048 }),
    )
    .await;
    let sentinel_pushes = expect_status_sentinel_without_marker_before(
        &mut stream,
        170,
        "lossy-pressure-sentinel",
        "lossy-pressure",
        HashSet::from([6]),
    )
    .await;
    assert_eq!(sentinel_pushes.len(), 1);
    assert_eq!(push_seq(&sentinel_pushes[0]), Some(2048));

    send_connection_goodbye(&mut stream).await;
}

async fn drive_response_finalizer_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        state,
        ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

    // L2 response finalizer: a normal route-channel read gets status_bar once
    // the actor has real Tier-2 counts, matching standalone response shape.
    send_tool_call(
        &mut stream,
        1,
        80,
        "echo",
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
    wait_for_bash_completion(&mut stream, 1, 82, "session-1", &task_id).await;

    // Terminal status precedes in-band completion enqueue, so settle until the
    // mirror is actually attached before asserting. Drain is non-destructive
    // (the mirror persists until an explicit ack), so the subsequent reads still
    // observe it — that persistence is what corr 120/121 verify.
    settle_until_bg_completion(&mut stream, 1, 8400, &task_id).await;

    send_tool_call(&mut stream, 1, 120, "echo", json!({ "case": "fast" })).await;
    let first_after_completion = read_frame_timeout(&mut stream, "first completion read").await;
    assert_eq!(first_after_completion.header.corr, 120);
    let first_after_completion_response = tool_response_json(&first_after_completion);
    assert_bg_completion(&first_after_completion_response, &task_id);

    send_tool_call(&mut stream, 1, 121, "echo", json!({ "case": "fast" })).await;
    let second_after_completion = read_frame_timeout(&mut stream, "second completion read").await;
    assert_eq!(second_after_completion.header.corr, 121);
    let second_after_completion_response = tool_response_json(&second_after_completion);
    assert_bg_completion(&second_after_completion_response, &task_id);

    // Two same-actor PureRead jobs finish/finalize concurrently. Both should
    // clone the pending bg completion safely; the status_bar dedup lock is also
    // exercised because counts were populated above.
    let finalizer_epoch_base = state.begin_epoch_wave();
    for corr in 122..124 {
        send_tool_call(&mut stream, 1, corr, "echo", json!({ "case": "epoch" })).await;
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

    send_connection_goodbye(&mut stream).await;
}

async fn drive_session_scoped_bg_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_routes_1_and_4(&mut stream, &root1).await;

    // A second session on the same root has its own transport session and bg state.
    send_tool_call(
        &mut stream,
        4,
        423,
        "bash",
        json!({
            "params": {
                "command": "echo subc-session-4-bg",
                "background": true,
                "timeout": 5000,
            },
        }),
    )
    .await;
    let bash_s4_started = read_frame_timeout(&mut stream, "session-4 bash start").await;
    assert_eq!(bash_s4_started.header.corr, 423);
    let bash_s4_started_response = tool_response_json(&bash_s4_started);
    assert!(
        bash_s4_started_response["success"]
            .as_bool()
            .unwrap_or(false),
        "session-4 background bash should start: {bash_s4_started_response:?}"
    );
    let task_id_s4 = bash_s4_started_response["task_id"]
        .as_str()
        .expect("session-4 bash task_id")
        .to_string();
    wait_for_bash_completion(&mut stream, 4, 424, "session-4", &task_id_s4).await;

    // Terminal status precedes in-band completion enqueue (see
    // settle_until_bg_completion), so settle until the mirror attaches before
    // asserting it on a specific response corr.
    settle_until_bg_completion(&mut stream, 4, 4250, &task_id_s4).await;

    send_tool_call(&mut stream, 4, 425, "echo", json!({ "case": "fast" })).await;
    let s4_after_completion = read_frame_timeout(&mut stream, "session-4 completion read").await;
    assert_eq!(s4_after_completion.header.corr, 425);
    assert_bg_completion_matching(
        &tool_response_json(&s4_after_completion),
        &task_id_s4,
        "subc-session-4-bg",
    );

    send_tool_call(&mut stream, 1, 426, "echo", json!({ "case": "fast" })).await;
    let s1_after_s4_bg = read_frame_timeout(&mut stream, "session-1 after session-4 bg").await;
    assert_eq!(s1_after_s4_bg.header.corr, 426);
    let s1_after_s4_body = tool_response_json(&s1_after_s4_bg);
    if let Some(completions) = s1_after_s4_body["bg_completions"].as_array() {
        assert!(
            !completions.iter().any(|c| {
                c.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|cmd| cmd.contains("subc-session-4-bg"))
            }),
            "session-1 must not see session-4 bg completion: {s1_after_s4_body:?}"
        );
    }

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

    // 4d-2b session-scoped Push isolation: a bash completion for session-1
    // must go only to route 1, not sibling route 4 (session-4) on the same root.
    send_tool_call(
        &mut stream,
        1,
        430,
        "subc_test_emit_bash_completed",
        json!({ "task_id": "session-scoped-isolation" }),
    )
    .await;
    let session_pushes = expect_bash_completed_pushes_for_tool(
        &mut stream,
        430,
        "session-scoped-isolation",
        HashSet::from([1]),
    )
    .await;
    assert_eq!(session_pushes.len(), 1);
    send_tool_call(
        &mut stream,
        4,
        431,
        "subc_test_emit_bash_completed",
        json!({ "task_id": "session-4-isolation-sentinel" }),
    )
    .await;
    let sentinel = expect_bash_completed_sentinel_without_task_before(
        &mut stream,
        431,
        "session-4-isolation-sentinel",
        "session-scoped-isolation",
        4,
    )
    .await;
    assert_eq!(
        push_task_id(&sentinel),
        Some("session-4-isolation-sentinel")
    );

    send_connection_goodbye(&mut stream).await;
}

async fn drive_bg_events_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;
    send_route_bind_with_session(&mut stream, 7, 70, &root1, "session-1").await;
    expect_route_bind_ack(&mut stream, 70).await;

    send_bg_events_subscribe(&mut stream, 7, 700).await;
    send_tool_call(&mut stream, 1, 701, "echo", json!({ "case": "fast" })).await;
    let route_usable = read_tool_response_allowing_bg_events(
        &mut stream,
        701,
        7,
        700,
        "route usable while bg_events is subscribed",
    )
    .await;
    assert_eq!(route_usable["success"].as_bool(), Some(true));

    drain_bg_events_for(
        &mut stream,
        7,
        700,
        Duration::from_millis(800),
        "optimistic bg_events subscribe seed",
    )
    .await;

    let burst_specs = [
        (710, "sleep 2; printf 'bg-events-a\n'"),
        (711, "sleep 2; printf 'bg-events-b\n'"),
        (712, "sleep 2; printf 'bg-events-c\n'"),
    ];
    for (corr, command) in burst_specs {
        send_bash_background(&mut stream, corr, command).await;
    }
    let (start_responses, mut early_bg_events) = collect_tool_responses_allowing_bg_events(
        &mut stream,
        HashSet::from([710, 711, 712]),
        7,
        700,
        "bg_events burst bash start responses",
    )
    .await;
    let task_ids: Vec<String> = [710, 711, 712]
        .into_iter()
        .map(|corr| {
            let response = start_responses.get(&corr).expect("bash start response");
            assert!(
                response["success"].as_bool().unwrap_or(false),
                "background bash should start: {response:?}"
            );
            response["task_id"]
                .as_str()
                .expect("background task_id")
                .to_string()
        })
        .collect();

    if early_bg_events.is_empty() {
        early_bg_events.push(
            wait_for_bg_event(
                &mut stream,
                7,
                700,
                Duration::from_secs(30),
                "completion wake",
            )
            .await,
        );
    }
    assert_bg_events_are_coalesced(&early_bg_events);
    if early_bg_events.len() == 1 {
        let elapsed = early_bg_events[0].elapsed();
        if elapsed < Duration::from_millis(150) {
            assert_no_bg_event_for(
                &mut stream,
                7,
                700,
                Duration::from_millis(150).saturating_sub(elapsed),
                "second bg_events nudge in the same tick",
            )
            .await;
        }
    }

    wait_for_bg_event(
        &mut stream,
        7,
        700,
        Duration::from_secs(2),
        "re-armed bg_events wake before ack",
    )
    .await;

    let drained = drain_bg_completions_until(&mut stream, 1, 7300, &task_ids, 7, 700).await;
    let drained_ids: HashSet<String> = drained["bg_completions"]
        .as_array()
        .expect("drained bg_completions")
        .iter()
        .filter_map(|completion| {
            completion
                .get("task_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect();
    for task_id in &task_ids {
        assert!(
            drained_ids.contains(task_id),
            "drain response should include {task_id}: {drained:?}"
        );
    }

    send_tool_call(
        &mut stream,
        1,
        7400,
        "bash_ack_completions",
        json!({ "task_ids": task_ids }),
    )
    .await;
    let ack = read_tool_response_allowing_bg_events(
        &mut stream,
        7400,
        7,
        700,
        "bg completion ack response",
    )
    .await;
    assert_eq!(ack["success"].as_bool(), Some(true));

    drain_bg_events_for(
        &mut stream,
        7,
        700,
        Duration::from_millis(1200),
        "bg_events clear grace after ack",
    )
    .await;
    assert_no_bg_event_for(
        &mut stream,
        7,
        700,
        Duration::from_millis(700),
        "bg_events quiet after ack clear",
    )
    .await;

    send_bg_events_cancel(&mut stream, 7, 700).await;
    expect_bg_events_stream_end(&mut stream, 7, 700).await;

    send_bash_background(
        &mut stream,
        7500,
        "sleep 1; printf 'bg-events-after-cancel\n'",
    )
    .await;
    let after_cancel_start =
        read_tool_response_rejecting_bg_events(&mut stream, 7500, 7, 700, "post-cancel bash start")
            .await;
    let after_cancel_task = after_cancel_start["task_id"]
        .as_str()
        .expect("post-cancel task_id")
        .to_string();
    wait_for_bash_completion_without_bg_event(&mut stream, 1, 7501, &after_cancel_task, 7, 700)
        .await;
    assert_no_bg_event_for(
        &mut stream,
        7,
        700,
        Duration::from_millis(700),
        "bg_events quiet after cancel",
    )
    .await;

    send_connection_goodbye(&mut stream).await;
}

fn minimal_bind_doc() -> Value {
    json!({
        "callgraph_store": false,
        "search_index": false,
        "semantic_search": false,
    })
}

fn subc_mcp_principal() -> Principal {
    Principal::Reserved {
        module_id: "subc-mcp".to_string(),
    }
}

async fn bind_route_with_principal(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
    harness: &str,
    session: &str,
    principal: Option<Principal>,
) {
    send_route_bind_with_harness_session_principal_and_doc(
        stream,
        route_channel,
        corr,
        root,
        harness,
        session,
        principal,
        minimal_bind_doc(),
    )
    .await;
    expect_route_bind_ack(stream, corr).await;
}

fn assert_tool_success(response: &Value, label: &str) {
    assert_eq!(
        response["success"].as_bool(),
        Some(true),
        "{label} should succeed: {response:?}"
    );
}

fn assert_tool_error_code(response: &Value, code: &str, label: &str) {
    assert_eq!(
        response["success"].as_bool(),
        Some(false),
        "{label} should fail: {response:?}"
    );
    assert_eq!(
        response["code"].as_str(),
        Some(code),
        "{label} returned wrong error: {response:?}"
    );
}

async fn call_tool_frame(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
    label: &str,
) -> Frame {
    send_tool_call(stream, channel, corr, name, arguments).await;
    let frame = read_frame_timeout(stream, label).await;
    assert_eq!(
        frame.header.channel, channel,
        "unexpected channel for {label}"
    );
    assert_eq!(frame.header.corr, corr, "unexpected corr for {label}");
    frame
}

async fn call_tool_response(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    name: &str,
    arguments: Value,
    label: &str,
) -> Value {
    let frame = call_tool_frame(stream, channel, corr, name, arguments, label).await;
    tool_response_json(&frame)
}

async fn drive_principal_trust_enforcement_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        root2,
        ..
    } = open_fake_daemon_session(input).await;
    let trusted_outside = root2.join("trusted-direct-outside.txt");
    let untrusted_outside = root2.join("untrusted-outside.txt");
    let untrusted_inside = root1.join("untrusted-inside.txt");

    // Anti-spoof: a client-claimed mcp:* harness remains trusted when the
    // daemon-stamped principal is Direct.
    bind_route_with_principal(
        &mut stream,
        1,
        101,
        &root1,
        "mcp:evil",
        "direct-session",
        Some(Principal::Direct),
    )
    .await;
    let trusted_write = call_tool_frame(
        &mut stream,
        1,
        102,
        "write",
        json!({ "filePath": trusted_outside.to_string_lossy(), "content": "trusted outside\n" }),
        "trusted direct out-of-root write",
    )
    .await;
    assert!(
        !tool_result_is_error(&trusted_write),
        "trusted direct out-of-root write should succeed: {:?}",
        tool_result_text(&trusted_write)
    );
    assert_eq!(
        std::fs::read_to_string(&trusted_outside).expect("trusted outside write exists"),
        "trusted outside\n"
    );
    send_tool_call(
        &mut stream,
        1,
        103,
        "bash",
        json!({ "params": { "command": "printf trusted-principal", "timeout": 5000 } }),
    )
    .await;
    let trusted_bash = read_frame_timeout(&mut stream, "trusted direct bash").await;
    assert_eq!(trusted_bash.header.channel, 1);
    assert_eq!(trusted_bash.header.corr, 103);
    assert!(
        !tool_result_is_error(&trusted_bash),
        "trusted direct bash should reach spawn: {:?}",
        tool_result_text(&trusted_bash)
    );

    // Anti-spoof in the other direction: a runner harness is untrusted when the
    // daemon-stamped principal is the MCP facade id.
    bind_route_with_principal(
        &mut stream,
        2,
        201,
        &root1,
        "runner",
        "facade-session",
        Some(subc_mcp_principal()),
    )
    .await;
    let untrusted_in_root = call_tool_frame(
        &mut stream,
        2,
        202,
        "write",
        json!({ "filePath": untrusted_inside.to_string_lossy(), "content": "untrusted inside\n" }),
        "untrusted in-root write",
    )
    .await;
    assert!(
        !tool_result_is_error(&untrusted_in_root),
        "untrusted in-root write should succeed: {:?}",
        tool_result_text(&untrusted_in_root)
    );
    assert_eq!(
        std::fs::read_to_string(&untrusted_inside).expect("untrusted inside write exists"),
        "untrusted inside\n"
    );

    let untrusted_out_root = call_tool_frame(
        &mut stream,
        2,
        203,
        "write",
        json!({ "filePath": untrusted_outside.to_string_lossy(), "content": "blocked\n" }),
        "untrusted out-of-root write",
    )
    .await;
    assert!(tool_result_is_error(&untrusted_out_root));
    assert!(
        tool_result_text(&untrusted_out_root).contains("outside the project root"),
        "untrusted out-of-root write should mention containment: {:?}",
        tool_result_text(&untrusted_out_root)
    );
    let validate_out_root = call_tool_response(
        &mut stream,
        2,
        204,
        "subc_test_validate_path",
        json!({ "path": untrusted_outside.to_string_lossy() }),
        "untrusted out-of-root validate_path",
    )
    .await;
    assert_tool_error_code(
        &validate_out_root,
        "path_outside_root",
        "untrusted out-of-root validate_path",
    );

    for (corr, name, args) in [
        (
            205,
            "bash",
            json!({ "params": { "command": "printf denied", "timeout": 5000 } }),
        ),
        (
            206,
            "bash_status",
            json!({ "params": { "task_id": "bash-denied" } }),
        ),
        (207, "bash_drain_completions", json!({})),
        (
            208,
            "bash_ack_completions",
            json!({ "task_ids": ["bash-denied"] }),
        ),
    ] {
        let denied = call_tool_response(
            &mut stream,
            2,
            corr,
            name,
            args,
            "untrusted bash-family deny",
        )
        .await;
        assert_tool_error_code(&denied, "bash_denied_untrusted", name);
    }

    send_connection_goodbye(&mut stream).await;
}

async fn drive_cross_bind_trust_isolation_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        root2,
        ..
    } = open_fake_daemon_session(input).await;
    let trusted_outside = root2.join("trusted-cross-bind-outside.txt");
    let untrusted_outside = root2.join("untrusted-cross-bind-outside.txt");

    bind_route_with_principal(
        &mut stream,
        1,
        301,
        &root1,
        "opencode",
        "shared-session",
        Some(Principal::Direct),
    )
    .await;
    bind_route_with_principal(
        &mut stream,
        2,
        302,
        &root1,
        "runner",
        "shared-session",
        Some(subc_mcp_principal()),
    )
    .await;

    let trusted_write = call_tool_frame(
        &mut stream,
        1,
        303,
        "write",
        json!({ "filePath": trusted_outside.to_string_lossy(), "content": "trusted unaffected\n" }),
        "trusted cross-bind out-of-root write",
    )
    .await;
    assert!(
        !tool_result_is_error(&trusted_write),
        "trusted cross-bind write should succeed: {:?}",
        tool_result_text(&trusted_write)
    );
    let untrusted_write = call_tool_frame(
        &mut stream,
        2,
        304,
        "write",
        json!({ "filePath": untrusted_outside.to_string_lossy(), "content": "blocked\n" }),
        "untrusted cross-bind out-of-root write",
    )
    .await;
    assert!(tool_result_is_error(&untrusted_write));
    assert!(tool_result_text(&untrusted_write).contains("outside the project root"));

    send_bg_events_subscribe(&mut stream, 2, 305).await;
    send_tool_call(
        &mut stream,
        1,
        306,
        "subc_test_emit_bash_completed",
        json!({ "task_id": "trusted-push-isolation" }),
    )
    .await;
    let pushes = expect_bash_completed_pushes_for_tool(
        &mut stream,
        306,
        "trusted-push-isolation",
        HashSet::from([1]),
    )
    .await;
    assert_eq!(pushes.len(), 1);
    assert_no_bg_event_for(
        &mut stream,
        2,
        305,
        Duration::from_millis(400),
        "untrusted bg_events subscription must stay silent",
    )
    .await;

    let bash_started = call_tool_response(
        &mut stream,
        1,
        307,
        "bash",
        json!({
            "params": {
                "command": "sleep 0.2; printf cross-bind-completion",
                "background": true,
                "timeout": 5000,
            },
        }),
        "trusted background bash",
    )
    .await;
    assert_tool_success(&bash_started, "trusted background bash");
    let task_id = bash_started["task_id"]
        .as_str()
        .expect("trusted background task id")
        .to_string();
    wait_for_bash_completion_without_bg_event(&mut stream, 1, 308, &task_id, 2, 305).await;
    settle_until_bg_completion(&mut stream, 1, 4300, &task_id).await;

    let denied_drain = call_tool_response(
        &mut stream,
        2,
        4400,
        "bash_drain_completions",
        json!({}),
        "untrusted drain denied",
    )
    .await;
    assert_tool_error_code(
        &denied_drain,
        "bash_denied_untrusted",
        "untrusted drain denied",
    );

    let trusted_after_drain = call_tool_response(
        &mut stream,
        1,
        4401,
        "echo",
        json!({ "case": "fast" }),
        "trusted completion still available",
    )
    .await;
    assert_tool_success(&trusted_after_drain, "trusted completion still available");
    assert_bg_completion_matching(&trusted_after_drain, &task_id, "cross-bind-completion");

    let untrusted_after_completion = call_tool_response(
        &mut stream,
        2,
        4402,
        "echo",
        json!({ "case": "fast" }),
        "untrusted finalizer skips completions",
    )
    .await;
    assert_tool_success(
        &untrusted_after_completion,
        "untrusted finalizer skips completions",
    );
    assert!(
        untrusted_after_completion.get("bg_completions").is_none(),
        "untrusted response must not observe trusted bg completions: {untrusted_after_completion:?}"
    );

    send_connection_goodbye(&mut stream).await;
}

async fn drive_detached_session_replay_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        state,
        ..
    } = open_fake_daemon_session(input).await;
    bind_routes_1_and_4(&mut stream, &root1).await;

    // 4d-2b reliable buffer + replay: release route 1, emit a reliable
    // session-1 frame while only sibling session-4 remains, then re-bind the
    // same logical session on route 7 and receive the buffered completion there.
    let reliable_task = "detached-reliable-replay";
    let deferred_base = state.begin_deferred_push_wave();
    send_tool_call(
        &mut stream,
        1,
        431,
        "subc_test_defer_bash_completed",
        json!({ "task_id": reliable_task, "session_id": "session-1" }),
    )
    .await;
    let deferred_response = read_frame_timeout(&mut stream, "defer reliable response").await;
    assert_eq!(deferred_response.header.corr, 431);
    state.wait_until("reliable deferred push waiter", |inner| {
        inner.deferred_push_started == deferred_base + 1
    });
    send_frame(
        &mut stream,
        Frame::build(FrameType::Goodbye, control_flags(), 1, 4310, Vec::new())
            .expect("route 1 goodbye"),
    )
    .await;
    send_tool_call(&mut stream, 1, 4311, "echo", json!({ "case": "fast" })).await;
    expect_error_frame(&mut stream, 1, 4311, "route_not_bound").await;
    state.release_deferred_pushes();
    send_route_bind_with_session(&mut stream, 7, 47, &root1, "session-1").await;
    let replayed =
        expect_route_bind_ack_and_bash_completed_push(&mut stream, 47, reliable_task, 7).await;
    assert_eq!(push_task_id(&replayed), Some(reliable_task));

    // 4d-2b lossy no-channel drop: a detached BashLongRunning frame for the
    // same session is dropped, not buffered/replayed on the next bind.
    let lossy_task = "detached-lossy-drop";
    let deferred_lossy_base = state.begin_deferred_push_wave();
    send_tool_call(
        &mut stream,
        7,
        432,
        "subc_test_defer_bash_long_running",
        json!({ "task_id": lossy_task, "session_id": "session-1" }),
    )
    .await;
    let lossy_deferred_response = read_frame_timeout(&mut stream, "defer lossy response").await;
    assert_eq!(lossy_deferred_response.header.corr, 432);
    state.wait_until("lossy deferred push waiter", |inner| {
        inner.deferred_push_started == deferred_lossy_base + 1
    });
    send_frame(
        &mut stream,
        Frame::build(FrameType::Goodbye, control_flags(), 7, 4320, Vec::new())
            .expect("route 7 goodbye"),
    )
    .await;
    send_tool_call(&mut stream, 7, 4321, "echo", json!({ "case": "fast" })).await;
    expect_error_frame(&mut stream, 7, 4321, "route_not_bound").await;
    state.release_deferred_pushes();
    send_route_bind_with_session(&mut stream, 8, 48, &root1, "session-1").await;
    expect_route_bind_ack_without_task_push(&mut stream, 48, lossy_task).await;
    send_tool_call(
        &mut stream,
        8,
        480,
        "subc_test_emit_bash_completed",
        json!({ "task_id": "lossy-drop-sentinel" }),
    )
    .await;
    let lossy_sentinel = expect_bash_completed_sentinel_without_task_before(
        &mut stream,
        480,
        "lossy-drop-sentinel",
        lossy_task,
        8,
    )
    .await;
    assert_eq!(push_task_id(&lossy_sentinel), Some("lossy-drop-sentinel"));

    // P5b B1 finding (b): a lossy BashLongRunning emitted after its reliable
    // BashCompleted is stale and should be suppressed by the subc edge.
    let stale_long_running_task = "completed-then-stale-long-running";
    send_tool_call(
        &mut stream,
        8,
        433,
        "subc_test_emit_bash_completed_then_long_running",
        json!({ "task_id": stale_long_running_task }),
    )
    .await;
    let stale_completion = expect_bash_completed_pushes_for_tool(
        &mut stream,
        433,
        stale_long_running_task,
        HashSet::from([8]),
    )
    .await;
    assert_eq!(stale_completion.len(), 1);
    send_tool_call(
        &mut stream,
        8,
        434,
        "subc_test_emit_bash_completed",
        json!({ "task_id": "stale-long-running-sentinel" }),
    )
    .await;
    let stale_sentinel = expect_bash_completed_sentinel_without_task_before(
        &mut stream,
        434,
        "stale-long-running-sentinel",
        stale_long_running_task,
        8,
    )
    .await;
    assert_eq!(
        push_task_id(&stale_sentinel),
        Some("stale-long-running-sentinel")
    );

    send_connection_goodbye(&mut stream).await;
}

async fn drive_failed_new_root_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root1,
        failed_root,
        ..
    } = open_fake_daemon_session(input).await;
    bind_route1(&mut stream, &root1).await;

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
    send_tool_call(
        &mut stream,
        1,
        551,
        "subc_test_emit_status",
        json!({ "marker": "failed-no-channel-sentinel", "seq": 1 }),
    )
    .await;
    let sentinel_pushes = expect_status_sentinel_without_marker_before(
        &mut stream,
        551,
        "failed-no-channel-sentinel",
        "failed-no-channel",
        HashSet::from([1]),
    )
    .await;
    assert_eq!(sentinel_pushes.len(), 1);
    assert_eq!(push_seq(&sentinel_pushes[0]), Some(1));
    send_tool_call(&mut stream, 5, 550, "echo", json!({ "case": "fast" })).await;
    expect_error_frame(&mut stream, 5, 550, "route_not_bound").await;

    send_connection_goodbye(&mut stream).await;
}

async fn drive_callgraph_maintenance_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream,
        root2,
        callgraph_root,
        callgraph_file,
        ..
    } = open_fake_daemon_session(input).await;

    send_route_bind(&mut stream, 2, 30, &root2).await;
    expect_route_bind_ack(&mut stream, 30).await;

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
    send_tool_call(&mut stream, 2, 700, "echo", json!({ "case": "fast" })).await;
    let route2_after_maintenance =
        read_frame_timeout(&mut stream, "route 2 read after route 3 maintenance").await;
    assert_eq!(route2_after_maintenance.header.channel, 2);
    assert_tool_project_root(&route2_after_maintenance, &root2);

    send_connection_goodbye(&mut stream).await;
}

async fn drive_manifest_reachability_daemon(input: FakeDaemonInput) {
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    prepare_manifest_reachability_fixture(&root1);

    send_route_bind_with_doc(
        &mut stream,
        1,
        10,
        &root1,
        json!({
            "callgraph_store": true,
            "search_index": false,
            "semantic_search": false,
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 10).await;

    let glob_text =
        expect_manifest_tool_success(&mut stream, 100, "glob", json!({ "pattern": "src/*.ts" }))
            .await;
    assert!(glob_text.contains("sample.ts"), "glob text: {glob_text:?}");

    let zoom_text = expect_manifest_tool_success(
        &mut stream,
        101,
        "zoom",
        json!({ "filePath": "src/sample.ts", "symbols": "sample" }),
    )
    .await;
    assert!(
        zoom_text.contains("function sample"),
        "zoom text: {zoom_text:?}"
    );

    let callgraph_text = poll_manifest_callgraph_until_ready(
        &mut stream,
        110,
        json!({
            "op": "callers",
            "filePath": "src/graph.rs",
            "symbol": "callee",
            "depth": 1,
        }),
    )
    .await;
    assert!(
        callgraph_text.contains("caller"),
        "callgraph text: {callgraph_text:?}"
    );

    let conflicts_text =
        expect_manifest_tool_success(&mut stream, 200, "conflicts", json!({})).await;
    assert!(
        conflicts_text.contains("No merge conflicts found"),
        "conflicts text: {conflicts_text:?}"
    );

    let ast_search_text = expect_manifest_tool_success(
        &mut stream,
        201,
        "ast_search",
        json!({
            "pattern": "console.log($MSG)",
            "lang": "typescript",
            "paths": ["src/sample.ts"],
        }),
    )
    .await;
    assert!(
        ast_search_text.contains("Found 1 match"),
        "ast_search text: {ast_search_text:?}"
    );

    expect_manifest_tool_success(
        &mut stream,
        300,
        "apply_patch",
        json!({
            "patchText": "*** Begin Patch\n*** Update File: patch-target.txt\n@@\n-old\n+new\n*** End Patch\n"
        }),
    )
    .await;
    expect_manifest_tool_success(
        &mut stream,
        301,
        "ast_replace",
        json!({
            "pattern": "console.log($MSG)",
            "rewrite": "logger.info($MSG)",
            "lang": "typescript",
            "paths": ["src/sample.ts"],
            "dryRun": true,
        }),
    )
    .await;
    expect_manifest_tool_success(
        &mut stream,
        302,
        "delete",
        json!({ "files": ["delete-target.txt"] }),
    )
    .await;
    expect_manifest_tool_success(
        &mut stream,
        303,
        "move",
        json!({ "filePath": "move-source.txt", "destination": "moved/move-target.txt" }),
    )
    .await;
    expect_manifest_tool_success(
        &mut stream,
        304,
        "import",
        json!({
            "op": "add",
            "filePath": root1.join("src/import-target.ts").to_string_lossy(),
            "module": "node:path",
            "names": ["join"],
        }),
    )
    .await;
    expect_manifest_tool_reaches_dispatch(
        &mut stream,
        305,
        "refactor",
        json!({
            "op": "move",
            "filePath": "src/missing.ts",
            "symbol": "missingSymbol",
            "destination": "src/missing-dest.ts",
        }),
    )
    .await;
    expect_manifest_tool_success(&mut stream, 306, "safety", json!({ "op": "list" })).await;

    send_connection_goodbye(&mut stream).await;
}

fn prepare_manifest_reachability_fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).expect("create manifest reachability src dir");
    std::fs::write(
        root.join("src/sample.ts"),
        "export function sample() { return 'sample'; }\nconsole.log('ast');\n",
    )
    .expect("write sample fixture");
    std::fs::write(
        root.join("src/graph.rs"),
        "pub fn caller() { callee(); }\npub fn callee() {}\n",
    )
    .expect("write callgraph fixture");
    std::fs::write(root.join("src/import-target.ts"), "const value = 1;\n")
        .expect("write import fixture");
    std::fs::write(root.join("patch-target.txt"), "old\n").expect("write patch fixture");
    std::fs::write(root.join("delete-target.txt"), "delete me\n").expect("write delete fixture");
    std::fs::write(root.join("move-source.txt"), "move me\n").expect("write move fixture");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(root)
        .status()
        .expect("run git init for conflicts fixture");
    assert!(status.success(), "git init failed for conflicts fixture");
}

async fn expect_manifest_tool_success(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    name: &str,
    arguments: Value,
) -> String {
    let (text, is_error) = expect_manifest_tool_frame(stream, corr, name, arguments).await;
    assert!(!is_error, "{name} should succeed over subc, got {text:?}");
    text
}

async fn expect_manifest_tool_reaches_dispatch(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    name: &str,
    arguments: Value,
) -> String {
    expect_manifest_tool_frame(stream, corr, name, arguments)
        .await
        .0
}

async fn expect_manifest_tool_frame(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    name: &str,
    arguments: Value,
) -> (String, bool) {
    send_tool_call(stream, 1, corr, name, arguments).await;
    let frame = read_frame_timeout(stream, "manifest reachability tool response").await;
    assert_eq!(frame.header.channel, 1, "unexpected channel for {name}");
    assert_eq!(frame.header.corr, corr, "unexpected corr for {name}");
    let text = tool_result_text(&frame);
    assert!(
        !text.contains("not in the AFT tool manifest") && !text.contains("unknown_tool"),
        "{name} was rejected before dispatch: {text:?}"
    );
    (text, tool_result_is_error(&frame))
}

async fn poll_manifest_callgraph_until_ready(
    stream: &mut tokio::net::TcpStream,
    first_corr: u64,
    arguments: Value,
) -> String {
    for attempt in 0..80 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let text = expect_manifest_tool_reaches_dispatch(
            stream,
            first_corr + attempt,
            "callgraph",
            arguments.clone(),
        )
        .await;
        if text.contains("caller") {
            return text;
        }
        assert!(
            text.to_ascii_lowercase().contains("building"),
            "callgraph should build or become ready, got {text:?}"
        );
    }
    panic!("callgraph store did not become ready for manifest reachability test");
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
    send_route_bind_with_session_and_doc(
        stream,
        route_channel,
        corr,
        root,
        &format!("session-{route_channel}"),
        doc,
    )
    .await;
}

async fn send_route_bind_with_session(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
    session: &str,
) {
    send_route_bind_with_session_and_doc(
        stream,
        route_channel,
        corr,
        root,
        session,
        json!({
            "callgraph_store": false,
            "search_index": false,
            "semantic_search": false,
        }),
    )
    .await;
}

async fn send_route_bind_with_session_and_doc(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
    session: &str,
    doc: Value,
) {
    send_route_bind_with_harness_session_principal_and_doc(
        stream,
        route_channel,
        corr,
        root,
        "opencode",
        session,
        Some(Principal::Direct),
        doc,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn send_route_bind_with_harness_session_principal_and_doc(
    stream: &mut tokio::net::TcpStream,
    route_channel: u16,
    corr: u64,
    root: &std::path::Path,
    harness: &str,
    session: &str,
    principal: Option<Principal>,
    doc: Value,
) {
    // Config is read by AFT from <root>/.cortexkit/aft.jsonc, NOT from the wire
    // (the wire `config` is ignored since the unification). Write the bind's doc
    // (real config fields + any subc_test_* directives the fake dispatch reads)
    // to the project config file so it reaches handle_control_request's local
    // read. The wire `config` is still attached but no longer consulted by
    // production; keeping it asserts it's harmlessly ignored.
    let project_cfg = root.join(".cortexkit").join("aft.jsonc");
    std::fs::create_dir_all(project_cfg.parent().expect("cortexkit dir parent"))
        .expect("create .cortexkit dir");
    std::fs::write(
        &project_cfg,
        serde_json::to_string(&doc).expect("serialize bind doc"),
    )
    .expect("write project config");

    let request = ModuleControlRequest::RouteBind {
        route_channel,
        target: RouteTarget::ToolProvider {
            module_id: "aft".to_string(),
        },
        identity: BindIdentity {
            project_root: root.to_path_buf(),
            harness: harness.to_string(),
            session: session.to_string(),
        },
        principal,
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

async fn send_bash_background(stream: &mut tokio::net::TcpStream, corr: u64, command: &str) {
    send_tool_call(
        stream,
        1,
        corr,
        "bash",
        json!({
            "params": {
                "command": command,
                "background": true,
                "timeout": 10_000,
            },
        }),
    )
    .await;
}

async fn send_bg_events_subscribe(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            serde_json::to_vec(&json!({ "op": "bg_events" })).expect("bg_events body"),
        )
        .expect("bg_events subscribe frame"),
    )
    .await;
}

async fn send_bg_events_cancel(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64) {
    send_frame(
        stream,
        Frame::build(
            FrameType::Cancel,
            Flags::new(false, Priority::Interactive, false),
            channel,
            corr,
            Vec::new(),
        )
        .expect("bg_events cancel frame"),
    )
    .await;
}

async fn send_frame(stream: &mut tokio::net::TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.expect("write frame");
}

async fn read_any_frame_timeout(stream: &mut tokio::net::TcpStream, label: &str) -> Frame {
    tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .expect("read frame")
        .unwrap_or_else(|| panic!("EOF waiting for {label}"))
}

async fn read_frame_timeout(stream: &mut tokio::net::TcpStream, label: &str) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(30);
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

async fn read_frame_within(
    stream: &mut tokio::net::TcpStream,
    timeout: Duration,
    label: &str,
) -> Option<Frame> {
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(now);
        let frame = match tokio::time::timeout(remaining, read_frame(stream)).await {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) => panic!("EOF waiting for {label}"),
            Ok(Err(error)) => panic!("read frame for {label}: {error}"),
            Err(_) => return None,
        };
        if frame.header.ty != FrameType::Push {
            return Some(frame);
        }
    }
}

async fn assert_no_response_frame_within(
    stream: &mut tokio::net::TcpStream,
    timeout: Duration,
    label: &str,
) {
    if let Some(frame) = read_frame_within(stream, timeout, label).await {
        panic!(
            "unexpected non-push frame while waiting for {label}: {:?}",
            frame.header
        );
    }
}

async fn read_any_frame_until(
    stream: &mut tokio::net::TcpStream,
    deadline: Instant,
    label: &str,
) -> Option<Frame> {
    let now = Instant::now();
    if now >= deadline {
        return None;
    }
    let remaining = deadline.saturating_duration_since(now);
    match tokio::time::timeout(remaining, read_frame(stream)).await {
        Ok(Ok(Some(frame))) => Some(frame),
        Ok(Ok(None)) => panic!("EOF waiting for {label}"),
        Ok(Err(error)) => panic!("read frame for {label}: {error}"),
        Err(_) => None,
    }
}

fn is_expected_bg_event_stream_data(frame: &Frame, channel: u16, corr: u64) -> bool {
    if frame.header.ty != FrameType::StreamData {
        return false;
    }
    assert_eq!(
        frame.header.channel, channel,
        "unexpected StreamData channel"
    );
    assert_eq!(frame.header.corr, corr, "unexpected StreamData corr");
    let body: Value = serde_json::from_slice(&frame.body).expect("bg_events StreamData body");
    assert_eq!(body.get("op").and_then(Value::as_str), Some("bg_events"));
    true
}

fn assert_no_bg_events_terminal(frame: &Frame, channel: u16, corr: u64, label: &str) {
    if frame.header.channel == channel
        && frame.header.corr == corr
        && matches!(
            frame.header.ty,
            FrameType::Response | FrameType::Error | FrameType::StreamEnd
        )
    {
        panic!(
            "bg_events subscribe ended while waiting for {label}: {:?}",
            frame.header
        );
    }
}

async fn read_tool_response_allowing_bg_events(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    bg_channel: u16,
    bg_corr: u64,
    label: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let frame = read_any_frame_until(stream, deadline, label)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for {label}"));
        if is_expected_bg_event_stream_data(&frame, bg_channel, bg_corr) {
            continue;
        }
        assert_no_bg_events_terminal(&frame, bg_channel, bg_corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            FrameType::Response if frame.header.corr == corr => return tool_response_json(&frame),
            other => panic!("unexpected frame while waiting for {label}: {other:?}"),
        }
    }
}

async fn read_tool_response_rejecting_bg_events(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    bg_channel: u16,
    bg_corr: u64,
    label: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let frame = read_any_frame_until(stream, deadline, label)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for {label}"));
        if is_expected_bg_event_stream_data(&frame, bg_channel, bg_corr) {
            panic!("unexpected bg_events StreamData while waiting for {label}");
        }
        assert_no_bg_events_terminal(&frame, bg_channel, bg_corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            FrameType::Response if frame.header.corr == corr => return tool_response_json(&frame),
            other => panic!("unexpected frame while waiting for {label}: {other:?}"),
        }
    }
}

async fn collect_tool_responses_allowing_bg_events(
    stream: &mut tokio::net::TcpStream,
    expected_corrs: HashSet<u64>,
    bg_channel: u16,
    bg_corr: u64,
    label: &str,
) -> (HashMap<u64, Value>, Vec<Instant>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut responses = HashMap::new();
    let mut bg_event_times = Vec::new();
    while responses.len() < expected_corrs.len() {
        let frame = read_any_frame_until(stream, deadline, label)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for {label}"));
        if is_expected_bg_event_stream_data(&frame, bg_channel, bg_corr) {
            bg_event_times.push(Instant::now());
            continue;
        }
        assert_no_bg_events_terminal(&frame, bg_channel, bg_corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            FrameType::Response if expected_corrs.contains(&frame.header.corr) => {
                responses.insert(frame.header.corr, tool_response_json(&frame));
            }
            other => panic!("unexpected frame while waiting for {label}: {other:?}"),
        }
    }
    (responses, bg_event_times)
}

fn assert_bg_events_are_coalesced(event_times: &[Instant]) {
    for pair in event_times.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= Duration::from_millis(150),
            "bg_events StreamData frames should be coalesced per tick: {event_times:?}"
        );
    }
}

async fn wait_for_bg_event(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    timeout: Duration,
    label: &str,
) -> Instant {
    let deadline = Instant::now() + timeout;
    loop {
        let frame = read_any_frame_until(stream, deadline, label)
            .await
            .unwrap_or_else(|| panic!("timed out waiting for {label}"));
        if is_expected_bg_event_stream_data(&frame, channel, corr) {
            return Instant::now();
        }
        assert_no_bg_events_terminal(&frame, channel, corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            other => panic!("unexpected frame while waiting for {label}: {other:?}"),
        }
    }
}

async fn assert_no_bg_event_for(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    duration: Duration,
    label: &str,
) {
    let deadline = Instant::now() + duration;
    while let Some(frame) = read_any_frame_until(stream, deadline, label).await {
        if is_expected_bg_event_stream_data(&frame, channel, corr) {
            panic!("unexpected bg_events StreamData while waiting for {label}");
        }
        assert_no_bg_events_terminal(&frame, channel, corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            other => panic!("unexpected frame while waiting for {label}: {other:?}"),
        }
    }
}

async fn drain_bg_events_for(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    duration: Duration,
    label: &str,
) -> usize {
    let deadline = Instant::now() + duration;
    let mut count = 0;
    while let Some(frame) = read_any_frame_until(stream, deadline, label).await {
        if is_expected_bg_event_stream_data(&frame, channel, corr) {
            count += 1;
            continue;
        }
        assert_no_bg_events_terminal(&frame, channel, corr, label);
        match frame.header.ty {
            FrameType::Push => {}
            other => panic!("unexpected frame while draining {label}: {other:?}"),
        }
    }
    count
}

async fn expect_bg_events_stream_end(stream: &mut tokio::net::TcpStream, channel: u16, corr: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let frame = read_any_frame_until(stream, deadline, "bg_events StreamEnd")
            .await
            .unwrap_or_else(|| panic!("timed out waiting for bg_events StreamEnd"));
        match frame.header.ty {
            FrameType::StreamEnd
                if frame.header.channel == channel && frame.header.corr == corr =>
            {
                assert!(frame.body.is_empty(), "StreamEnd body should be empty");
                return;
            }
            FrameType::Push => {}
            FrameType::StreamData
                if frame.header.channel == channel && frame.header.corr == corr =>
            {
                panic!("unexpected bg_events StreamData before StreamEnd")
            }
            other => panic!("unexpected frame while waiting for bg_events StreamEnd: {other:?}"),
        }
    }
}

async fn drain_bg_completions_until(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    expected_task_ids: &[String],
    bg_channel: u16,
    bg_corr: u64,
) -> Value {
    let expected: HashSet<String> = expected_task_ids.iter().cloned().collect();
    let mut last_response = Value::Null;
    for attempt in 0_u64..300 {
        let corr = first_corr + attempt;
        send_tool_call(stream, channel, corr, "bash_drain_completions", json!({})).await;
        let response = read_tool_response_allowing_bg_events(
            stream,
            corr,
            bg_channel,
            bg_corr,
            "drain bg completions",
        )
        .await;
        let seen: HashSet<String> = response["bg_completions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|completion| {
                completion
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect();
        if expected.is_subset(&seen) {
            return response;
        }
        last_response = response;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "bg completions never contained expected task ids: {expected:?}; last response: {last_response:?}"
    );
}

async fn wait_for_bash_completion_without_bg_event(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    task_id: &str,
    bg_channel: u16,
    bg_corr: u64,
) {
    for attempt in 0_u64..120 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let corr = first_corr + attempt;
        send_tool_call(
            stream,
            channel,
            corr,
            "bash_status",
            json!({ "params": { "task_id": task_id } }),
        )
        .await;
        let response = read_tool_response_rejecting_bg_events(
            stream,
            corr,
            bg_channel,
            bg_corr,
            "post-cancel bash status",
        )
        .await;
        assert_no_finalizer_fields(&response);
        if response["success"].as_bool() == Some(true)
            && response
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(is_terminal_status)
        {
            return;
        }
    }
    panic!("background bash task did not complete after bg_events cancel: {task_id}");
}

fn tool_result_body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).expect("tool result body")
}

fn tool_result_text(frame: &Frame) -> String {
    let body = tool_result_body(frame);
    body["content"][0]["text"]
        .as_str()
        .expect("tool result text")
        .to_string()
}

fn tool_result_is_error(frame: &Frame) -> bool {
    tool_result_body(frame)["isError"]
        .as_bool()
        .unwrap_or(false)
}

fn extract_bash_task_id(text: &str) -> String {
    let start = text
        .find("bash-")
        .unwrap_or_else(|| panic!("no bash task id in {text:?}"));
    let tail = &text[start..];
    let end = tail
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .unwrap_or(tail.len());
    tail[..end].to_string()
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

/// Like `expect_error_frame`, but tolerates one benign late `RouteBindAck` for a
/// bind whose `Goodbye` raced its configure completion. When a `Goodbye` arrives
/// while a slow bind's configure is in flight, the transport `select!` may pick
/// the completion before the `Goodbye`: it then emits an ack on channel 0 for the
/// abandoned corr and immediately tears the route down. Either ordering leaves the
/// route unbound, so the only durable assertion is the subsequent
/// `route_not_bound` error — the optional ack is correlated to a corr the gateway
/// already abandoned and ignored. At most one such ack may appear.
async fn expect_error_frame_skipping_optional_ack(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    code: &str,
    optional_ack_corr: u64,
) {
    let frame = read_frame_timeout(stream, "Error frame (maybe after late ack)").await;
    let frame = if frame.header.ty == FrameType::Response
        && frame.header.channel == 0
        && frame.header.corr == optional_ack_corr
    {
        // Benign late ack for the cancelled bind — skip it and read the real error.
        read_frame_timeout(stream, "Error frame after late ack").await
    } else {
        frame
    };
    assert_error_frame(&frame, channel, corr, code);
}

fn assert_error_frame(frame: &Frame, channel: u16, corr: u64, code: &str) {
    if frame.header.ty != FrameType::Error
        || frame.header.channel != channel
        || frame.header.corr != corr
    {
        panic!(
            "expect_error_frame(channel={channel}, corr={corr}, code={code}): got ty={:?} channel={} corr={} body={}",
            frame.header.ty,
            frame.header.channel,
            frame.header.corr,
            String::from_utf8_lossy(&frame.body),
        );
    }
    let body: Value = serde_json::from_slice(&frame.body).expect("error body");
    assert_eq!(body.get("code").and_then(Value::as_str), Some(code));
}

async fn expect_error_frame(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    corr: u64,
    code: &str,
) {
    let frame = read_frame_timeout(stream, "Error frame").await;
    assert_error_frame(&frame, channel, corr, code);
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

fn push_task_id(body: &Value) -> Option<&str> {
    body.get("task_id").and_then(Value::as_str)
}

fn push_type(body: &Value) -> Option<&str> {
    body.get("type").and_then(Value::as_str)
}

fn configure_warning_message_from_push(body: &Value) -> Option<&str> {
    body.get("warnings")
        .and_then(Value::as_array)
        .and_then(|warnings| warnings.first())
        .and_then(|warning| warning.get("message"))
        .and_then(Value::as_str)
}

async fn expect_configure_warning_pushes(
    stream: &mut tokio::net::TcpStream,
    expected_channels: HashSet<u16>,
    expected_root: &std::path::Path,
    expected_message: &str,
    expected_session: &str,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let expected_root = expected_root.to_string_lossy().into_owned();
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for configure_warnings push"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for configure_warnings push"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for configure_warnings push"));
        if frame.header.ty != FrameType::Push {
            panic!(
                "unexpected non-Push frame while waiting for configure_warnings: {:?}",
                frame.header.ty
            );
        }
        assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
        let body: Value = serde_json::from_slice(&frame.body).expect("push body");
        if push_type(&body) == Some("configure_warnings")
            && body.get("project_root").and_then(Value::as_str) == Some(expected_root.as_str())
            && configure_warning_message_from_push(&body) == Some(expected_message)
        {
            assert_eq!(
                body.get("session_id").and_then(Value::as_str),
                Some(expected_session),
                "configure_warnings push should carry initiating session id"
            );
            assert!(
                expected_channels.contains(&frame.header.channel),
                "configure_warnings push leaked to unexpected channel {}",
                frame.header.channel
            );
            channels.insert(frame.header.channel);
            pushes.push(body);
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_tool_response_without_configure_warning_for_message(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    expected_root: &std::path::Path,
    expected_message: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let expected_root = expected_root.to_string_lossy().into_owned();
    loop {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for sentinel response without configure warning"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for sentinel response without configure warning")
            })
            .expect("read frame")
            .unwrap_or_else(|| {
                panic!("EOF waiting for sentinel response without configure warning")
            });
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                let matches_warning = push_type(&body) == Some("configure_warnings")
                    && body.get("project_root").and_then(Value::as_str)
                        == Some(expected_root.as_str())
                    && configure_warning_message_from_push(&body) == Some(expected_message);
                assert!(
                    !matches_warning,
                    "configure_warnings push leaked before sentinel response on channel {}: {body:?}",
                    frame.header.channel
                );
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "sentinel tool response should succeed: {response:?}"
                );
                return;
            }
            other => panic!(
                "unexpected frame while waiting for sentinel response without configure warning: {other:?}"
            ),
        }
    }
}

async fn expect_bash_completed_pushes_for_tool(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    task_id: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut response_seen = false;
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while !response_seen || channels != expected_channels {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for bash push {task_id}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for bash push {task_id}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for bash push {task_id}"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_type(&body) == Some("bash_completed")
                    && push_task_id(&body) == Some(task_id)
                {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "bash push {task_id} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    channels.insert(frame.header.channel);
                    pushes.push(body);
                } else if push_type(&body) == Some("bash_long_running")
                    && push_task_id(&body) == Some(task_id)
                {
                    panic!(
                        "unexpected long-running push for task {task_id} while waiting for completion on channel {}: {body:?}",
                        frame.header.channel
                    );
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
            other => panic!("unexpected frame while waiting for bash push {task_id}: {other:?}"),
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_route_bind_ack_and_status_pushes(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    marker: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ack_seen = false;
    let mut status_pushes = Vec::new();
    let mut status_channels = HashSet::new();

    while !ack_seen || status_channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for RouteBindAck {corr} and marker {marker}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for RouteBindAck {corr} and marker {marker}")
            })
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for RouteBindAck {corr} and marker {marker}"));
        match frame.header.ty {
            FrameType::Response if frame.header.corr == corr => {
                assert_eq!(frame.header.channel, 0);
                let ack: ModuleControlResponse =
                    serde_json::from_slice(&frame.body).expect("ack body");
                assert_eq!(ack, ModuleControlResponse::RouteBindAck {});
                ack_seen = true;
            }
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_marker(&body) == Some(marker) {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "status push {marker} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    status_channels.insert(frame.header.channel);
                    status_pushes.push(body);
                }
            }
            other => panic!(
                "unexpected frame while waiting for RouteBindAck {corr} and marker {marker}: {other:?}"
            ),
        }
    }

    assert_eq!(status_channels, expected_channels);
    status_pushes
}

async fn expect_route_bind_ack_status_and_bash_completed_pushes(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    marker: &str,
    task_id: &str,
    expected_channels: HashSet<u16>,
) -> (Vec<Value>, Vec<Value>) {
    // 90s: the lossy-pressure test waits through a 2048-frame burst here; under
    // contended CI the burst is load-proportionally slow (memory 6987).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut ack_seen = false;
    let mut status_pushes = Vec::new();
    let mut status_channels = HashSet::new();
    let mut bash_pushes = Vec::new();
    let mut bash_channels = HashSet::new();

    while !ack_seen || status_channels != expected_channels || bash_channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for RouteBindAck {corr}, marker {marker}, and bash push {task_id}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for RouteBindAck {corr}, marker {marker}, and bash push {task_id}"
                )
            })
            .expect("read frame")
            .unwrap_or_else(|| {
                panic!(
                    "EOF waiting for RouteBindAck {corr}, marker {marker}, and bash push {task_id}"
                )
            });
        match frame.header.ty {
            FrameType::Response if frame.header.corr == corr => {
                assert_eq!(frame.header.channel, 0);
                let ack: ModuleControlResponse =
                    serde_json::from_slice(&frame.body).expect("ack body");
                assert_eq!(ack, ModuleControlResponse::RouteBindAck {});
                ack_seen = true;
            }
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_marker(&body) == Some(marker) {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "status push {marker} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    status_channels.insert(frame.header.channel);
                    status_pushes.push(body);
                } else if push_type(&body) == Some("bash_completed")
                    && push_task_id(&body) == Some(task_id)
                {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "bash push {task_id} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    bash_channels.insert(frame.header.channel);
                    bash_pushes.push(body);
                }
            }
            other => panic!(
                "unexpected frame while waiting for RouteBindAck {corr}, marker {marker}, and bash push {task_id}: {other:?}"
            ),
        }
    }

    assert_eq!(status_channels, expected_channels);
    assert_eq!(bash_channels, expected_channels);
    (status_pushes, bash_pushes)
}

async fn expect_bash_completed_sentinel_without_task_before(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    sentinel_task_id: &str,
    forbidden_task_id: &str,
    expected_channel: u16,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut response_seen = false;
    let mut sentinel_seen = None;

    while !response_seen || sentinel_seen.is_none() {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for sentinel bash push {sentinel_task_id}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for sentinel bash push {sentinel_task_id}")
            })
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for sentinel bash push {sentinel_task_id}"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if sentinel_seen.is_none() {
                    assert_ne!(
                        push_task_id(&body),
                        Some(forbidden_task_id),
                        "forbidden push for task {forbidden_task_id} was queued before sentinel {sentinel_task_id} on channel {}: {body:?}",
                        frame.header.channel
                    );
                }
                if push_type(&body) == Some("bash_completed")
                    && push_task_id(&body) == Some(sentinel_task_id)
                {
                    assert_eq!(frame.header.channel, expected_channel);
                    sentinel_seen = Some(body);
                }
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "sentinel bash emit response should succeed: {response:?}"
                );
                response_seen = true;
            }
            other => panic!(
                "unexpected frame while waiting for sentinel bash push {sentinel_task_id}: {other:?}"
            ),
        }
    }

    sentinel_seen.expect("sentinel bash push seen")
}

async fn expect_route_bind_ack_and_bash_completed_push(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    task_id: &str,
    expected_channel: u16,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ack_seen = false;
    let mut push_seen = None;

    while !ack_seen || push_seen.is_none() {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for RouteBindAck {corr} and replay {task_id}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for RouteBindAck {corr} and replay {task_id}")
            })
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for RouteBindAck {corr} and replay {task_id}"));
        match frame.header.ty {
            FrameType::Response if frame.header.corr == corr => {
                assert_eq!(frame.header.channel, 0);
                ack_seen = true;
            }
            FrameType::Push => {
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_type(&body) == Some("bash_completed") && push_task_id(&body) == Some(task_id) {
                    assert_eq!(frame.header.channel, expected_channel);
                    push_seen = Some(body);
                }
            }
            other => panic!(
                "unexpected frame while waiting for RouteBindAck {corr} and replay {task_id}: {other:?}"
            ),
        }
    }

    push_seen.expect("push seen")
}

async fn expect_route_bind_ack_without_task_push(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    task_id: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for RouteBindAck {corr}");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for RouteBindAck {corr}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for RouteBindAck {corr}"));
        match frame.header.ty {
            FrameType::Response if frame.header.corr == corr => {
                assert_eq!(frame.header.channel, 0);
                return;
            }
            FrameType::Push => {
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                assert_ne!(
                    push_task_id(&body),
                    Some(task_id),
                    "lossy task {task_id} should not replay on RouteBind: {body:?}"
                );
            }
            other => panic!("unexpected frame while waiting for RouteBindAck {corr}: {other:?}"),
        }
    }
}

async fn expect_watcher_stale_status_pushes_for_tool(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    expected_channels: HashSet<u16>,
    expected_root: &std::path::Path,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let expected_root = expected_root.to_string_lossy().into_owned();
    let mut response_seen = false;
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while !response_seen || channels != expected_channels {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for watcher stale push");
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for watcher stale push"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for watcher stale push"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                let snapshot = body.get("snapshot").unwrap_or(&Value::Null);
                let stale = snapshot
                    .get("status_bar")
                    .and_then(|status_bar| status_bar.get("tier2_stale"))
                    .and_then(Value::as_bool)
                    == Some(true);
                let root_matches = snapshot.get("project_root").and_then(Value::as_str)
                    == Some(expected_root.as_str());
                if push_type(&body) == Some("status_changed") && stale && root_matches {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "watcher stale push leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    channels.insert(frame.header.channel);
                    pushes.push(body);
                }
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "watcher injection response should succeed: {response:?}"
                );
                response_seen = true;
            }
            other => panic!("unexpected frame while waiting for watcher stale push: {other:?}"),
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_semantic_refresh_status_pushes_for_tool(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    expected_channels: HashSet<u16>,
    expected_root: &std::path::Path,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let expected_root = expected_root.to_string_lossy().into_owned();
    let mut response_seen = false;
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while !response_seen || channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for semantic refresh status push"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for semantic refresh status push"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for semantic refresh status push"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                let snapshot = body.get("snapshot").unwrap_or(&Value::Null);
                let refreshing = snapshot
                    .get("semantic_index")
                    .and_then(|semantic| semantic.get("refreshing_count"))
                    .and_then(Value::as_u64)
                    == Some(1);
                let root_matches = snapshot.get("project_root").and_then(Value::as_str)
                    == Some(expected_root.as_str());
                if push_type(&body) == Some("status_changed") && refreshing && root_matches {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "semantic refresh push leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    channels.insert(frame.header.channel);
                    pushes.push(body);
                }
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "semantic refresh injection response should succeed: {response:?}"
                );
                response_seen = true;
            }
            other => panic!("unexpected frame while waiting for semantic refresh push: {other:?}"),
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_status_pushes_for_tool(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    marker: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
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

async fn expect_status_pushes_for_marker_seq(
    stream: &mut tokio::net::TcpStream,
    marker: &str,
    seq: u64,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    // 90s: waits for the coalesced latest of a 2048-frame burst; load-heavy on
    // contended CI runners (memory 6987).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut pushes = Vec::new();
    let mut channels = HashSet::new();

    while channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for push marker {marker} seq {seq}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for push marker {marker} seq {seq}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for push marker {marker} seq {seq}"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if push_marker(&body) == Some(marker) && push_seq(&body) == Some(seq) {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "status push {marker} seq {seq} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    channels.insert(frame.header.channel);
                    pushes.push(body);
                }
            }
            other => panic!(
                "unexpected frame while waiting for push marker {marker} seq {seq}: {other:?}"
            ),
        }
    }

    assert_eq!(channels, expected_channels);
    pushes
}

async fn expect_status_sentinel_without_marker_before(
    stream: &mut tokio::net::TcpStream,
    corr: u64,
    sentinel_marker: &str,
    forbidden_marker: &str,
    expected_channels: HashSet<u16>,
) -> Vec<Value> {
    // 90s: a longer patience ceiling only matters when frames are slow under
    // load; it returns early on success, so it's safe for all callers and gives
    // the lossy-pressure burst path headroom on contended CI (memory 6987).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut response_seen = false;
    let mut sentinel_pushes = Vec::new();
    let mut sentinel_channels = HashSet::new();

    while !response_seen || sentinel_channels != expected_channels {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for sentinel marker {sentinel_marker}"
        );
        let remaining = deadline.saturating_duration_since(now);
        let frame = tokio::time::timeout(remaining, read_frame(stream))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for sentinel marker {sentinel_marker}"))
            .expect("read frame")
            .unwrap_or_else(|| panic!("EOF waiting for sentinel marker {sentinel_marker}"));
        match frame.header.ty {
            FrameType::Push => {
                assert_eq!(frame.header.corr, 0, "Push frames are server-initiated");
                let body: Value = serde_json::from_slice(&frame.body).expect("push body");
                if sentinel_channels != expected_channels {
                    assert_ne!(
                        push_marker(&body),
                        Some(forbidden_marker),
                        "forbidden status marker {forbidden_marker} was queued before sentinel {sentinel_marker} on channel {}: {body:?}",
                        frame.header.channel
                    );
                }
                if push_marker(&body) == Some(sentinel_marker) {
                    assert!(
                        expected_channels.contains(&frame.header.channel),
                        "sentinel status push {sentinel_marker} leaked to unexpected channel {}",
                        frame.header.channel
                    );
                    sentinel_channels.insert(frame.header.channel);
                    sentinel_pushes.push(body);
                }
            }
            FrameType::Response if frame.header.corr == corr => {
                let response = tool_response_json(&frame);
                assert!(
                    response["success"].as_bool().unwrap_or(false),
                    "sentinel status emit response should succeed: {response:?}"
                );
                response_seen = true;
            }
            other => panic!(
                "unexpected frame while waiting for sentinel marker {sentinel_marker}: {other:?}"
            ),
        }
    }

    assert_eq!(sentinel_channels, expected_channels);
    sentinel_pushes
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

/// Read `tool_corrs.len() + 1` Response frames and demux them by channel:
/// tool responses arrive on their route channel (>= 1) and carry a
/// content/text body, while a RouteBindAck arrives on the control channel (0)
/// and has none. When a same-root configure only starts after in-flight reads
/// drain, the ack and those read responses are emitted concurrently and can
/// reach the shared stream in any order, so a positional reader that assumes
/// "tool responses first, ack last" can mis-read the ack as a tool response.
/// A real gateway routes frames by channel/corr; this helper does the same so
/// the assertion is order-independent. Returns the set of tool-response corrs.
async fn collect_tool_responses_and_route_bind_ack(
    stream: &mut tokio::net::TcpStream,
    tool_corrs: &[u64],
    ack_corr: u64,
) -> HashSet<u64> {
    let mut tool_seen = HashSet::new();
    let mut ack_seen = false;
    for _ in 0..tool_corrs.len() + 1 {
        let frame = read_frame_timeout(stream, "tool response or route-bind ack").await;
        assert_eq!(frame.header.ty, FrameType::Response);
        if frame.header.channel == 0 {
            assert_eq!(
                frame.header.corr, ack_corr,
                "unexpected channel-0 control frame corr"
            );
            let ack: ModuleControlResponse = serde_json::from_slice(&frame.body).expect("ack body");
            assert_eq!(ack, ModuleControlResponse::RouteBindAck {});
            assert!(!ack_seen, "duplicate route-bind ack {ack_corr}");
            ack_seen = true;
        } else {
            assert!(
                tool_response_json(&frame)["success"]
                    .as_bool()
                    .unwrap_or(false),
                "expected successful tool response"
            );
            tool_seen.insert(frame.header.corr);
        }
    }
    assert!(ack_seen, "route-bind ack {ack_corr} not received");
    tool_seen
}

fn tool_response_json(frame: &Frame) -> Value {
    let body: Value = serde_json::from_slice(&frame.body).expect("tool result body");
    let text = body["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    serde_json::from_str(text).expect("embedded AFT response JSON")
}

/// Wait until a finished bg task's completion is actually attached in-band to a
/// tool response, then return.
///
/// `wait_for_bash_completion` returns as soon as `bash_status` reports the task
/// terminal, but a task's status flips terminal (under the task state lock)
/// slightly BEFORE its completion is enqueued into the drainable queue: the
/// enqueue runs after the lock releases and does heavy work (terminal render,
/// token counting, DB write). So the in-band `bg_completions` mirror on the next
/// tool response is EVENTUAL, not immediate — the reliable `BashCompleted` push
/// is the immediate delivery path. A consumer that reads the very next response
/// after observing terminal status can therefore see the mirror appear one
/// response later (the window widens on slow/loaded runners). `bash_status` is
/// excluded from finalizer attachment, so deliverability can only be observed
/// via a non-status tool call. Poll fast reads until the mirror carries this
/// task, so subsequent assertions observe the settled state; a genuinely
/// never-delivered completion still fails via the bound.
async fn settle_until_bg_completion(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    task_id: &str,
) {
    for attempt in 0..120 {
        let corr = first_corr + attempt;
        send_tool_call(stream, channel, corr, "echo", json!({ "case": "fast" })).await;
        let frame = read_frame_timeout(stream, "bg-completion settle read").await;
        assert_eq!(frame.header.corr, corr);
        let response = tool_response_json(&frame);
        let attached = response
            .get("bg_completions")
            .and_then(Value::as_array)
            .is_some_and(|completions| {
                completions.iter().any(|completion| {
                    completion.get("task_id").and_then(Value::as_str) == Some(task_id)
                })
            });
        if attached {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("bg completion for {task_id} never became in-band deliverable");
}

async fn wait_for_bash_completion(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    first_corr: u64,
    _session_id: &str,
    task_id: &str,
) -> Value {
    for attempt in 0..120 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let corr = first_corr + attempt;
        send_tool_call(
            stream,
            channel,
            corr,
            "bash_status",
            json!({ "params": { "task_id": task_id } }),
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
    assert_bg_completion_matching(response, task_id, "subc-bg-completion");
}

fn assert_bg_completion_matching(response: &Value, task_id: &str, command_contains: &str) {
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
                    .is_some_and(|command| command.contains(command_contains))
        }),
        "expected completion for task {task_id} (command ~ {command_contains}), got {completions:?}"
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
    for attempt in 0..80 {
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

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}
