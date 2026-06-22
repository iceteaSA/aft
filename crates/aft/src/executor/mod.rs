mod single_flight;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::{Receiver, RecvError, RecvTimeoutError, Sender};
use parking_lot::{Mutex, RwLock};
use tokio::sync::oneshot;

use crate::{context::AppContext, path_identity::ProjectRootId, protocol::Response};

pub use single_flight::SingleFlight;

const JOB_COST: isize = 1;

/// Scheduler lane for command-handler execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// Pure read-only work. Runs under the actor epoch read gate and is capped
    /// per actor.
    PureRead,
    /// LSP/status work. Serialized per actor by scheduler admission while still
    /// using the shared epoch read gate.
    SerialLspStatus,
    /// Heavy lazy initialization. The scheduler acquires a process-wide heavy
    /// permit before dispatch; the worker runs the build outside the epoch and
    /// then takes a short write gate for the install point.
    HeavyInit,
    /// Mutating work. Becomes a writer barrier at the actor queue head, drains
    /// in-flight reads, and runs under the actor epoch write gate.
    Mutating,
}

pub type ExecutorJob = Box<dyn FnOnce(&AppContext) -> Response + Send + 'static>;

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub pool_size: usize,
    pub read_cap: usize,
    pub actor_cap: usize,
    pub heavy_permits: usize,
    pub drr_quantum: isize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        let available = thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2);
        let pool_size = available.saturating_sub(1).clamp(2, 8);
        let actor_cap = pool_size.saturating_sub(1).clamp(1, 4);
        let read_cap = actor_cap.clamp(1, 4);
        let heavy_permits = pool_size.saturating_sub(1).clamp(2, 3);

        Self {
            pool_size,
            read_cap,
            actor_cap,
            heavy_permits,
            drr_quantum: 1,
        }
    }
}

#[derive(Debug, Clone)]
struct EffectiveConfig {
    pool_size: usize,
    read_cap: usize,
    actor_cap: usize,
    heavy_permits: usize,
    drr_quantum: isize,
    deficit_cap: isize,
}

impl ExecutorConfig {
    fn effective(&self) -> EffectiveConfig {
        let pool_size = self.pool_size.clamp(2, 8);
        let max_actor_cap = pool_size.saturating_sub(1).max(1);
        let actor_cap = self.actor_cap.max(1).min(max_actor_cap);
        let read_cap = self.read_cap.max(1).min(actor_cap).min(4);
        let heavy_permits = self.heavy_permits.clamp(2, 3);
        let drr_quantum = self.drr_quantum.max(1);
        let deficit_cap = (actor_cap.max(1) as isize) * 4;

        EffectiveConfig {
            pool_size,
            read_cap,
            actor_cap,
            heavy_permits,
            drr_quantum,
            deficit_cap,
        }
    }
}

/// Synchronous completion handle used by the Phase 2a executor tests and the
/// future standalone bridge.
pub struct CompletionHandle {
    rx: Receiver<Response>,
}

impl CompletionHandle {
    pub fn recv(self) -> Result<Response, RecvError> {
        self.rx.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Response, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    pub fn into_receiver(self) -> Receiver<Response> {
        self.rx
    }
}

/// Concurrent scheduler-dispatch executor.
pub struct Executor {
    inner: Arc<ExecutorInner>,
}

impl Executor {
    pub fn new() -> Self {
        Self::with_config(ExecutorConfig::default())
    }

