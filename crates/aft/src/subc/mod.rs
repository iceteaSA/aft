//! subc daemon attach — transport edge.
//!
//! When AFT is launched as `aft --subc <connection-file>`, it does NOT run the
//! standalone NDJSON-over-stdin loop. Instead it connects to a running subc
//! daemon over loopback TCP, authenticates with the pre-envelope HMAC handshake
//! (`subc-transport`), then speaks the subc frame protocol (`subc-protocol`):
//! ModuleHello → HelloAck (register as a tool provider), then a channel-0
//! control loop (Ping/Pong, RouteBind) plus route-channel tool calls.
//!
//! Concurrency: subc routes tool calls through the executor. The tokio
//! edge never dispatches against `AppContext` inline; per-actor executor lanes
//! own the reader/mutator epoch, while a writer task serializes outbound frames.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::config_resolve::ConfigTier;
use crate::context::{App, AppContext, ProgressSender, RootHealthSnapshot};
use crate::executor::{Executor, Lane};
use crate::jsonc::strip_jsonc;
use crate::log_ctx;
use crate::path_identity::ProjectRootId;
use crate::protocol::{ProgressKind, PushFrame, RawRequest, Response};
use crate::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome, ToolCallResult};
use crate::runtime_drain;

use subc_protocol::manifest::{
    Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
    ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
};
use subc_protocol::session::{
    HealthReport, HealthStatus, ModuleControlRequest, ModuleControlResponse,
    MODULE_CONTROL_OP_HEALTH_CHECK,
};
use subc_protocol::{
    ErrorBody, Flags, Frame, FrameType, ModuleHelloBody, Principal, Priority, PROTOCOL_VERSION,
};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// Handshake budget. subc binds-before-spawn, so a reachable daemon authenticates
/// well within this; an unreachable/socket-stale daemon fails loud rather than
/// silently downgrading to standalone (the --subc contract).
const AUTH_DEADLINE: Duration = Duration::from_secs(5);

/// Correlation id for the initial ModuleHello (channel 0).
const HELLO_CORR: u64 = 1;

/// Per-session in-memory replay cap for must-deliver Push frames. This covers
/// detach/re-attach while AFT stays alive; cross-restart replay is phased later.
const PUSH_BUFFER_MAX_PER_KEY: usize = 256;

/// Bounded guard for control-frame sends. If the daemon stops reading and the
/// writer queue stays full, tear the subc edge down instead of stalling the
/// route loop indefinitely.
const CONTROL_SEND_TIMEOUT: Duration = Duration::from_millis(250);

const WRITER_QUEUE_CAPACITY: usize = 256;

/// Keep reliable Push bursts from monopolizing the current-thread subc loop;
/// any remaining must-deliver frames stay queued for the next loop turn.
const RELIABLE_PUSH_DRAIN_BUDGET: usize = 32;

/// Limit maintenance submissions per tick so background drains cannot delay
/// control-plane work such as completed RouteBind acknowledgements.
const MAINTENANCE_SUBMIT_BUDGET: usize = 4;

const RELIABLE_WRITER_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const RELIABLE_WRITER_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);

const DISPATCH_PATH_BIND_WARN_AFTER: Duration = Duration::from_secs(6);

/// Small bounded memory of completed task ids used to suppress stale lossy
/// long-running reminders that arrive after their reliable completion event.
const COMPLETED_TASK_SUPPRESSION_MAX: usize = 4096;

/// Bash foreground orchestration polls detached tasks with short read-lane jobs.
/// The sleep between polls is outside the executor so no read or write worker is
/// pinned while a foreground command is still running.
const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(100);

type RouteChannel = u32;
type PushEnvelope = (ProjectRootId, PushFrame);
type RetryBuffer = HashMap<RouteChannel, VecDeque<(push::ReplayKey, PushFrame)>>;
mod bash;
mod health;
mod manifest;
mod push;
mod wire;

use self::health::{
    build_health_report, warn_slow_pending_binds, DispatchPathMetrics, ResponseTaskGuard,
};
use self::manifest::{
    build_manifest, command_lane, control_flags, control_ops, is_bash_family_tool,
    is_subc_agent_core_tool, is_subc_native_plumbing_tool,
};
pub use self::wire::SubcError;
use self::wire::{
    build_error_frame, build_goodbye_frame, build_tool_response_frame, decrement_counted_channel,
    response_is_fatal_panic, response_message, send_counted_channel, send_frame,
    send_reliable_writer_frame,
};

#[derive(Clone)]
struct PushSenders {
    lossy_tx: mpsc::Sender<PushEnvelope>,
    reliable_tx: mpsc::UnboundedSender<PushEnvelope>,
}

#[derive(Clone)]
struct PersistentCancelSignal {
    inner: Arc<PersistentCancelInner>,
}

