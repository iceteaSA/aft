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

/// Cadence for the loop's deadline-driven drain work (retry-buffer flush,
/// bg-wake emission, maintenance submission). Checked at the top of every
/// loop turn so busy select arms cannot starve it.
const DRAIN_TICK_PERIOD: Duration = Duration::from_millis(250);

/// Root-scoped stores and watcher runtimes are reopened lazily after this
/// period without tool traffic. Keeping the value fixed avoids per-client
/// eviction policies competing inside the module loop.
const IDLE_ROOT_TTL: Duration = Duration::from_secs(30 * 60);

const WRITER_QUEUE_CAPACITY: usize = 256;

/// Keep reliable Push bursts from monopolizing the current-thread subc loop;
/// any remaining must-deliver frames stay queued for the next loop turn.
const RELIABLE_PUSH_DRAIN_BUDGET: usize = 32;

/// Limit maintenance submissions per tick so background drains cannot delay
/// control-plane work such as completed RouteBind acknowledgements.
///
/// The decomposed maintenance pass charges this budget by Mutating job, not by
/// root. Set the default burst to 24 so one maintenance pass over eight live
/// roots fits in a single tick, while follow-up batches still re-enter the
/// capped queue instead of bypassing the budget.
const MAINTENANCE_SUBMIT_BUDGET: usize = INITIAL_MAINTENANCE_DRAIN_KINDS.len() * 8;
const INITIAL_MAINTENANCE_DRAIN_KINDS: [MaintenanceDrainKind; 3] = [
    MaintenanceDrainKind::Watcher,
    MaintenanceDrainKind::Lsp,
    MaintenanceDrainKind::Short,
];
#[cfg(test)]
const INITIAL_MAINTENANCE_JOB_COUNT: usize = INITIAL_MAINTENANCE_DRAIN_KINDS.len();

const RELIABLE_WRITER_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(10);
const RELIABLE_WRITER_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);

const DISPATCH_PATH_BIND_WARN_AFTER: Duration = Duration::from_secs(6);
const ROUTE_BIND_DEADLINE: Duration = Duration::from_secs(12);

/// Small bounded memory of completed task ids used to suppress stale lossy
/// long-running reminders that arrive after their reliable completion event.
const COMPLETED_TASK_SUPPRESSION_MAX: usize = 4096;

/// Bash foreground orchestration polls detached tasks with short read-lane jobs.
/// The sleep between polls is outside the executor so no read or write worker is
/// pinned while a foreground command is still running.
const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Host elicitation asks fail closed if the MCP facade does not answer promptly.
const BASH_ELICITATION_TIMEOUT: Duration = Duration::from_secs(60);
const BASH_ELICITATION_CREATE_METHOD: &str = "elicitation/create";

type RouteChannel = u32;
type PushEnvelope = (ProjectRootId, PushFrame);
type LossyPushEnvelope = (u64, ProjectRootId, PushFrame);
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

/// Test-only view of the fail-closed tool-call gate: would `name` be admitted
/// on a bound route (as an agent tool or native plumbing)? Used by the
/// plugin-send drift guard in `subc_plumbing_drift_test.rs`.
pub fn is_tool_call_admitted_for_test(name: &str) -> bool {
    manifest::is_subc_agent_core_tool(name) || manifest::is_subc_native_plumbing_tool(name)
}
use self::wire::{
    build_error_frame, build_goodbye_frame, build_tool_response_frame, decrement_counted_channel,
    response_is_fatal_panic, response_message, send_counted_channel, send_frame,
    send_reliable_writer_frame,
};

#[derive(Clone)]
struct PushSenders {
    lossy_tx: mpsc::Sender<LossyPushEnvelope>,
    reliable_tx: mpsc::UnboundedSender<PushEnvelope>,
    lossy_overflow: Arc<push::LossyOverflow>,
    lossy_seq: Arc<AtomicU64>,
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
    maintenance_jobs_in_flight: usize,
    maintenance_queued_kinds: VecDeque<MaintenanceDrainKind>,
    maintenance_last_submitted: Option<Instant>,
    maintenance_poisoned: bool,
    last_touched: Instant,
    diagnostics_on_edit: bool,
    active_bash_waits: usize,
    idle_artifacts_evicted: bool,
}

#[derive(Debug)]
struct PendingBind {
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    cancelled: bool,
    configure_request_id: String,
    started_at: Instant,
    warned_half_deadline: bool,
    deadline_reported: bool,
    corr: u64,
    ver: u8,
    flags: Flags,
}

struct RouteBindCompletion {
    route_channel: u16,
    identity: RouteIdentity,
    bind_root_id: ProjectRootId,
    inserted_new_actor: bool,
    configure_response: Response,
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
    consumer_elicitation_capable: bool,
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
    requeue_kind: Option<MaintenanceDrainKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceDrainKind {
    Watcher,
    Lsp,
    Short,
}

impl MaintenanceDrainKind {
    fn label(self) -> &'static str {
        match self {
            Self::Watcher => "watcher",
            Self::Lsp => "lsp",
            Self::Short => "short",
        }
    }
}

#[derive(Debug, Default)]
struct MaintenanceJobOutcome {
    empty_bg_sessions: Vec<(String, u64)>,
    requeue_kind: Option<MaintenanceDrainKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ReverseCorrKey {
    route: RouteChannel,
    corr: u64,
}

struct PendingBashAsk {
    route_channel: u16,
    tool_corr: u64,
    tool_flags: Flags,
    tool_ver: u8,
    root: ProjectRootId,
    project_root: PathBuf,
    session_id: String,
    request_id: String,
    arguments: Value,
    format_context: crate::subc_format::FormatContext,
    cancel: bash::BashWaitCancel,
    grants: Vec<String>,
    expires_at: Instant,
}

impl RootMeta {
    fn new(now: Instant) -> Self {
        Self {
            maintenance_pending: false,
            maintenance_jobs_in_flight: 0,
            maintenance_queued_kinds: VecDeque::new(),
            maintenance_last_submitted: None,
            maintenance_poisoned: false,
            last_touched: now,
            diagnostics_on_edit: false,
            active_bash_waits: 0,
            idle_artifacts_evicted: false,
        }
    }