    pub fn with_config(config: ExecutorConfig) -> Self {
        let effective = config.effective();
        let state = Arc::new(Mutex::new(SchedulerState::new(effective.clone())));
        let heavy = Arc::new(HeavySemaphore::new(effective.heavy_permits));
        let nonrunnable_dispatches = Arc::new(AtomicUsize::new(0));
        let (run_tx, run_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        let scheduler_state = Arc::clone(&state);
        let scheduler_heavy = Arc::clone(&heavy);
        let scheduler_violations = Arc::clone(&nonrunnable_dispatches);
        let scheduler_handle = thread::Builder::new()
            .name("aft-executor-scheduler".to_string())
            .spawn(move || {
                scheduler_loop(
                    scheduler_state,
                    scheduler_heavy,
                    run_tx,
                    event_rx,
                    scheduler_violations,
                );
            })
            .expect("spawn AFT executor scheduler");

        let mut worker_handles = Vec::with_capacity(effective.pool_size);
        for worker_id in 0..effective.pool_size {
            let worker_rx = run_rx.clone();
            let worker_events = event_tx.clone();
            let handle = thread::Builder::new()
                .name(format!("aft-executor-worker-{worker_id}"))
                .spawn(move || worker_loop(worker_rx, worker_events))
                .expect("spawn AFT executor worker");
            worker_handles.push(handle);
        }

        Self {
            inner: Arc::new(ExecutorInner {
                state,
                event_tx,
                scheduler_handle: Mutex::new(Some(scheduler_handle)),
                worker_handles: Mutex::new(worker_handles),
                config: effective,
                nonrunnable_dispatches,
            }),
        }
    }

    /// Register an actor if one is not already present.
    ///
    /// Existing actors keep their current context and scheduler state; Phase 4
    /// subc routing reuses them and reconfigures through the Mutating lane
    /// rather than replacing the per-root [`AppContext`]. Returns `true` when a
    /// new actor was inserted.
    pub fn register_actor(&self, root_id: ProjectRootId, ctx: Arc<AppContext>) -> bool {
        let inserted = {
            let mut state = self.inner.state.lock();
            if state.actors.contains_key(&root_id) {
                false
            } else {
                state.actor_order.push(root_id.clone());
                state.actors.insert(root_id, ActorState::new(ctx));
                true
            }
        };
        self.wake_scheduler();
        inserted
    }

    /// Remove an actor from scheduler state.
    ///
    /// This is intentionally minimal: Phase 4a uses it only for a just-created
    /// RouteBind actor whose configure failed before any route was installed, so
    /// there is no in-flight work to quiesce. The removed [`AppContext`] is
    /// dropped after releasing the scheduler lock so watcher/LSP teardown never
    /// runs under that mutex.
    pub fn remove_actor(&self, root_id: &ProjectRootId) {
        let removed = {
            let mut state = self.inner.state.lock();
            state.actor_order.retain(|actor_root| actor_root != root_id);
            state.actors.remove(root_id)
        };
        drop(removed);
        self.wake_scheduler();
    }

    /// Snapshot the registered actor contexts.
    ///
    /// The returned [`Arc`]s keep contexts alive after the scheduler lock is
    /// released, so callers can run teardown without holding executor state.
    pub fn actor_contexts(&self) -> Vec<Arc<AppContext>> {
        let state = self.inner.state.lock();
        state
            .actors
            .values()
            .map(|actor_state| Arc::clone(&actor_state.ctx))
            .collect()
    }

    pub fn submit(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> CompletionHandle {
        let (completion_tx, completion_rx) = crossbeam_channel::bounded(1);
        self.submit_with_completion(
            root_id,
            lane,
            request_id,
            job,
            CompletionSender::Sync(completion_tx),
        );
        CompletionHandle { rx: completion_rx }
    }

    pub fn submit_async(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
    ) -> oneshot::Receiver<Response> {
        let (completion_tx, completion_rx) = oneshot::channel();
        self.submit_with_completion(
            root_id,
            lane,
            request_id,
            job,
            CompletionSender::Async(completion_tx),
        );
        completion_rx
    }

    fn submit_with_completion(
        &self,
        root_id: ProjectRootId,
        lane: Lane,
        request_id: String,
        job: ExecutorJob,
        completion: CompletionSender,
    ) {
        let command = lane_command(lane);
        let mut job = Some(job);
        let mut completion = Some(completion);

        let response = {
            let mut state = self.inner.state.lock();
            match state.actors.get_mut(&root_id) {
                Some(actor) if actor.fatal => Some(actor_fatal_response(request_id.clone())),
                Some(actor) => {
                    actor.push_job(
                        lane,
                        QueuedJob {
                            job: job.take().expect("executor job already queued"),
                            completion: completion
                                .take()
                                .expect("executor completion already queued"),
                            request_id: request_id.clone(),
                            command,
                        },
                    );
                    None
                }
                None => Some(Response::error(
                    request_id.clone(),
                    "actor_not_registered",
                    "executor actor is not registered",
                )),
            }
        };

        if let Some(response) = response {
            if let Some(completion) = completion {
                completion.send(response);
            }
            return;
        }

        self.wake_scheduler();
    }

    pub fn pool_size(&self) -> usize {
        self.inner.config.pool_size
    }

    pub fn actor_cap(&self) -> usize {
        self.inner.config.actor_cap
    }

    pub fn read_cap(&self) -> usize {
        self.inner.config.read_cap
    }

    pub fn heavy_permits(&self) -> usize {
        self.inner.config.heavy_permits
    }

    pub fn nonrunnable_dispatch_count(&self) -> usize {
        self.inner.nonrunnable_dispatches.load(Ordering::Acquire)
    }

    pub fn actor_is_fatal(&self, root_id: &ProjectRootId) -> bool {
        self.inner
            .state
            .lock()
            .actors
            .get(root_id)
            .map(|actor| actor.fatal)
            .unwrap_or(false)
    }

    fn wake_scheduler(&self) {
        let _ = self.inner.event_tx.send(SchedulerEvent::Wake);
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

struct ExecutorInner {
    state: Arc<Mutex<SchedulerState>>,
    event_tx: Sender<SchedulerEvent>,
    scheduler_handle: Mutex<Option<JoinHandle<()>>>,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
    config: EffectiveConfig,
    nonrunnable_dispatches: Arc<AtomicUsize>,
}

impl Drop for ExecutorInner {
    fn drop(&mut self) {
        let _ = self.event_tx.send(SchedulerEvent::Shutdown);

        if let Some(handle) = self.scheduler_handle.lock().take() {
            let _ = handle.join();
        }

        let mut workers = self.worker_handles.lock();
        for handle in workers.drain(..) {
            let _ = handle.join();
        }
    }
}

struct SchedulerState {
    actors: HashMap<ProjectRootId, ActorState>,
    actor_order: Vec<ProjectRootId>,
    cursor: usize,
    idle_workers: usize,
    config: EffectiveConfig,
}

impl SchedulerState {
    fn new(config: EffectiveConfig) -> Self {
        Self {
            actors: HashMap::new(),
            actor_order: Vec::new(),
            cursor: 0,
            idle_workers: config.pool_size,
            config,
        }
    }
}

struct ActorState {
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
    read_inflight: usize,
    lsp_inflight: bool,
    actor_total_inflight: usize,
    writer_pending: bool,
    deficit: isize,
    order: VecDeque<Lane>,
    pure_reads: VecDeque<QueuedJob>,
    lsp_status: VecDeque<QueuedJob>,
    heavy_init: VecDeque<QueuedJob>,
    mutating: VecDeque<QueuedJob>,
    fatal: bool,
}

impl ActorState {
    fn new(ctx: Arc<AppContext>) -> Self {
        Self {
            ctx,
            epoch: Arc::new(RwLock::new(())),
            read_inflight: 0,
            lsp_inflight: false,
            actor_total_inflight: 0,
            writer_pending: false,
            deficit: 0,
            order: VecDeque::new(),
            pure_reads: VecDeque::new(),
            lsp_status: VecDeque::new(),
            heavy_init: VecDeque::new(),
            mutating: VecDeque::new(),
            fatal: false,
        }
    }

    fn push_job(&mut self, lane: Lane, job: QueuedJob) {
        self.order.push_back(lane);
        self.queue_mut(lane).push_back(job);
    }

    fn has_queued_jobs(&self) -> bool {
        !self.order.is_empty()
    }

    fn pop_front_job(&mut self, lane: Lane) -> Option<QueuedJob> {
        let order_lane = self.order.pop_front()?;
        debug_assert_eq!(order_lane, lane);
        self.queue_mut(lane).pop_front()
    }

    fn queue_mut(&mut self, lane: Lane) -> &mut VecDeque<QueuedJob> {
        match lane {
            Lane::PureRead => &mut self.pure_reads,
            Lane::SerialLspStatus => &mut self.lsp_status,
            Lane::HeavyInit => &mut self.heavy_init,
            Lane::Mutating => &mut self.mutating,
        }
    }

    fn fail_queued_jobs(&mut self) {
        self.order.clear();
        fail_queued_job_queue(&mut self.pure_reads);
        fail_queued_job_queue(&mut self.lsp_status);
        fail_queued_job_queue(&mut self.heavy_init);
        fail_queued_job_queue(&mut self.mutating);
    }
}

struct QueuedJob {
    job: ExecutorJob,
    completion: CompletionSender,
    request_id: String,
    command: String,
}

fn fail_queued_job_queue(queue: &mut VecDeque<QueuedJob>) {
    for queued in queue.drain(..) {
        queued
            .completion
            .send(actor_fatal_response(queued.request_id));
    }
}

fn lane_command(lane: Lane) -> String {
    format!("executor::{lane:?}")
}

fn actor_fatal_response(request_id: impl Into<String>) -> Response {
    Response::error(
        request_id,
        "actor_fatal",
        "executor actor is fatal after a mutating job panic",
    )
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn panic_response(
    request_id: impl Into<String>,
    command: &str,
    payload: &(dyn std::any::Any + Send),
) -> Response {
    let panic_message = panic_payload_message(payload);
    Response::error(
        request_id,
        "internal_error",
        format!("command '{command}' panicked: {panic_message}"),
    )
}

enum CompletionSender {
    Sync(Sender<Response>),
    Async(oneshot::Sender<Response>),
}

impl CompletionSender {
    fn send(self, response: Response) {
        match self {
            Self::Sync(tx) => {
                let _ = tx.send(response);
            }
            Self::Async(tx) => {
                let _ = tx.send(response);
            }
        }
    }
}

struct RunJob {
    root_id: ProjectRootId,
    lane: Lane,
    ctx: Arc<AppContext>,
    epoch: Arc<RwLock<()>>,
    job: ExecutorJob,
    completion: Option<CompletionSender>,
    request_id: String,
    command: String,
    heavy_permit: Option<HeavyPermit>,
}

struct CompletionEvent {
    root_id: ProjectRootId,
    lane: Lane,
    heavy_permit: Option<HeavyPermit>,
    panicked: bool,
}

enum SchedulerEvent {
    Wake,
    Completed(CompletionEvent),
    Shutdown,
}

fn scheduler_loop(
    state: Arc<Mutex<SchedulerState>>,
    heavy: Arc<HeavySemaphore>,
    run_tx: Sender<RunJob>,
    event_rx: Receiver<SchedulerEvent>,
    nonrunnable_dispatches: Arc<AtomicUsize>,
) {
    while let Ok(event) = event_rx.recv() {
        let mut shutdown = false;
        {
            let mut state = state.lock();
            shutdown |= process_scheduler_event(event, &mut state);
            while !shutdown {
                match event_rx.try_recv() {
                    Ok(event) => shutdown |= process_scheduler_event(event, &mut state),
                    Err(_) => break,
                }
            }

            if !shutdown {
                dispatch_runnable(&mut state, &heavy, &run_tx, &nonrunnable_dispatches);
            }
        }

        if shutdown {
            break;
        }
    }
}

fn process_scheduler_event(event: SchedulerEvent, state: &mut SchedulerState) -> bool {
    match event {
        SchedulerEvent::Wake => false,
        SchedulerEvent::Completed(event) => {
            complete_job(state, event);
            false
        }
        SchedulerEvent::Shutdown => true,
    }
}

fn complete_job(state: &mut SchedulerState, event: CompletionEvent) {
    let CompletionEvent {
        root_id,
        lane,
        heavy_permit,
        panicked,
    } = event;

    if let Some(actor) = state.actors.get_mut(&root_id) {
        actor.actor_total_inflight = actor.actor_total_inflight.saturating_sub(1);
        match lane {
            Lane::PureRead => {
                actor.read_inflight = actor.read_inflight.saturating_sub(1);
            }
            Lane::SerialLspStatus => {
                actor.lsp_inflight = false;
            }
            Lane::HeavyInit => {}
            Lane::Mutating => {
                actor.writer_pending = false;
            }
        }

        if panicked && lane == Lane::Mutating {
            actor.fatal = true;
            actor.fail_queued_jobs();
        }
    }

    drop(heavy_permit);
    state.idle_workers += 1;
}

fn dispatch_runnable(
    state: &mut SchedulerState,
    heavy: &Arc<HeavySemaphore>,
    run_tx: &Sender<RunJob>,
    nonrunnable_dispatches: &AtomicUsize,
) {
    while state.idle_workers > 0 && !state.actor_order.is_empty() {
        let actor_count = state.actor_order.len();
        let mut made_progress = false;

        for _ in 0..actor_count {
            if state.idle_workers == 0 || state.actor_order.is_empty() {
                break;
            }

            if state.cursor >= state.actor_order.len() {
                state.cursor = 0;
            }
            let root_id = state.actor_order[state.cursor].clone();
            state.cursor = (state.cursor + 1) % state.actor_order.len();

            let run_job = {
                let Some(actor) = state.actors.get_mut(&root_id) else {
                    continue;
                };

                if actor.fatal {
                    actor.fail_queued_jobs();
                    actor.deficit = 0;
                    continue;
                }

                if !actor.has_queued_jobs() {
                    actor.deficit = 0;
                    continue;
                }

                actor.deficit =
                    (actor.deficit + state.config.drr_quantum).min(state.config.deficit_cap);
                if actor.deficit < JOB_COST {
                    continue;
                }

                try_admit_actor(&root_id, actor, &state.config, heavy)
            };

            if let Some(run_job) = run_job {
                state.idle_workers -= 1;
                made_progress = true;
                if run_tx.send(run_job).is_err() {
                    nonrunnable_dispatches.fetch_add(1, Ordering::AcqRel);
                    return;
                }
            }
        }

        if !made_progress {
            break;
        }
    }
}

fn try_admit_actor(
    root_id: &ProjectRootId,
    actor: &mut ActorState,
    config: &EffectiveConfig,
    heavy: &Arc<HeavySemaphore>,
) -> Option<RunJob> {
    let lane = *actor.order.front()?;
    let mut heavy_permit = None;

    let runnable = match lane {
        Lane::PureRead => {
            !actor.writer_pending
                && actor.read_inflight < config.read_cap
                && actor.actor_total_inflight < config.actor_cap
        }
        Lane::SerialLspStatus => {
            !actor.writer_pending
                && !actor.lsp_inflight
                && actor.actor_total_inflight < config.actor_cap
        }
        Lane::HeavyInit => {
            if actor.actor_total_inflight >= config.actor_cap {
                false
            } else if let Some(permit) = heavy.try_acquire() {
                heavy_permit = Some(permit);
                true
            } else {
                false
            }
        }
        Lane::Mutating => {
            actor.writer_pending = true;
            actor.read_inflight == 0 && actor.actor_total_inflight < config.actor_cap
        }
    };

    if !runnable {
        return None;
    }

    let queued = actor.pop_front_job(lane)?;
    actor.deficit -= JOB_COST;
    match lane {
        Lane::PureRead => {
            actor.read_inflight += 1;
            actor.actor_total_inflight += 1;
        }
        Lane::SerialLspStatus => {
            actor.lsp_inflight = true;
            actor.actor_total_inflight += 1;
        }
        Lane::HeavyInit => {
            actor.actor_total_inflight += 1;
        }
        Lane::Mutating => {
            actor.actor_total_inflight += 1;
        }
    }

    Some(RunJob {
        root_id: root_id.clone(),
        lane,
        ctx: Arc::clone(&actor.ctx),
        epoch: Arc::clone(&actor.epoch),
        job: queued.job,
        completion: Some(queued.completion),
        request_id: queued.request_id,
        command: queued.command,
        heavy_permit,
    })
}

fn worker_loop(run_rx: Receiver<RunJob>, event_tx: Sender<SchedulerEvent>) {
    while let Ok(mut run_job) = run_rx.recv() {
        let response =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_lane_job(&mut run_job)));
        let panicked = response.is_err();
        let response = match response {
            Ok(response) => response,
            Err(payload) => panic_response(
                run_job.request_id.clone(),
                &run_job.command,
                payload.as_ref(),
            ),
        };

        if let Some(completion) = run_job.completion.take() {
            completion.send(response);
        }
        let completion = CompletionEvent {
            root_id: run_job.root_id,
            lane: run_job.lane,
            heavy_permit: run_job.heavy_permit.take(),
            panicked,
        };
        let _ = event_tx.send(SchedulerEvent::Completed(completion));
    }
}

fn run_lane_job(run_job: &mut RunJob) -> Response {
    let missing_request_id = run_job.request_id.clone();
    let job = std::mem::replace(
        &mut run_job.job,
        Box::new(move |_| {
            Response::error(
                missing_request_id,
                "job_missing",
                "executor job already taken",
            )
        }),
    );

    match run_job.lane {
        Lane::PureRead | Lane::SerialLspStatus => {
            let _epoch = run_job.epoch.read();
            job(&run_job.ctx)
        }
        Lane::HeavyInit => {
            let response = job(&run_job.ctx);
            {
                let _install = run_job.epoch.write();
            }
            response
        }
        Lane::Mutating => {
            let _epoch = run_job.epoch.write();
            job(&run_job.ctx)
        }
    }
}

#[derive(Debug)]
struct HeavySemaphore {
    available: AtomicUsize,
    max: usize,
}

impl HeavySemaphore {
    fn new(permits: usize) -> Self {
        Self {
            available: AtomicUsize::new(permits),
            max: permits,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<HeavyPermit> {
        loop {
            let available = self.available.load(Ordering::Acquire);
            if available == 0 {
                return None;
            }
            if self
                .available
                .compare_exchange(
                    available,
                    available - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(HeavyPermit {
                    semaphore: Arc::clone(self),
                });
            }
        }
    }
}

struct HeavyPermit {
    semaphore: Arc<HeavySemaphore>,
}

impl Drop for HeavyPermit {
    fn drop(&mut self) {
        let previous = self.semaphore.available.fetch_add(1, Ordering::Release);
        debug_assert!(previous < self.semaphore.max);
    }
}