struct PersistentCancelInner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl PersistentCancelSignal {
    fn new() -> Self {
        Self {
            inner: Arc::new(PersistentCancelInner {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        // `enable()` REGISTERS this waiter before we read the flag, closing the
        // lost-wakeup window: `notify_waiters()` only wakes already-registered
        // waiters and stores no permit, so without enable() a `cancel()` firing
        // between the flag read and `.await` would be missed and the future
        // would park forever (cancel() fires only once). With enable(), a cancel
        // racing the flag read still wakes the registered waiter. The loop is a
        // belt-and-suspenders re-check on spurious wakeups.
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindTrust {
    FirstParty,
    Untrusted,
}

impl BindTrust {
    fn allows_bash_observation(self) -> bool {
        matches!(self, Self::FirstParty)
    }

    fn label(self) -> &'static str {
        match self {
            Self::FirstParty => "first_party",
            Self::Untrusted => "untrusted",
        }
    }
}

pub(super) fn trust_for_principal(principal: &Option<Principal>) -> BindTrust {
    match principal {
        Some(Principal::Direct) => BindTrust::FirstParty,
        Some(Principal::Reserved { module_id })
            if module_id == "llm-runner" || module_id == "aft" =>
        {
            BindTrust::FirstParty
        }
        Some(Principal::Reserved { .. }) | Some(Principal::Unverified) | None => {
            BindTrust::Untrusted
        }
    }
}

fn harness_forces_untrusted(harness: &str) -> bool {
    harness.starts_with("fed:")
}

pub(super) fn trust_for_bind(harness: &str, principal: &Option<Principal>) -> BindTrust {
    if harness_forces_untrusted(harness) {
        BindTrust::Untrusted
    } else {
        trust_for_principal(principal)
    }
}

fn principal_label(principal: &Option<Principal>) -> String {
    match principal {
        Some(Principal::Direct) => "direct".to_string(),
        Some(Principal::Reserved { module_id }) => format!("reserved:{module_id}"),
        Some(Principal::Unverified) => "unverified".to_string(),
        None => "absent".to_string(),
    }
}

#[derive(Debug)]
/// Per-root route metadata owned by the subc loop. The `active_bash_waits` field
/// counts detached bash processes that are still being observed for this root.
/// Any future logic that evicts roots based on idle time must not evict a root
/// while this count is greater than zero, because a foreground bash response may
/// still arrive later.
struct RootMeta {
    maintenance_pending: bool,
    maintenance_poisoned: bool,
    last_touched: Instant,
    diagnostics_on_edit: bool,
    active_bash_waits: usize,
}

#[derive(Debug)]
struct PendingBind {
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    cancelled: bool,
    configure_request_id: String,
    started_at: Instant,
    warned_half_deadline: bool,
}

struct RouteBindCompletion {
    route_channel: u16,
    identity: RouteIdentity,
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    configure_response: Response,
    drain_response: Option<Response>,
    diagnostics_on_edit: bool,
    ver: u8,
    corr: u64,
    flags: Flags,
}

#[derive(Debug, Clone)]
struct RouteIdentity {
    root: ProjectRootId,
    project_root: PathBuf,
    harness: String,
    session: String,
    trust: BindTrust,
}

#[derive(Debug, Clone)]
struct RetainedSessionIdentity {
    harness: String,
    trust: BindTrust,
}

#[derive(Clone, Copy)]
struct BgSub {
    corr: u64,
    ver: u8,
    flags: Flags,
}

struct MaintenanceCompletion {
    root_id: ProjectRootId,
    response: Response,
    empty_bg_sessions: Vec<(String, u64)>,
}

impl RootMeta {
    fn new(now: Instant) -> Self {
        Self {
            maintenance_pending: false,
            maintenance_poisoned: false,
            last_touched: now,
            diagnostics_on_edit: false,
            active_bash_waits: 0,
        }
    }

    fn touch(&mut self) {
        self.last_touched = Instant::now();
    }
}

fn due_maintenance_roots(
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    budget: usize,
) -> (Vec<ProjectRootId>, bool) {
    let mut roots = Vec::new();
    let mut deferred = false;

    for (root_id, meta) in live_roots.iter_mut() {
        if meta.maintenance_pending || meta.maintenance_poisoned {
            continue;
        }
        if roots.len() >= budget {
            deferred = true;
            continue;
        }
        meta.maintenance_pending = true;
        roots.push(root_id.clone());
    }

    (roots, deferred)
}

fn route_key(channel: u16) -> RouteChannel {
    RouteChannel::from(channel)
}

fn remove_root_channel(
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    channel: RouteChannel,
) {
    let remove_root = if let Some(channels) = root_channels.get_mut(root) {
        channels.remove(&channel);
        channels.is_empty()
    } else {
        false
    };
    if remove_root {
        root_channels.remove(root);
    }
}

fn remove_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
) -> Option<RouteIdentity> {
    let removed = routes.remove(&channel);
    if let Some(identity) = &removed {
        remove_root_channel(root_channels, &identity.root, channel);
    }
    removed
}

fn insert_route_channel(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    channel: RouteChannel,
    identity: RouteIdentity,
) {
    if let Some(previous) = routes.insert(channel, identity.clone()) {
        remove_root_channel(root_channels, &previous.root, channel);
    }
    root_channels
        .entry(identity.root.clone())
        .or_default()
        .insert(channel);
}

fn remove_bg_subscription_index(
    bg_sub_by_session: &mut HashMap<(ProjectRootId, String), RouteChannel>,
    channel: RouteChannel,
    identity: Option<&RouteIdentity>,
) {
    if let Some(identity) = identity {
        let key = (identity.root.clone(), identity.session.clone());
        if bg_sub_by_session.get(&key).copied() == Some(channel) {
            bg_sub_by_session.remove(&key);
        }
    } else {
        bg_sub_by_session.retain(|_, mapped_channel| *mapped_channel != channel);
    }
}

fn end_bg_subscription(
    writer_tx: &mpsc::Sender<Frame>,
    metrics: &DispatchPathMetrics,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    channel: RouteChannel,
    identity: Option<&RouteIdentity>,
) {
    if let Some(sub) = bg_subs.get(&channel).copied() {
        let _ = push::try_send_bg_stream_end(writer_tx, metrics, channel, &sub);
        bg_subs.remove(&channel);
        bg_wake_pending.remove(&channel);
        remove_bg_subscription_index(bg_sub_by_session, channel, identity);
    }
}

fn remember_session_identity(
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    identity: &RouteIdentity,
) {
    let key = (identity.root.clone(), identity.session.clone());
    if matches!(identity.trust, BindTrust::Untrusted)
        && session_identity
            .get(&key)
            .is_some_and(|retained| matches!(retained.trust, BindTrust::FirstParty))
    {
        return;
    }

    // Retained after route Goodbye so reliable session-scoped frames emitted while
    // the session is detached can still be keyed by the full (root,harness,session)
    // replay triple. Untrusted binds never overwrite a retained first-party
    // session identity, because bash completion replay is an observation channel.
    session_identity.insert(
        key,
        RetainedSessionIdentity {
            harness: identity.harness.clone(),
            trust: identity.trust,
        },
    );
}

fn replay_key_for_session(
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    root: &ProjectRootId,
    session: &str,
) -> Option<(push::ReplayKey, BindTrust)> {
    let retained = session_identity.get(&(root.clone(), session.to_string()))?;
    Some((
        push::ReplayKey {
            root: root.clone(),
            harness: retained.harness.clone(),
            session: session.to_string(),
        },
        retained.trust,
    ))
}
/// Sync command dispatch, passed in from `main` (the binary owns the command
/// table). Invoked only inside executor jobs in subc mode.
pub type DispatchFn = fn(RawRequest, &AppContext) -> Response;

/// Entry point for `aft --subc <connection-file>`. Synchronous on the outside;
/// owns an isolated current-thread tokio runtime for the async transport.
/// Returns `Err` (fail-loud) on any connect/auth/protocol failure — we never
/// fall back to the standalone loop, to avoid split-brain index state.
pub fn run_subc_mode(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
) -> Result<(), SubcError> {
    // Production NEVER allows non-manifest tool names on route channels: AFT
    // fails closed and does not trust subc to enforce the manifest. The
    // test-only harness sets this through `run_subc_mode_for_test`.
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        false,
    )
}

fn run_subc_mode_inner(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
) -> Result<(), SubcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SubcError::Runtime)?;

    let executor_for_loop = Arc::clone(&executor);
    let loop_result = runtime.block_on(async move {
        let shared_app = ctx.app();
        drop(ctx);
        let stream = connect_and_authenticate(connection_file_path).await?;
        log::info!(
            "subc attach: authenticated to daemon via {}",
            connection_file_path.display()
        );
        let (read_half, write_half) = tokio::io::split(stream);
        run_module_loop(
            read_half,
            write_half,
            shared_app,
            executor_for_loop,
            dispatch,
            user_config_path,
            allow_native_passthrough,
        )
        .await
    });

    for actor_ctx in executor.actor_contexts() {
        actor_ctx.lsp().shutdown_all();
        actor_ctx.bash_background().detach();
    }

    loop_result
}

/// Test-only entry that enables the non-manifest native-command passthrough on
/// route channels. Integration tests drive synthetic native commands (`glob`,
/// `callers`, `subc_test_echo_session`, …) through the executor to exercise
/// mechanics; production callers use [`run_subc_mode`], which fails closed.
#[doc(hidden)]
pub fn run_subc_mode_for_test(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
) -> Result<(), SubcError> {
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        true,
    )
}

/// Read the connection file → resolve the first endpoint → TCP connect → HMAC
/// handshake. Mirrors the reference `fake-aft-stub::connect_to_subc`.
async fn connect_and_authenticate(connection_file_path: &Path) -> Result<TcpStream, SubcError> {
    let conn = connection_file::read(connection_file_path).map_err(|source| {
        SubcError::ConnectionFile {
            path: connection_file_path.to_path_buf(),
            source,
        }
    })?;

    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| SubcError::NoEndpoint {
            path: connection_file_path.to_path_buf(),
        })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let ip = endpoint
        .host
        .parse::<IpAddr>()
        .map_err(|_| SubcError::InvalidEndpoint {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label.clone(),
        })?;
    let addr = SocketAddr::new(ip, endpoint.port);

    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|source| SubcError::Connect {
            endpoint: endpoint_label.clone(),
            source,
        })?;

    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| SubcError::Auth {
            endpoint: endpoint_label,
            source,
        })?;

    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