    fn touch(&mut self) {
        self.last_touched = Instant::now();
        self.idle_artifacts_evicted = false;
    }
}

fn due_maintenance_jobs(
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    budget: usize,
    pending_bind_roots: &HashSet<ProjectRootId>,
) -> (Vec<(ProjectRootId, MaintenanceDrainKind)>, bool) {
    let mut jobs = Vec::new();
    let mut deferred = false;
    let mut roots = live_roots.keys().cloned().collect::<Vec<_>>();
    roots.sort_by(|left, right| {
        let left_last = live_roots
            .get(left)
            .and_then(|meta| meta.maintenance_last_submitted);
        let right_last = live_roots
            .get(right)
            .and_then(|meta| meta.maintenance_last_submitted);
        left_last
            .cmp(&right_last)
            .then_with(|| left.as_path().cmp(right.as_path()))
    });

    for root_id in roots {
        let Some(meta) = live_roots.get_mut(&root_id) else {
            continue;
        };
        if meta.maintenance_poisoned {
            continue;
        }

        if pending_bind_roots.contains(&root_id) {
            if meta.maintenance_pending || !meta.maintenance_queued_kinds.is_empty() {
                deferred = true;
            }
            continue;
        }

        if !meta.maintenance_pending {
            if jobs.len() >= budget {
                deferred = true;
                continue;
            }
            meta.maintenance_pending = true;
            meta.maintenance_queued_kinds
                .extend(INITIAL_MAINTENANCE_DRAIN_KINDS);
        }

        while let Some(kind) = meta.maintenance_queued_kinds.pop_front() {
            if jobs.len() >= budget {
                meta.maintenance_queued_kinds.push_front(kind);
                deferred = true;
                break;
            }
            meta.maintenance_jobs_in_flight += 1;
            meta.maintenance_last_submitted = Some(Instant::now());
            jobs.push((root_id.clone(), kind));
        }

        meta.maintenance_pending =
            meta.maintenance_jobs_in_flight > 0 || !meta.maintenance_queued_kinds.is_empty();
    }

    (jobs, deferred)
}

/// Reap root-scoped resources from the existing subc maintenance loop. The
/// actor remains registered so a bound route can continue to receive requests;
/// only disposable handles are cleared and the next request reopens them.
fn reap_idle_roots(
    now: Instant,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    executor: &Arc<Executor>,
) -> usize {
    let pending_bind_roots = pending_binds
        .values()
        .map(|pending| pending.bind_root_id.clone())
        .collect::<HashSet<_>>();
    let candidates = live_roots
        .iter()
        .filter_map(|(root_id, meta)| {
            if meta.idle_artifacts_evicted
                || now.saturating_duration_since(meta.last_touched) < IDLE_ROOT_TTL
                || meta.active_bash_waits > 0
                || meta.maintenance_pending
                || !meta.maintenance_queued_kinds.is_empty()
                || pending_bind_roots.contains(root_id)
            {
                return None;
            }
            Some(root_id.clone())
        })
        .collect::<Vec<_>>();

    let mut reaped = 0;
    for root_id in candidates {
        let Some(ctx) = executor.actor_context(&root_id) else {
            continue;
        };
        if !ctx.evict_idle_artifacts() {
            continue;
        }
        // The watcher backend owns the OS watcher and can block while joining
        // on macOS. Request shutdown here, but let a dedicated reaper thread
        // perform the join rather than holding up an executor maintenance lane.
        ctx.stop_watcher_runtime_in_background();
        if let Some(meta) = live_roots.get_mut(&root_id) {
            meta.idle_artifacts_evicted = true;
        }
        reaped += 1;
        log::info!(
            "subc attach: evicted idle root artifacts for {}",
            root_id.as_path().display()
        );
    }
    reaped
}

#[allow(clippy::too_many_arguments)]
fn submit_due_maintenance_jobs(
    executor: &Arc<Executor>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    pending_binds: &HashMap<RouteChannel, PendingBind>,
    bg_sub_by_session: &HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_epoch: &HashMap<(ProjectRootId, String), u64>,
    maintenance_tx: &mpsc::Sender<MaintenanceCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
) {
    let pending_bind_roots = pending_binds
        .values()
        .map(|pending| pending.bind_root_id.clone())
        .collect::<HashSet<_>>();
    let (due_jobs, deferred_jobs) =
        due_maintenance_jobs(live_roots, MAINTENANCE_SUBMIT_BUDGET, &pending_bind_roots);
    if deferred_jobs {
        metrics
            .maintenance_budget_deferrals
            .fetch_add(1, Ordering::Relaxed);
    }
    for (root_id, kind) in due_jobs {
        let bg_sessions_to_check = if kind == MaintenanceDrainKind::Short {
            bg_sub_by_session
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
                .collect()
        } else {
            Vec::new()
        };
        submit_maintenance_job(
            executor,
            root_id,
            kind,
            bg_sessions_to_check,
            maintenance_tx,
            metrics,
        );
    }
}

fn note_maintenance_completion(
    meta: &mut RootMeta,
    requeue_kind: Option<MaintenanceDrainKind>,
    fatal: bool,
    defer_requeue: bool,
) {
    if fatal {
        meta.maintenance_poisoned = true;
    }

    if let Some(kind) = requeue_kind.filter(|_| !meta.maintenance_poisoned && !defer_requeue) {
        meta.maintenance_queued_kinds.push_back(kind);
    }

    meta.maintenance_jobs_in_flight = meta.maintenance_jobs_in_flight.saturating_sub(1);
    meta.maintenance_pending =
        meta.maintenance_jobs_in_flight > 0 || !meta.maintenance_queued_kinds.is_empty();
}

fn route_key(channel: u16) -> RouteChannel {
    RouteChannel::from(channel)
}

fn bash_elicitation_timeout() -> Duration {
    if cfg!(debug_assertions) {
        if let Ok(raw) = std::env::var("AFT_TEST_SUBC_BASH_ELICITATION_TTL_MS") {
            if let Ok(ms) = raw.parse::<u64>() {
                if ms > 0 {
                    return Duration::from_millis(ms);
                }
            }
        }
    }
    BASH_ELICITATION_TIMEOUT
}

fn allocate_reverse_corr(
    pending_bash_asks: &HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    next_corr: &mut u64,
) -> u64 {
    loop {
        let corr = *next_corr;
        *next_corr = (*next_corr).wrapping_add(1).max(1);
        if !pending_bash_asks.contains_key(&ReverseCorrKey { route, corr }) {
            return corr;
        }
    }
}

fn bash_permission_kind_label(kind: &crate::bash_permissions::PermissionKind) -> &'static str {
    match kind {
        crate::bash_permissions::PermissionKind::ExternalDirectory => "external directory",
        crate::bash_permissions::PermissionKind::Bash => "bash",
    }
}

fn bash_elicitation_patterns(asks: &[crate::bash_permissions::PermissionAsk]) -> Vec<String> {
    let mut patterns = Vec::new();
    let mut seen = HashSet::new();
    for ask in asks {
        for pattern in ask.patterns.iter().chain(ask.always.iter()) {
            if seen.insert(pattern.clone()) {
                patterns.push(pattern.clone());
            }
        }
    }
    patterns
}

fn bash_elicitation_message(
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> String {
    let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let patterns = bash_elicitation_patterns(asks);
    let pattern_text = if patterns.is_empty() {
        "no matched permission patterns".to_string()
    } else {
        patterns.join(", ")
    };
    let ask_kinds = asks
        .iter()
        .map(|ask| bash_permission_kind_label(&ask.kind))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    if ask_kinds.is_empty() {
        format!("Allow bash command `{command}`? Matched patterns: {pattern_text}")
    } else {
        format!("Allow bash command `{command}`? Matched {ask_kinds} patterns: {pattern_text}")
    }
}

fn bash_elicitation_request_body(
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> Value {
    json!({
        "method": BASH_ELICITATION_CREATE_METHOD,
        "params": {
            "mode": "form",
            "message": bash_elicitation_message(command, asks),
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "decision": {
                        "type": "string",
                        "enum": ["allow", "deny"],
                        "description": "Choose allow to run this bash command once, or deny to block it."
                    }
                },
                "required": ["decision"],
                "additionalProperties": false
            },
            "_meta": {
                "aft": {
                    "tool": "bash",
                    "command": command,
                    "asks": asks
                }
            }
        }
    })
}

