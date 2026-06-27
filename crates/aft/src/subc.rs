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
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::config_resolve::ConfigTier;
use crate::context::{App, AppContext, ProgressSender};
use crate::executor::{Executor, Lane};
use crate::log_ctx;
use crate::path_identity::ProjectRootId;
use crate::protocol::{ProgressKind, PushFrame, RawRequest, Response};
use crate::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome, ToolCallResult};
use crate::runtime_drain;

use subc_protocol::manifest::{
    Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
    ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    ErrorBody, Flags, Frame, FrameType, ModuleHelloBody, Priority, PROTOCOL_VERSION,
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

/// Small bounded memory of completed task ids used to suppress stale lossy
/// long-running reminders that arrive after their reliable completion event.
const COMPLETED_TASK_SUPPRESSION_MAX: usize = 4096;

type RouteChannel = u32;
type PushEnvelope = (ProjectRootId, PushFrame);
type RetryBuffer = HashMap<RouteChannel, VecDeque<(ReplayKey, PushFrame)>>;

#[derive(Clone)]
struct PushSenders {
    lossy_tx: mpsc::Sender<PushEnvelope>,
    reliable_tx: mpsc::UnboundedSender<PushEnvelope>,
}

#[derive(Debug)]
struct RootMeta {
    maintenance_pending: bool,
    last_touched: Instant,
    diagnostics_on_edit: bool,
}

#[derive(Debug)]
struct PendingBind {
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    cancelled: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReplayKey {
    root: ProjectRootId,
    harness: String,
    session: String,
}

impl ReplayKey {
    fn from_identity(identity: &RouteIdentity) -> Self {
        Self {
            root: identity.root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
        }
    }
}

#[derive(Debug, Default)]
struct CompletedTaskIds {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl CompletedTaskIds {
    fn remember(&mut self, task_id: &str) {
        if self.set.contains(task_id) {
            return;
        }
        if self.order.len() >= COMPLETED_TASK_SUPPRESSION_MAX {
            if let Some(evicted) = self.order.pop_front() {
                self.set.remove(&evicted);
            }
        }
        let task_id = task_id.to_string();
        self.order.push_back(task_id.clone());
        self.set.insert(task_id);
    }

    fn contains(&self, task_id: &str) -> bool {
        self.set.contains(task_id)
    }
}

impl RootMeta {
    fn new(now: Instant) -> Self {
        Self {
            maintenance_pending: false,
            last_touched: now,
            diagnostics_on_edit: false,
        }
    }

    fn touch(&mut self) {
        self.last_touched = Instant::now();
    }
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

fn remember_session_identity(
    session_identity: &mut HashMap<(ProjectRootId, String), String>,
    identity: &RouteIdentity,
) {
    // Retained after route Goodbye so reliable session-scoped frames emitted while
    // the session is detached can still be keyed by the full (root,harness,session)
    // replay triple. The idle-TTL actor reaper is responsible for pruning stale
    // identities/buffers once an actor is evicted.
    session_identity.insert(
        (identity.root.clone(), identity.session.clone()),
        identity.harness.clone(),
    );
}

fn replay_key_for_session(
    session_identity: &HashMap<(ProjectRootId, String), String>,
    root: &ProjectRootId,
    session: &str,
) -> Option<ReplayKey> {
    let harness = session_identity.get(&(root.clone(), session.to_string()))?;
    Some(ReplayKey {
        root: root.clone(),
        harness: harness.clone(),
        session: session.to_string(),
    })
}

fn frame_session(frame: &PushFrame) -> Option<&str> {
    match frame {
        PushFrame::BashCompleted(completed) => Some(completed.session_id.as_str()),
        PushFrame::BashLongRunning(long_running) => Some(long_running.session_id.as_str()),
        PushFrame::BashPatternMatch(pattern_match) => Some(pattern_match.session_id.as_str()),
        PushFrame::ConfigureWarnings(warnings) => warnings.session_id.as_deref(),
        PushFrame::StatusChanged(status) => status.session_id.as_deref(),
        PushFrame::Progress(_) => None,
    }
}

fn frame_is_reliable(frame: &PushFrame) -> bool {
    matches!(
        frame,
        PushFrame::BashCompleted(_)
            | PushFrame::BashPatternMatch(_)
            | PushFrame::ConfigureWarnings(_)
    )
}

fn completed_task_id(frame: &PushFrame) -> Option<&str> {
    match frame {
        PushFrame::BashCompleted(completed) => Some(completed.task_id.as_str()),
        _ => None,
    }
}

fn long_running_task_id(frame: &PushFrame) -> Option<&str> {
    match frame {
        PushFrame::BashLongRunning(long_running) => Some(long_running.task_id.as_str()),
        _ => None,
    }
}

fn should_drop_lossy_push(completed_tasks: &CompletedTaskIds, frame: &PushFrame) -> bool {
    long_running_task_id(frame).is_some_and(|task_id| completed_tasks.contains(task_id))
}

fn progress_sender_for_root(push_senders: PushSenders, root_id: ProjectRootId) -> ProgressSender {
    Arc::new(Box::new(move |frame: PushFrame| {
        // Emitters can run on executor workers, maintenance jobs, watcher drains,
        // semantic refresh workers, or bg-bash watchdog threads. Never block any
        // of them on subc routing/backpressure: reliable frames take an
        // unbounded non-blocking lane; lossy frames stay bounded and coalesced.
        if frame_is_reliable(&frame) {
            let _ = push_senders.reliable_tx.send((root_id.clone(), frame));
        } else {
            let _ = push_senders.lossy_tx.try_send((root_id.clone(), frame));
        }
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LossyProgressKind {
    Stdout,
    Stderr,
}

impl From<&ProgressKind> for LossyProgressKind {
    fn from(kind: &ProgressKind) -> Self {
        match kind {
            ProgressKind::Stdout => Self::Stdout,
            ProgressKind::Stderr => Self::Stderr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LossyPushKey {
    Progress {
        request_id: String,
        kind: LossyProgressKind,
    },
    StatusChanged,
    BashLongRunning {
        task_id: String,
    },
}

fn lossy_push_key(frame: &PushFrame) -> Option<LossyPushKey> {
    match frame {
        PushFrame::Progress(progress) => Some(LossyPushKey::Progress {
            request_id: progress.request_id.clone(),
            kind: LossyProgressKind::from(&progress.kind),
        }),
        PushFrame::StatusChanged(_) => Some(LossyPushKey::StatusChanged),
        PushFrame::BashLongRunning(long_running) => Some(LossyPushKey::BashLongRunning {
            task_id: long_running.task_id.clone(),
        }),
        PushFrame::BashCompleted(_)
        | PushFrame::BashPatternMatch(_)
        | PushFrame::ConfigureWarnings(_) => None,
    }
}

fn coalesce_push_batch(batch: Vec<(ProjectRootId, PushFrame)>) -> Vec<(ProjectRootId, PushFrame)> {
    let mut slots: Vec<Option<(ProjectRootId, PushFrame)>> = Vec::with_capacity(batch.len());
    let mut latest_lossy: HashMap<(ProjectRootId, LossyPushKey), usize> = HashMap::new();

    for (root, frame) in batch {
        if let Some(lossy_key) = lossy_push_key(&frame) {
            let map_key = (root.clone(), lossy_key);
            if let Some(previous_index) = latest_lossy.insert(map_key, slots.len()) {
                slots[previous_index] = None;
            }
        }
        slots.push(Some((root, frame)));
    }

    slots.into_iter().flatten().collect()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FanOutResult {
    /// Channels matching the frame's project/session scope. Reliable Push frames
    /// that match a channel but hit writer backpressure are held in retry_buffer
    /// instead of being mistaken for detach replay.
    matched_channels: usize,
    /// Frames accepted by the writer queue immediately. Lossy frames that are not
    /// accepted are dropped; reliable frames are retried on transient backpressure.
    sent_frames: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushSendOutcome {
    Sent,
    Backpressure,
    PermanentFailure,
}

fn try_send_push_body(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    body: &[u8],
) -> PushSendOutcome {
    let Ok(route_channel) = u16::try_from(channel) else {
        log::warn!("subc attach: invalid route channel {channel} for Push fan-out");
        return PushSendOutcome::PermanentFailure;
    };
    let push_frame = match Frame::build_with_version(
        PROTOCOL_VERSION,
        FrameType::Push,
        control_flags(),
        route_channel,
        0,
        body.to_vec(),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            log::warn!("subc attach: failed to build Push frame: {error}");
            return PushSendOutcome::PermanentFailure;
        }
    };
    match writer_tx.try_send(push_frame) {
        Ok(()) => PushSendOutcome::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => PushSendOutcome::Backpressure,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            log::warn!("subc attach: writer closed while sending Push frame");
            PushSendOutcome::PermanentFailure
        }
    }
}

fn try_send_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    frame: &PushFrame,
) -> PushSendOutcome {
    let body = match serde_json::to_vec(frame) {
        Ok(body) => body,
        Err(error) => {
            log::warn!("subc attach: failed to serialize PushFrame: {error}");
            return PushSendOutcome::PermanentFailure;
        }
    };
    try_send_push_body(writer_tx, channel, &body)
}

fn bounded_push_back<T>(queue: &mut VecDeque<T>, item: T) {
    if queue.len() >= PUSH_BUFFER_MAX_PER_KEY {
        queue.pop_front();
    }
    queue.push_back(item);
}

fn buffer_push_frame(
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    key: ReplayKey,
    frame: PushFrame,
) {
    bounded_push_back(push_buffer.entry(key).or_default(), frame);
}

fn buffer_retry_frame(
    retry_buffer: &mut RetryBuffer,
    channel: RouteChannel,
    key: ReplayKey,
    frame: PushFrame,
) {
    bounded_push_back(retry_buffer.entry(channel).or_default(), (key, frame));
}

fn migrate_retry_buffer_to_push_buffer(
    retry_buffer: &mut RetryBuffer,
    channel: RouteChannel,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
) -> usize {
    let Some(frames) = retry_buffer.remove(&channel) else {
        return 0;
    };
    let migrated = frames.len();
    for (key, frame) in frames {
        buffer_push_frame(push_buffer, key, frame);
    }
    migrated
}

fn replay_buffered_push_frames(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    key: &ReplayKey,
) -> usize {
    let mut sent = 0;
    let remove_empty;

    {
        let Some(queue) = push_buffer.get_mut(key) else {
            return 0;
        };

        while let Some(frame) = queue.pop_front() {
            match try_send_push_frame(writer_tx, channel, &frame) {
                PushSendOutcome::Sent => sent += 1,
                PushSendOutcome::Backpressure => {
                    queue.push_front(frame);
                    break;
                }
                PushSendOutcome::PermanentFailure => {
                    log::warn!(
                        "subc attach: dropping buffered reliable Push for root {} harness {} session {} after permanent send failure",
                        key.root.as_path().display(),
                        key.harness,
                        key.session
                    );
                }
            }
        }

        remove_empty = queue.is_empty();
    }

    if remove_empty {
        push_buffer.remove(key);
    }

    sent
}

fn drain_retry_buffer_for_channel(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    retry_buffer: &mut RetryBuffer,
) -> usize {
    let mut sent = 0;
    let remove_empty;

    {
        let Some(queue) = retry_buffer.get_mut(&channel) else {
            return 0;
        };

        while let Some((key, frame)) = queue.pop_front() {
            match try_send_push_frame(writer_tx, channel, &frame) {
                PushSendOutcome::Sent => sent += 1,
                PushSendOutcome::Backpressure => {
                    queue.push_front((key, frame));
                    break;
                }
                PushSendOutcome::PermanentFailure => {
                    log::warn!(
                        "subc attach: dropping retry-buffered reliable Push for route {channel} root {} harness {} session {} after permanent send failure",
                        key.root.as_path().display(),
                        key.harness,
                        key.session
                    );
                }
            }
        }

        remove_empty = queue.is_empty();
    }

    if remove_empty {
        retry_buffer.remove(&channel);
    }

    sent
}

fn drain_retry_buffers_for_bound_routes(
    writer_tx: &mpsc::Sender<Frame>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    retry_buffer: &mut RetryBuffer,
) -> usize {
    let channels: Vec<RouteChannel> = routes.keys().copied().collect();
    channels
        .into_iter()
        .map(|channel| drain_retry_buffer_for_channel(writer_tx, channel, retry_buffer))
        .sum()
}

fn matching_route_channels(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    frame: &PushFrame,
) -> Vec<RouteChannel> {
    let Some(channels) = root_channels.get(root) else {
        return Vec::new();
    };

    let session = frame_session(frame);
    channels
        .iter()
        .copied()
        .filter(|channel| match session {
            Some(session) => routes
                .get(channel)
                .is_some_and(|identity| identity.session == session),
            None => true,
        })
        .collect()
}

fn buffer_detached_reliable_push_frame(
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    session_identity: &HashMap<(ProjectRootId, String), String>,
    root: &ProjectRootId,
    frame: &PushFrame,
) {
    let Some(session) = frame_session(frame) else {
        log::warn!(
            "subc attach: dropping reliable project-scoped Push for root {} because no route is bound",
            root.as_path().display()
        );
        return;
    };

    if let Some(key) = replay_key_for_session(session_identity, root, session) {
        buffer_push_frame(push_buffer, key, frame.clone());
    } else {
        log::warn!(
            "subc attach: dropping reliable Push for root {} session {} because no retained harness identity is known",
            root.as_path().display(),
            session
        );
    }
}

fn fan_out_lossy_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    frame: &PushFrame,
) -> FanOutResult {
    let matching_channels = matching_route_channels(routes, root_channels, root, frame);
    let matched_channels = matching_channels.len();
    if matched_channels == 0 {
        return FanOutResult::default();
    }

    let body = match serde_json::to_vec(frame) {
        Ok(body) => body,
        Err(error) => {
            log::warn!("subc attach: failed to serialize PushFrame for fan-out: {error}");
            return FanOutResult {
                matched_channels,
                sent_frames: 0,
            };
        }
    };

    let sent_frames = matching_channels
        .into_iter()
        .filter(|&channel| {
            matches!(
                try_send_push_body(writer_tx, channel, &body),
                PushSendOutcome::Sent
            )
        })
        .count();

    FanOutResult {
        matched_channels,
        sent_frames,
    }
}

fn fan_out_reliable_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &HashMap<(ProjectRootId, String), String>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    root: &ProjectRootId,
    frame: &PushFrame,
) -> FanOutResult {
    let matching_channels = matching_route_channels(routes, root_channels, root, frame);
    let matched_channels = matching_channels.len();
    if matched_channels == 0 {
        buffer_detached_reliable_push_frame(push_buffer, session_identity, root, frame);
        return FanOutResult::default();
    }

    let mut sent_frames = 0;
    for channel in matching_channels {
        let Some(identity) = routes.get(&channel) else {
            log::warn!(
                "subc attach: dropping reliable Push for stale route channel {channel} with no route identity"
            );
            continue;
        };
        let key = ReplayKey::from_identity(identity);

        if retry_buffer
            .get(&channel)
            .is_some_and(|queue| !queue.is_empty())
        {
            buffer_retry_frame(retry_buffer, channel, key, frame.clone());
            continue;
        }

        match try_send_push_frame(writer_tx, channel, frame) {
            PushSendOutcome::Sent => sent_frames += 1,
            PushSendOutcome::Backpressure => {
                buffer_retry_frame(retry_buffer, channel, key, frame.clone());
            }
            PushSendOutcome::PermanentFailure => {
                log::warn!(
                    "subc attach: dropping reliable Push for route {channel} root {} harness {} session {} after permanent send failure",
                    key.root.as_path().display(),
                    key.harness,
                    key.session
                );
            }
        }
    }

    FanOutResult {
        matched_channels,
        sent_frames,
    }
}

fn process_reliable_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &HashMap<(ProjectRootId, String), String>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    completed_tasks: &mut CompletedTaskIds,
    root: ProjectRootId,
    frame: PushFrame,
) {
    if let Some(task_id) = completed_task_id(&frame) {
        completed_tasks.remember(task_id);
    }
    let _ = fan_out_reliable_push_frame(
        writer_tx,
        routes,
        root_channels,
        session_identity,
        retry_buffer,
        push_buffer,
        &root,
        &frame,
    );
}

fn process_lossy_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    completed_tasks: &CompletedTaskIds,
    root: ProjectRootId,
    frame: PushFrame,
) {
    if should_drop_lossy_push(completed_tasks, &frame) {
        if let Some(task_id) = long_running_task_id(&frame) {
            log::debug!(
                "subc attach: dropping stale BashLongRunning Push for completed task {task_id}"
            );
        }
        return;
    }

    let _ = fan_out_lossy_push_frame(writer_tx, routes, root_channels, &root, &frame);
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
    // ModuleHello: register as a tool provider. control_ops:None = full baseline.
    // Echo the one-time launch nonce the daemon injected via SUBC_LAUNCH_NONCE so a
    // reserved module_id's HELLO is accepted; absent for non-reserved/self-connect.
    let hello = ModuleHelloBody {
        manifest: build_manifest(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
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

    let (writer_tx, writer_rx) = mpsc::channel::<Frame>(256);
    let writer_task = spawn_writer_task(write, writer_rx);
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
    let (maintenance_tx, mut maintenance_rx) = mpsc::channel::<(ProjectRootId, Response)>(256);
    let (control_completion_tx, mut control_completion_rx) =
        mpsc::channel::<RouteBindCompletion>(256);
    let (lossy_tx, mut lossy_rx) = mpsc::channel::<PushEnvelope>(1024);
    let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
    let push_senders = PushSenders {
        lossy_tx,
        reliable_tx,
    };
    let mut routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
    let mut root_channels: HashMap<ProjectRootId, HashSet<RouteChannel>> = HashMap::new();
    let mut session_identity: HashMap<(ProjectRootId, String), String> = HashMap::new();
    let mut push_buffer: HashMap<ReplayKey, VecDeque<PushFrame>> = HashMap::new();
    let mut retry_buffer: RetryBuffer = HashMap::new();
    let mut completed_tasks = CompletedTaskIds::default();
    let mut live_roots: HashMap<ProjectRootId, RootMeta> = HashMap::new();
    let mut pending_binds: HashMap<RouteChannel, PendingBind> = HashMap::new();

    let loop_result: Result<(), SubcError> = loop {
        tokio::select! {
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
                        if let Err(error) = send_frame(&writer_tx, pong).await {
                            break Err(error);
                        }
                    }
                    FrameType::Goodbye if frame.header.channel == 0 => {
                        log::info!("subc attach: received channel-0 Goodbye");
                        break Ok(());
                    }
                    FrameType::Goodbye => {
                        let channel = route_key(frame.header.channel);
                        if let Some(pending) = pending_binds.get_mut(&channel) {
                            pending.cancelled = true;
                            log::debug!(
                                "subc attach: cancelled pending RouteBind for route {} on Goodbye",
                                frame.header.channel
                            );
                        }
                        let migrated = migrate_retry_buffer_to_push_buffer(
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
                            dispatch,
                            allow_native_passthrough,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    // Cancel/Push/etc. are not yet handled: in-flight tool-call
                    // cancellation is not implemented, so these frame types are
                    // ignored rather than acted on.
                    _ => {}
                }
            }
            Some((root_id, frame)) = reliable_rx.recv() => {
                // Drain reliable frames in FIFO order. They are intentionally not
                // coalesced: completion, pattern-match, and warning frames are
                // must-deliver events.
                let mut batch = vec![(root_id, frame)];
                while let Ok(item) = reliable_rx.try_recv() {
                    batch.push(item);
                }

                for (root, frame) in batch {
                    process_reliable_push_frame(
                        &writer_tx,
                        &routes,
                        &root_channels,
                        &session_identity,
                        &mut retry_buffer,
                        &mut push_buffer,
                        &mut completed_tasks,
                        root,
                        frame,
                    );
                }
            }
            Some((root_id, frame)) = lossy_rx.recv() => {
                // If both lanes are ready, process any already-queued reliable
                // completions first so a following stale BashLongRunning frame can
                // be suppressed even if select! happened to wake on the lossy lane.
                while let Ok((reliable_root, reliable_frame)) = reliable_rx.try_recv() {
                    process_reliable_push_frame(
                        &writer_tx,
                        &routes,
                        &root_channels,
                        &session_identity,
                        &mut retry_buffer,
                        &mut push_buffer,
                        &mut completed_tasks,
                        reliable_root,
                        reliable_frame,
                    );
                }

                // Drain the currently queued burst in one loop turn so lossy
                // status/progress classes coalesce before reaching subc's shared
                // egress queue.
                let mut batch = vec![(root_id, frame)];
                while let Ok(item) = lossy_rx.try_recv() {
                    batch.push(item);
                }

                for (root, frame) in coalesce_push_batch(batch) {
                    process_lossy_push_frame(
                        &writer_tx,
                        &routes,
                        &root_channels,
                        &completed_tasks,
                        root,
                        frame,
                    );
                }
            }
            Some(completion) = control_completion_rx.recv() => {
                if let Err(error) = handle_route_bind_completion(
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
                )
                .await
                {
                    break Err(error);
                }
            }
            Some((root_id, response)) = maintenance_rx.recv() => {
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.maintenance_pending = false;
                }
                if response_is_internal_error(&response) {
                    signal_fatal_teardown(&writer_tx, None, PROTOCOL_VERSION, 0, &shutdown).await;
                }
            }
            _ = drain_interval.tick() => {
                let retried = drain_retry_buffers_for_bound_routes(
                    &writer_tx,
                    &routes,
                    &mut retry_buffer,
                );
                if retried > 0 {
                    log::debug!(
                        "subc attach: retried {retried} reliable Push frame(s) after writer backpressure"
                    );
                }

                let due_roots: Vec<ProjectRootId> = live_roots
                    .iter_mut()
                    .filter_map(|(root_id, meta)| {
                        if meta.maintenance_pending {
                            None
                        } else {
                            meta.maintenance_pending = true;
                            Some(root_id.clone())
                        }
                    })
                    .collect();
                for root_id in due_roots {
                    submit_maintenance_drain(&executor, root_id, &maintenance_tx);
                }
            }
        }
    };

    // The reader task may be parked on `read_frame`; abort it (we are done with
    // the connection) and flush the writer.
    reader_task.abort();
    drop(writer_tx);
    let writer_result = finish_writer_task(writer_task).await;
    loop_result.and(writer_result)
}

fn spawn_writer_task<W>(
    mut write: W,
    mut rx: mpsc::Receiver<Frame>,
) -> JoinHandle<Result<(), subc_transport::FrameIoError>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
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

async fn send_frame(tx: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), SubcError> {
    match tokio::time::timeout(CONTROL_SEND_TIMEOUT, tx.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(SubcError::WriterClosed),
        Err(_) => Err(SubcError::WriterBackpressureTimeout),
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
    session_identity: &mut HashMap<(ProjectRootId, String), String>,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
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
        let fatal = response_is_internal_error(response);
        send_route_bind_error_parts(
            tx,
            completion.ver,
            completion.corr,
            completion.flags,
            "config_divergence",
            &message,
        )
        .await?;
        if fatal {
            signal_fatal_teardown(
                tx,
                Some(completion.route_channel),
                completion.ver,
                completion.corr,
                shutdown,
            )
            .await;
        }
        return Ok(());
    }

    remember_session_identity(session_identity, &completion.identity);
    let replay_key = ReplayKey::from_identity(&completion.identity);
    insert_route_channel(routes, root_channels, route_id, completion.identity);
    live_roots
        .entry(completion.bind_root_id.clone())
        .and_modify(|meta| {
            meta.touch();
            meta.diagnostics_on_edit = completion.diagnostics_on_edit;
        })
        .or_insert_with(|| RootMeta::new(Instant::now()));
    if let Some(meta) = live_roots.get_mut(&completion.bind_root_id) {
        meta.diagnostics_on_edit = completion.diagnostics_on_edit;
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
    send_frame(tx, response).await?;
    let replayed = replay_buffered_push_frames(tx, route_id, push_buffer, &replay_key);
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

/// channel-0 control request — currently only RouteBind. Reconciles the route's
/// RootConfig through the executor's Mutating lane and resolves completion on a
/// loop-owned control-completion channel so slow configure jobs do not block the
/// transport loop.
async fn handle_control_request(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    control_completion_tx: &mpsc::Sender<RouteBindCompletion>,
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
            // Any wire-relayed `config` field is ignored via `..`: AFT reads config
            // from CortexKit files, never the wire. `..` keeps this tolerant whether
            // the protocol version still carries the field or has dropped it, so a
            // protocol field-removal cannot break this destructure either way.
            ..
        } => {
            let route_id = route_key(route_channel);
            if pending_binds.contains_key(&route_id) {
                return send_route_bind_error(
                    tx,
                    frame,
                    "config_divergence",
                    "route bind is already pending for channel",
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

            // Config is single-per-project, read by AFT directly from the
            // CortexKit config files (user: ~/.config/cortexkit/aft.jsonc,
            // project: <root>/.cortexkit/aft.jsonc). Wire-relayed config tiers are
            // IGNORED entirely: a front (runner or mcp:*) cannot push config over
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
                    )
                    .await;
                }
            };

            let route_identity = RouteIdentity {
                root: bind_root_id.clone(),
                project_root: PathBuf::from(&bind_project_root),
                harness: bind_harness.clone(),
                session: bind_session.clone(),
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
                actor_ctx.set_progress_sender(Some(progress_sender_for_root(
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

            pending_binds.insert(
                route_id,
                PendingBind {
                    bind_root_id: bind_root_id.clone(),
                    inserted_new_actor,
                    cancelled: false,
                },
            );

            let configure_request_id = configure_req.id.clone();
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
            tokio::spawn(async move {
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
                if completion_tx.send(completion).await.is_err() {
                    log::debug!(
                        "subc attach: dropped RouteBind completion for route {} after loop exit",
                        completion_route_channel
                    );
                }
            });

            Ok(())
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
    let stripped = strip_jsonc_for_subc(doc);
    let value = serde_json::from_str::<Value>(&stripped).ok()?;
    value
        .get("lsp")
        .and_then(Value::as_object)?
        .get("diagnostics_on_edit")
        .and_then(Value::as_bool)
}

fn strip_jsonc_for_subc(source: &str) -> String {
    strip_trailing_commas_for_subc(&strip_jsonc_comments_for_subc(source))
}

fn strip_jsonc_comments_for_subc(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            continue;
        }

        if ch == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for next in chars.by_ref() {
                        if next == '\n' {
                            output.push('\n');
                        }
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                }
                _ => output.push(ch),
            }
            continue;
        }

        output.push(ch);
    }

    output
}

fn strip_trailing_commas_for_subc(source: &str) -> String {
    let chars = source.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    while index < chars.len() {
        let ch = chars[index];
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        if ch == '"' {
            in_string = true;
            output.push(ch);
            index += 1;
            continue;
        }

        if ch == ',' {
            let mut next = index + 1;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            if next < chars.len() && matches!(chars[next], '}' | ']') {
                index += 1;
                continue;
            }
        }

        output.push(ch);
        index += 1;
    }

    output
}

async fn send_route_bind_error(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    code: &str,
    message: &str,
) -> Result<(), SubcError> {
    send_route_bind_error_parts(
        tx,
        frame.header.ver,
        frame.header.corr,
        frame.header.flags,
        code,
        message,
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
) -> Result<(), SubcError> {
    let response = build_error_frame(ver, 0, corr, flags, code, message)?;
    send_frame(tx, response).await?;
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
        return send_frame(tx, error).await;
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
        return send_frame(tx, error).await;
    };
    if let Some(meta) = live_roots.get_mut(&identity.root) {
        meta.touch();
    }

    let call = serde_json::from_slice::<ToolCallRequest>(&frame.body).map_err(SubcError::Json)?;
    let bare_name = call.name.clone();
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        &bare_name,
        &call.arguments,
        identity.project_root.as_path(),
    );

    let request_id = format!("subc-{}-{}", frame.header.channel, frame.header.corr);
    let diagnostics_on_edit = live_roots
        .get(&identity.root)
        .map(|meta| meta.diagnostics_on_edit)
        .unwrap_or(false);

    // A non-core name is NOT in the tool manifest. AFT fails closed and
    // does not trust subc to enforce the manifest: rejecting here is the
    // defense-in-depth backstop that prevents a forwarded native command
    // (e.g. `configure`, which would reach handle_configure and bypass
    // the RouteBind config-trust cap) from ever reaching dispatch. Only
    // the integration-test harness (run_subc_mode_for_test) opens this to
    // drive synthetic native commands through the executor.
    if !is_subc_agent_core_tool(&call.name) && !allow_native_passthrough {
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
        return send_frame(tx, response_frame).await;
    }

    let lane = command_lane(&bare_name);
    let tool_call_context = ToolCallContext {
        project_root: identity.project_root.clone(),
        session_id: Some(identity.session.clone()),
        request_id: request_id.clone(),
        diagnostics_on_edit,
    };
    let arguments_for_run = call.arguments.clone();
    let bare_name_for_run = bare_name.clone();
    let bare_name_for_frame = bare_name.clone();
    let bare_name_for_finalize = bare_name.clone();
    let session_for_log = identity.session.clone();
    let session_for_finalize = identity.session.clone();
    let format_context_for_frame = format_context;
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let rx = executor.submit_async(
        identity.root,
        lane,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_log.clone()), || {
                let dispatch_with_finalize = |raw_req: RawRequest, app_ctx: &AppContext| {
                    let mut response = dispatch(raw_req, app_ctx);
                    crate::response_finalize::attach_bg_completions(
                        &mut response,
                        app_ctx,
                        &session_for_finalize,
                        &bare_name_for_finalize,
                    );
                    crate::response_finalize::attach_status_bar(
                        &mut response,
                        app_ctx,
                        &bare_name_for_finalize,
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
            })
        }),
    );
    let completion_tx = tx.clone();
    let completion_shutdown = Arc::clone(shutdown);
    let route_channel = frame.header.channel;
    let corr = frame.header.corr;
    let flags = frame.header.flags;
    let ver = frame.header.ver;
    let is_mutating = lane == Lane::Mutating;
    tokio::spawn(async move {
        let response = await_executor_response(rx, request_id.clone()).await;
        let text = text_rx.await.unwrap_or_else(|_| {
            crate::subc_format::format_response_with_context(
                &bare_name_for_frame,
                &response,
                &format_context_for_frame,
            )
        });
        let result = ToolCallResult { text, response };
        let fatal = is_mutating && response_is_internal_error(&result.response);
        match build_tool_response_frame(ver, route_channel, corr, flags, &result) {
            Ok(response_frame) => {
                let _ = completion_tx.send(response_frame).await;
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
            )
            .await;
        }
    });
    Ok(())
}

fn submit_maintenance_drain(
    executor: &Arc<Executor>,
    root_id: ProjectRootId,
    completion_tx: &mpsc::Sender<(ProjectRootId, Response)>,
) {
    let request_id = format!(
        "subc-maintenance-drain-{}",
        root_id.as_path().to_string_lossy()
    );
    let response_id = request_id.clone();
    let completion_root_id = root_id.clone();
    let rx = executor.submit_async(
        root_id,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            runtime_drain::drain_configure_warning_events(ctx);
            runtime_drain::drain_search_index_events(ctx);
            runtime_drain::drain_callgraph_store_events(ctx);
            runtime_drain::drain_semantic_index_events(ctx);
            runtime_drain::drain_semantic_refresh_events(ctx);
            runtime_drain::drain_inspect_events(ctx);
            runtime_drain::drain_watcher_events(ctx);
            runtime_drain::drain_lsp_events(ctx);
            Response::success(response_id, json!({ "drained": true }))
        }),
    );
    let completion_tx = completion_tx.clone();
    tokio::spawn(async move {
        let response = await_executor_response(rx, request_id).await;
        let _ = completion_tx.send((completion_root_id, response)).await;
    });
}

async fn await_executor_response(rx: oneshot::Receiver<Response>, request_id: String) -> Response {
    rx.await
        .unwrap_or_else(|_| Response::error(request_id, "internal_error", "executor dropped"))
}

fn build_tool_response_frame(
    ver: u8,
    route_channel: u16,
    corr: u64,
    flags: Flags,
    result: &ToolCallResult,
) -> Result<Frame, SubcError> {
    let is_error = !result.response.success;
    let result = json!({
        "content": [{ "type": "text", "text": result.text.as_str() }],
        "isError": is_error,
    });
    let body = serde_json::to_vec(&result).map_err(SubcError::Json)?;

    Frame::build_with_version(ver, FrameType::Response, flags, route_channel, corr, body)
        .map_err(SubcError::FrameBuild)
}

fn build_error_frame(
    ver: u8,
    channel: u16,
    corr: u64,
    flags: Flags,
    code: &str,
    message: &str,
) -> Result<Frame, SubcError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(SubcError::Json)?;
    Frame::build_with_version(ver, FrameType::Error, flags, channel, corr, body)
        .map_err(SubcError::FrameBuild)
}

fn build_goodbye_frame(ver: u8, channel: u16, corr: u64) -> Result<Frame, SubcError> {
    Frame::build_with_version(
        ver,
        FrameType::Goodbye,
        control_flags(),
        channel,
        corr,
        Vec::new(),
    )
    .map_err(SubcError::FrameBuild)
}

async fn signal_fatal_teardown(
    tx: &mpsc::Sender<Frame>,
    route_channel: Option<u16>,
    ver: u8,
    corr: u64,
    shutdown: &Arc<Notify>,
) {
    if let Some(route_channel) = route_channel {
        if let Ok(frame) = build_goodbye_frame(ver, route_channel, corr) {
            if let Err(error) = send_frame(tx, frame).await {
                log::warn!(
                    "subc attach: failed to queue fatal route Goodbye for route {route_channel}: {error}"
                );
            }
        }
    }
    if let Ok(frame) = build_goodbye_frame(ver, 0, 0) {
        if let Err(error) = send_frame(tx, frame).await {
            log::warn!("subc attach: failed to queue fatal channel-0 Goodbye: {error}");
        }
    }
    shutdown.notify_one();
}

fn response_message(response: &Response, fallback: &str) -> String {
    response
        .data
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback.to_string())
}