async fn process_route_bind_completion(
    writer_tx: &mpsc::Sender<Frame>,
    completion: RouteBindCompletion,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    decrement_counted_channel(&metrics.control_completion_queued);
    handle_route_bind_completion(
        writer_tx,
        completion,
        routes,
        root_channels,
        session_identity,
        push_buffer,
        live_roots,
        pending_binds,
        executor,
        shutdown,
        metrics,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drain_pending_route_bind_completions(
    control_completion_rx: &mut mpsc::Receiver<RouteBindCompletion>,
    writer_tx: &mpsc::Sender<Frame>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    while let Ok(completion) = control_completion_rx.try_recv() {
        process_route_bind_completion(
            writer_tx,
            completion,
            routes,
            root_channels,
            session_identity,
            push_buffer,
            live_roots,
            pending_binds,
            executor,
            shutdown,
            metrics,
        )
        .await?;
    }
    Ok(())
}

/// ModuleHello → HelloAck → control/route loop. Runs until the daemon closes
/// the connection (EOF), sends channel-0 Goodbye, or a fatal mutating executor
/// response requests whole-connection teardown.
async fn run_module_loop<R, W>(
    mut read: R,
    mut write: W,
    shared_app: Arc<App>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
) -> Result<(), SubcError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // ModuleHello: register as a tool provider and advertise the supported control-plane operations.
    // Echo the one-time launch nonce the daemon injected via SUBC_LAUNCH_NONCE so a
    // reserved module_id's HELLO is accepted; absent for non-reserved/self-connect.
    let hello = ModuleHelloBody {
        manifest: build_manifest(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: control_ops(),
        launch_nonce: std::env::var("SUBC_LAUNCH_NONCE").ok(),
    };
    let hello_frame = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        HELLO_CORR,
        serde_json::to_vec(&hello).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)?;
    write_frame(&mut write, &hello_frame)
        .await
        .map_err(SubcError::FrameIo)?;

    // Expect HelloAck (registered) or a channel-0 Error (manifest/version reject).
    match read_frame(&mut read).await.map_err(SubcError::FrameIo)? {
        None => return Err(SubcError::ClosedBeforeHelloAck),
        Some(frame) => match frame.header.ty {
            FrameType::HelloAck => {
                log::info!("subc attach: registered (HelloAck received)");
            }
            FrameType::Error => {
                let body = serde_json::from_slice::<ErrorBody>(&frame.body).ok();
                return Err(SubcError::HelloRejected { body });
            }
            other => return Err(SubcError::UnexpectedFrame { ty: other }),
        },
    }

    let dispatch_path_metrics = Arc::new(DispatchPathMetrics::new());
    let (writer_tx, writer_rx) = mpsc::channel::<Frame>(WRITER_QUEUE_CAPACITY);
    let writer_task = spawn_writer_task(write, writer_rx, Arc::clone(&dispatch_path_metrics));
    // `read_frame` is NOT cancellation-safe, so it must never sit directly inside
    // the `select!` below: a drain-interval tick (or shutdown) firing while a
    // frame is mid-transit would drop the partially-consumed bytes and desync the
    // stream (the next read would parse a body byte as a frame header). A
    // dedicated reader task owns the socket, reads whole frames sequentially, and
    // forwards them over a channel; the loop selects on the cancel-safe `recv()`.
    let (reader_tx, mut reader_rx) = mpsc::channel::<Result<Frame, SubcError>>(256);
    let reader_task = spawn_reader_task(read, reader_tx);
    let shutdown = Arc::new(Notify::new());
    let mut drain_interval = tokio::time::interval(Duration::from_millis(250));
    let (maintenance_tx, mut maintenance_rx) = mpsc::channel::<MaintenanceCompletion>(256);
    let (bash_deferred_tx, mut bash_deferred_rx) =
        mpsc::channel::<bash::BashDeferredCompletion>(256);
    let (bash_poll_touch_tx, mut bash_poll_touch_rx) = mpsc::channel::<ProjectRootId>(256);
    let (control_completion_tx, mut control_completion_rx) =
        mpsc::channel::<RouteBindCompletion>(256);
    let (lossy_tx, mut lossy_rx) = mpsc::channel::<PushEnvelope>(1024);
    let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
    let push_senders = PushSenders {
        lossy_tx,
        reliable_tx,
    };
    let connection_cancel = PersistentCancelSignal::new();
    let mut routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
    let mut bg_subs: HashMap<RouteChannel, BgSub> = HashMap::new();
    let mut bg_sub_by_session: HashMap<(ProjectRootId, String), RouteChannel> = HashMap::new();
    let mut bg_wake_pending: HashSet<RouteChannel> = HashSet::new();
    let mut bg_wake_epoch: HashMap<(ProjectRootId, String), u64> = HashMap::new();
    let mut root_channels: HashMap<ProjectRootId, HashSet<RouteChannel>> = HashMap::new();
    let mut session_identity: HashMap<(ProjectRootId, String), RetainedSessionIdentity> =
        HashMap::new();
    let mut push_buffer: HashMap<push::ReplayKey, VecDeque<PushFrame>> = HashMap::new();
    let mut retry_buffer: RetryBuffer = HashMap::new();
    let mut completed_tasks = push::CompletedTaskIds::default();
    let mut live_roots: HashMap<ProjectRootId, RootMeta> = HashMap::new();
    let mut pending_binds: HashMap<RouteChannel, PendingBind> = HashMap::new();
    let mut route_bash_cancels: HashMap<RouteChannel, bash::RouteBashCancel> = HashMap::new();

    let loop_result: Result<(), SubcError> = loop {
        dispatch_path_metrics.mark_frame_loop_tick();
        // RouteBind completions are control-plane unblockers. Drain any completed
        // binds before entering other branch work so Push and maintenance bursts
        // can only add one loop-turn of latency.
        if let Err(error) = drain_pending_route_bind_completions(
            &mut control_completion_rx,
            &writer_tx,
            &mut routes,
            &mut root_channels,
            &mut session_identity,
            &mut push_buffer,
            &mut live_roots,
            &mut pending_binds,
            &executor,
            &shutdown,
            &dispatch_path_metrics,
        )
        .await
        {
            break Err(error);
        }

        tokio::select! {
            biased;
            Some(completion) = control_completion_rx.recv() => {
                if let Err(error) = process_route_bind_completion(
                    &writer_tx,
                    completion,
                    &mut routes,
                    &mut root_channels,
                    &mut session_identity,
                    &mut push_buffer,
                    &mut live_roots,
                    &mut pending_binds,
                    &executor,
                    &shutdown,
                    &dispatch_path_metrics,
                )
                .await
                {
                    break Err(error);
                }
            }
            _ = shutdown.notified() => {
                log::warn!("subc attach: fatal executor response requested teardown");
                break Ok(());
            }
            maybe_frame = reader_rx.recv() => {
                let frame = match maybe_frame {
                    None => {
                        log::info!("subc attach: daemon closed connection");
                        break Ok(());
                    }
                    Some(Err(error)) => break Err(error),
                    Some(Ok(frame)) => frame,
                };

                match frame.header.ty {
                    FrameType::Ping if frame.header.channel == 0 => {
                        let pong = match Frame::build_with_version(
                            frame.header.ver,
                            FrameType::Pong,
                            frame.header.flags,
                            0,
                            frame.header.corr,
                            Vec::new(),
                        ) {
                            Ok(pong) => pong,
                            Err(error) => break Err(SubcError::FrameBuild(error)),
                        };
                        if let Err(error) = send_frame(&writer_tx, &dispatch_path_metrics, pong).await {
                            break Err(error);
                        }
                    }
                    FrameType::Goodbye if frame.header.channel == 0 => {
                        log::info!("subc attach: received channel-0 Goodbye");
                        break Ok(());
                    }
                    FrameType::Goodbye => {
                        let channel = route_key(frame.header.channel);
                        end_bg_subscription(
                            &writer_tx,
                            &dispatch_path_metrics,
                            &mut bg_subs,
                            &mut bg_sub_by_session,
                            &mut bg_wake_pending,
                            channel,
                            routes.get(&channel),
                        );
                        if let Some(cancel) = route_bash_cancels.remove(&channel) {
                            cancel.token.cancel();
                        }
                        if let Some(pending) = pending_binds.get_mut(&channel) {
                            pending.cancelled = true;
                            log::debug!(
                                "subc attach: cancelled pending RouteBind for route {} on Goodbye",
                                frame.header.channel
                            );
                        }
                        let migrated = push::migrate_retry_buffer_to_push_buffer(
                            &mut retry_buffer,
                            channel,
                            &mut push_buffer,
                        );
                        if let Some(identity) = remove_route_channel(&mut routes, &mut root_channels, channel) {
                            if migrated > 0 {
                                log::debug!(
                                    "subc attach: migrated {migrated} retry-buffered reliable Push frame(s) from route {} into detach replay",
                                    frame.header.channel
                                );
                            }
                            if let Some(meta) = live_roots.get_mut(&identity.root) {
                                let idle_for = meta.last_touched.elapsed();
                                meta.touch();
                                log::debug!(
                                    "subc attach: route {} torn down for root {} harness {} session {} (last touched {:?} ago)",
                                    frame.header.channel,
                                    identity.root.as_path().display(),
                                    identity.harness,
                                    identity.session,
                                    idle_for
                                );
                            } else {
                                log::debug!(
                                    "subc attach: route {} torn down for root {} harness {} session {}",
                                    frame.header.channel,
                                    identity.root.as_path().display(),
                                    identity.harness,
                                    identity.session
                                );
                            }
                        } else {
                            if migrated > 0 {
                                log::debug!(
                                    "subc attach: migrated {migrated} retry-buffered reliable Push frame(s) from unbound route {} into detach replay",
                                    frame.header.channel
                                );
                            }
                            log::debug!("subc attach: unbound route {} torn down", frame.header.channel);
                        }
                    }
                    FrameType::Request if frame.header.channel == 0 => {
                        if let Err(error) = handle_control_request(
                            &writer_tx,
                            &frame,
                            &shared_app,
                            &executor,
                            &mut live_roots,
                            &mut pending_binds,
                            &control_completion_tx,
                            &dispatch_path_metrics,
                            &push_senders,
                            dispatch,
                            user_config_path.as_deref(),
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Request => {
                        if let Err(error) = handle_tool_call(
                            &writer_tx,
                            &frame,
                            &routes,
                            &pending_binds,
                            &mut live_roots,
                            &executor,
                            &shutdown,
                            &connection_cancel,
                            &bash_deferred_tx,
                            &bash_poll_touch_tx,
                            &dispatch_path_metrics,
                            &mut route_bash_cancels,
                            &mut bg_subs,
                            &mut bg_sub_by_session,
                            &mut bg_wake_pending,
                            &mut bg_wake_epoch,
                            dispatch,
                            allow_native_passthrough,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    FrameType::Cancel => {
                        let channel = route_key(frame.header.channel);
                        if bg_subs.contains_key(&channel) {
                            end_bg_subscription(
                                &writer_tx,
                                &dispatch_path_metrics,
                                &mut bg_subs,
                                &mut bg_sub_by_session,
                                &mut bg_wake_pending,
                                channel,
                                routes.get(&channel),
                            );
                        }
                    }
                    // Push/etc. are not handled on ingress. In-flight tool-call
                    // cancellation is not implemented, so non-bg_events Cancels
                    // and unrelated frame types are ignored rather than acted on.
                    _ => {}
                }
            }
            Some((root_id, frame)) = reliable_rx.recv() => {
                // Reliable Push frames are FIFO and must-deliver, but draining an
                // unbounded burst in one current-thread turn can starve RouteBind
                // completions. The budget defers excess frames, never drops them.
                let (_, deferred) = push::drain_reliable_push_turn(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &session_identity,
                    &mut retry_buffer,
                    &mut push_buffer,
                    &mut completed_tasks,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &mut bg_wake_epoch,
                    &mut reliable_rx,
                    Some((root_id, frame)),
                );
                if deferred {
                    tokio::task::yield_now().await;
                }
            }
            Some((root_id, frame)) = lossy_rx.recv() => {
                // When both push lanes have work, handle a small reliable slice before lossy work.
                // That ordering lets completed task ids suppress stale BashLongRunning frames.
                // The slice stays bounded so reliable bursts cannot monopolize this loop turn.
                let (_, deferred) = push::drain_reliable_push_turn(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &session_identity,
                    &mut retry_buffer,
                    &mut push_buffer,
                    &mut completed_tasks,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &mut bg_wake_epoch,
                    &mut reliable_rx,
                    None,
                );
                if deferred {
                    tokio::task::yield_now().await;
                }

                // Drain the currently queued burst in one loop turn so lossy
                // status/progress classes coalesce before reaching subc's shared
                // egress queue.
                let mut batch = vec![(root_id, frame)];
                while let Ok(item) = lossy_rx.try_recv() {
                    batch.push(item);
                }

                for (root, frame) in push::coalesce_push_batch(batch) {
                    push::process_lossy_push_frame(
                        &writer_tx,
                        &dispatch_path_metrics,
                        &routes,
                        &root_channels,
                        &completed_tasks,
                        root,
                        frame,
                    );
                }
            }
            Some(done) = bash_deferred_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.bash_deferred_queued);
                if let Err(error) = bash::handle_bash_deferred_completion(
                    &writer_tx,
                    done,
                    &routes,
                    &mut live_roots,
                    &mut route_bash_cancels,
                    &shutdown,
                    &dispatch_path_metrics,
                )
                .await
                {
                    break Err(error);
                }
            }
            Some(root_id) = bash_poll_touch_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.bash_poll_touch_queued);
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.touch();
                }
            }
            Some(completion) = maintenance_rx.recv() => {
                decrement_counted_channel(&dispatch_path_metrics.maintenance_queued);
                let root_id = completion.root_id;
                let response = completion.response;
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.maintenance_pending = false;
                }
                push::clear_stale_bg_wakes_for_empty_sessions(
                    &root_id,
                    &completion.empty_bg_sessions,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &bg_wake_epoch,
                );
                if response_is_fatal_panic(&response) {
                    if let Some(meta) = live_roots.get_mut(&root_id) {
                        meta.maintenance_poisoned = true;
                    }
                    log::warn!(
                        "subc attach: maintenance drain observed a fatal actor; deferring teardown until a route request can receive actor_fatal"
                    );
                }
            }
            _ = drain_interval.tick() => {
                push::emit_bg_event_wakes(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &bg_subs,
                    &mut bg_wake_pending,
                );
                warn_slow_pending_binds(&mut pending_binds, &executor);

                let retried = push::drain_retry_buffers_for_bound_routes(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &mut retry_buffer,
                );
                if retried > 0 {
                    log::debug!(
                        "subc attach: retried {retried} reliable Push frame(s) after writer backpressure"
                    );
                }

                let (due_roots, deferred_roots) =
                    due_maintenance_roots(&mut live_roots, MAINTENANCE_SUBMIT_BUDGET);
                if deferred_roots {
                    dispatch_path_metrics
                        .maintenance_budget_deferrals
                        .fetch_add(1, Ordering::Relaxed);
                }
                for root_id in due_roots {
                    let bg_sessions_to_check: Vec<(String, u64)> = bg_sub_by_session
                        .iter()
                        .filter_map(|((root, session), _)| {
                            if root == &root_id {
                                Some((
                                    session.clone(),
                                    bg_wake_epoch
                                        .get(&(root_id.clone(), session.clone()))
                                        .copied()
                                        .unwrap_or(0),
                                ))
                            } else {
                                None
                            }
                        })
                        .collect();
                    submit_maintenance_drain(
                        &executor,
                        root_id,
                        bg_sessions_to_check,
                        &maintenance_tx,
                        &dispatch_path_metrics,
                    );
                }
            }
        }
    };

    // The reader task may be parked on `read_frame`; abort it (we are done with
    // the connection) and flush the writer.
    connection_cancel.cancel();
    reader_task.abort();
    drop(writer_tx);
    let writer_result = finish_writer_task(writer_task).await;
    loop_result.and(writer_result)
}