fn build_bash_elicitation_request_frame(
    ver: u8,
    channel: u16,
    corr: u64,
    flags: Flags,
    command: &str,
    asks: &[crate::bash_permissions::PermissionAsk],
) -> Result<Frame, SubcError> {
    let body = bash_elicitation_request_body(command, asks);
    Frame::build_with_version(
        ver,
        FrameType::Request,
        flags,
        channel,
        corr,
        serde_json::to_vec(&body).map_err(SubcError::Json)?,
    )
    .map_err(SubcError::FrameBuild)
}

fn bash_elicitation_reply_is_allow(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    flat_bash_elicitation_reply_is_allow(&value) || mcp_bash_elicitation_reply_is_allow(&value)
}

fn flat_bash_elicitation_reply_is_allow(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 1 && object.get("decision").and_then(Value::as_str) == Some("allow")
}

fn mcp_bash_elicitation_reply_is_allow(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.len() != 2 || object.get("action").and_then(Value::as_str) != Some("accept") {
        return false;
    }
    let Some(content) = object.get("content").and_then(Value::as_object) else {
        return false;
    };
    content.len() == 1 && content.get("decision").and_then(Value::as_str) == Some("allow")
}

#[allow(clippy::too_many_arguments)]
async fn settle_pending_bash_ask_denied(
    tx: &mpsc::Sender<Frame>,
    pending: PendingBashAsk,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let completion = bash::bash_denied_untrusted_completion(
        pending.route_channel,
        pending.tool_corr,
        pending.tool_flags,
        pending.tool_ver,
        pending.root,
        pending.request_id,
        pending.format_context,
    );
    bash::handle_bash_deferred_completion(
        tx,
        completion,
        routes,
        live_roots,
        route_bash_cancels,
        shutdown,
        metrics,
    )
    .await
}

