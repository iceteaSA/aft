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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Small bounded memory of completed task ids used to suppress stale lossy
/// long-running reminders that arrive after their reliable completion event.
const COMPLETED_TASK_SUPPRESSION_MAX: usize = 4096;

/// Bash foreground orchestration polls detached tasks with short read-lane jobs.
/// The sleep between polls is outside the executor so no read or write worker is
/// pinned while a foreground command is still running.
const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(100);

type RouteChannel = u32;
type PushEnvelope = (ProjectRootId, PushFrame);
type RetryBuffer = HashMap<RouteChannel, VecDeque<(ReplayKey, PushFrame)>>;

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

#[derive(Clone)]
struct BashWaitCancel {
    connection: PersistentCancelSignal,
    route: PersistentCancelSignal,
}

impl BashWaitCancel {
    async fn cancelled(&self) {
        tokio::select! {
            _ = self.connection.cancelled() => {}
            _ = self.route.cancelled() => {}
        }
    }
}

struct RouteBashCancel {
    token: PersistentCancelSignal,
    active_waits: usize,
}

struct BashDeferredCompletion {
    channel: u16,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
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

pub(crate) fn trust_for_principal(principal: &Option<Principal>) -> BindTrust {
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
    last_touched: Instant,
    diagnostics_on_edit: bool,
    active_bash_waits: usize,
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
            active_bash_waits: 0,
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
    bg_subs: &mut HashMap<RouteChannel, BgSub>,
    bg_sub_by_session: &mut HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    channel: RouteChannel,
    identity: Option<&RouteIdentity>,
) {
    if let Some(sub) = bg_subs.get(&channel).copied() {
        let _ = try_send_bg_stream_end(writer_tx, channel, &sub);
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
) -> Option<(ReplayKey, BindTrust)> {
    let retained = session_identity.get(&(root.clone(), session.to_string()))?;
    Some((
        ReplayKey {
            root: root.clone(),
            harness: retained.harness.clone(),
            session: session.to_string(),
        },
        retained.trust,
    ))
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

fn frame_is_bash_observation(frame: &PushFrame) -> bool {
    matches!(
        frame,
        PushFrame::BashCompleted(_)
            | PushFrame::BashLongRunning(_)
            | PushFrame::BashPatternMatch(_)
    )
}

fn completed_task_id(frame: &PushFrame) -> Option<&str> {
    match frame {
        PushFrame::BashCompleted(completed) => Some(completed.task_id.as_str()),
        _ => None,
    }
}

fn completed_bg_session_key(
    root: &ProjectRootId,
    frame: &PushFrame,
) -> Option<(ProjectRootId, String)> {
    match frame {
        PushFrame::BashCompleted(completed) => Some((root.clone(), completed.session_id.clone())),
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

fn try_send_bg_stream_frame(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    sub: &BgSub,
    ty: FrameType,
    body: Vec<u8>,
) -> PushSendOutcome {
    let Ok(route_channel) = u16::try_from(channel) else {
        log::warn!("subc attach: invalid route channel {channel} for bg_events stream");
        return PushSendOutcome::PermanentFailure;
    };
    let frame =
        match Frame::build_with_version(sub.ver, ty, sub.flags, route_channel, sub.corr, body) {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("subc attach: failed to build bg_events stream frame: {error}");
                return PushSendOutcome::PermanentFailure;
            }
        };
    match writer_tx.try_send(frame) {
        Ok(()) => PushSendOutcome::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => PushSendOutcome::Backpressure,
        Err(mpsc::error::TrySendError::Closed(_)) => {
            log::warn!("subc attach: writer closed while sending bg_events stream frame");
            PushSendOutcome::PermanentFailure
        }
    }
}

fn try_send_bg_stream_data(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    sub: &BgSub,
) -> PushSendOutcome {
    let body = match serde_json::to_vec(&json!({ "op": "bg_events" })) {
        Ok(body) => body,
        Err(error) => {
            log::warn!("subc attach: failed to serialize bg_events stream payload: {error}");
            return PushSendOutcome::PermanentFailure;
        }
    };
    try_send_bg_stream_frame(writer_tx, channel, sub, FrameType::StreamData, body)
}

fn try_send_bg_stream_end(
    writer_tx: &mpsc::Sender<Frame>,
    channel: RouteChannel,
    sub: &BgSub,
) -> PushSendOutcome {
    try_send_bg_stream_frame(writer_tx, channel, sub, FrameType::StreamEnd, Vec::new())
}

fn emit_bg_event_wakes(
    writer_tx: &mpsc::Sender<Frame>,
    bg_subs: &HashMap<RouteChannel, BgSub>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
) {
    let pending_channels: Vec<RouteChannel> = bg_wake_pending.iter().copied().collect();
    let mut stale_channels = Vec::new();
    for channel in pending_channels {
        if let Some(sub) = bg_subs.get(&channel) {
            let _ = try_send_bg_stream_data(writer_tx, channel, sub);
        } else {
            stale_channels.push(channel);
        }
    }
    for channel in stale_channels {
        bg_wake_pending.remove(&channel);
    }
}

/// Always bump the epoch for (root, session) when arming a wake on `channel`,
/// even if the channel was already present in the pending set. This ensures
/// that later maintenance logic holding an older epoch value cannot suppress a
/// wake that was armed after the maintenance snapshot was taken.
fn arm_bg_wake(
    root: ProjectRootId,
    session: String,
    channel: RouteChannel,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
) {
    *bg_wake_epoch.entry((root, session)).or_default() += 1;
    bg_wake_pending.insert(channel);
}

fn clear_stale_bg_wakes_for_empty_sessions(
    root_id: &ProjectRootId,
    empty_bg_sessions: &[(String, u64)],
    bg_sub_by_session: &HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &HashMap<(ProjectRootId, String), u64>,
) {
    for (session, epoch_at_submit) in empty_bg_sessions {
        let key = (root_id.clone(), session.clone());
        if bg_wake_epoch.get(&key).copied() == Some(*epoch_at_submit) {
            if let Some(channel) = bg_sub_by_session.get(&key).copied() {
                bg_wake_pending.remove(&channel);
            }
        }
    }
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
    trust: BindTrust,
) -> usize {
    let mut sent = 0;
    let remove_empty;

    {
        let Some(queue) = push_buffer.get_mut(key) else {
            return 0;
        };

        while let Some(frame) = queue.pop_front() {
            if frame_is_bash_observation(&frame) && !trust.allows_bash_observation() {
                continue;
            }
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
    let bash_observation = frame_is_bash_observation(frame);
    channels
        .iter()
        .copied()
        .filter(|channel| {
            let Some(identity) = routes.get(channel) else {
                return !bash_observation && session.is_none();
            };
            if bash_observation && !identity.trust.allows_bash_observation() {
                return false;
            }
            match session {
                Some(session) => identity.session == session,
                None => true,
            }
        })
        .collect()
}

fn buffer_detached_reliable_push_frame(
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
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

    if let Some((key, trust)) = replay_key_for_session(session_identity, root, session) {
        if frame_is_bash_observation(frame) && !trust.allows_bash_observation() {
            return;
        }
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
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
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
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    completed_tasks: &mut CompletedTaskIds,
    root: ProjectRootId,
    frame: PushFrame,
) -> Option<(ProjectRootId, String)> {
    let completed_bg_session = completed_bg_session_key(&root, &frame);
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
    completed_bg_session
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
    let (maintenance_tx, mut maintenance_rx) = mpsc::channel::<MaintenanceCompletion>(256);
    let (bash_deferred_tx, mut bash_deferred_rx) = mpsc::channel::<BashDeferredCompletion>(256);
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
    let mut push_buffer: HashMap<ReplayKey, VecDeque<PushFrame>> = HashMap::new();
    let mut retry_buffer: RetryBuffer = HashMap::new();
    let mut completed_tasks = CompletedTaskIds::default();
    let mut live_roots: HashMap<ProjectRootId, RootMeta> = HashMap::new();
    let mut pending_binds: HashMap<RouteChannel, PendingBind> = HashMap::new();
    let mut route_bash_cancels: HashMap<RouteChannel, RouteBashCancel> = HashMap::new();

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
                        end_bg_subscription(
                            &writer_tx,
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
                            &connection_cancel,
                            &bash_deferred_tx,
                            &bash_poll_touch_tx,
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
                // Drain reliable frames in FIFO order. They are intentionally not
                // coalesced: completion, pattern-match, and warning frames are
                // must-deliver events.
                let mut batch = vec![(root_id, frame)];
                while let Ok(item) = reliable_rx.try_recv() {
                    batch.push(item);
                }

                for (root, frame) in batch {
                    if let Some((root, session)) = process_reliable_push_frame(
                        &writer_tx,
                        &routes,
                        &root_channels,
                        &session_identity,
                        &mut retry_buffer,
                        &mut push_buffer,
                        &mut completed_tasks,
                        root,
                        frame,
                    ) {
                        if let Some(channel) = bg_sub_by_session
                            .get(&(root.clone(), session.clone()))
                            .copied()
                        {
                            arm_bg_wake(
                                root,
                                session,
                                channel,
                                &mut bg_wake_pending,
                                &mut bg_wake_epoch,
                            );
                        }
                    }
                }
            }
            Some((root_id, frame)) = lossy_rx.recv() => {
                // If both lanes are ready, process any already-queued reliable
                // completions first so a following stale BashLongRunning frame can
                // be suppressed even if select! happened to wake on the lossy lane.
                while let Ok((reliable_root, reliable_frame)) = reliable_rx.try_recv() {
                    if let Some((root, session)) = process_reliable_push_frame(
                        &writer_tx,
                        &routes,
                        &root_channels,
                        &session_identity,
                        &mut retry_buffer,
                        &mut push_buffer,
                        &mut completed_tasks,
                        reliable_root,
                        reliable_frame,
                    ) {
                        if let Some(channel) = bg_sub_by_session
                            .get(&(root.clone(), session.clone()))
                            .copied()
                        {
                            arm_bg_wake(
                                root,
                                session,
                                channel,
                                &mut bg_wake_pending,
                                &mut bg_wake_epoch,
                            );
                        }
                    }
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
            Some(done) = bash_deferred_rx.recv() => {
                if let Err(error) = handle_bash_deferred_completion(
                    &writer_tx,
                    done,
                    &routes,
                    &mut live_roots,
                    &mut route_bash_cancels,
                    &shutdown,
                )
                .await
                {
                    break Err(error);
                }
            }
            Some(root_id) = bash_poll_touch_rx.recv() => {
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.touch();
                }
            }
            Some(completion) = maintenance_rx.recv() => {
                let root_id = completion.root_id;
                let response = completion.response;
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    meta.maintenance_pending = false;
                }
                clear_stale_bg_wakes_for_empty_sessions(
                    &root_id,
                    &completion.empty_bg_sessions,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &bg_wake_epoch,
                );
                if response_is_fatal_panic(&response) {
                    signal_fatal_teardown(&writer_tx, None, PROTOCOL_VERSION, 0, &shutdown).await;
                }
            }
            _ = drain_interval.tick() => {
                emit_bg_event_wakes(&writer_tx, &bg_subs, &mut bg_wake_pending);

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
    session_identity: &mut HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
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
        let fatal = response_is_fatal_panic(response);
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
    let bind_trust = completion.identity.trust;
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
    let replayed = replay_buffered_push_frames(tx, route_id, push_buffer, &replay_key, bind_trust);
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
            let bind_trust = trust_for_principal(&principal);
            log::info!(
                "subc attach: route {} principal={} trust={}",
                route_channel,
                principal_label(&principal),
                bind_trust.label()
            );

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
        ModuleControlRequest::HealthCheck {} => {
            let report = build_health_report(executor);
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
            send_frame(tx, response).await
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
    connection_cancel: &PersistentCancelSignal,
    bash_deferred_tx: &mpsc::Sender<BashDeferredCompletion>,
    bash_poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    route_bash_cancels: &mut HashMap<RouteChannel, RouteBashCancel>,
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

    let is_bg_events_subscribe = serde_json::from_slice::<BgEventsProbe>(&frame.body)
        .ok()
        .and_then(|probe| probe.op)
        .as_deref()
        == Some("bg_events");
    if is_bg_events_subscribe {
        if let Some(old_sub) = bg_subs.get(&route_id).copied() {
            let _ = try_send_bg_stream_end(tx, route_id, &old_sub);
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
        arm_bg_wake(
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
        let response = bash_denied_untrusted_response(request_id.clone());
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
        return send_frame(tx, response_frame).await;
    }

    if bare_name == "bash" {
        let meta = live_roots
            .entry(identity.root.clone())
            .or_insert_with(|| RootMeta::new(Instant::now()));
        meta.active_bash_waits = meta.active_bash_waits.saturating_add(1);
        meta.touch();

        let route_cancel = route_bash_cancels
            .entry(route_id)
            .or_insert_with(|| RouteBashCancel {
                token: PersistentCancelSignal::new(),
                active_waits: 0,
            });
        route_cancel.active_waits = route_cancel.active_waits.saturating_add(1);
        let cancel = BashWaitCancel {
            connection: connection_cancel.clone(),
            route: route_cancel.token.clone(),
        };

        submit_deferred_bash(
            executor,
            bash_deferred_tx,
            bash_poll_touch_tx,
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
        let fatal = response_is_fatal_panic(&result.response);
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

#[derive(Clone, Copy, Debug, Default)]
struct BashTranslatedSettings {
    background: bool,
    pty: bool,
    wait: bool,
    block_to_completion: bool,
    timeout: Option<u64>,
}

enum BashSpawnControl {
    Immediate,
    Foreground {
        task_id: String,
        session_id: String,
        project_root: Option<PathBuf>,
        storage_dir: PathBuf,
        deadline: Instant,
        block_to_completion: bool,
        timeout: Option<u64>,
        wait_window_ms: u64,
    },
}

enum BashPollControl {
    Done,
    Promote,
    Wait,
}

fn bash_settings_from_translated(args: &serde_json::Map<String, Value>) -> BashTranslatedSettings {
    BashTranslatedSettings {
        background: args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pty: args.get("pty").and_then(Value::as_bool).unwrap_or(false),
        wait: args.get("wait").and_then(Value::as_bool).unwrap_or(false),
        block_to_completion: args
            .get("block_to_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        timeout: args.get("timeout").and_then(Value::as_u64),
    }
}

fn finalized_bash_result(
    mut response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    allow_bg_completions: bool,
) -> ToolCallResult {
    crate::response_finalize::finalize_response_with_bg_completions(
        &mut response,
        ctx,
        session_id,
        "bash",
        allow_bg_completions,
    );
    bash_result_from_response(response, format_context)
}

fn bash_result_from_response(
    response: Response,
    format_context: &crate::subc_format::FormatContext,
) -> ToolCallResult {
    let text = crate::subc_format::format_response_with_context("bash", &response, format_context);
    ToolCallResult { text, response }
}

fn bash_background_launch_response(request_id: &str, task_id: &str, is_pty: bool) -> Response {
    Response::success(
        request_id,
        json!({
            "output": crate::commands::bash_orchestrate::format_background_launch(task_id, is_pty),
            "task_id": task_id,
            "status": "running",
            "mode": if is_pty { "pty" } else { "pipes" },
        }),
    )
}

fn finish_bash_spawn_immediate(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashSpawnControl>>,
    allow_bg_completions: bool,
) -> Response {
    let result = finalized_bash_result(
        response,
        ctx,
        session_id,
        format_context,
        allow_bg_completions,
    );
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashSpawnControl::Immediate);
    }
    response
}

fn finish_bash_poll_done(
    response: Response,
    ctx: &AppContext,
    session_id: &str,
    format_context: &crate::subc_format::FormatContext,
    text_tx: &mut Option<oneshot::Sender<String>>,
    control_tx: &mut Option<oneshot::Sender<BashPollControl>>,
) -> Response {
    let result = finalized_bash_result(response, ctx, session_id, format_context, true);
    let ToolCallResult { text, response } = result;
    if let Some(tx) = text_tx.take() {
        let _ = tx.send(text);
    }
    if let Some(tx) = control_tx.take() {
        let _ = tx.send(BashPollControl::Done);
    }
    response
}

#[allow(clippy::too_many_arguments)]
fn submit_deferred_bash(
    executor: &Arc<Executor>,
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    dispatch: DispatchFn,
    root: ProjectRootId,
    project_root: PathBuf,
    session_id: String,
    request_id: String,
    route_channel: u16,
    corr: u64,
    flags: Flags,
    ver: u8,
    arguments: Value,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
    bind_trust: BindTrust,
) {
    let (spawn_control_tx, spawn_control_rx) = oneshot::channel::<BashSpawnControl>();
    let (spawn_text_tx, spawn_text_rx) = oneshot::channel::<String>();
    let root_for_spawn = root.clone();
    let request_id_for_spawn = request_id.clone();
    let session_for_spawn = session_id.clone();
    let project_root_for_spawn = project_root.clone();
    let format_context_for_spawn = format_context.clone();
    let spawn_rx = executor.submit_async(
        root_for_spawn,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_spawn.clone()), || {
                let mut spawn_text_tx = Some(spawn_text_tx);
                let mut spawn_control_tx = Some(spawn_control_tx);

                if matches!(bind_trust, BindTrust::Untrusted) {
                    let response = bash_denied_untrusted_response(request_id_for_spawn.clone());
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        false,
                    );
                }

                let translated = match crate::subc_translate::subc_translate(
                    "bash",
                    &arguments,
                    &project_root_for_spawn,
                ) {
                    Ok(translated) => translated,
                    Err(error) => {
                        let response = Response::error(
                            request_id_for_spawn.clone(),
                            error.code,
                            error.message,
                        );
                        return finish_bash_spawn_immediate(
                            response,
                            ctx,
                            &session_for_spawn,
                            &format_context_for_spawn,
                            &mut spawn_text_tx,
                            &mut spawn_control_tx,
                            true,
                        );
                    }
                };
                let settings = bash_settings_from_translated(&translated.args);
                let raw_req = RawRequest {
                    id: request_id_for_spawn.clone(),
                    command: "bash".to_string(),
                    lsp_hints: None,
                    session_id: Some(session_for_spawn.clone()),
                    params: Value::Object(translated.args),
                };
                let response = dispatch(raw_req, ctx);
                if !response.success {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let Some(task_id) = response
                    .data
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                };
                if response.data.get("status").and_then(Value::as_str) != Some("running") {
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let mode = response
                    .data
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("pipes");
                let is_pty = mode == "pty" || settings.pty;
                if is_pty || settings.background {
                    let response =
                        bash_background_launch_response(&request_id_for_spawn, &task_id, is_pty);
                    return finish_bash_spawn_immediate(
                        response,
                        ctx,
                        &session_for_spawn,
                        &format_context_for_spawn,
                        &mut spawn_text_tx,
                        &mut spawn_control_tx,
                        true,
                    );
                }

                let wait_window_ms =
                    crate::commands::bash_orchestrate::select_foreground_wait_window_ms(
                        ctx.config().foreground_wait_window_ms,
                        settings.timeout,
                        settings.wait,
                    );
                let deadline = Instant::now() + Duration::from_millis(wait_window_ms);
                let storage_dir =
                    crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
                let project_root = ctx.config().project_root.clone();
                if let Some(tx) = spawn_control_tx.take() {
                    let _ = tx.send(BashSpawnControl::Foreground {
                        task_id,
                        session_id: session_for_spawn.clone(),
                        project_root,
                        storage_dir,
                        deadline,
                        block_to_completion: settings.block_to_completion || settings.wait,
                        timeout: settings.timeout,
                        wait_window_ms,
                    });
                }
                response
            })
        }),
    );

    let executor = Arc::clone(executor);
    let completion_tx = completion_tx.clone();
    let poll_touch_tx = poll_touch_tx.clone();
    let root_for_task = root.clone();
    tokio::spawn(async move {
        let spawn_response = await_executor_response(spawn_rx, request_id.clone()).await;
        let spawn_control = spawn_control_rx.await;
        match spawn_control {
            Ok(BashSpawnControl::Immediate) => {
                let text = spawn_text_rx.await.unwrap_or_else(|_| {
                    crate::subc_format::format_response_with_context(
                        "bash",
                        &spawn_response,
                        &format_context,
                    )
                });
                let result = ToolCallResult {
                    text,
                    response: spawn_response,
                };
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    route_channel,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    Some(result),
                    fatal,
                )
                .await;
            }
            Ok(BashSpawnControl::Foreground {
                task_id,
                session_id,
                project_root,
                storage_dir,
                deadline,
                block_to_completion,
                timeout,
                wait_window_ms,
            }) => {
                run_deferred_bash_wait(
                    executor,
                    completion_tx,
                    poll_touch_tx,
                    route_channel,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    task_id,
                    session_id,
                    project_root,
                    storage_dir,
                    deadline,
                    block_to_completion,
                    timeout,
                    wait_window_ms,
                    format_context,
                    cancel,
                )
                .await;
            }
            Err(_) => {
                let result = bash_result_from_response(spawn_response, &format_context);
                let fatal = response_is_fatal_panic(&result.response);
                send_bash_deferred_completion(
                    &completion_tx,
                    route_channel,
                    corr,
                    flags,
                    ver,
                    root_for_task,
                    request_id,
                    Some(result),
                    fatal,
                )
                .await;
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_deferred_bash_wait(
    executor: Arc<Executor>,
    completion_tx: mpsc::Sender<BashDeferredCompletion>,
    poll_touch_tx: mpsc::Sender<ProjectRootId>,
    route_channel: u16,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    project_root: Option<PathBuf>,
    storage_dir: PathBuf,
    deadline: Instant,
    block_to_completion: bool,
    timeout: Option<u64>,
    wait_window_ms: u64,
    format_context: crate::subc_format::FormatContext,
    cancel: BashWaitCancel,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                send_bash_deferred_completion(
                    &completion_tx,
                    route_channel,
                    corr,
                    flags,
                    ver,
                    root,
                    request_id,
                    None,
                    false,
                )
                .await;
                break;
            }
            _ = tokio::time::sleep(PENDING_POLL_INTERVAL) => {
                let (poll_control_tx, poll_control_rx) = oneshot::channel::<BashPollControl>();
                let (poll_text_tx, poll_text_rx) = oneshot::channel::<String>();
                let root_for_poll = root.clone();
                let request_id_for_poll = request_id.clone();
                let task_id_for_poll = task_id.clone();
                let session_for_poll = session_id.clone();
                let storage_for_poll = storage_dir.clone();
                let project_root_for_poll = project_root.clone();
                let format_context_for_poll = format_context.clone();
                let poll_rx = executor.submit_async(
                    root_for_poll,
                    Lane::PureRead,
                    request_id.clone(),
                    Box::new(move |ctx| {
                        log_ctx::with_session(Some(session_for_poll.clone()), || {
                            let mut poll_text_tx = Some(poll_text_tx);
                            let mut poll_control_tx = Some(poll_control_tx);

                            let Some(snapshot) = crate::commands::bash_orchestrate::poll_bash_status(
                                ctx,
                                &task_id_for_poll,
                                &session_for_poll,
                                project_root_for_poll.as_deref(),
                                &storage_for_poll,
                                crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES,
                            ) else {
                                return finish_bash_poll_done(
                                    crate::commands::bash_orchestrate::task_not_found_response(
                                        &request_id_for_poll,
                                        &task_id_for_poll,
                                    ),
                                    ctx,
                                    &session_for_poll,
                                    &format_context_for_poll,
                                    &mut poll_text_tx,
                                    &mut poll_control_tx,
                                );
                            };

                            match crate::commands::bash_orchestrate::decide_bash_step(
                                snapshot,
                                deadline,
                                block_to_completion,
                                Instant::now(),
                                &request_id_for_poll,
                            ) {
                                crate::commands::bash_orchestrate::BashStep::Done(response) => {
                                    finish_bash_poll_done(
                                        response,
                                        ctx,
                                        &session_for_poll,
                                        &format_context_for_poll,
                                        &mut poll_text_tx,
                                        &mut poll_control_tx,
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Promote => {
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Promote);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "promote" }),
                                    )
                                }
                                crate::commands::bash_orchestrate::BashStep::Wait => {
                                    if let Some(tx) = poll_control_tx.take() {
                                        let _ = tx.send(BashPollControl::Wait);
                                    }
                                    Response::success(
                                        request_id_for_poll,
                                        json!({ "subc_bash_step": "wait" }),
                                    )
                                }
                            }
                        })
                    }),
                );
                let poll_response = await_executor_response(poll_rx, request_id.clone()).await;
                let _ = poll_touch_tx.send(root.clone()).await;
                match poll_control_rx.await.unwrap_or(BashPollControl::Done) {
                    BashPollControl::Done => {
                        let text = poll_text_rx.await.unwrap_or_else(|_| {
                            crate::subc_format::format_response_with_context(
                                "bash",
                                &poll_response,
                                &format_context,
                            )
                        });
                        let result = ToolCallResult {
                            text,
                            response: poll_response,
                        };
                        let fatal = response_is_fatal_panic(&result.response);
                        send_bash_deferred_completion(
                            &completion_tx,
                            route_channel,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            Some(result),
                            fatal,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Promote => {
                        let result = submit_bash_promote(
                            &executor,
                            root.clone(),
                            request_id.clone(),
                            task_id.clone(),
                            session_id.clone(),
                            timeout,
                            wait_window_ms,
                            format_context.clone(),
                        )
                        .await;
                        let fatal = response_is_fatal_panic(&result.response);
                        send_bash_deferred_completion(
                            &completion_tx,
                            route_channel,
                            corr,
                            flags,
                            ver,
                            root,
                            request_id,
                            Some(result),
                            fatal,
                        )
                        .await;
                        break;
                    }
                    BashPollControl::Wait => {}
                }
            }
        }
    }
}

async fn submit_bash_promote(
    executor: &Arc<Executor>,
    root: ProjectRootId,
    request_id: String,
    task_id: String,
    session_id: String,
    timeout: Option<u64>,
    wait_window_ms: u64,
    format_context: crate::subc_format::FormatContext,
) -> ToolCallResult {
    let (text_tx, text_rx) = oneshot::channel::<String>();
    let request_id_for_promote = request_id.clone();
    let task_id_for_promote = task_id.clone();
    let session_for_promote = session_id.clone();
    let format_context_for_promote = format_context.clone();
    let promote_rx = executor.submit_async(
        root,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            log_ctx::with_session(Some(session_for_promote.clone()), || {
                let response = if let Some(value) =
                    std::env::var_os("AFT_TEST_FORCE_SUBC_BASH_PROMOTE_ERROR")
                {
                    if value.to_string_lossy() == "panic" {
                        panic!("forced subc bash promote panic");
                    }
                    Response::error(
                        &request_id_for_promote,
                        "execution_failed",
                        "forced subc bash promote failure",
                    )
                } else {
                    crate::commands::bash_orchestrate::promote_bash(
                        ctx,
                        &task_id_for_promote,
                        &session_for_promote,
                        ctx.config().project_root.as_deref(),
                        timeout,
                        wait_window_ms,
                        &request_id_for_promote,
                    )
                };
                let result = finalized_bash_result(
                    response,
                    ctx,
                    &session_for_promote,
                    &format_context_for_promote,
                    true,
                );
                let ToolCallResult { text, response } = result;
                let _ = text_tx.send(text);
                response
            })
        }),
    );
    let response = await_executor_response(promote_rx, request_id).await;
    let text = text_rx.await.unwrap_or_else(|_| {
        crate::subc_format::format_response_with_context("bash", &response, &format_context)
    });
    ToolCallResult { text, response }
}

#[allow(clippy::too_many_arguments)]
async fn send_bash_deferred_completion(
    completion_tx: &mpsc::Sender<BashDeferredCompletion>,
    channel: u16,
    corr: u64,
    flags: Flags,
    ver: u8,
    root: ProjectRootId,
    request_id: String,
    result: Option<ToolCallResult>,
    fatal: bool,
) {
    let _ = completion_tx
        .send(BashDeferredCompletion {
            channel,
            corr,
            flags,
            ver,
            root,
            request_id,
            result,
            fatal,
        })
        .await;
}

async fn handle_bash_deferred_completion(
    tx: &mpsc::Sender<Frame>,
    done: BashDeferredCompletion,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, RouteBashCancel>,
    shutdown: &Arc<Notify>,
) -> Result<(), SubcError> {
    if let Some(meta) = live_roots.get_mut(&done.root) {
        meta.active_bash_waits = meta.active_bash_waits.saturating_sub(1);
        meta.touch();
    }
    let route_id = route_key(done.channel);
    let remove_route_cancel = if let Some(cancel) = route_bash_cancels.get_mut(&route_id) {
        cancel.active_waits = cancel.active_waits.saturating_sub(1);
        cancel.active_waits == 0
    } else {
        false
    };
    if remove_route_cancel {
        route_bash_cancels.remove(&route_id);
    }

    if let Some(result) = done.result {
        if routes.contains_key(&route_id) {
            let frame =
                build_tool_response_frame(done.ver, done.channel, done.corr, done.flags, &result)?;
            send_frame(tx, frame).await?;
        } else {
            log::debug!(
                "subc attach: dropping deferred bash response {} for unbound route {}",
                done.request_id,
                done.channel
            );
        }
    } else {
        log::debug!(
            "subc attach: deferred bash wait {} cancelled before delivery on route {}",
            done.request_id,
            done.channel
        );
    }

    if done.fatal {
        signal_fatal_teardown(tx, Some(done.channel), done.ver, done.corr, shutdown).await;
    }
    Ok(())
}
fn submit_maintenance_drain(
    executor: &Arc<Executor>,
    root_id: ProjectRootId,
    bg_sessions_to_check: Vec<(String, u64)>,
    completion_tx: &mpsc::Sender<MaintenanceCompletion>,
) {
    let request_id = format!(
        "subc-maintenance-drain-{}",
        root_id.as_path().to_string_lossy()
    );
    let response_id = request_id.clone();
    let completion_root_id = root_id.clone();
    let (empty_bg_sessions_tx, empty_bg_sessions_rx) = oneshot::channel::<Vec<(String, u64)>>();
    let rx = executor.submit_async(
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
    tokio::spawn(async move {
        let response = await_executor_response(rx, request_id).await;
        let empty_bg_sessions = empty_bg_sessions_rx.await.unwrap_or_default();
        let _ = completion_tx
            .send(MaintenanceCompletion {
                root_id: completion_root_id,
                response,
                empty_bg_sessions,
            })
            .await;
    });
}

async fn await_executor_response(rx: oneshot::Receiver<Response>, request_id: String) -> Response {
    rx.await
        .unwrap_or_else(|_| Response::error(request_id, "internal_error", "executor dropped"))
}

/// Flatten a tool-call `Response` + server-rendered `text` into the SAME flat
/// object the standalone NDJSON `tool_call` command puts on the wire:
/// `{id, success, ...data, text}` (Response flattens `data` to the top level —
/// protocol.rs — and `response_with_text` merges `text` in). Mirrors
/// `commands::tool_call::response_with_text` exactly, including its non-object
/// `data` fallback (data replaced by `{text}`), so the subc `structuredContent`
/// is byte-identical to the standalone response body. Built field-by-field
/// rather than via `serde_json::to_value(response)` because `#[serde(flatten)]`
/// of a non-object `data` would error.
fn flat_tool_response(response: &crate::protocol::Response, text: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_string(), Value::String(response.id.clone()));
    obj.insert("success".to_string(), Value::Bool(response.success));
    if let Some(data) = response.data.as_object() {
        for (key, value) in data {
            obj.insert(key.clone(), value.clone());
        }
    }
    obj.insert("text".to_string(), Value::String(text.to_string()));
    Value::Object(obj)
}

fn build_tool_response_frame(
    ver: u8,
    route_channel: u16,
    corr: u64,
    flags: Flags,
    result: &ToolCallResult,
) -> Result<Frame, SubcError> {
    let is_error = !result.response.success;
    // `content`/`isError` is the MCP-native surface a GENERIC host reads (and a
    // generic host ignores `structuredContent`, per the MCP spec). The
    // FIRST-PARTY AFT plugin instead reads `structuredContent`, which carries
    // the full flat standalone shape ({id, success, ...data, text}) so every
    // structured sidecar the plugin drives UI from — status_bar, bg_completions
    // (in-band drain), preview_diff, code, message, attachments — survives the
    // route. subc relays the body byte-for-byte, so this reaches the plugin
    // unchanged. SubcTransport.toolCall re-lifts `structuredContent` straight to
    // the flat ToolCallResult, so nothing downstream of the transport differs
    // from the NDJSON path.
    let payload = json!({
        "content": [{ "type": "text", "text": result.text.as_str() }],
        "isError": is_error,
        "structuredContent": flat_tool_response(&result.response, &result.text),
    });
    let body = serde_json::to_vec(&payload).map_err(SubcError::Json)?;

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

fn response_is_fatal_panic(response: &Response) -> bool {
    !response.success && response.data.get("code").and_then(Value::as_str) == Some("actor_fatal")
}

fn bash_denied_untrusted_response(request_id: impl Into<String>) -> Response {
    Response::error(
        request_id.into(),
        "bash_denied_untrusted",
        "remote/MCP-facade binds cannot run shell commands",
    )
}

fn is_bash_family_tool(name: &str) -> bool {
    name == "bash" || name.starts_with("bash_")
}

fn is_subc_agent_core_tool(name: &str) -> bool {
    matches!(
        name,
        "status"
            | "bash"
            | "read"
            | "write"
            | "edit"
            | "apply_patch"
            | "grep"
            | "glob"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
            | "conflicts"
            | "ast_search"
            | "ast_replace"
            | "delete"
            | "move"
            | "import"
            | "refactor"
            | "safety"
    )
}

/// Internal bg-completion plumbing commands the harness consumer (NOT the agent)
/// invokes over a bound route to drain and acknowledge background-bash
/// completions for its session. These are NOT agent-facing tools — they carry no
/// agent surface and never reach the model — so they're not in the manifest /
/// `is_subc_agent_core_tool`, but the plugin's bg-notification drain/ack path
/// (bg-notifications.ts: `bridge.send("bash_drain_completions"|"bash_ack_completions")`)
/// must reach dispatch over subc, otherwise an idle agent can never drain a
/// completion the wake lane nudges it about.
///
/// This is a DELIBERATELY TIGHT allowlist, kept separate from the agent
/// core-tool gate so it cannot widen the fail-closed backstop in
/// `handle_tool_call`. Every entry is session-scoped (the bind session is
/// reinjected by `run_tool_call`, overriding any body `session_id`) and
/// carries NO config/trust surface, so admitting them does not reopen the
/// `configure`-bypass hole the gate exists to close:
/// - `bash_drain_completions` / `bash_ack_completions`: per-session completion
///   registry plumbing for the bg_events wake lane.
/// - `undo_preview` / `checkpoint_paths`: read-only permission-preview reads
///   over the session's own backup/checkpoint state — the plugin safety tool
///   calls them BEFORE `aft_safety undo`/`restore` to know which paths to ask
///   permission for. Without them, safety undo/restore fails over subc.
/// Lanes are already assigned in `command_lane` (drain/undo_preview/
/// checkpoint_paths = PureRead, ack = Mutating).
fn is_subc_native_plumbing_tool(name: &str) -> bool {
    matches!(
        name,
        "bash_drain_completions" | "bash_ack_completions" | "undo_preview" | "checkpoint_paths"
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
        | "conflicts"
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

        "semantic_search" | "search" | "callgraph" | "callers" | "impact" | "call_tree"
        | "trace_to" | "trace_to_symbol" | "trace_data" | "inspect_tier2_run" => Lane::HeavyInit,

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

fn tool_description(name: &str) -> Option<String> {
    SUBC_TOOL_SCHEMAS
        .get(name)
        .and_then(|schema| schema.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// AFT's subc-mode capability manifest. It uses bare internal tool names
/// because the gateway adds any `aft_` prefix for agent-facing displays; AFT
/// schedules concurrent calls itself; the gateway runs AFT directly without a
/// sandbox. The manifest lists every tool an agent can call over subc.
fn build_manifest() -> ModuleManifest {
    let tool = |name: &str, execution_mode: ExecutionMode| Tool {
        name: name.to_string(),
        description: tool_description(name),
        execution_mode,
        schema: tool_schema(name),
    };
    // execution_mode keys on externally-observable side effects, NOT internal
    // ctx mutation: the readers warm AFT's own index/cache/symbol artifacts
    // (internal), not the user's workspace, so they are Pure. Bash is Mutating
    // because spawning a detached process changes external state, and edit/write
    // produce observable file writes. Unfenceable stays unused here because AFT
    // schedules bash internally and releases the Mutating worker after spawn.
    ModuleManifest {
        module_id: "aft".to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![
                tool("status", ExecutionMode::Pure),
                tool("bash", ExecutionMode::Mutating),
                tool("read", ExecutionMode::Pure),
                tool("write", ExecutionMode::Mutating),
                tool("edit", ExecutionMode::Mutating),
                tool("apply_patch", ExecutionMode::Mutating),
                tool("grep", ExecutionMode::Pure),
                tool("glob", ExecutionMode::Pure),
                tool("search", ExecutionMode::Pure),
                tool("outline", ExecutionMode::Pure),
                tool("zoom", ExecutionMode::Pure),
                tool("inspect", ExecutionMode::Pure),
                tool("callgraph", ExecutionMode::Pure),
                tool("conflicts", ExecutionMode::Pure),
                tool("ast_search", ExecutionMode::Pure),
                tool("ast_replace", ExecutionMode::Mutating),
                tool("delete", ExecutionMode::Mutating),
                tool("move", ExecutionMode::Mutating),
                tool("import", ExecutionMode::Mutating),
                tool("refactor", ExecutionMode::Mutating),
                tool("safety", ExecutionMode::Mutating),
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

fn build_health_report(executor: &Executor) -> HealthReport {
    let mut roots: Vec<RootHealthSnapshot> = executor
        .actor_entries()
        .into_iter()
        .map(|(root_id, ctx)| ctx.try_health_snapshot(root_id.as_path()))
        .collect();
    roots.sort_by(|left, right| left.project_root.cmp(&right.project_root));

    let all_roots_ready = roots.iter().all(RootHealthSnapshot::is_fully_ready);
    let detail = if roots.is_empty() || all_roots_ready {
        None
    } else {
        Some("one or more root actors are still warming, degraded, or busy".to_string())
    };

    HealthReport {
        status: if all_roots_ready {
            HealthStatus::Ok
        } else {
            HealthStatus::Degraded
        },
        detail,
        metrics: Some(json!({
            "actor_count": roots.iter().map(|root| root.actor_count).sum::<usize>(),
            "root_count": roots.len(),
            "roots": roots,
        })),
    }
}

fn control_ops() -> Option<Vec<String>> {
    Some(vec![
        "route.bind".to_string(),
        "route.status".to_string(),
        MODULE_CONTROL_OP_HEALTH_CHECK.to_string(),
    ])
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
        route_identity_with_trust(root, session_id, BindTrust::FirstParty)
    }

    fn route_identity_with_trust(
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

        let replayed = replay_buffered_push_frames(
            &writer_tx,
            route_key(3),
            &mut push_buffer,
            &key,
            BindTrust::FirstParty,
        );

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
    fn replay_buffered_push_frames_skips_bash_for_untrusted_route() {
        let (_root_dir, root) = test_root("subc-buffer-replay-untrusted-root");
        let key = ReplayKey {
            root,
            harness: "mcp".to_string(),
            session: "session-1".to_string(),
        };
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(4);
        let mut push_buffer = HashMap::new();
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-a"));

        let replayed = replay_buffered_push_frames(
            &writer_tx,
            route_key(3),
            &mut push_buffer,
            &key,
            BindTrust::Untrusted,
        );

        assert_eq!(replayed, 0);
        assert!(!push_buffer.contains_key(&key));
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
            replay_buffered_push_frames(
                &writer_tx,
                route_key(4),
                &mut push_buffer,
                &key,
                BindTrust::FirstParty
            ),
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
            replay_buffered_push_frames(
                &writer_tx,
                route_key(4),
                &mut push_buffer,
                &key,
                BindTrust::FirstParty
            ),
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
            replay_buffered_push_frames(
                &writer_tx,
                route_key(4),
                &mut push_buffer,
                &key,
                BindTrust::FirstParty
            ),
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

    #[test]
    fn arm_bg_wake_bumps_epoch_even_when_channel_is_already_pending() {
        let (_root_dir, root) = test_root("subc-bg-wake-epoch-root");
        let session = "session-1".to_string();
        let key = (root.clone(), session.clone());
        let channel = route_key(7);
        let mut bg_wake_pending = HashSet::from([channel]);
        let mut bg_wake_epoch = HashMap::from([(key.clone(), 41_u64)]);

        arm_bg_wake(
            root,
            session,
            channel,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
        );

        assert_eq!(bg_wake_pending, HashSet::from([channel]));
        assert_eq!(bg_wake_epoch.get(&key).copied(), Some(42));
    }

    #[test]
    fn stale_maintenance_epoch_does_not_clear_newer_bg_wake() {
        let (_root_dir, root) = test_root("subc-bg-wake-stale-root");
        let session = "session-1".to_string();
        let key = (root.clone(), session.clone());
        let channel = route_key(8);
        let mut bg_sub_by_session = HashMap::new();
        bg_sub_by_session.insert(key.clone(), channel);
        let mut bg_wake_pending = HashSet::new();
        let mut bg_wake_epoch = HashMap::new();

        arm_bg_wake(
            root.clone(),
            session.clone(),
            channel,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
        );
        let epoch_at_submit = bg_wake_epoch[&key];
        arm_bg_wake(
            root.clone(),
            session.clone(),
            channel,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
        );

        clear_stale_bg_wakes_for_empty_sessions(
            &root,
            &[(session, epoch_at_submit)],
            &bg_sub_by_session,
            &mut bg_wake_pending,
            &bg_wake_epoch,
        );

        assert!(bg_wake_pending.contains(&channel));
        assert_eq!(bg_wake_epoch.get(&key).copied(), Some(epoch_at_submit + 1));
    }

    #[test]
    fn matching_maintenance_epoch_clears_genuinely_stale_bg_wake() {
        let (_root_dir, root) = test_root("subc-bg-wake-clear-root");
        let session = "session-1".to_string();
        let key = (root.clone(), session.clone());
        let channel = route_key(9);
        let mut bg_sub_by_session = HashMap::new();
        bg_sub_by_session.insert(key.clone(), channel);
        let mut bg_wake_pending = HashSet::new();
        let mut bg_wake_epoch = HashMap::new();

        arm_bg_wake(
            root.clone(),
            session.clone(),
            channel,
            &mut bg_wake_pending,
            &mut bg_wake_epoch,
        );
        let epoch_at_submit = bg_wake_epoch[&key];

        clear_stale_bg_wakes_for_empty_sessions(
            &root,
            &[(session, epoch_at_submit)],
            &bg_sub_by_session,
            &mut bg_wake_pending,
            &bg_wake_epoch,
        );

        assert!(!bg_wake_pending.contains(&channel));
    }

    #[test]
    fn response_is_fatal_panic_only_matches_panic_exclusive_code() {
        let tool_error = Response::error("request-1", "internal_error", "ordinary tool error");
        let panic_error = Response::error("request-2", "actor_fatal", "mutating panic");

        assert!(!response_is_fatal_panic(&tool_error));
        assert!(response_is_fatal_panic(&panic_error));
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

    const CORE_TOOLS: [&str; 21] = [
        "status",
        "bash",
        "read",
        "write",
        "edit",
        "apply_patch",
        "grep",
        "glob",
        "search",
        "outline",
        "zoom",
        "inspect",
        "callgraph",
        "conflicts",
        "ast_search",
        "ast_replace",
        "delete",
        "move",
        "import",
        "refactor",
        "safety",
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
                tool.description
                    .as_deref()
                    .is_some_and(|description| !description.is_empty()),
                "{name} must carry a non-empty manifest description"
            );
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
        for name in [
            "status",
            "read",
            "grep",
            "glob",
            "search",
            "outline",
            "zoom",
            "inspect",
            "callgraph",
            "conflicts",
            "ast_search",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Pure,
                "{name} produces no observable side effect and must be Pure"
            );
        }
        // Mutating tools can write files, change safety state, or spawn processes.
        for name in [
            "bash",
            "write",
            "edit",
            "apply_patch",
            "ast_replace",
            "delete",
            "move",
            "import",
            "refactor",
            "safety",
        ] {
            assert_eq!(
                by_name[name].execution_mode,
                ExecutionMode::Mutating,
                "{name} writes files and must be Mutating"
            );
        }
    }

    #[test]
    fn subc_agent_lanes_classify_new_read_tools() {
        assert_eq!(command_lane("callgraph"), Lane::HeavyInit);
        assert_eq!(command_lane("conflicts"), Lane::PureRead);
    }

    #[test]
    fn native_plumbing_allowlist_admits_exactly_drain_ack_and_safety_previews() {
        // BC2: the route gate admits a name when it's an agent core tool OR a
        // native plumbing command. These carry no agent surface and no
        // config/trust surface, so they're admitted to dispatch over a bound
        // route while everything else (notably `configure`) stays fail-closed.
        assert!(is_subc_native_plumbing_tool("bash_drain_completions"));
        assert!(is_subc_native_plumbing_tool("bash_ack_completions"));
        // Safety-tool permission previews: read-only, session-scoped. Without
        // these, aft_safety undo/restore breaks over the subc transport.
        assert!(is_subc_native_plumbing_tool("undo_preview"));
        assert!(is_subc_native_plumbing_tool("checkpoint_paths"));

        // The allowlist is TIGHT — it must not admit the config-bypass vector
        // the fail-closed gate exists to block, nor any other native command.
        assert!(!is_subc_native_plumbing_tool("configure"));
        assert!(!is_subc_native_plumbing_tool("bash"));
        assert!(!is_subc_native_plumbing_tool("bash_kill"));
        assert!(!is_subc_native_plumbing_tool("db_set_state"));
        assert!(!is_subc_native_plumbing_tool("undo"));

        // The plumbing commands are NOT agent-facing tools — they must stay out
        // of the manifest gate so they never reach the model surface.
        assert!(!is_subc_agent_core_tool("bash_drain_completions"));
        assert!(!is_subc_agent_core_tool("bash_ack_completions"));

        // Lanes are already assigned (pre-existing): drain reads, ack mutates.
        assert_eq!(command_lane("bash_drain_completions"), Lane::PureRead);
        assert_eq!(command_lane("bash_ack_completions"), Lane::Mutating);
    }

    #[test]
    fn tool_response_frame_carries_flat_standalone_shape_in_structured_content() {
        use crate::protocol::Response;

        // A response with sidecars the FIRST-PARTY plugin drives UI from
        // (status_bar, bg_completions, code) plus a normal result field.
        let response = Response::success(
            "req-7",
            json!({
                "complete": true,
                "matches": 3,
                "status_bar": { "errors": 0, "warnings": 1 },
                "bg_completions": [{ "task_id": "bash-abc" }],
            }),
        );
        let result = ToolCallResult {
            text: "rendered text".to_string(),
            response,
        };

        // The flat shape must equal the standalone NDJSON `tool_call` body:
        // {id, success, ...data, text}. Build the standalone expectation the
        // same way commands::tool_call::response_with_text does.
        let expected_flat = json!({
            "id": "req-7",
            "success": true,
            "complete": true,
            "matches": 3,
            "status_bar": { "errors": 0, "warnings": 1 },
            "bg_completions": [{ "task_id": "bash-abc" }],
            "text": "rendered text",
        });
        assert_eq!(
            flat_tool_response(&result.response, &result.text),
            expected_flat,
            "structuredContent must be byte-identical to the standalone flat response"
        );

        // The frame body carries the MCP surface for generic hosts AND the flat
        // sidecar shape under structuredContent for the first-party plugin.
        let frame =
            build_tool_response_frame(PROTOCOL_VERSION, 1, 42, control_flags(), &result).unwrap();
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["isError"], json!(false));
        assert_eq!(body["content"][0]["type"], json!("text"));
        assert_eq!(body["content"][0]["text"], json!("rendered text"));
        assert_eq!(body["structuredContent"], expected_flat);

        // A failed response flips isError and still carries the flat shape
        // (with success:false + code) for the plugin's error path.
        let err = Response::error_with_data(
            "req-8",
            "ambiguous_match",
            "too many matches",
            json!({ "candidates": ["a", "b"] }),
        );
        let err_result = ToolCallResult {
            text: "error text".to_string(),
            response: err,
        };
        let err_frame =
            build_tool_response_frame(PROTOCOL_VERSION, 1, 43, control_flags(), &err_result)
                .unwrap();
        let err_body: Value = serde_json::from_slice(&err_frame.body).unwrap();
        assert_eq!(err_body["isError"], json!(true));
        assert_eq!(err_body["structuredContent"]["success"], json!(false));
        assert_eq!(
            err_body["structuredContent"]["code"],
            json!("ambiguous_match")
        );
        assert_eq!(
            err_body["structuredContent"]["candidates"],
            json!(["a", "b"])
        );
        assert_eq!(err_body["structuredContent"]["text"], json!("error text"));
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