fn spawn_writer_task<W>(
    mut write: W,
    mut rx: mpsc::Receiver<Frame>,
    metrics: Arc<DispatchPathMetrics>,
) -> JoinHandle<Result<(), subc_transport::FrameIoError>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            decrement_counted_channel(&metrics.writer_queued);
            write_frame(&mut write, &frame).await?;
        }
        Ok(())
    })
}

/// Owns the read half and reads whole frames sequentially. `read_frame` is not
/// cancellation-safe, so it must run here — never inside the main loop's
/// `select!` — to keep the inbound stream framed. Each frame (or the terminal
/// error / EOF) is forwarded over `tx`; the loop consumes them via cancel-safe
/// `recv()`. Exits on EOF (Ok(None)), a read error, or when `tx` is dropped
/// (the loop ended and aborted us).
fn spawn_reader_task<R>(mut read: R, tx: mpsc::Sender<Result<Frame, SubcError>>) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read).await {
                Ok(Some(frame)) => {
                    if tx.send(Ok(frame)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    // EOF: let the loop observe channel close as "daemon closed".
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Err(SubcError::FrameIo(error))).await;
                    return;
                }
            }
        }
    })
}

async fn finish_writer_task(
    mut writer_task: JoinHandle<Result<(), subc_transport::FrameIoError>>,
) -> Result<(), SubcError> {
    match tokio::time::timeout(Duration::from_millis(100), &mut writer_task).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(SubcError::FrameIo(error)),
        Ok(Err(error)) => Err(SubcError::WriterJoin(error)),
        Err(_) => {
            writer_task.abort();
            Ok(())
        }
    }
}

fn rollback_pending_bind_actor(
    executor: &Arc<Executor>,
    live_roots: &HashMap<ProjectRootId, RootMeta>,
    root_id: &ProjectRootId,
    inserted_new_actor: bool,
) {
    if inserted_new_actor && !live_roots.contains_key(root_id) {
        executor.remove_actor(root_id);
    }
}