fn response_is_internal_error(response: &Response) -> bool {
    !response.success && response.data.get("code").and_then(Value::as_str) == Some("internal_error")
}

fn is_subc_agent_core_tool(name: &str) -> bool {
    matches!(
        name,
        "status" | "read" | "write" | "edit" | "grep" | "search" | "outline" | "inspect"
    )
}

fn command_lane(command: &str) -> Lane {
    match command {
        "ping"
        | "version"
        | "echo"
        | "bash_drain_completions"
        | "bash_regex_match"
        | "db_get_state"
        | "db_get_host_state"
        | "read"
        | "undo_preview"
        | "edit_history"
        | "checkpoint_paths"
        | "list_checkpoints"
        | "glob"
        | "grep"
        | "git_conflicts"
        | "ast_search" => Lane::PureRead,

        // Lazy reads mutate parser/terminal/url caches on a miss, but are still
        // classified onto the reader pool; install races are handled at the
        // individual cache sites.
        "bash_status" | "outline" | "zoom" => Lane::PureRead,

        "status"
        | "inspect"
        | "lsp_diagnostics"
        | "lsp_inspect"
        | "lsp_hover"
        | "lsp_goto_definition"
        | "lsp_find_references"
        | "lsp_prepare_rename" => Lane::SerialLspStatus,

        "semantic_search" | "search" | "callers" | "impact" | "call_tree" | "trace_to"
        | "trace_to_symbol" | "trace_data" | "inspect_tier2_run" => Lane::HeavyInit,

        "bash"
        | "bash_ack_completions"
        | "bash_notify"
        | "bash_unnotify"
        | "bash_promote"
        | "bash_kill"
        | "bash_write"
        | "db_set_state"
        | "db_set_host_state"
        | "undo"
        | "checkpoint"
        | "restore_checkpoint"
        | "write"
        | "delete_file"
        | "move_file"
        | "edit"
        | "edit_symbol"
        | "edit_match"
        | "batch"
        | "add_import"
        | "remove_import"
        | "organize_imports"
        | "configure"
        | "move_symbol"
        | "extract_function"
        | "inline_symbol"
        | "ast_replace"
        | "lsp_rename"
        | "list_filters"
        | "trust_filter_project"
        | "untrust_filter_project"
        | "snapshot" => Lane::Mutating,

        _ => Lane::Mutating,
    }
}