fn take_pending_bash_asks_for_route(
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
) -> Vec<PendingBashAsk> {
    let keys = pending_bash_asks
        .keys()
        .copied()
        .filter(|key| key.route == route)
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending_bash_asks.remove(&key))
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn settle_pending_bash_asks_for_route(
    tx: &mpsc::Sender<Frame>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    for pending in take_pending_bash_asks_for_route(pending_bash_asks, route) {
        settle_pending_bash_ask_denied(
            tx,
            pending,
            routes,
            live_roots,
            route_bash_cancels,
            shutdown,
            metrics,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn settle_all_pending_bash_asks(
    tx: &mpsc::Sender<Frame>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let pending = pending_bash_asks
        .drain()
        .map(|(_, pending)| pending)
        .collect::<Vec<_>>();
    for pending in pending {
        settle_pending_bash_ask_denied(
            tx,
            pending,
            routes,
            live_roots,
            route_bash_cancels,
            shutdown,
            metrics,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn expire_pending_bash_asks(
    tx: &mpsc::Sender<Frame>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let now = Instant::now();
    let expired = pending_bash_asks
        .iter()
        .filter_map(|(key, pending)| (pending.expires_at <= now).then_some(*key))
        .collect::<Vec<_>>();
    for key in expired {
        if let Some(pending) = pending_bash_asks.remove(&key) {
            log::debug!(
                "subc attach: bash elicitation request {} on route {} expired fail-closed",
                key.corr,
                pending.route_channel
            );
            settle_pending_bash_ask_denied(
                tx,
                pending,
                routes,
                live_roots,
                route_bash_cancels,
                shutdown,
                metrics,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_bash_elicitation_reply(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    bash_deferred_tx: &mpsc::Sender<bash::BashDeferredCompletion>,
    bash_poll_touch_tx: &mpsc::Sender<ProjectRootId>,
    metrics: &Arc<DispatchPathMetrics>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    dispatch: DispatchFn,
) -> Result<(), SubcError> {
    let key = ReverseCorrKey {
        route: route_key(frame.header.channel),
        corr: frame.header.corr,
    };
    let Some(pending) = pending_bash_asks.remove(&key) else {
        return Ok(());
    };

    if frame.header.ty == FrameType::Response && bash_elicitation_reply_is_allow(&frame.body) {
        if routes.contains_key(&key.route) {
            bash::submit_deferred_bash(
                executor,
                bash_deferred_tx,
                bash_poll_touch_tx,
                metrics,
                dispatch,
                pending.root,
                pending.project_root,
                pending.session_id,
                pending.request_id,
                pending.route_channel,
                pending.tool_corr,
                pending.tool_flags,
                pending.tool_ver,
                pending.arguments,
                pending.format_context,
                pending.cancel,
                BindTrust::Untrusted,
                Some(pending.grants),
            );
            return Ok(());
        }
        log::debug!(
            "subc attach: dropping allowed bash elicitation reply {} for unbound route {}",
            key.corr,
            pending.route_channel
        );
    }

    settle_pending_bash_ask_denied(
        tx,
        pending,
        routes,
        live_roots,
        route_bash_cancels,
        shutdown,
        metrics,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn cancel_pending_bash_ask_for_tool_call(
    tx: &mpsc::Sender<Frame>,
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    route: RouteChannel,
    tool_corr: u64,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    route_bash_cancels: &mut HashMap<RouteChannel, bash::RouteBashCancel>,
    shutdown: &Arc<Notify>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let keys = pending_bash_asks
        .iter()
        .filter_map(|(key, pending)| {
            (key.route == route && pending.tool_corr == tool_corr).then_some(*key)
        })
        .collect::<Vec<_>>();
    for key in keys {
        if let Some(pending) = pending_bash_asks.remove(&key) {
            settle_pending_bash_ask_denied(
                tx,
                pending,
                routes,
                live_roots,
                route_bash_cancels,
                shutdown,
                metrics,
            )
            .await?;
        }
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModuleLoopExit {
    Graceful,
    SkipSearchFlush,
}

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

    let actor_contexts = executor.actor_contexts();
    if matches!(loop_result, Ok(ModuleLoopExit::Graceful)) {
        // EOF/Goodbye teardown flushes each root's in-memory trigram delta.
        // Fatal/panic-driven connection teardown skips this best-effort disk work.
        flush_actor_search_indexes_on_graceful_shutdown(&actor_contexts);
    }
    for actor_ctx in &actor_contexts {
        actor_ctx.lsp().shutdown_all();
        actor_ctx.bash_background().detach();
    }

    loop_result.map(|_| ())
}

fn flush_actor_search_indexes_on_graceful_shutdown(actor_contexts: &[Arc<AppContext>]) {
    for actor_ctx in actor_contexts {
        let _ = actor_ctx.flush_search_index_on_graceful_shutdown();
    }
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
    metrics: &Arc<DispatchPathMetrics>,
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
    metrics: &Arc<DispatchPathMetrics>,
) -> Result<usize, SubcError> {
    let mut drained = 0;
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
        drained += 1;
    }
    Ok(drained)
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
) -> Result<ModuleLoopExit, SubcError>
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
    // Drain-tick deadline is tracked manually and checked at the TOP of every
    // loop turn rather than as an Interval select arm: the select below is
    // `biased` (bind completions first), and biased polling means a saturated
    // higher arm (sustained lossy push traffic keeps lossy_rx always-ready)
    // would starve every arm below it, including a timer arm — leaving
    // backpressured reliable frames parked in the retry buffer past their
    // delivery deadline. The pre-turn check cannot be starved by arm order;
    // the sleep_until arm below only exists to wake an otherwise-idle loop.
    let mut next_drain_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
    let mut next_maintenance_at = next_drain_at;
    let (maintenance_tx, mut maintenance_rx) = mpsc::channel::<MaintenanceCompletion>(256);
    let (bash_deferred_tx, mut bash_deferred_rx) =
        mpsc::channel::<bash::BashDeferredCompletion>(256);
    let (bash_poll_touch_tx, mut bash_poll_touch_rx) = mpsc::channel::<ProjectRootId>(256);
    let (control_completion_tx, mut control_completion_rx) =
        mpsc::channel::<RouteBindCompletion>(256);
    let (lossy_tx, mut lossy_rx) = mpsc::channel::<LossyPushEnvelope>(1024);
    let lossy_overflow = Arc::new(push::LossyOverflow::default());
    let lossy_seq = Arc::new(AtomicU64::new(0));
    let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
    let push_senders = PushSenders {
        lossy_tx,
        reliable_tx,
        lossy_overflow: Arc::clone(&lossy_overflow),
        lossy_seq,
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
    let mut pending_bash_asks: HashMap<ReverseCorrKey, PendingBashAsk> = HashMap::new();
    let mut next_bash_ask_corr: u64 = 1;
    let mut route_bash_cancels: HashMap<RouteChannel, bash::RouteBashCancel> = HashMap::new();

    let loop_result: Result<ModuleLoopExit, SubcError> = loop {
        crate::logging::perf_tick(Some(&executor));
        dispatch_path_metrics.mark_frame_loop_tick();
        if let Err(error) = expire_pending_bash_asks(
            &writer_tx,
            &mut pending_bash_asks,
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

        // RouteBind completions are control-plane unblockers. Drain any completed
        // binds before entering other branch work so Push and maintenance bursts
        // can only add one loop-turn of latency.
        match drain_pending_route_bind_completions(
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
            Ok(drained) => {
                if drained > 0 {
                    next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
                }
            }
            Err(error) => break Err(error),
        }

        if tokio::time::Instant::now() >= next_drain_at {
            push::emit_bg_event_wakes(
                &writer_tx,
                &dispatch_path_metrics,
                &bg_subs,
                &mut bg_wake_pending,
            );
            warn_slow_pending_binds(&mut pending_binds, &executor);
            if let Err(error) =
                expire_overdue_route_binds(&writer_tx, &mut pending_binds, &dispatch_path_metrics)
                    .await
            {
                break Err(error);
            }

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

            next_drain_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
        }

        // A lossy emitter may place its newest update in the overflow buffer
        // when the bounded channel is full, while this receive loop is draining
        // the channel. Drain overflow before selecting again so that raced
        // update is delivered on the next timer tick instead of waiting for
        // another lossy enqueue.
        let overflow_batch = lossy_overflow.drain();
        if !overflow_batch.is_empty() {
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

            let mut batch = Vec::new();
            while let Ok(item) = lossy_rx.try_recv() {
                batch.push(item);
            }
            batch.extend(overflow_batch);
            push::process_lossy_push_envelope_batch(
                &writer_tx,
                &dispatch_path_metrics,
                &routes,
                &root_channels,
                &completed_tasks,
                batch,
            );
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
                next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
            }
            _ = shutdown.notified() => {
                log::warn!("subc attach: fatal executor response requested teardown");
                break Ok(ModuleLoopExit::SkipSearchFlush);
            }
            maybe_frame = reader_rx.recv() => {
                let frame = match maybe_frame {
                    None => {
                        log::info!("subc attach: daemon closed connection");
                        break Ok(ModuleLoopExit::Graceful);
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
                        break Ok(ModuleLoopExit::Graceful);
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
                        if let Err(error) = settle_pending_bash_asks_for_route(
                            &writer_tx,
                            &mut pending_bash_asks,
                            channel,
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
                    FrameType::Response | FrameType::Error if frame.header.channel != 0 => {
                        if let Err(error) = handle_bash_elicitation_reply(
                            &writer_tx,
                            &frame,
                            &mut pending_bash_asks,
                            &routes,
                            &mut live_roots,
                            &executor,
                            &shutdown,
                            &bash_deferred_tx,
                            &bash_poll_touch_tx,
                            &dispatch_path_metrics,
                            &mut route_bash_cancels,
                            dispatch,
                        )
                        .await
                        {
                            break Err(error);
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
                            &mut pending_bash_asks,
                            &mut next_bash_ask_corr,
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
                        if let Err(error) = cancel_pending_bash_ask_for_tool_call(
                            &writer_tx,
                            &mut pending_bash_asks,
                            channel,
                            frame.header.corr,
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
                    // Incoming push messages are ignored here. Cancel frames only
                    // stop pending bash elicitation requests; executor-level
                    // cancellation for tool calls that are already running is not
                    // implemented.
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
            Some((order, root_id, frame)) = lossy_rx.recv() => {
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
                // status/progress updates can be merged before reaching subc's
                // shared egress queue. Each lossy frame gets a sequence number
                // before it goes to the channel or overflow buffer, so the
                // combined batch is sorted back into producer order before
                // coalescing drops stale updates for the same key.
                let mut batch = vec![(order, root_id, frame)];
                while let Ok(item) = lossy_rx.try_recv() {
                    batch.push(item);
                }
                batch.extend(lossy_overflow.drain());
                push::process_lossy_push_envelope_batch(
                    &writer_tx,
                    &dispatch_path_metrics,
                    &routes,
                    &root_channels,
                    &completed_tasks,
                    batch,
                );
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
                let root_id = completion.root_id.clone();
                let response = completion.response;
                let response_is_fatal = response_is_fatal_panic(&response);
                if let Some(meta) = live_roots.get_mut(&root_id) {
                    let defer_requeue = pending_binds
                        .values()
                        .any(|pending| pending.bind_root_id == root_id);
                    note_maintenance_completion(
                        meta,
                        completion.requeue_kind,
                        response_is_fatal,
                        defer_requeue,
                    );
                }
                push::clear_stale_bg_wakes_for_empty_sessions(
                    &root_id,
                    &completion.empty_bg_sessions,
                    &bg_sub_by_session,
                    &mut bg_wake_pending,
                    &bg_wake_epoch,
                );
                if response_is_fatal {
                    if let Some(meta) = live_roots.get_mut(&root_id) {
                        meta.maintenance_poisoned = true;
                    }
                    log::warn!(
                        "subc attach: maintenance drain observed a fatal actor; deferring teardown until a route request can receive actor_fatal"
                    );
                }
            }
            _ = tokio::time::sleep_until(next_drain_at) => {
                // Wakes an otherwise-idle loop so the pre-turn drain check
                // above runs on schedule; the drain work itself lives there.
            }
            _ = tokio::time::sleep_until(next_maintenance_at) => {
                // Delay cache-draining maintenance until any already-ready
                // inbound route/control messages and push completions have run,
                // so maintenance does not block the actor from handling the
                // first request that arrives after a route bind is acknowledged.
                let reaped = reap_idle_roots(
                    Instant::now(),
                    &mut live_roots,
                    &pending_binds,
                    &executor,
                );
                if reaped > 0 {
                    log::debug!("subc attach: reaped {reaped} idle root(s)");
                }
                submit_due_maintenance_jobs(
                    &executor,
                    &mut live_roots,
                    &pending_binds,
                    &bg_sub_by_session,
                    &bg_wake_epoch,
                    &maintenance_tx,
                    &dispatch_path_metrics,
                );
                next_maintenance_at = tokio::time::Instant::now() + DRAIN_TICK_PERIOD;
            }
        }
    };

    let mut loop_result = loop_result;
    if !pending_bash_asks.is_empty() {
        let no_routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
        if let Err(error) = settle_all_pending_bash_asks(
            &writer_tx,
            &mut pending_bash_asks,
            &no_routes,
            &mut live_roots,
            &mut route_bash_cancels,
            &shutdown,
            &dispatch_path_metrics,
        )
        .await
        {
            loop_result = loop_result.and(Err(error));
        }
    }

    // The reader task may be parked on `read_frame`; abort it (we are done with
    // the connection) and flush the writer.
    connection_cancel.cancel();
    reader_task.abort();
    drop(writer_tx);
    let writer_result = finish_writer_task(writer_task).await;
    loop_result.and_then(|exit| writer_result.map(|_| exit))
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
                    // A killed daemon surfaces as ConnectionReset (RST) on
                    // Windows where Unix delivers a clean EOF (FIN); a
                    // mid-teardown daemon can also abort the socket. Both mean
                    // "daemon went away", not a wire fault — normalize them to
                    // the clean-close path so module exit behavior matches
                    // across platforms (same class subc-core fixed in d33d9a71).
                    if let subc_transport::FrameIoError::Io(io_error) = &error {
                        if matches!(
                            io_error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) {
                            log::info!(
                                "subc attach: connection reset by daemon; treating as close"
                            );
                            return;
                        }
                    }
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

fn register_actor_for_bind(
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    push_senders: &PushSenders,
    bind_root_id: &ProjectRootId,
    route_channel: u16,
    root_was_live: bool,
) -> bool {
    if executor.actor_registered(bind_root_id) {
        log::debug!(
            "subc attach: reusing actor for route {} root {}",
            route_channel,
            bind_root_id.as_path().display()
        );
        return false;
    }

    if root_was_live {
        log::warn!(
            "subc attach: recreating missing actor for live root {} on route {}",
            bind_root_id.as_path().display(),
            route_channel
        );
    }

    let actor_ctx = Arc::new(AppContext::from_app(
        Arc::clone(shared_app),
        Config::default(),
    ));
    install_bash_compressor(&actor_ctx);
    actor_ctx.set_progress_sender(Some(push::progress_sender_for_root(
        push_senders.clone(),
        bind_root_id.clone(),
    )));
    let inserted = executor.register_actor(bind_root_id.clone(), Arc::clone(&actor_ctx));
    drop(actor_ctx);
    if inserted {
        // Do not insert into live_roots until configure succeeds: live_roots
        // drives maintenance, and a half-configured new actor must not be
        // maintenance-eligible before its route/session identity exists.
        log::debug!(
            "subc attach: registered actor for route {} root {}",
            route_channel,
            bind_root_id.as_path().display()
        );
    } else {
        log::debug!(
            "subc attach: actor appeared while binding route {} root {}; reusing it",
            route_channel,
            bind_root_id.as_path().display()
        );
    }
    inserted
}

fn rollback_pending_bind_actor(
    executor: &Arc<Executor>,
    live_roots: &HashMap<ProjectRootId, RootMeta>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    root_id: &ProjectRootId,
    inserted_new_actor: bool,
) {
    if !inserted_new_actor || live_roots.contains_key(root_id) {
        return;
    }

    if let Some((route, pending)) = pending_binds
        .iter_mut()
        .find(|(_, pending)| &pending.bind_root_id == root_id)
    {
        pending.inserted_new_actor = true;
        log::debug!(
            "subc attach: transferred rollback ownership for root {} to pending route {}",
            root_id.as_path().display(),
            route
        );
        return;
    }

    executor.remove_actor(root_id);
}

fn route_bind_error_code_for_configure_response(response: &Response) -> &'static str {
    match response.data.get("code").and_then(|code| code.as_str()) {
        // Preserve typed configure rejections across the bind boundary: a
        // malformed fed fingerprint means a federation-module bug or
        // fingerprint-format drift, and the fed side matches on the code rather
        // than parsing prose.
        Some("bad_harness_fingerprint") => "bad_harness_fingerprint",
        // Cache-key probe failures are transient (fd pressure, git spawn
        // contention); the client retries the bind rather than treating the
        // root as permanently divergent.
        Some("cache_key_probe_failed") => "cache_key_probe_failed",
        // Actor lifecycle gaps are transient from the daemon/client viewpoint:
        // a fresh bind can create or join a healthy actor, so do not classify
        // them as permanent config divergence.
        Some("actor_not_registered" | "actor_fatal") => "actor_not_ready",
        _ => "config_divergence",
    }
}

fn queue_initial_short_maintenance_after_bind(
    root_id: &ProjectRootId,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
) {
    let Some(meta) = live_roots.get_mut(root_id) else {
        return;
    };
    if meta.maintenance_poisoned || meta.maintenance_pending {
        return;
    }

    meta.maintenance_pending = true;
    meta.maintenance_queued_kinds
        .push_back(MaintenanceDrainKind::Short);
}

#[allow(clippy::too_many_arguments)]
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
    metrics: &Arc<DispatchPathMetrics>,
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
            pending_binds,
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
            pending_binds,
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
    } else {
        None
    };

    if let Some((response, fallback)) = failure {
        rollback_pending_bind_actor(
            executor,
            live_roots,
            pending_binds,
            &completion.bind_root_id,
            inserted_new_actor,
        );
        let message = response_message(response, fallback);
        let fatal = response_is_fatal_panic(response);
        let error_code = route_bind_error_code_for_configure_response(response);
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
    let restore_watcher = live_roots
        .get(&completion.bind_root_id)
        .is_some_and(|meta| meta.idle_artifacts_evicted);
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
    if restore_watcher {
        if let Some(ctx) = executor.actor_context(&completion.bind_root_id) {
            crate::commands::configure::ensure_project_watcher(&ctx);
        }
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
    queue_initial_short_maintenance_after_bind(&completion.bind_root_id, live_roots);
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

async fn expire_overdue_route_binds(
    tx: &mpsc::Sender<Frame>,
    pending_binds: &mut HashMap<RouteChannel, PendingBind>,
    metrics: &DispatchPathMetrics,
) -> Result<(), SubcError> {
    let now = Instant::now();
    let expired: Vec<_> = pending_binds
        .iter()
        .filter_map(|(route, pending)| {
            let age = now.saturating_duration_since(pending.started_at);
            (!pending.deadline_reported && age >= ROUTE_BIND_DEADLINE).then(|| {
                (
                    *route,
                    pending.corr,
                    pending.ver,
                    pending.flags,
                    pending.bind_root_id.clone(),
                    pending.configure_request_id.clone(),
                    age,
                )
            })
        })
        .collect();

    for (route, corr, ver, flags, root_id, configure_request_id, age) in expired {
        if let Some(pending) = pending_binds.get_mut(&route) {
            pending.cancelled = true;
            pending.deadline_reported = true;
        }
        let age_ms = age.as_millis().min(u128::from(u64::MAX)) as u64;
        let deadline_ms = ROUTE_BIND_DEADLINE.as_millis();
        send_route_bind_error_parts(
            tx,
            ver,
            corr,
            flags,
            "actor_not_ready",
            &format!("route bind deadline exceeded after {age_ms}ms (deadline {deadline_ms}ms)"),
            metrics,
        )
        .await?;
        log::warn!(
            "subc attach: route {} bind for root {} exceeded {}ms deadline (configure_request_id={})",
            route,
            root_id.as_path().display(),
            deadline_ms,
            configure_request_id
        );
    }

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
            consumer_capabilities,
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
            // Typed capability DECLARATION (protocol 0.8): the facade stamps it
            // from the MCP host's initialize-advertised capabilities. Absent
            // means no reverse-request capability — flat deny, fail-closed. A
            // consumer over-declaring only earns asks that TTL-deny.
            let consumer_elicitation_capable = consumer_capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.iter().any(|c| c == "elicitation"));
            log::info!(
                "subc attach: route {} harness={} principal={} trust={} elicitation={}",
                route_channel,
                bind_harness,
                principal_label(&principal),
                bind_trust.label(),
                consumer_elicitation_capable
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
                consumer_elicitation_capable,
            };
            let configure_session = route_identity.session.clone();
            let root_was_live = live_roots.contains_key(&bind_root_id);
            let inserted_new_actor = register_actor_for_bind(
                shared_app,
                executor,
                push_senders,
                &bind_root_id,
                route_channel,
                root_was_live,
            );

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
                    deadline_reported: false,
                    corr: frame.header.corr,
                    ver: frame.header.ver,
                    flags: frame.header.flags,
                },
            );
            if let Some(meta) = live_roots.get_mut(&bind_root_id) {
                meta.maintenance_queued_kinds.clear();
                meta.maintenance_pending = meta.maintenance_jobs_in_flight > 0;
            }
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
                // Send the route-bind acknowledgment as soon as configure succeeds.
                // Installing completed search or callgraph builds only refreshes cached
                // read data, so a later maintenance pass can do it without delaying the
                // daemon's confirmation that the route is usable.
                let completion = RouteBindCompletion {
                    route_channel: completion_route_channel,
                    identity: completion_identity,
                    bind_root_id: completion_root,
                    inserted_new_actor,
                    configure_response,
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
    pending_bash_asks: &mut HashMap<ReverseCorrKey, PendingBashAsk>,
    next_bash_ask_corr: &mut u64,
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
    let restore_watcher = live_roots
        .get(&identity.root)
        .is_some_and(|meta| meta.idle_artifacts_evicted);
    if let Some(meta) = live_roots.get_mut(&identity.root) {
        meta.touch();
    }
    if restore_watcher {
        if let Some(ctx) = executor.actor_context(&identity.root) {
            crate::commands::configure::ensure_project_watcher(&ctx);
        }
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

    if matches!(bind_trust, BindTrust::Untrusted)
        && is_bash_family_tool(&bare_name)
        && (bare_name != "bash" || !identity.consumer_elicitation_capable)
    {
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
        if matches!(bind_trust, BindTrust::Untrusted) {
            let plan = match bash::prepare_bash_elicitation_plan(
                &call.arguments,
                identity.project_root.as_path(),
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let response = Response::error(request_id.clone(), error.code, error.message);
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
                    return send_reliable_writer_frame(
                        tx,
                        metrics,
                        response_frame,
                        "tool response",
                    )
                    .await;
                }
            };

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

            let reverse_corr =
                allocate_reverse_corr(pending_bash_asks, route_id, next_bash_ask_corr);
            let ask_frame = build_bash_elicitation_request_frame(
                frame.header.ver,
                frame.header.channel,
                reverse_corr,
                frame.header.flags,
                &plan.command,
                &plan.asks,
            )?;
            pending_bash_asks.insert(
                ReverseCorrKey {
                    route: route_id,
                    corr: reverse_corr,
                },
                PendingBashAsk {
                    route_channel: frame.header.channel,
                    tool_corr: frame.header.corr,
                    tool_flags: frame.header.flags,
                    tool_ver: frame.header.ver,
                    root: identity.root,
                    project_root: identity.project_root,
                    session_id: identity.session,
                    request_id,
                    arguments: call.arguments,
                    format_context,
                    cancel,
                    grants: plan.grants,
                    expires_at: Instant::now() + bash_elicitation_timeout(),
                },
            );
            return send_reliable_writer_frame(tx, metrics, ask_frame, "bash elicitation request")
                .await;
        }

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
            None,
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

fn submit_maintenance_job(
    executor: &Arc<Executor>,
    root_id: ProjectRootId,
    kind: MaintenanceDrainKind,
    bg_sessions_to_check: Vec<(String, u64)>,
    completion_tx: &mpsc::Sender<MaintenanceCompletion>,
    metrics: &Arc<DispatchPathMetrics>,
) {
    let request_id = format!(
        "subc-maintenance-drain-{}-{}",
        kind.label(),
        root_id.as_path().to_string_lossy()
    );
    let response_id = request_id.clone();
    let completion_root_id = root_id.clone();
    let (outcome_tx, outcome_rx) = oneshot::channel::<MaintenanceJobOutcome>();
    let rx = executor.submit_maintenance_async(
        root_id,
        Lane::Mutating,
        request_id.clone(),
        Box::new(move |ctx| {
            let outcome = match kind {
                MaintenanceDrainKind::Watcher => {
                    let drained = runtime_drain::drain_watcher_events_bounded(
                        ctx,
                        runtime_drain::WATCHER_EVENT_DRAIN_BATCH_CAP,
                    );
                    MaintenanceJobOutcome {
                        empty_bg_sessions: Vec::new(),
                        requeue_kind: drained.has_more.then_some(kind),
                    }
                }
                MaintenanceDrainKind::Lsp => {
                    let drained = runtime_drain::drain_lsp_events_bounded(
                        ctx,
                        runtime_drain::LSP_EVENT_DRAIN_BATCH_CAP,
                    );
                    MaintenanceJobOutcome {
                        empty_bg_sessions: Vec::new(),
                        requeue_kind: drained.has_more.then_some(kind),
                    }
                }
                MaintenanceDrainKind::Short => {
                    runtime_drain::drain_configure_warning_events(ctx);
                    runtime_drain::drain_search_index_events(ctx);
                    runtime_drain::drain_callgraph_store_events(ctx);
                    runtime_drain::drain_semantic_index_events(ctx);
                    runtime_drain::drain_semantic_refresh_events(ctx);
                    runtime_drain::drain_inspect_events(ctx);
                    let empty_bg_sessions = bg_sessions_to_check
                        .into_iter()
                        .filter(|(session, _)| {
                            !ctx.bash_background()
                                .has_completions_for_session(Some(session.as_str()))
                        })
                        .collect();
                    MaintenanceJobOutcome {
                        empty_bg_sessions,
                        requeue_kind: None,
                    }
                }
            };
            let requeued = outcome.requeue_kind.is_some();
            let _ = outcome_tx.send(outcome);
            Response::success(
                response_id,
                json!({ "drained": true, "kind": kind.label(), "requeued": requeued }),
            )
        }),
    );
    let completion_tx = completion_tx.clone();
    let completion_metrics = Arc::clone(metrics);
    tokio::spawn(async move {
        let _response_task = ResponseTaskGuard::new(&completion_metrics);
        let response = await_executor_response(rx, request_id).await;
        let outcome = outcome_rx.await.unwrap_or_default();
        let _ = send_counted_channel(
            &completion_tx,
            &completion_metrics.maintenance_queued,
            MaintenanceCompletion {
                root_id: completion_root_id,
                response,
                empty_bg_sessions: outcome.empty_bg_sessions,
                requeue_kind: outcome.requeue_kind,
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
            consumer_elicitation_capable: false,
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

    fn actor_ctx_with_dirty_search_index(
        root: &Path,
        storage: &Path,
        file_name: &str,
        old_contents: &str,
        new_contents: &str,
    ) -> (Arc<AppContext>, PathBuf, PathBuf) {
        let file = root.join(file_name);
        std::fs::write(&file, old_contents).expect("write source");
        let canonical_root = std::fs::canonicalize(root).expect("canonical root");
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(canonical_root.clone());

        let cache_dir = crate::search_index::resolve_cache_dir(&canonical_root, Some(storage));
        let mut index = crate::search_index::SearchIndex::build(&canonical_root);
        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref());

        std::fs::write(&file, new_contents).expect("edit source");
        index.update_file(&file);
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        (ctx, canonical_root, cache_dir)
    }

    #[test]
    fn graceful_shutdown_flushes_every_actor_search_index() {
        let storage = tempfile::tempdir().expect("storage tempdir");
        let (root1_dir, root1) = test_root("shutdown-flush-root-1");
        let (root2_dir, root2) = test_root("shutdown-flush-root-2");
        let (ctx1, canonical_root1, cache_dir1) = actor_ctx_with_dirty_search_index(
            root1_dir.path(),
            storage.path(),
            "alpha.txt",
            "old actor one token\n",
            "new actor one token\n",
        );
        let (ctx2, canonical_root2, cache_dir2) = actor_ctx_with_dirty_search_index(
            root2_dir.path(),
            storage.path(),
            "beta.txt",
            "old actor two token\n",
            "new actor two token\n",
        );

        let executor = Executor::new();
        assert!(executor.register_actor(root1.clone(), Arc::clone(&ctx1)));
        assert!(executor.register_actor(root2.clone(), Arc::clone(&ctx2)));

        flush_actor_search_indexes_on_graceful_shutdown(&executor.actor_contexts());

        let mut restored1 =
            crate::search_index::SearchIndex::read_from_disk(&cache_dir1, &canonical_root1)
                .expect("load flushed root one index");
        restored1.ready = true;
        assert_eq!(
            restored1
                .grep("new actor one token", true, &[], &[], &canonical_root1, 10)
                .matches
                .len(),
            1,
            "graceful subc shutdown should flush the first root's trigram delta"
        );

        let mut restored2 =
            crate::search_index::SearchIndex::read_from_disk(&cache_dir2, &canonical_root2)
                .expect("load flushed root two index");
        restored2.ready = true;
        assert_eq!(
            restored2
                .grep("new actor two token", true, &[], &[], &canonical_root2, 10)
                .matches
                .len(),
            1,
            "graceful subc shutdown should flush every registered root"
        );
    }

    #[test]
    fn idle_root_reaper_closes_artifacts_and_stops_watcher() {
        let (root_dir, root) = test_root("idle-root-reaper");
        let storage = tempfile::tempdir().expect("storage tempdir");
        std::fs::write(
            root_dir.path().join("main.rs"),
            "fn entry() { leaf(); }\nfn leaf() {}\n",
        )
        .expect("source file");
        let canonical_root = std::fs::canonicalize(root_dir.path()).expect("canonical root");
        let app = App::default_shared();
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(canonical_root.clone()),
                storage_dir: Some(storage.path().to_path_buf()),
                callgraph_store: true,
                search_index: true,
                ..Config::default()
            },
        ));
        ctx.set_canonical_cache_root(canonical_root.clone());
        assert!(ctx
            .ensure_callgraph_store()
            .expect("build callgraph store")
            .is_some());

        let cache_dir =
            crate::search_index::resolve_cache_dir(&canonical_root, Some(storage.path()));
        let mut index = crate::search_index::SearchIndex::build(&canonical_root);
        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref());
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);

        let (dispatch_tx, dispatch_rx) = crate::watcher_filter::watcher_dispatch_channel();
        let _dispatch_tx = dispatch_tx;
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let join = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
        });
        ctx.install_watcher_runtime(
            dispatch_rx,
            crate::watcher_filter::WatcherThreadHandle::new(shutdown, join),
        );
        assert_eq!(ctx.watcher_registry_count(), 1);

        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let mut live_roots = HashMap::new();
        let mut meta = RootMeta::new(Instant::now());
        meta.last_touched = Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        live_roots.insert(root.clone(), meta);

        assert_eq!(
            reap_idle_roots(Instant::now(), &mut live_roots, &HashMap::new(), &executor),
            1
        );
        assert!(ctx.search_index().read().unwrap().is_none());
        assert_eq!(ctx.watcher_registry_count(), 0);
        assert!(
            crate::search_index::SearchIndex::read_from_disk(&cache_dir, &canonical_root).is_some()
        );
        assert!(ctx
            .ensure_callgraph_store()
            .expect("reopen callgraph store")
            .is_some());
        assert!(live_roots[&root].idle_artifacts_evicted);
    }

    #[test]
    fn due_maintenance_jobs_skip_poisoned_roots() {
        let (_healthy_dir, healthy_root) = test_root("maintenance-healthy");
        let (_poisoned_dir, poisoned_root) = test_root("maintenance-poisoned");
        let mut live_roots = HashMap::new();
        live_roots.insert(healthy_root.clone(), RootMeta::new(Instant::now()));
        let mut poisoned_meta = RootMeta::new(Instant::now());
        poisoned_meta.maintenance_poisoned = true;
        live_roots.insert(poisoned_root.clone(), poisoned_meta);

        let (due, deferred) =
            due_maintenance_jobs(&mut live_roots, MAINTENANCE_SUBMIT_BUDGET, &HashSet::new());

        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(root, _)| root == &healthy_root));
        assert!(!deferred);
        assert!(live_roots[&healthy_root].maintenance_pending);
        assert_eq!(
            live_roots[&healthy_root].maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT
        );
        assert!(!live_roots[&poisoned_root].maintenance_pending);
    }

    #[test]
    fn initial_short_maintenance_is_queued_without_starting_actor_work() {
        let (_dir, root) = test_root("maintenance-initial-short");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));

        queue_initial_short_maintenance_after_bind(&root, &mut live_roots);

        let meta = live_roots.get(&root).expect("root metadata");
        assert!(meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
        assert_eq!(
            meta.maintenance_queued_kinds
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![MaintenanceDrainKind::Short]
        );

        let (due, deferred) =
            due_maintenance_jobs(&mut live_roots, MAINTENANCE_SUBMIT_BUDGET, &HashSet::new());

        assert_eq!(due, vec![(root.clone(), MaintenanceDrainKind::Short)]);
        assert!(!deferred);
        assert_eq!(live_roots[&root].maintenance_jobs_in_flight, 1);
        assert!(live_roots[&root].maintenance_queued_kinds.is_empty());
    }

    #[test]
    fn due_maintenance_jobs_defers_unsubmitted_roots_without_marking_pending() {
        let mut live_roots = HashMap::new();
        let mut root_ids = Vec::new();
        let mut _dirs = Vec::new();
        for index in 0..4 {
            let (dir, root_id) = test_root(&format!("maintenance-budget-{index}"));
            live_roots.insert(root_id.clone(), RootMeta::new(Instant::now()));
            root_ids.push(root_id);
            _dirs.push(dir);
        }

        let small_budget = INITIAL_MAINTENANCE_JOB_COUNT + 1;
        let (first_due, first_deferred) =
            due_maintenance_jobs(&mut live_roots, small_budget, &HashSet::new());

        assert_eq!(first_due.len(), small_budget);
        assert!(first_deferred);
        let first_due_set: HashSet<_> = first_due.into_iter().map(|(root, _)| root).collect();
        assert!(first_due_set
            .iter()
            .all(|root| live_roots[root].maintenance_pending));
        assert!(first_due_set
            .iter()
            .any(|root| !live_roots[root].maintenance_queued_kinds.is_empty()));

        let all_roots: HashSet<_> = root_ids.into_iter().collect();
        let deferred_roots: HashSet<_> = all_roots.difference(&first_due_set).cloned().collect();
        assert!(deferred_roots
            .iter()
            .all(|root| !live_roots[root].maintenance_pending));
    }

    #[test]
    fn due_maintenance_jobs_defers_pending_bind_roots() {
        let (_bind_dir, bind_root) = test_root("maintenance-pending-bind");
        let (_healthy_dir, healthy_root) = test_root("maintenance-no-bind");
        let mut live_roots = HashMap::new();
        live_roots.insert(bind_root.clone(), RootMeta::new(Instant::now()));
        live_roots.insert(healthy_root.clone(), RootMeta::new(Instant::now()));
        let pending_bind_roots = HashSet::from([bind_root.clone()]);

        let (due, deferred) =
            due_maintenance_jobs(&mut live_roots, usize::MAX, &pending_bind_roots);

        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(root, _)| root == &healthy_root));
        assert!(!deferred);
        assert!(!live_roots[&bind_root].maintenance_pending);
        assert!(live_roots[&bind_root].maintenance_queued_kinds.is_empty());
    }

    #[test]
    fn maintenance_pending_survives_requeue_and_clears_after_final_batch() {
        let (_dir, root) = test_root("maintenance-requeue");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, deferred) = due_maintenance_jobs(&mut live_roots, usize::MAX, &HashSet::new());
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);
        assert!(due.iter().all(|(due_root, _)| due_root == &root));
        assert!(!deferred);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), false, false);
        assert!(meta.maintenance_pending);
        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT - 1
        );
        assert_eq!(meta.maintenance_queued_kinds.len(), 1);

        let (requeued, deferred) = due_maintenance_jobs(&mut live_roots, 1, &HashSet::new());
        assert_eq!(
            requeued,
            vec![(root.clone(), MaintenanceDrainKind::Watcher)]
        );
        assert!(!deferred);
        let meta = live_roots.get_mut(&root).unwrap();
        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT
        );
        assert!(meta.maintenance_queued_kinds.is_empty());

        for _ in 0..INITIAL_MAINTENANCE_JOB_COUNT {
            note_maintenance_completion(meta, None, false, false);
        }
        assert!(!meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
    }

    #[test]
    fn maintenance_requeue_drops_while_bind_is_pending() {
        let (_dir, root) = test_root("maintenance-bind-requeue");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, _) = due_maintenance_jobs(&mut live_roots, usize::MAX, &HashSet::new());
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), false, true);

        assert_eq!(
            meta.maintenance_jobs_in_flight,
            INITIAL_MAINTENANCE_JOB_COUNT - 1
        );
        assert!(meta.maintenance_queued_kinds.is_empty());
        assert!(meta.maintenance_pending);
    }

    #[test]
    fn maintenance_pending_clears_and_poison_stops_requeue_after_fatal() {
        let (_dir, root) = test_root("maintenance-fatal");
        let mut live_roots = HashMap::new();
        live_roots.insert(root.clone(), RootMeta::new(Instant::now()));
        let (due, _) = due_maintenance_jobs(&mut live_roots, usize::MAX, &HashSet::new());
        assert_eq!(due.len(), INITIAL_MAINTENANCE_JOB_COUNT);

        let meta = live_roots.get_mut(&root).unwrap();
        note_maintenance_completion(meta, Some(MaintenanceDrainKind::Watcher), true, false);
        assert!(meta.maintenance_poisoned);
        assert!(meta.maintenance_queued_kinds.is_empty());

        for _ in 1..INITIAL_MAINTENANCE_JOB_COUNT {
            note_maintenance_completion(meta, None, false, false);
        }
        assert!(!meta.maintenance_pending);
        assert_eq!(meta.maintenance_jobs_in_flight, 0);
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