async fn handle_route_bind_completion(
    tx: &mpsc::Sender<Frame>,
    completion: RouteBindCompletion,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    push_buffer: &mut HashMap<push::ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let route_id = route_key(completion.route_channel);
    let Some(pending) = pending_binds.remove(&route_id) else {
        log::warn!(
            "subc attach: dropping RouteBind completion for non-pending route {}",
            completion.route_channel
        );
        rollback_pending_bind_actor(
            executor,
            live_roots,
            &completion.bind_root_id,
            completion.inserted_new_actor,
        );
        return Ok(());
    };

    if pending.bind_root_id != completion.bind_root_id {
        log::warn!(
            "subc attach: pending RouteBind root mismatch for route {} (pending {} completion {})",
            completion.route_channel,
            pending.bind_root_id.as_path().display(),
            completion.bind_root_id.as_path().display()
        );
    }

    let inserted_new_actor = pending.inserted_new_actor || completion.inserted_new_actor;
    if pending.cancelled {
        rollback_pending_bind_actor(
            executor,
            live_roots,
            &completion.bind_root_id,
            inserted_new_actor,
        );
        log::debug!(
            "subc attach: discarded completed RouteBind for cancelled route {} root {}",
            completion.route_channel,
            completion.bind_root_id.as_path().display()
        );
        return Ok(());
    }

    let failure = if !completion.configure_response.success {
        Some((
            &completion.configure_response,
            "configure failed during route bind",
        ))
    } else if let Some(drain_response) = completion.drain_response.as_ref() {
        if drain_response.success {
            None
        } else {
            Some((
                drain_response,
                "build-completion drain failed during route bind",
            ))
        }
    } else {
        None
    };

    if let Some((response, fallback)) = failure {
        rollback_pending_bind_actor(
            executor,
            live_roots,
            &completion.bind_root_id,
            inserted_new_actor,
        );
        let message = response_message(response, fallback);
        let fatal = response_is_fatal_panic(response);
        // Preserve typed configure rejections across the bind boundary: a
        // malformed fed fingerprint means a federation-module bug or
        // fingerprint-format drift, and the fed side matches on the code
        // rather than parsing prose.
        let error_code = if response.data.get("code").and_then(|c| c.as_str())
            == Some("bad_harness_fingerprint")
        {
            "bad_harness_fingerprint"
        } else {
            "config_divergence"
        };
        send_route_bind_error_parts(
            tx,
            completion.ver,
            completion.corr,
            completion.flags,
            error_code,
            &message,
            metrics,
        )
        .await?;
        if fatal {
            signal_fatal_teardown(
                tx,
                Some(completion.route_channel),
                completion.ver,
                completion.corr,
                shutdown,
                metrics,
            )
            .await;
        }
        return Ok(());
    }

    remember_session_identity(session_identity, &completion.identity);
    let replay_key = push::ReplayKey::from_identity(&completion.identity);
    let bind_trust = completion.identity.trust;
    insert_route_channel(routes, root_channels, route_id, completion.identity);
    live_roots
        .entry(completion.bind_root_id.clone())
        .and_modify(|meta| {
            meta.touch();
            meta.diagnostics_on_edit = completion.diagnostics_on_edit;
            meta.maintenance_poisoned = false;
        })
        .or_insert_with(|| RootMeta::new(Instant::now()));
    if let Some(meta) = live_roots.get_mut(&completion.bind_root_id) {
        meta.diagnostics_on_edit = completion.diagnostics_on_edit;
        meta.maintenance_poisoned = false;
    }

    let ack =
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).map_err(SubcError::Json)?;
    let response = Frame::build_with_version(
        completion.ver,
        FrameType::Response,
        control_flags(),
        0,
        completion.corr,
        ack,
    )
    .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBindAck").await?;
    let replayed = push::replay_buffered_push_frames(
        tx,
        metrics,
        route_id,
        push_buffer,
        &replay_key,
        bind_trust,
    );
    if replayed > 0 {
        log::debug!(
            "subc attach: replayed {} buffered Push frame(s) to route {} root {} harness {} session {}",
            replayed,
            completion.route_channel,
            replay_key.root.as_path().display(),
            replay_key.harness,
            replay_key.session
        );
    }
    log::info!(
        "subc attach: route {} bound to root {}",
        completion.route_channel,
        completion.bind_root_id.as_path().display()
    );
    Ok(())
}

/// channel-0 control requests: RouteBind plus the cached health probe. RouteBind
/// still reconciles the route's RootConfig through the executor's Mutating lane
/// and resolves completion on a loop-owned control-completion channel so slow
/// configure jobs do not block the transport loop.
async fn handle_control_request(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    control_completion_tx: &mpsc::Sender<RouteBindCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
    push_senders: &PushSenders,
    dispatch: DispatchFn,
    user_config_path: Option<&Path>,
) -> Result<(), SubcError> {
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(SubcError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            target: _,
            identity,
            principal,
        } => {
            let route_id = route_key(route_channel);
            if pending_binds.contains_key(&route_id) {
                return send_route_bind_error(
                    tx,
                    frame,
                    "config_divergence",
                    "route bind is already pending for channel",
                    metrics,
                )
                .await;
            }

            let bind_root_id = match ProjectRootId::from_path(&identity.project_root) {
                Ok(root_id) => root_id,
                Err(error) => {
                    return send_route_bind_error(
                        tx,
                        frame,
                        "config_divergence",
                        &format!("invalid route project root: {error}"),
                        metrics,
                    )
                    .await;
                }
            };

            // Reconcile RootConfig: build a configure request from the bind
            // identity + forwarded config tiers and run it through the executor.
            let request_id = format!("subc-bind-{route_channel}");
            let bind_project_root = identity.project_root.clone();
            let bind_harness = identity.harness.clone();
            let bind_session = identity.session.clone();
            let bind_trust = trust_for_bind(&bind_harness, &principal);
            log::info!(
                "subc attach: route {} harness={} principal={} trust={}",
                route_channel,
                bind_harness,
                principal_label(&principal),
                bind_trust.label()
            );

            // Config is single-per-project, read by AFT directly from the
            // CortexKit config files (user: ~/.config/cortexkit/aft.jsonc,
            // project: <root>/.cortexkit/aft.jsonc). Wire-relayed config tiers are
            // IGNORED entirely: a front (runner, mcp:*, or fed:*) cannot push config over
            // the wire. This is what makes config harness-INDEPENDENT — every
            // harness binding a project gets the identical on-disk config, so two
            // trust domains sharing the per-root actor can never diverge or
            // inherit each other's capabilities (the cross-bind escalation class).
            // Wire-relayed config tiers (if the protocol still carries them) are
            // ignored entirely; the per-tier trust boundary (user trusted, project
            // privileged-dropped) is applied to the FILE tiers in handle_configure.
            let local_tiers = crate::subc_config::read_local_cortexkit_config_tiers(
                user_config_path,
                Path::new(&bind_project_root),
            );
            let config_tiers: Vec<Value> = local_tiers
                .iter()
                .map(|t| json!({ "tier": t.tier, "source": t.source, "doc": t.doc }))
                .collect();
            let diagnostics_on_edit = diagnostics_on_edit_from_tiers(&local_tiers);
            let configure_json = json!({
                "id": request_id,
                "command": "configure",
                "project_root": bind_project_root,
                "harness": bind_harness,
                "session_id": bind_session.clone(),
                "config": config_tiers,
            });
            let configure_req = match serde_json::from_value::<RawRequest>(configure_json) {
                Ok(req) => req,
                Err(error) => {
                    return send_route_bind_error(
                        tx,
                        frame,
                        "config_divergence",
                        &format!("failed to build configure request: {error}"),
                        metrics,
                    )
                    .await;
                }
            };

            let route_identity = RouteIdentity {
                root: bind_root_id.clone(),
                project_root: PathBuf::from(&bind_project_root),
                harness: bind_harness.clone(),
                session: bind_session.clone(),
                trust: bind_trust,
            };
            let configure_session = route_identity.session.clone();
            let root_was_live = live_roots.contains_key(&bind_root_id);
            let inserted_new_actor = if root_was_live {
                log::debug!(
                    "subc attach: reusing actor for route {} root {}",
                    route_channel,
                    bind_root_id.as_path().display()
                );
                false
            } else {
                let actor_ctx = Arc::new(AppContext::from_app(
                    Arc::clone(shared_app),
                    Config::default(),
                ));
                install_bash_compressor(&actor_ctx);
                actor_ctx.set_progress_sender(Some(push::progress_sender_for_root(
                    push_senders.clone(),
                    bind_root_id.clone(),
                )));
                let inserted =
                    executor.register_actor(bind_root_id.clone(), Arc::clone(&actor_ctx));
                drop(actor_ctx);
                // Do not insert into live_roots until configure succeeds: live_roots
                // drives maintenance, and a half-configured new actor must not be
                // maintenance-eligible before its route/session identity exists.
                log::debug!(
                    "subc attach: registered actor for route {} root {}",
                    route_channel,
                    bind_root_id.as_path().display()
                );
                inserted
            };

            let configure_request_id = configure_req.id.clone();
            pending_binds.insert(
                route_id,
                PendingBind {
                    bind_root_id: bind_root_id.clone(),
                    inserted_new_actor,
                    cancelled: false,
                    configure_request_id: configure_request_id.clone(),
                    started_at: Instant::now(),
                    warned_half_deadline: false,
                },
            );
            let configure_rx = executor.submit_async(
                bind_root_id.clone(),
                Lane::Mutating,
                configure_request_id.clone(),
                Box::new(move |ctx| {
                    log_ctx::with_session(Some(configure_session.clone()), || {
                        dispatch(configure_req, ctx)
                    })
                }),
            );

            let completion_tx = control_completion_tx.clone();
            let completion_executor = Arc::clone(executor);
            let completion_identity = route_identity;
            let completion_root = bind_root_id.clone();
            let completion_route_channel = route_channel;
            let completion_ver = frame.header.ver;
            let completion_corr = frame.header.corr;
            let completion_flags = frame.header.flags;
            let completion_metrics = Arc::clone(metrics);
            tokio::spawn(async move {
                let _response_task = ResponseTaskGuard::new(&completion_metrics);
                let configure_response =
                    await_executor_response(configure_rx, configure_request_id.clone()).await;
                let drain_response = if configure_response.success && !root_was_live {
                    let drain_request_id = format!("subc-bind-drain-{completion_route_channel}");
                    let drain_response_id = drain_request_id.clone();
                    let drain_rx = completion_executor.submit_async(
                        completion_root.clone(),
                        Lane::Mutating,
                        drain_request_id.clone(),
                        Box::new(move |ctx| {
                            runtime_drain::drain_build_completions(ctx);
                            Response::success(drain_response_id, json!({ "drained": true }))
                        }),
                    );
                    Some(await_executor_response(drain_rx, drain_request_id).await)
                } else {
                    None
                };

                let completion = RouteBindCompletion {
                    route_channel: completion_route_channel,
                    identity: completion_identity,
                    bind_root_id: completion_root,
                    inserted_new_actor,
                    configure_response,
                    drain_response,
                    diagnostics_on_edit,
                    ver: completion_ver,
                    corr: completion_corr,
                    flags: completion_flags,
                };
                if send_counted_channel(
                    &completion_tx,
                    &completion_metrics.control_completion_queued,
                    completion,
                )
                .await
                .is_err()
                {
                    log::debug!(
                        "subc attach: dropped RouteBind completion for route {} after loop exit",
                        completion_route_channel
                    );
                }
            });

            Ok(())
        }
        ModuleControlRequest::HealthCheck {} => {
            let report = build_health_report(executor, pending_binds, metrics);
            let body = serde_json::to_vec(&ModuleControlResponse::from(report))
                .map_err(SubcError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                0,
                frame.header.corr,
                body,
            )
            .map_err(SubcError::FrameBuild)?;
            send_frame(tx, metrics, response).await
        }
    }
}