#[derive(Debug, Deserialize)]
struct ToolCallRequest {
    name: String,
    #[serde(default)]
    arguments: Value,
}

static SUBC_TOOL_SCHEMAS: LazyLock<serde_json::Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("subc_tool_schemas.json"))
        .unwrap_or_else(|e| panic!("subc_tool_schemas.json: {e}"))
});

fn tool_schema(name: &str) -> Value {
    SUBC_TOOL_SCHEMAS.get(name).cloned().unwrap_or_else(|| {
        log::warn!(
            "subc build_manifest: missing embedded schema for tool {name:?}; using placeholder"
        );
        json!({ "type": "object" })
    })
}

/// AFT's subc-mode capability manifest. BARE tool names (the gateway owns the
/// `aft_` prefix); ModuleManaged concurrency (AFT schedules internally);
/// FirstParty trust. Minimal-but-conformant tool set for the spike — the full
/// bare set is locked before the gateway fronts AFT.
fn build_manifest() -> ModuleManifest {
    let tool = |name: &str, execution_mode: ExecutionMode| Tool {
        name: name.to_string(),
        execution_mode,
        schema: tool_schema(name),
    };
    // execution_mode keys on externally-observable side effects, NOT internal
    // ctx mutation: the readers warm AFT's own index/cache/symbol artifacts
    // (internal), not the user's workspace, so they are Pure. Only edit/write
    // produce observable file writes -> Mutating. There is no bash tool in this
    // core, so Unfenceable is unused here (it stays in the enum for bash/other
    // modules).
    ModuleManifest {
        module_id: "aft".to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![
                tool("status", ExecutionMode::Pure),
                tool("read", ExecutionMode::Pure),
                tool("grep", ExecutionMode::Pure),
                tool("search", ExecutionMode::Pure),
                tool("outline", ExecutionMode::Pure),
                tool("inspect", ExecutionMode::Pure),
                tool("edit", ExecutionMode::Mutating),
                tool("write", ExecutionMode::Mutating),
            ],
            identity_scope: vec![IdentityScope::Session, IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::BgTaskStatus;
    use crate::protocol::{
        BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame, ConfigureWarningsFrame,
        ProgressFrame, StatusChangedFrame,
    };
    use serde_json::json;

    fn test_root(name: &str) -> (tempfile::TempDir, ProjectRootId) {
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("temp root");
        let root = ProjectRootId::from_path(dir.path()).expect("project root id");
        (dir, root)
    }

    fn status_frame(seq: u64) -> PushFrame {
        status_frame_with_session(seq, None)
    }

    fn status_frame_with_session(seq: u64, session_id: Option<&str>) -> PushFrame {
        PushFrame::StatusChanged(StatusChangedFrame {
            frame_type: "status_changed",
            session_id: session_id.map(str::to_string),
            snapshot: json!({ "seq": seq }),
        })
    }

    fn completion_frame(task_id: &str) -> PushFrame {
        completion_frame_with_session(task_id, "session-1")
    }

    fn completion_frame_with_session(task_id: &str, session_id: &str) -> PushFrame {
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

    fn long_running_frame(task_id: &str, elapsed_ms: u64) -> PushFrame {
        long_running_frame_with_session(task_id, "session-1", elapsed_ms)
    }

    fn long_running_frame_with_session(
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

    fn pattern_match_frame(session_id: &str) -> PushFrame {
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

    fn configure_warnings_frame(session_id: Option<&str>) -> PushFrame {
        PushFrame::ConfigureWarnings(ConfigureWarningsFrame {
            frame_type: "configure_warnings",
            session_id: session_id.map(str::to_string),
            project_root: "/tmp/subc-test".to_string(),
            source_file_count: 0,
            warnings: Vec::new(),
        })
    }

    fn route_identity(root: &ProjectRootId, session_id: &str) -> RouteIdentity {
        RouteIdentity {
            root: root.clone(),
            project_root: root.as_path().to_path_buf(),
            harness: "opencode".to_string(),
            session: session_id.to_string(),
        }
    }

    fn progress_frame(request_id: &str, kind: ProgressKind, chunk: &str) -> PushFrame {
        PushFrame::Progress(ProgressFrame::new(request_id, kind, chunk))
    }

    fn status_seq(frame: &PushFrame) -> Option<u64> {
        match frame {
            PushFrame::StatusChanged(status) => status.snapshot.get("seq").and_then(|v| v.as_u64()),
            _ => None,
        }
    }

    fn completion_task(frame: &PushFrame) -> Option<&str> {
        match frame {
            PushFrame::BashCompleted(completion) => Some(completion.task_id.as_str()),
            _ => None,
        }
    }

    fn push_frame_task_id(frame: &Frame) -> Option<String> {
        let body: serde_json::Value = serde_json::from_slice(&frame.body).expect("push body");
        body.get("task_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn frame_classification_matches_push_delivery_contract() {
        let completion = completion_frame_with_session("done", "session-a");
        assert_eq!(frame_session(&completion), Some("session-a"));
        assert!(frame_is_reliable(&completion));

        let long_running = long_running_frame_with_session("long", "session-b", 42);
        assert_eq!(frame_session(&long_running), Some("session-b"));
        assert!(!frame_is_reliable(&long_running));

        let pattern_match = pattern_match_frame("session-c");
        assert_eq!(frame_session(&pattern_match), Some("session-c"));
        assert!(frame_is_reliable(&pattern_match));

        let tagged_warnings = configure_warnings_frame(Some("session-d"));
        assert_eq!(frame_session(&tagged_warnings), Some("session-d"));
        assert!(frame_is_reliable(&tagged_warnings));

        let untagged_warnings = configure_warnings_frame(None);
        assert_eq!(frame_session(&untagged_warnings), None);
        assert!(frame_is_reliable(&untagged_warnings));

        let tagged_status = status_frame_with_session(1, Some("session-e"));
        assert_eq!(frame_session(&tagged_status), Some("session-e"));
        assert!(!frame_is_reliable(&tagged_status));

        let project_status = status_frame(2);
        assert_eq!(frame_session(&project_status), None);
        assert!(!frame_is_reliable(&project_status));

        let progress = progress_frame("request-1", ProgressKind::Stdout, "chunk");
        assert_eq!(frame_session(&progress), None);
        assert!(!frame_is_reliable(&progress));
    }

    #[test]
    fn fan_out_push_frame_routes_session_scoped_and_project_scoped_frames() {
        let (_root_dir, root) = test_root("subc-session-routing-root");
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(8);
        let identity1 = route_identity(&root, "session-1");
        let identity2 = route_identity(&root, "session-2");
        let mut routes = HashMap::new();
        routes.insert(route_key(1), identity1.clone());
        routes.insert(route_key(2), identity2.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(1), route_key(2)]));
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity1);
        remember_session_identity(&mut session_identity, &identity2);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();

        let session_result = fan_out_reliable_push_frame(
            &writer_tx,
            &routes,
            &root_channels,
            &session_identity,
            &mut retry_buffer,
            &mut push_buffer,
            &root,
            &completion_frame_with_session("session-only", "session-1"),
        );
        assert_eq!(
            session_result,
            FanOutResult {
                matched_channels: 1,
                sent_frames: 1,
            }
        );
        assert!(retry_buffer.is_empty());
        assert!(push_buffer.is_empty());
        let session_push = writer_rx.try_recv().expect("session push queued");
        assert_eq!(session_push.header.ty, FrameType::Push);
        assert_eq!(session_push.header.channel, 1);
        assert!(
            writer_rx.try_recv().is_err(),
            "session-scoped frame must not broadcast to sibling sessions"
        );

        let project_result =
            fan_out_lossy_push_frame(&writer_tx, &routes, &root_channels, &root, &status_frame(9));
        assert_eq!(
            project_result,
            FanOutResult {
                matched_channels: 2,
                sent_frames: 2,
            }
        );
        let project_channels: HashSet<_> = [
            writer_rx
                .try_recv()
                .expect("first project push")
                .header
                .channel,
            writer_rx
                .try_recv()
                .expect("second project push")
                .header
                .channel,
        ]
        .into_iter()
        .collect();
        assert_eq!(project_channels, HashSet::from([1, 2]));
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn push_buffer_drops_oldest_per_replay_key() {
        let (_root_dir, root) = test_root("subc-buffer-bound-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let mut push_buffer = HashMap::new();
        let total = PUSH_BUFFER_MAX_PER_KEY + 3;

        for index in 0..total {
            buffer_push_frame(
                &mut push_buffer,
                key.clone(),
                completion_frame(&format!("task-{index}")),
            );
        }

        let buffered = push_buffer.get(&key).expect("buffer entry");
        assert_eq!(buffered.len(), PUSH_BUFFER_MAX_PER_KEY);
        let tasks: Vec<String> = buffered
            .iter()
            .filter_map(completion_task)
            .map(str::to_string)
            .collect();
        assert_eq!(tasks.first().map(String::as_str), Some("task-3"));
        assert_eq!(
            tasks.last().map(String::as_str),
            Some(format!("task-{}", total - 1).as_str())
        );
    }

    #[test]
    fn replay_buffered_push_frames_drains_to_bound_channel() {
        let (_root_dir, root) = test_root("subc-buffer-replay-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(4);
        let mut push_buffer = HashMap::new();
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-a"));
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-b"));

        let replayed =
            replay_buffered_push_frames(&writer_tx, route_key(3), &mut push_buffer, &key);

        assert_eq!(replayed, 2);
        assert!(!push_buffer.contains_key(&key));
        for expected_task in ["task-a", "task-b"] {
            let frame = writer_rx.try_recv().expect("replayed push");
            assert_eq!(frame.header.ty, FrameType::Push);
            assert_eq!(frame.header.channel, 3);
            let body: serde_json::Value = serde_json::from_slice(&frame.body).expect("push body");
            assert_eq!(body["task_id"].as_str(), Some(expected_task));
        }
        assert!(writer_rx.try_recv().is_err());
    }

    #[test]
    fn coalesce_push_batch_collapses_lossy_and_preserves_reliable_fifo() {
        let (_root_dir, root) = test_root("subc-coalesce-root");
        let (_other_dir, other_root) = test_root("subc-coalesce-other");

        let output = coalesce_push_batch(vec![
            (root.clone(), status_frame(1)),
            (root.clone(), completion_frame("task-1")),
            (root.clone(), status_frame(2)),
            (root.clone(), completion_frame("task-2")),
            (root.clone(), long_running_frame("long-task", 100)),
            (root.clone(), long_running_frame("long-task", 200)),
            (other_root.clone(), status_frame(9)),
        ]);

        let completion_tasks: Vec<_> = output
            .iter()
            .filter_map(|(_, frame)| completion_task(frame))
            .collect();
        assert_eq!(completion_tasks, vec!["task-1", "task-2"]);

        let root_statuses: Vec<_> = output
            .iter()
            .filter(|(output_root, _)| output_root == &root)
            .filter_map(|(_, frame)| status_seq(frame))
            .collect();
        assert_eq!(root_statuses, vec![2]);

        let other_statuses: Vec<_> = output
            .iter()
            .filter(|(output_root, _)| output_root == &other_root)
            .filter_map(|(_, frame)| status_seq(frame))
            .collect();
        assert_eq!(other_statuses, vec![9]);

        let long_running_elapsed: Vec<_> = output
            .iter()
            .filter_map(|(_, frame)| match frame {
                PushFrame::BashLongRunning(long_running) => Some(long_running.elapsed_ms),
                _ => None,
            })
            .collect();
        assert_eq!(long_running_elapsed, vec![200]);
    }

    #[test]
    fn coalesce_push_batch_keeps_progress_stream_keys_separate() {
        let (_root_dir, root) = test_root("subc-progress-coalesce-root");

        let output = coalesce_push_batch(vec![
            (
                root.clone(),
                progress_frame("request-1", ProgressKind::Stdout, "old stdout"),
            ),
            (
                root.clone(),
                progress_frame("request-1", ProgressKind::Stderr, "stderr"),
            ),
            (
                root.clone(),
                progress_frame("request-2", ProgressKind::Stdout, "other stdout"),
            ),
            (
                root.clone(),
                progress_frame("request-1", ProgressKind::Stdout, "new stdout"),
            ),
        ]);

        let progress: Vec<_> = output
            .iter()
            .filter_map(|(_, frame)| match frame {
                PushFrame::Progress(progress) => Some((
                    progress.request_id.as_str(),
                    match progress.kind {
                        ProgressKind::Stdout => "stdout",
                        ProgressKind::Stderr => "stderr",
                    },
                    progress.chunk.as_str(),
                )),
                _ => None,
            })
            .collect();

        assert_eq!(
            progress,
            vec![
                ("request-1", "stderr", "stderr"),
                ("request-2", "stdout", "other stdout"),
                ("request-1", "stdout", "new stdout"),
            ]
        );
    }

    #[test]
    fn progress_sender_keeps_reliable_off_saturated_lossy_funnel_without_blocking() {
        let (_root_dir, root) = test_root("subc-push-full-root");
        let (lossy_tx, mut lossy_rx) = mpsc::channel::<PushEnvelope>(1);
        let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
        let sender = progress_sender_for_root(
            PushSenders {
                lossy_tx,
                reliable_tx,
            },
            root.clone(),
        );

        let started = Instant::now();
        sender(status_frame(1));
        sender(status_frame(2));
        sender(completion_frame("reliable-after-lossy-full"));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "saturated push sender must return immediately"
        );

        let (received_root, received_frame) =
            lossy_rx.try_recv().expect("first lossy frame queued");
        assert_eq!(received_root, root);
        assert_eq!(status_seq(&received_frame), Some(1));
        assert!(
            lossy_rx.try_recv().is_err(),
            "second lossy frame should be dropped"
        );

        let (reliable_root, reliable_frame) = reliable_rx
            .try_recv()
            .expect("reliable frame bypasses lossy backpressure");
        assert_eq!(reliable_root, root);
        assert_eq!(
            completion_task(&reliable_frame),
            Some("reliable-after-lossy-full")
        );
        assert!(reliable_rx.try_recv().is_err());
    }

    #[test]
    fn fan_out_lossy_push_frame_drops_when_writer_is_full_without_blocking() {
        let (_root_dir, root) = test_root("subc-writer-full-root");
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");

        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(7)]));

        let routes = HashMap::new();
        let started = Instant::now();
        let result =
            fan_out_lossy_push_frame(&writer_tx, &routes, &root_channels, &root, &status_frame(1));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "saturated writer fan-out must return immediately"
        );
        assert_eq!(
            result,
            FanOutResult {
                matched_channels: 1,
                sent_frames: 0,
            }
        );

        let queued = writer_rx
            .try_recv()
            .expect("prefilled frame remains queued");
        assert_eq!(queued.header.ty, FrameType::Ping);
        assert!(
            writer_rx.try_recv().is_err(),
            "push should be dropped on full writer"
        );
    }

    #[test]
    fn reliable_push_backpressure_buffers_and_retries_on_tick() {
        let (_root_dir, root) = test_root("subc-retry-buffer-root");
        let identity = route_identity(&root, "session-1");
        let key = ReplayKey::from_identity(&identity);
        let mut routes = HashMap::new();
        routes.insert(route_key(9), identity.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(9)]));
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");

        let result = fan_out_reliable_push_frame(
            &writer_tx,
            &routes,
            &root_channels,
            &session_identity,
            &mut retry_buffer,
            &mut push_buffer,
            &root,
            &completion_frame("retry-task"),
        );

        assert_eq!(
            result,
            FanOutResult {
                matched_channels: 1,
                sent_frames: 0,
            }
        );
        assert!(push_buffer.is_empty());
        assert_eq!(retry_buffer.get(&route_key(9)).map(VecDeque::len), Some(1));
        assert_eq!(&retry_buffer[&route_key(9)][0].0, &key);

        let queued = writer_rx.try_recv().expect("prefilled frame");
        assert_eq!(queued.header.ty, FrameType::Ping);
        assert_eq!(
            drain_retry_buffer_for_channel(&writer_tx, route_key(9), &mut retry_buffer),
            1
        );
        let retried = writer_rx.try_recv().expect("retried reliable push");
        assert_eq!(retried.header.ty, FrameType::Push);
        assert_eq!(retried.header.channel, 9);
        assert_eq!(push_frame_task_id(&retried).as_deref(), Some("retry-task"));
        assert!(!retry_buffer.contains_key(&route_key(9)));
    }

    #[test]
    fn reliable_push_fifo_gates_new_frames_behind_retry_buffer() {
        let (_root_dir, root) = test_root("subc-retry-fifo-root");
        let identity = route_identity(&root, "session-1");
        let mut routes = HashMap::new();
        routes.insert(route_key(9), identity.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(9)]));
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");

        let first = completion_frame("fifo-1");
        let second = completion_frame("fifo-2");
        let _ = fan_out_reliable_push_frame(
            &writer_tx,
            &routes,
            &root_channels,
            &session_identity,
            &mut retry_buffer,
            &mut push_buffer,
            &root,
            &first,
        );
        let queued = writer_rx.try_recv().expect("free writer capacity");
        assert_eq!(queued.header.ty, FrameType::Ping);

        let _ = fan_out_reliable_push_frame(
            &writer_tx,
            &routes,
            &root_channels,
            &session_identity,
            &mut retry_buffer,
            &mut push_buffer,
            &root,
            &second,
        );
        assert!(
            writer_rx.try_recv().is_err(),
            "second reliable frame must not bypass pending retry frame"
        );
        let queued_tasks: Vec<_> = retry_buffer[&route_key(9)]
            .iter()
            .filter_map(|(_, frame)| completion_task(frame))
            .collect();
        assert_eq!(queued_tasks, vec!["fifo-1", "fifo-2"]);

        assert_eq!(
            drain_retry_buffer_for_channel(&writer_tx, route_key(9), &mut retry_buffer),
            1
        );
        let first_sent = writer_rx.try_recv().expect("first reliable push");
        assert_eq!(push_frame_task_id(&first_sent).as_deref(), Some("fifo-1"));
        assert_eq!(
            drain_retry_buffer_for_channel(&writer_tx, route_key(9), &mut retry_buffer),
            1
        );
        let second_sent = writer_rx.try_recv().expect("second reliable push");
        assert_eq!(push_frame_task_id(&second_sent).as_deref(), Some("fifo-2"));
        assert!(!retry_buffer.contains_key(&route_key(9)));
    }

    #[test]
    fn replay_buffered_push_frames_drains_incrementally_on_backpressure() {
        let (_root_dir, root) = test_root("subc-incremental-replay-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(2);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");
        let mut push_buffer = HashMap::new();
        for task in ["replay-1", "replay-2", "replay-3"] {
            buffer_push_frame(&mut push_buffer, key.clone(), completion_frame(task));
        }

        assert_eq!(
            replay_buffered_push_frames(&writer_tx, route_key(4), &mut push_buffer, &key),
            1
        );
        assert_eq!(push_buffer.get(&key).map(VecDeque::len), Some(2));
        let remaining: Vec<_> = push_buffer[&key]
            .iter()
            .filter_map(completion_task)
            .collect();
        assert_eq!(remaining, vec!["replay-2", "replay-3"]);

        let queued = writer_rx.try_recv().expect("prefilled frame");
        assert_eq!(queued.header.ty, FrameType::Ping);
        let first = writer_rx.try_recv().expect("first replayed push");
        assert_eq!(push_frame_task_id(&first).as_deref(), Some("replay-1"));

        assert_eq!(
            replay_buffered_push_frames(&writer_tx, route_key(4), &mut push_buffer, &key),
            2
        );
        let second = writer_rx.try_recv().expect("second replayed push");
        let third = writer_rx.try_recv().expect("third replayed push");
        assert_eq!(push_frame_task_id(&second).as_deref(), Some("replay-2"));
        assert_eq!(push_frame_task_id(&third).as_deref(), Some("replay-3"));
        assert!(!push_buffer.contains_key(&key));
    }

    #[test]
    fn goodbye_migrates_retry_buffer_into_detach_replay() {
        let (_root_dir, root) = test_root("subc-goodbye-migration-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let mut retry_buffer = HashMap::new();
        buffer_retry_frame(
            &mut retry_buffer,
            route_key(5),
            key.clone(),
            completion_frame("migrated-task"),
        );
        let mut push_buffer = HashMap::new();

        assert_eq!(
            migrate_retry_buffer_to_push_buffer(&mut retry_buffer, route_key(5), &mut push_buffer),
            1
        );

        assert!(!retry_buffer.contains_key(&route_key(5)));
        assert_eq!(push_buffer.get(&key).map(VecDeque::len), Some(1));
        assert_eq!(
            completion_task(&push_buffer[&key][0]),
            Some("migrated-task")
        );
    }

    #[test]
    fn permanent_push_send_failure_is_dropped_not_retried_forever() {
        let (_root_dir, root) = test_root("subc-permanent-failure-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let (writer_tx, writer_rx) = mpsc::channel::<Frame>(1);
        drop(writer_rx);

        let mut push_buffer = HashMap::new();
        buffer_push_frame(
            &mut push_buffer,
            key.clone(),
            completion_frame("closed-replay"),
        );
        assert_eq!(
            replay_buffered_push_frames(&writer_tx, route_key(4), &mut push_buffer, &key),
            0
        );
        assert!(!push_buffer.contains_key(&key));

        let mut retry_buffer = HashMap::new();
        buffer_retry_frame(
            &mut retry_buffer,
            route_key(4),
            key,
            completion_frame("closed-retry"),
        );
        assert_eq!(
            drain_retry_buffer_for_channel(&writer_tx, route_key(4), &mut retry_buffer),
            0
        );
        assert!(!retry_buffer.contains_key(&route_key(4)));
    }

    #[test]
    fn completed_task_suppresses_stale_long_running_lossy_push() {
        let mut completed_tasks = CompletedTaskIds::default();
        assert!(!should_drop_lossy_push(
            &completed_tasks,
            &long_running_frame("stale-task", 100)
        ));

        completed_tasks.remember("stale-task");

        assert!(should_drop_lossy_push(
            &completed_tasks,
            &long_running_frame("stale-task", 200)
        ));
        assert!(!should_drop_lossy_push(
            &completed_tasks,
            &long_running_frame("other-task", 200)
        ));
    }

    #[tokio::test]
    async fn control_send_times_out_when_writer_queue_remains_full() {
        let (writer_tx, _writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");
        let started = Instant::now();

        let result = send_frame(
            &writer_tx,
            Frame::build(FrameType::Pong, control_flags(), 0, 2, Vec::new()).unwrap(),
        )
        .await;

        assert!(matches!(result, Err(SubcError::WriterBackpressureTimeout)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "control send guard should be bounded"
        );
    }

    const CORE_TOOLS: [&str; 8] = [
        "status", "read", "grep", "search", "outline", "inspect", "edit", "write",
    ];

    fn is_bare_placeholder_schema(schema: &Value) -> bool {
        schema == &json!({ "type": "object" })
    }

    #[test]
    fn build_manifest_serves_embedded_tool_schemas() {
        let manifest = build_manifest();
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();
        for name in CORE_TOOLS {
            let tool = by_name
                .get(name)
                .unwrap_or_else(|| panic!("missing tool {name}"));
            assert!(
                !is_bare_placeholder_schema(&tool.schema),
                "{name} must not use bare placeholder schema"
            );
            assert_eq!(
                tool.schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{name} schema must be an object"
            );
        }

        let read = by_name["read"]
            .schema
            .get("properties")
            .and_then(|p| p.as_object());
        let read_props = read.expect("read schema properties");
        assert!(
            read_props.contains_key("filePath"),
            "read schema must expose filePath"
        );

        let status = &by_name["status"].schema;
        assert_eq!(
            status.get("properties").and_then(|v| v.as_object()),
            Some(&serde_json::Map::new()),
            "status schema must have empty properties"
        );
        assert_eq!(
            status.get("additionalProperties").and_then(|v| v.as_bool()),
            Some(false),
            "status schema must forbid additionalProperties"
        );
    }

    #[test]
    fn build_manifest_classifies_execution_mode_by_observable_effect() {
        let manifest = build_manifest();
        let tools = match manifest.provides.first() {
            Some(ProviderRole::ToolProvider { tools, .. }) => tools,
            _ => panic!("expected ToolProvider"),
        };
        let by_name: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();

        // Readers warm AFT's own index/cache/symbol artifacts (internal ctx
        // mutation), not the user's observable workspace, so they are Pure.
        for name in ["status", "read", "grep", "search", "outline", "inspect"] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Pure,
                "{name} produces no observable side effect and must be Pure"
            );
        }
        // Only edit/write produce observable file writes -> Mutating.
        for name in ["edit", "write"] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Mutating,
                "{name} writes files and must be Mutating"
            );
        }
    }
}