fn install_bash_compressor(ctx: &AppContext) {
    // Mirrors main.rs per-actor compressor installation for subc-created actors.
    let filter_registry_handle = ctx.shared_filter_registry();
    let compress_flag = ctx.bash_compress_flag();
    ctx.bash_background().set_compressor_with_exit_code(
        move |command: &str, output: String, exit_code: Option<i32>| {
            if !compress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return crate::compress::CompressionResult::new(output);
            }
            let registry_guard = match filter_registry_handle.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            crate::compress::compress_with_registry_exit_code(
                command,
                &output,
                exit_code,
                &registry_guard,
            )
        },
    );
}

fn diagnostics_on_edit_from_tiers(tiers: &[ConfigTier]) -> bool {
    let mut diagnostics_on_edit = false;
    for tier in tiers {
        if let Some(value) = diagnostics_on_edit_from_doc(&tier.doc) {
            diagnostics_on_edit = value;
        }
    }
    diagnostics_on_edit
}

fn diagnostics_on_edit_from_doc(doc: &str) -> Option<bool> {
    let stripped = strip_jsonc(doc);
    let value = serde_json::from_str::<Value>(&stripped).ok()?;
    value
        .get("lsp")
        .and_then(Value::as_object)?
        .get("diagnostics_on_edit")
        .and_then(Value::as_bool)
}

async fn send_route_bind_error(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    send_route_bind_error_parts(
        tx,
        frame.header.ver,
        frame.header.corr,
        frame.header.flags,
        code,
        message,
        metrics,
    )
    .await
}

async fn send_route_bind_error_parts(
    tx: &mpsc::Sender<Frame>,
    ver: u8,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let response = build_error_frame(ver, 0, corr, flags, code, message)?;
    send_reliable_writer_frame(tx, metrics, response, "RouteBind error").await?;
    log::warn!("subc attach: route bind rejected ({code}): {message}");
    Ok(())
}

/// Route-channel tool call: `{name, arguments}` → executor lane → dispatch to
/// the sync command core → wrap the structured Response in a CallToolResult
/// `{content, isError}`. v1 mapping: the whole `{success, ...}` Response
/// serialized into ONE text block; `isError` carries `success == false`.
async fn handle_tool_call(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    connection_cancel: &PersistentCancelSignal,
    bash_deferred_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    bash_poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
    dispatch: DispatchFn,
    allow_native_passthrough: bool,
) -> Result<(), SubcError> {
    let route_id = route_key(frame.header.channel);
    if pending_binds.contains_key(&route_id) {
        let error = build_error_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
            frame.header.flags,
            "route_not_bound",
            "route is not bound before tool call",
        )?;
        return send_reliable_writer_frame(tx, metrics, error, "route_not_bound error").await;
    }

    let Some(identity) = routes.get(&route_id).cloned() else {
        let error = build_error_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
            frame.header.flags,
            "route_not_bound",
            "route is not bound before tool call",
        )?;
        return send_reliable_writer_frame(tx, metrics, error, "route_not_bound error").await;
    };
    if let Some(meta) = live_roots.get_mut(&identity.root) {
        meta.touch();
    }

    let is_bg_events_subscribe = serde_json::from_slice::<BgEventsProbe>(&frame.body)
        .ok()
        .and_then(|probe| probe.op)
        .as_deref()
        == Some("bg_events");
    if is_bg_events_subscribe {
        if let Some(old_sub) = bg_subs.get(&route_id).copied() {
            let _ = push::try_send_bg_stream_end(tx, metrics, route_id, &old_sub);
        }
        if !identity.trust.allows_bash_observation() {
            bg_subs.remove(&route_id);
            bg_wake_pending.remove(&route_id);
            remove_bg_subscription_index(bg_sub_by_session, route_id, Some(&identity));
            return Ok(());
        }
        bg_subs.insert(
            route_id,
            BgSub {
                corr: frame.header.corr,
                ver: frame.header.ver,
                flags: frame.header.flags,
            },
        );
        bg_sub_by_session.insert((identity.root.clone(), identity.session.clone()), route_id);
        push::arm_bg_wake(
            identity.root,
            identity.session,
            route_id,
            bg_wake_pending,
            bg_wake_epoch,
        );
        return Ok(());
    }

    let call = serde_json::from_slice::<ToolCallRequest>(&frame.body).map_err(SubcError::Json)?;
    let bare_name = call.name.clone();
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        &bare_name,
        &call.arguments,
        identity.project_root.as_path(),
    );

    let request_id = format!("subc-{}-{}", frame.header.channel, frame.header.corr);
    let bind_trust = identity.trust;
    let diagnostics_on_edit = live_roots
        .get(&identity.root)
        .map(|meta| meta.diagnostics_on_edit)
        .unwrap_or(false);

    if matches!(bind_trust, BindTrust::Untrusted) && is_bash_family_tool(&bare_name) {
        let response = bash::bash_denied_untrusted_response(request_id.clone());
        let text = crate::subc_format::format_response_with_context(
            &bare_name,
            &response,
            &format_context,
        );
        let result = ToolCallResult { text, response };
        let response_frame = build_tool_response_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
            frame.header.flags,
            &result,
        )?;
        return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
    }

    // A non-core name is NOT in the tool manifest. AFT fails closed and
    // does not trust subc to enforce the manifest: rejecting here is the
    // defense-in-depth backstop that prevents a forwarded native command
    // (e.g. `configure`, which would reach handle_configure and bypass
    // the RouteBind config-trust cap) from ever reaching dispatch. Only
    // the integration-test harness (run_subc_mode_for_test) opens this to
    // drive synthetic native commands through the executor.
    if !is_subc_agent_core_tool(&call.name)
        && !is_subc_native_plumbing_tool(&call.name)
        && !allow_native_passthrough
    {
        log::warn!(
            "subc tool call: rejecting non-manifest tool name {:?} on route {} (fail-closed)",
            call.name,
            frame.header.channel
        );
        let response = Response::error(
            request_id.clone(),
            "unknown_tool",
            format!("tool {:?} is not in the AFT tool manifest", call.name),
        );
        let text = crate::subc_format::format_response_with_context(
            &bare_name,
            &response,
            &format_context,
        );
        let result = ToolCallResult { text, response };
        let response_frame = build_tool_response_frame(
            frame.header.ver,
            frame.header.channel,
            frame.header.corr,
            frame.header.flags,
            &result,
        )?;
        return send_reliable_writer_frame(tx, metrics, response_frame, "tool response").await;
    }

    if bare_name == "bash" {
        let meta = live_roots
            .entry(identity.root.clone())
            .or_insert_with(|| RootMeta::new(Instant::now()));
        meta.active_bash_waits = meta.active_bash_waits.saturating_add(1);
        meta.touch();

        let route_cancel =
            route_bash_cancels
                .entry(route_id)
                .or_insert_with(|| bash::RouteBashCancel {
                    token: PersistentCancelSignal::new(),
                    active_waits: 0,
                });
        route_cancel.active_waits = route_cancel.active_waits.saturating_add(1);
        let cancel = bash::BashWaitCancel {
            connection: connection_cancel.clone(),
            route: route_cancel.token.clone(),
        };

        bash::submit_deferred_bash(
            executor,
            bash_deferred_tx,
            bash_poll_touch_tx,
            metrics,
            dispatch,
            identity.root,
            identity.project_root,
            identity.session,
            request_id,
            frame.header.channel,
            frame.header.corr,
            frame.header.flags,
            frame.header.ver,
            call.arguments,
            format_context,
            cancel,
            bind_trust,
        );
        return Ok(());
    }

    let lane = command_lane(&bare_name);
    let tool_call_context = ToolCallContext {
        project_root: identity.project_root.clone(),
        session_id: Some(identity.session.clone()),
        request_id: request_id.clone(),
        diagnostics_on_edit,
        preview: call.preview,
    };
    let arguments_for_run = call.arguments.clone();
    let bare_name_for_run = bare_name.clone();
    let bare_name_for_frame = bare_name.clone();
    let bare_name_for_finalize = bare_name.clone();
    let session_for_log = identity.session.clone();
    let session_for_finalize = identity.session.clone();
    let request_id_for_force = request_id.clone();
    let format_context_for_frame = format_context;
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let rx = executor.submit_async(
        identity.root,
        lane,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_log.clone()), || {
                let run = || {
                    let dispatch_with_finalize = |raw_req: RawRequest, app_ctx: &AppContext| {
                        let mut response = dispatch(raw_req, app_ctx);
                        crate::response_finalize::finalize_response_with_bg_completions(
                            &mut response,
                            app_ctx,
                            &session_for_finalize,
                            &bare_name_for_finalize,
                            bind_trust.allows_bash_observation(),
                        );
                        response
                    };
                    match run_tool_call(
                        &bare_name_for_run,
                        &arguments_for_run,
                        &tool_call_context,
                        ctx,
                        &dispatch_with_finalize,
                    ) {
                        ToolCallOutcome::Unary(result) => {
                            let _ = text_tx.send(result.text);
                            result.response
                        }
                    }
                };
                if matches!(bind_trust, BindTrust::Untrusted) {
                    ctx.with_force_restrict(&request_id_for_force, run)
                } else {
                    run()
                }
            })
        }),
    );
    let completion_tx = tx.clone();
    let completion_shutdown = Arc::clone(shutdown);
    let route_channel = frame.header.channel;
    let corr = frame.header.corr;
    let flags = frame.header.flags;
    let ver = frame.header.ver;
    let completion_metrics = Arc::clone(metrics);
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&completion_metrics);
        let response = await_executor_response(rx, request_id.clone()).await;
        let text = text_rx.await.unwrap_or_else(|_| {
            crate::subc_format::format_response_with_context(
                &bare_name_for_frame,
                &response,
                &format_context_for_frame,
            )
        });
        let result = ToolCallResult { text, response };
        let fatal = response_is_fatal_panic(&result.response);
        match build_tool_response_frame(ver, route_channel, corr, flags, &result) {
            Ok(response_frame) => {
                if let Err(error) = send_reliable_writer_frame(
                    &completion_tx,
                    &completion_metrics,
                    response_frame,
                    "tool response",
                )
                .await
                {
                    log::warn!("subc attach: failed to queue tool response frame: {error}");
                }
            }
            Err(error) => {
                log::error!("subc attach: failed to build tool response frame: {error}");
            }
        }
        if fatal {
            signal_fatal_teardown(
                &completion_tx,
                Some(route_channel),
                ver,
                corr,
                &completion_shutdown,
                &completion_metrics,
            )
            .await;
        }
    });
    Ok(())
}

fn submit_maintenance_drain(
    executor: &Arc<Executor>,
    root_id: ProjectRootId,
    bg_sessions_to_check: Vec<(String, u64)>,
    completion_tx: &mpsc::Sender<MaintenanceCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
) {
    let request_id = format!(
        "subc-maintenance-drain-{}",
        root_id.as_path().to_string_lossy()
    );
    let response_id = request_id.clone();
    let completion_root_id = root_id.clone();
    let (empty_bg_sessions_tx, empty_bg_sessions_rx) = oneshot::channel::<Vec<(String, u64)>>();
    let rx = executor.submit_maintenance_async(
        root_id,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            ctx.heartbeat_artifact_owner_lease();
            runtime_drain::drain_configure_warning_events(ctx);
            runtime_drain::drain_search_index_events(ctx);
            runtime_drain::drain_callgraph_store_events(ctx);
            runtime_drain::drain_semantic_index_events(ctx);
            runtime_drain::drain_semantic_refresh_events(ctx);
            runtime_drain::drain_inspect_events(ctx);
            runtime_drain::drain_watcher_events(ctx);
            runtime_drain::drain_lsp_events(ctx);
            let empty_bg_sessions = bg_sessions_to_check
                .into_iter()
                .filter(|(session, _)| {
                    !ctx.bash_background()
                        .has_completions_for_session(Some(session.as_str()))
                })
                .collect();
            let _ = empty_bg_sessions_tx.send(empty_bg_sessions);
            Response::success(response_id, json!({ "drained": true }))
        }),
    );
    let completion_tx = completion_tx.clone();
    let completion_metrics = Arc::clone(metrics);
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&completion_metrics);
        let response = await_executor_response(rx, request_id).await;
        let empty_bg_sessions = empty_bg_sessions_rx.await.unwrap_or_default();
        let _ = send_counted_channel(
            &completion_tx,
            &completion_metrics.maintenance_queued,
            MaintenanceCompletion {
                root_id: completion_root_id,
                response,
                empty_bg_sessions,
            },
        )
        .await;
    });
}

async fn await_executor_response(rx: oneshot::Receiver<Response>, request_id: String) -> Response {
    rx.await
        .unwrap_or_else(|_| Response::error(request_id, "internal_error", "executor dropped"))
}
async fn signal_fatal_teardown(
    tx: &mpsc::Sender<Frame>,
    route_channel: Option<u16>,
    ver: u8,
    corr: u64,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) {
    if let Some(route_channel) = route_channel {
        if let Ok(frame) = build_goodbye_frame(ver, route_channel, corr) {
            if let Err(error) = send_frame(tx, metrics, frame).await {
                log::warn!(
                    "subc attach: failed to queue fatal route Goodbye for route {route_channel}: {error}"
                );
            }
        }
    }
    if let Ok(frame) = build_goodbye_frame(ver, 0, 0) {
        if let Err(error) = send_frame(tx, metrics, frame).await {
            log::warn!("subc attach: failed to queue fatal channel-0 Goodbye: {error}");
        }
    }
    shutdown.notify_one();
}
#[derive(Deserialize)]
struct BgEventsProbe {
    op: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
    /// Server-owned preview control (B1c-0): the plugin's mutation flow is
    /// preview -> permission ask -> apply. Dropping this field made "preview"
    /// calls mutate disk before the permission prompt and the subsequent
    /// apply fail with not-found.
    #[serde(default)]
    preview: bool,
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::bash_background::BgTaskStatus;
    use crate::protocol::{
        BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame, ConfigureWarningsFrame,
        ProgressFrame, StatusChangedFrame,
    };
    use serde_json::json;