#[derive(Debug)]
pub enum SubcError {
    Runtime(std::io::Error),
    ConnectionFile {
        path: PathBuf,
        source: subc_transport::ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    InvalidEndpoint {
        path: PathBuf,
        endpoint: String,
    },
    Connect {
        endpoint: String,
        source: std::io::Error,
    },
    Auth {
        endpoint: String,
        source: subc_transport::AuthError,
    },
    FrameIo(subc_transport::FrameIoError),
    FrameBuild(subc_protocol::FrameBuildError),
    WriterClosed,
    WriterBackpressureTimeout,
    WriterJoin(tokio::task::JoinError),
    Json(serde_json::Error),
    ClosedBeforeHelloAck,
    HelloRejected {
        body: Option<ErrorBody>,
    },
    UnexpectedFrame {
        ty: FrameType,
    },
}

impl fmt::Display for SubcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "failed to build subc tokio runtime: {e}"),
            Self::ConnectionFile { path, source } => {
                write!(f, "failed to read subc connection file {path:?}: {source}")
            }
            Self::NoEndpoint { path } => {
                write!(f, "subc connection file {path:?} has no endpoints")
            }
            Self::InvalidEndpoint { path, endpoint } => {
                write!(
                    f,
                    "subc connection file {path:?} has invalid endpoint {endpoint}"
                )
            }
            Self::Connect { endpoint, source } => {
                write!(f, "failed to connect to subc endpoint {endpoint}: {source}")
            }
            Self::Auth { endpoint, source } => {
                write!(
                    f,
                    "failed to authenticate to subc endpoint {endpoint}: {source}"
                )
            }
            Self::FrameIo(e) => write!(f, "subc frame I/O error: {e}"),
            Self::FrameBuild(e) => write!(f, "subc frame build error: {e}"),
            Self::WriterClosed => write!(f, "subc writer task closed"),
            Self::WriterBackpressureTimeout => write!(
                f,
                "subc writer task stayed backpressured while sending a control frame"
            ),
            Self::WriterJoin(e) => write!(f, "subc writer task join error: {e}"),
            Self::Json(e) => write!(f, "subc JSON error: {e}"),
            Self::ClosedBeforeHelloAck => {
                write!(f, "subc daemon closed the connection before HelloAck")
            }
            Self::HelloRejected { body } => match body {
                Some(b) => write!(f, "subc rejected ModuleHello: {} ({})", b.code, b.message),
                None => write!(f, "subc rejected ModuleHello (unparseable error body)"),
            },
            Self::UnexpectedFrame { ty } => {
                write!(f, "subc sent unexpected frame in place of HelloAck: {ty:?}")
            }
        }
    }
}

impl std::error::Error for SubcError {}