    pub(super) fn test_root(name: &str) -> (tempfile::TempDir, ProjectRootId) {
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("temp root");
        let root = ProjectRootId::from_path(dir.path()).expect("project root id");
        (dir, root)
    }

    pub(super) fn test_ctx() -> Arc<AppContext> {
        Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ))
    }

    pub(super) fn status_frame(seq: u64) -> PushFrame {
        status_frame_with_session(seq, None)
    }

    pub(super) fn status_frame_with_session(seq: u64, session_id: Option<&str>) -> PushFrame {
        PushFrame::StatusChanged(StatusChangedFrame {
            frame_type: "status_changed",
            session_id: session_id.map(str::to_string),
            snapshot: json!({ "seq": seq }),
        })
    }

    pub(super) fn completion_frame(task_id: &str) -> PushFrame {
        completion_frame_with_session(task_id, "session-1")
    }

    pub(super) fn completion_frame_with_session(task_id: &str, session_id: &str) -> PushFrame {
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

    pub(super) fn long_running_frame(task_id: &str, elapsed_ms: u64) -> PushFrame {
        long_running_frame_with_session(task_id, "session-1", elapsed_ms)
    }

    pub(super) fn long_running_frame_with_session(
        task_id: &str,
        session_id: &str,
        elapsed_ms: u64,
    ) -> PushFrame {
        PushFrame::BashLongRunning(BashLongRunningFrame {
            frame_type: "bash_long_running",
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            command: format!("sleep {elapsed_ms}"),
            elapsed_ms,
        })
    }

    pub(super) fn pattern_match_frame(session_id: &str) -> PushFrame {
        PushFrame::BashPatternMatch(BashPatternMatchFrame {
            frame_type: "bash_pattern_match",
            task_id: "task-pattern".to_string(),
            session_id: session_id.to_string(),
            watch_id: "watch-1".to_string(),
            match_text: "needle".to_string(),
            match_offset: 7,
            context: "haystack needle".to_string(),
            once: true,
            reason: "pattern_match",
        })
    }

    pub(super) fn configure_warnings_frame(session_id: Option<&str>) -> PushFrame {
        PushFrame::ConfigureWarnings(ConfigureWarningsFrame {
            frame_type: "configure_warnings",
            session_id: session_id.map(str::to_string),
            project_root: "/tmp/subc-test".to_string(),
            source_file_count: 0,
            warnings: Vec::new(),
        })
    }

    pub(super) fn route_identity(root: &ProjectRootId, session_id: &str) -> RouteIdentity {
        route_identity_with_trust(root, session_id, BindTrust::FirstParty)
    }

    pub(super) fn route_identity_with_trust(
        root: &ProjectRootId,
        session_id: &str,
        trust: BindTrust,
    ) -> RouteIdentity {
        RouteIdentity {
            root: root.clone(),
            project_root: root.as_path().to_path_buf(),
            harness: "opencode".to_string(),
            session: session_id.to_string(),
            trust,
        }
    }

    pub(super) fn progress_frame(request_id: &str, kind: ProgressKind, chunk: &str) -> PushFrame {
        PushFrame::Progress(ProgressFrame::new(request_id, kind, chunk))
    }

    pub(super) fn status_seq(frame: &PushFrame) -> Option<u64> {
        match frame {
            PushFrame::StatusChanged(status) => status.snapshot.get("seq").and_then(|v| v.as_u64()),
            _ => None,
        }
    }

    pub(super) fn completion_task(frame: &PushFrame) -> Option<&str> {
        match frame {
            PushFrame::BashCompleted(completion) => Some(completion.task_id.as_str()),
            _ => None,
        }
    }

    pub(super) fn push_frame_task_id(frame: &Frame) -> Option<String> {
        let body: serde_json::Value = serde_json::from_slice(&frame.body).expect("push body");
        body.get("task_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::test_root;
    use super::*;

    #[test]
    fn due_maintenance_roots_skip_poisoned_roots() {
        let (_healthy_dir, healthy_root) = test_root("maintenance-healthy");
        let (_poisoned_dir, poisoned_root) = test_root("maintenance-poisoned");
        let mut live_roots = HashMap::new();
        live_roots.insert(healthy_root.clone(), RootMeta::new(Instant::now()));
        let mut poisoned_meta = RootMeta::new(Instant::now());
        poisoned_meta.maintenance_poisoned = true;
        live_roots.insert(poisoned_root.clone(), poisoned_meta);

        let (due, deferred) = due_maintenance_roots(&mut live_roots, MAINTENANCE_SUBMIT_BUDGET);

        assert_eq!(due, vec![healthy_root.clone()]);
        assert!(!deferred);
        assert!(live_roots[&healthy_root].maintenance_pending);
        assert!(!live_roots[&poisoned_root].maintenance_pending);
    }

    #[test]
    fn due_maintenance_roots_defers_unsubmitted_roots_without_marking_pending() {
        let mut live_roots = HashMap::new();
        let mut root_ids = Vec::new();
        let mut _dirs = Vec::new();
        for index in 0..(MAINTENANCE_SUBMIT_BUDGET + 2) {
            let (dir, root_id) = test_root(&format!("maintenance-budget-{index}"));
            live_roots.insert(root_id.clone(), RootMeta::new(Instant::now()));
            root_ids.push(root_id);
            _dirs.push(dir);
        }

        let (first_due, first_deferred) =
            due_maintenance_roots(&mut live_roots, MAINTENANCE_SUBMIT_BUDGET);

        assert_eq!(first_due.len(), MAINTENANCE_SUBMIT_BUDGET);
        assert!(first_deferred);
        let first_due_set: HashSet<_> = first_due.into_iter().collect();
        assert!(first_due_set
            .iter()
            .all(|root| live_roots[root].maintenance_pending));

        let all_roots: HashSet<_> = root_ids.into_iter().collect();
        let deferred_roots: HashSet<_> = all_roots.difference(&first_due_set).cloned().collect();
        assert!(deferred_roots
            .iter()
            .all(|root| !live_roots[root].maintenance_pending));

        let (second_due, _) = due_maintenance_roots(&mut live_roots, usize::MAX);
        let second_due_set: HashSet<_> = second_due.into_iter().collect();
        assert_eq!(second_due_set, deferred_roots);
    }

    #[test]
    fn trust_for_principal_matrix() {
        assert_eq!(
            trust_for_principal(&Some(Principal::Direct)),
            BindTrust::FirstParty
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "llm-runner".to_string(),
            })),
            BindTrust::FirstParty
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "aft".to_string(),
            })),
            BindTrust::FirstParty
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "subc-mcp".to_string(),
            })),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Reserved {
                module_id: "anything-unknown".to_string(),
            })),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_principal(&Some(Principal::Unverified)),
            BindTrust::Untrusted
        );
        assert_eq!(trust_for_principal(&None), BindTrust::Untrusted);
    }

    #[test]
    fn fed_harness_class_maps_to_untrusted_regardless_of_fingerprint_value() {
        let principal = Some(Principal::Direct);
        let fingerprint_a = "fed:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let fingerprint_b = "fed:0123456789abcdef111111111111111111111111111111111111111111111111";

        assert_eq!(
            trust_for_bind(fingerprint_a, &principal),
            BindTrust::Untrusted
        );
        assert_eq!(
            trust_for_bind(fingerprint_b, &principal),
            BindTrust::Untrusted
        );
    }

    #[tokio::test]
    async fn persistent_cancel_resolves_when_fired_before_await() {
        // The lost-wakeup guard: cancel() fires exactly once via notify_waiters()
        // (no stored permit). A waiter that registers AFTER the cancel must still
        // observe it via the flag; a waiter racing the cancel must still be woken.
        let signal = PersistentCancelSignal::new();
        signal.cancel();
        // Fired before we ever call cancelled() — must return immediately, not park.
        tokio::time::timeout(Duration::from_secs(1), signal.cancelled())
            .await
            .expect("cancelled() must resolve when cancel fired beforehand");

        // A fresh signal cancelled concurrently with an in-flight cancelled().
        let racing = PersistentCancelSignal::new();
        let racing_for_task = racing.clone();
        let waiter = tokio::spawn(async move { racing_for_task.cancelled().await });
        racing.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancelled() must resolve when cancel races the await")
            .expect("waiter task panicked");
    }
}
