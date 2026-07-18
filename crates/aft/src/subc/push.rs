//! Push-frame classification, fan-out, buffering, replay, and background wake plumbing.

use super::wire::{send_reliable_writer_frame, try_enqueue_writer_frame, WriterEnqueueOutcome};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ReplayKey {
    pub(super) root: ProjectRootId,
    pub(super) harness: String,
    pub(super) session: String,
}

impl ReplayKey {
    pub(super) fn from_identity(identity: &RouteIdentity) -> Self {
        Self {
            root: identity.root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct CompletedTaskIds {
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

pub(super) fn progress_sender_for_root(
    push_senders: PushSenders,
    root_id: ProjectRootId,
) -> ProgressSender {
    Arc::new(Box::new(move |frame: PushFrame| {
        // Emitters can run on executor workers, maintenance jobs, watcher drains,
        // semantic refresh workers, or bg-bash watchdog threads. Never block any
        // of them on subc routing/backpressure: reliable frames use an unbounded
        // non-blocking channel, while lossy frames use a bounded channel and keep
        // only the newest update when that channel fills up.
        if frame_is_reliable(&frame) {
            let _ = push_senders.reliable_tx.send((root_id.clone(), frame));
        } else {
            enqueue_lossy_push_frame(
                &push_senders.lossy_tx,
                &push_senders.lossy_overflow,
                &push_senders.lossy_seq,
                root_id.clone(),
                frame,
            );
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

#[derive(Debug, Default)]
pub(super) struct LossyOverflow {
    inner: std::sync::Mutex<LossyOverflowInner>,
}

#[derive(Debug, Default)]
struct LossyOverflowInner {
    slots: Vec<LossyPushEnvelope>,
    latest: HashMap<(ProjectRootId, LossyPushKey), usize>,
}

impl LossyOverflow {
    fn push_latest(&self, order: u64, root: ProjectRootId, frame: PushFrame) {
        let Some(lossy_key) = lossy_push_key(&frame) else {
            return;
        };
        let map_key = (root.clone(), lossy_key);
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = inner.latest.get(&map_key).copied() {
            inner.slots[slot] = (order, root, frame);
        } else {
            let slot = inner.slots.len();
            inner.latest.insert(map_key, slot);
            inner.slots.push((order, root, frame));
        }
    }

    pub(super) fn drain(&self) -> Vec<LossyPushEnvelope> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.latest.clear();
        std::mem::take(&mut inner.slots)
    }
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

pub(super) fn enqueue_lossy_push_frame(
    tx: &mpsc::Sender<LossyPushEnvelope>,
    overflow: &LossyOverflow,
    sequence: &AtomicU64,
    root: ProjectRootId,
    frame: PushFrame,
) {
    let order = sequence.fetch_add(1, Ordering::Relaxed);
    match tx.try_send((order, root, frame)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full((order, root, frame))) => {
            // The bounded lossy channel protects emitters from blocking. When the
            // channel is saturated, keep the newest coalescable frame out-of-band
            // so a full channel drops stale status/progress, not the latest update.
            overflow.push_latest(order, root, frame);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

pub(super) fn coalesce_push_batch(
    batch: Vec<(ProjectRootId, PushFrame)>,
) -> Vec<(ProjectRootId, PushFrame)> {
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
pub(super) enum PushSendOutcome {
    Sent,
    Backpressure,
    PermanentFailure,
}

fn try_send_push_body(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    channel: RouteChannel,
    body: &[u8],
) -> PushSendOutcome {
    let push_frame = match Frame::build_with_version(
        PROTOCOL_VERSION,
        FrameType::Push,
        control_flags(),
        channel.channel,
        channel.epoch,
        0,
        body.to_vec(),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            log::warn!("subc attach: failed to build Push frame: {error}");
            return PushSendOutcome::PermanentFailure;
        }
    };
    match try_enqueue_writer_frame(writer_tx, metrics, push_frame) {
        WriterEnqueueOutcome::Enqueued => PushSendOutcome::Sent,
        WriterEnqueueOutcome::Full(_) => PushSendOutcome::Backpressure,
        WriterEnqueueOutcome::Closed => {
            log::warn!("subc attach: writer closed while sending Push frame");
            PushSendOutcome::PermanentFailure
        }
    }
}

fn try_send_push_frame(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
    try_send_push_body(writer_tx, metrics, channel, &body)
}

fn try_send_bg_stream_frame(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    channel: RouteChannel,
    sub: &BgSub,
    ty: FrameType,
    body: Vec<u8>,
) -> PushSendOutcome {
    let frame = match Frame::build_with_version(
        sub.ver,
        ty,
        sub.flags,
        channel.channel,
        channel.epoch,
        sub.corr,
        body,
    ) {
        Ok(frame) => frame,
        Err(error) => {
            log::warn!("subc attach: failed to build bg_events stream frame: {error}");
            return PushSendOutcome::PermanentFailure;
        }
    };
    match try_enqueue_writer_frame(writer_tx, metrics, frame) {
        WriterEnqueueOutcome::Enqueued => PushSendOutcome::Sent,
        WriterEnqueueOutcome::Full(_) => PushSendOutcome::Backpressure,
        WriterEnqueueOutcome::Closed => {
            log::warn!("subc attach: writer closed while sending bg_events stream frame");
            PushSendOutcome::PermanentFailure
        }
    }
}

fn try_send_bg_stream_data(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
    try_send_bg_stream_frame(
        writer_tx,
        metrics,
        channel,
        sub,
        FrameType::StreamData,
        body,
    )
}

pub(super) async fn send_reliable_bg_stream_end(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    channel: RouteChannel,
    sub: &BgSub,
) -> Result<(), SubcError> {
    let frame = Frame::build_with_version(
        sub.ver,
        FrameType::StreamEnd,
        sub.flags,
        channel.channel,
        channel.epoch,
        sub.corr,
        Vec::new(),
    )
    .map_err(SubcError::FrameBuild)?;
    send_reliable_writer_frame(writer_tx, metrics, frame, "bg_events StreamEnd").await
}

pub(super) fn emit_bg_event_wakes(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    bg_subs: &HashMap<RouteChannel, BgSub>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
) {
    let pending_channels: Vec<RouteChannel> = bg_wake_pending.iter().copied().collect();
    let mut stale_channels = Vec::new();
    for channel in pending_channels {
        if let Some(sub) = bg_subs.get(&channel) {
            let _ = try_send_bg_stream_data(writer_tx, metrics, channel, sub);
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
pub(super) fn arm_bg_wake(
    root: ProjectRootId,
    session: String,
    channel: RouteChannel,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
) {
    *bg_wake_epoch.entry((root, session)).or_default() += 1;
    bg_wake_pending.insert(channel);
}

pub(super) fn clear_stale_bg_wakes_for_empty_sessions(
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

pub(super) fn migrate_retry_buffer_to_push_buffer(
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

pub(super) fn replay_buffered_push_frames(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
            match try_send_push_frame(writer_tx, metrics, channel, &frame) {
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
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
            match try_send_push_frame(writer_tx, metrics, channel, &frame) {
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

pub(super) fn drain_retry_buffers_for_bound_routes(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    retry_buffer: &mut RetryBuffer,
) -> usize {
    let channels: Vec<RouteChannel> = routes.keys().copied().collect();
    channels
        .into_iter()
        .map(|channel| drain_retry_buffer_for_channel(writer_tx, metrics, channel, retry_buffer))
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
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
                try_send_push_body(writer_tx, metrics, channel, &body),
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
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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

        match try_send_push_frame(writer_tx, metrics, channel, frame) {
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
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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
        metrics,
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

#[allow(clippy::too_many_arguments)]
fn process_reliable_push_and_arm_bg_wake(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    completed_tasks: &mut CompletedTaskIds,
    bg_sub_by_session: &HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
    root: ProjectRootId,
    frame: PushFrame,
) {
    if let Some((root, session)) = process_reliable_push_frame(
        writer_tx,
        metrics,
        routes,
        root_channels,
        session_identity,
        retry_buffer,
        push_buffer,
        completed_tasks,
        root,
        frame,
    ) {
        if let Some(channel) = bg_sub_by_session
            .get(&(root.clone(), session.clone()))
            .copied()
        {
            arm_bg_wake(root, session, channel, bg_wake_pending, bg_wake_epoch);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn drain_reliable_push_turn(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    session_identity: &HashMap<(ProjectRootId, String), RetainedSessionIdentity>,
    retry_buffer: &mut RetryBuffer,
    push_buffer: &mut HashMap<ReplayKey, VecDeque<PushFrame>>,
    completed_tasks: &mut CompletedTaskIds,
    bg_sub_by_session: &HashMap<(ProjectRootId, String), RouteChannel>,
    bg_wake_pending: &mut HashSet<RouteChannel>,
    bg_wake_epoch: &mut HashMap<(ProjectRootId, String), u64>,
    reliable_rx: &mut mpsc::UnboundedReceiver<PushEnvelope>,
    first: Option<PushEnvelope>,
) -> (usize, bool) {
    let mut processed = 0;

    if let Some((root, frame)) = first {
        process_reliable_push_and_arm_bg_wake(
            writer_tx,
            metrics,
            routes,
            root_channels,
            session_identity,
            retry_buffer,
            push_buffer,
            completed_tasks,
            bg_sub_by_session,
            bg_wake_pending,
            bg_wake_epoch,
            root,
            frame,
        );
        processed += 1;
    }

    while processed < RELIABLE_PUSH_DRAIN_BUDGET {
        let Ok((root, frame)) = reliable_rx.try_recv() else {
            return (processed, false);
        };
        process_reliable_push_and_arm_bg_wake(
            writer_tx,
            metrics,
            routes,
            root_channels,
            session_identity,
            retry_buffer,
            push_buffer,
            completed_tasks,
            bg_sub_by_session,
            bg_wake_pending,
            bg_wake_epoch,
            root,
            frame,
        );
        processed += 1;
    }

    let deferred = !reliable_rx.is_empty();
    if deferred {
        metrics
            .reliable_push_budget_deferrals
            .fetch_add(1, Ordering::Relaxed);
    }

    (processed, deferred)
}

pub(super) fn process_lossy_push_frame(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
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

    let _ = fan_out_lossy_push_frame(writer_tx, metrics, routes, root_channels, &root, &frame);
}

pub(super) fn process_lossy_push_batch(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    completed_tasks: &CompletedTaskIds,
    batch: Vec<PushEnvelope>,
) {
    for (root, frame) in coalesce_push_batch(batch) {
        process_lossy_push_frame(
            writer_tx,
            metrics,
            routes,
            root_channels,
            completed_tasks,
            root,
            frame,
        );
    }
}

pub(super) fn process_lossy_push_envelope_batch(
    writer_tx: &WriterSender,
    metrics: &DispatchPathMetrics,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    completed_tasks: &CompletedTaskIds,
    mut batch: Vec<LossyPushEnvelope>,
) {
    batch.sort_by_key(|(order, _, _)| *order);
    let batch = batch
        .into_iter()
        .map(|(_, root, frame)| (root, frame))
        .collect();
    process_lossy_push_batch(
        writer_tx,
        metrics,
        routes,
        root_channels,
        completed_tasks,
        batch,
    );
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn bg_stream_end_waits_for_writer_capacity_instead_of_dropping() {
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(1);
        let blocker = Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new())
            .expect("build blocker frame");
        assert!(try_enqueue_writer_frame(&writer_tx, &metrics, blocker).is_enqueued());

        let send_tx = writer_tx.clone();
        let send_metrics = Arc::clone(&metrics);
        let send = tokio::spawn(async move {
            send_reliable_bg_stream_end(
                &send_tx,
                &send_metrics,
                route_key(7, 3),
                &BgSub {
                    corr: 41,
                    ver: PROTOCOL_VERSION,
                    flags: control_flags(),
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(!send.is_finished(), "StreamEnd was dropped on backpressure");

        let first = writer_rx.recv().await.expect("blocker frame");
        assert_eq!(first.header.ty, FrameType::Ping);
        tokio::time::timeout(Duration::from_secs(1), send)
            .await
            .expect("StreamEnd enqueue timed out")
            .expect("StreamEnd task panicked")
            .expect("StreamEnd enqueue failed");
        let terminal = writer_rx.recv().await.expect("StreamEnd frame");
        assert_eq!(terminal.header.ty, FrameType::StreamEnd);
        assert_eq!(terminal.header.channel, 7);
        assert_eq!(terminal.header.epoch, 3);
        assert_eq!(terminal.header.corr, 41);
    }

    #[tokio::test]
    async fn reliable_push_drain_budget_defers_without_reordering() {
        let (_dir, root) = test_root("reliable-budget-root");
        let session = "session-budget".to_string();
        let mut routes = HashMap::new();
        routes.insert(
            route_key(1, 1),
            RouteIdentity(Arc::new(RouteIdentityData {
                root: root.clone(),
                project_root: root.as_path().to_path_buf(),
                harness: "opencode".to_string(),
                session: session.clone(),
                trust: BindTrust::FirstParty,
                spawn_principal: AuthenticatedPrincipal::FirstParty,
                consumer_elicitation_capable: false,
            })),
        );
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(1, 1)]));
        let session_identity = HashMap::new();
        let mut retry_buffer = RetryBuffer::new();
        let mut push_buffer = HashMap::new();
        let mut completed_tasks = CompletedTaskIds::default();
        let bg_sub_by_session = HashMap::new();
        let mut bg_wake_pending = HashSet::new();
        let mut bg_wake_epoch = HashMap::new();
        let metrics = DispatchPathMetrics::new();
        let (writer_tx, mut writer_rx) =
            mpsc::channel::<WriterFrame>(RELIABLE_PUSH_DRAIN_BUDGET + 8);
        let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
        let total = RELIABLE_PUSH_DRAIN_BUDGET + 3;
        let first = (
            root.clone(),
            completion_frame_with_session("budget-0", &session),
        );
        for index in 1..total {
            reliable_tx
                .send((
                    root.clone(),
                    completion_frame_with_session(&format!("budget-{index}"), &session),
                ))
                .expect("queue reliable frame");
        }

        let (processed, deferred_by_budget) = drain_reliable_push_turn(
            &writer_tx,
            &metrics,
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
            Some(first),
        );

        assert_eq!(processed, RELIABLE_PUSH_DRAIN_BUDGET);
        assert!(deferred_by_budget);
        assert_eq!(
            metrics
                .reliable_push_budget_deferrals
                .load(Ordering::Relaxed),
            1
        );

        let mut delivered = Vec::new();
        for _ in 0..RELIABLE_PUSH_DRAIN_BUDGET {
            let frame = writer_rx.try_recv().expect("budgeted push frame");
            delivered.push(push_frame_task_id(&frame).expect("push task id"));
        }
        assert_eq!(
            delivered,
            (0..RELIABLE_PUSH_DRAIN_BUDGET)
                .map(|index| format!("budget-{index}"))
                .collect::<Vec<_>>()
        );

        let next_first = reliable_rx.recv().await.expect("deferred reliable frame");
        drain_reliable_push_turn(
            &writer_tx,
            &metrics,
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
            Some(next_first),
        );
        let mut deferred = Vec::new();
        while let Ok(frame) = writer_rx.try_recv() {
            deferred.push(push_frame_task_id(&frame).expect("deferred push task id"));
        }
        assert_eq!(deferred, vec!["budget-32", "budget-33", "budget-34"]);
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
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(8);
        let metrics = DispatchPathMetrics::new();
        let identity1 = route_identity(&root, "session-1");
        let identity2 = route_identity(&root, "session-2");
        let mut routes = HashMap::new();
        routes.insert(route_key(1, 1), identity1.clone());
        routes.insert(route_key(2, 1), identity2.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(
            root.clone(),
            HashSet::from([route_key(1, 1), route_key(2, 1)]),
        );
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity1);
        remember_session_identity(&mut session_identity, &identity2);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();

        let session_result = fan_out_reliable_push_frame(
            &writer_tx,
            &metrics,
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

        let project_result = fan_out_lossy_push_frame(
            &writer_tx,
            &metrics,
            &routes,
            &root_channels,
            &root,
            &status_frame(9),
        );
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
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(4);
        let metrics = DispatchPathMetrics::new();
        let mut push_buffer = HashMap::new();
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-a"));
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-b"));

        let replayed = replay_buffered_push_frames(
            &writer_tx,
            &metrics,
            route_key(3, 1),
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
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(4);
        let metrics = DispatchPathMetrics::new();
        let mut push_buffer = HashMap::new();
        buffer_push_frame(&mut push_buffer, key.clone(), completion_frame("task-a"));

        let replayed = replay_buffered_push_frames(
            &writer_tx,
            &metrics,
            route_key(3, 1),
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
    fn lossy_overflow_coalesces_after_saturated_channel_backlog() {
        let (_root_dir, root) = test_root("subc-lossy-overflow-root");
        let (lossy_tx, mut lossy_rx) = mpsc::channel::<LossyPushEnvelope>(1);
        let lossy_overflow = LossyOverflow::default();
        let lossy_seq = AtomicU64::new(0);

        enqueue_lossy_push_frame(
            &lossy_tx,
            &lossy_overflow,
            &lossy_seq,
            root.clone(),
            status_frame(1),
        );
        enqueue_lossy_push_frame(
            &lossy_tx,
            &lossy_overflow,
            &lossy_seq,
            root.clone(),
            status_frame(2),
        );
        enqueue_lossy_push_frame(
            &lossy_tx,
            &lossy_overflow,
            &lossy_seq,
            root.clone(),
            status_frame(3),
        );

        let mut batch = vec![lossy_rx.try_recv().expect("first lossy frame queued")];
        batch.extend(lossy_overflow.drain());
        batch.sort_by_key(|(order, _, _)| *order);
        let statuses: Vec<_> = coalesce_push_batch(
            batch
                .into_iter()
                .map(|(_, root, frame)| (root, frame))
                .collect(),
        )
        .iter()
        .filter_map(|(_, frame)| status_seq(frame))
        .collect();

        assert_eq!(statuses, vec![3]);
    }

    #[test]
    fn progress_sender_keeps_reliable_off_saturated_lossy_funnel_without_blocking() {
        let (_root_dir, root) = test_root("subc-push-full-root");
        let (lossy_tx, mut lossy_rx) = mpsc::channel::<LossyPushEnvelope>(1);
        let lossy_overflow = Arc::new(LossyOverflow::default());
        let (reliable_tx, mut reliable_rx) = mpsc::unbounded_channel::<PushEnvelope>();
        let sender = progress_sender_for_root(
            PushSenders {
                lossy_tx,
                reliable_tx,
                lossy_overflow: Arc::clone(&lossy_overflow),
                lossy_seq: Arc::new(AtomicU64::new(0)),
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

        let (_, received_root, received_frame) =
            lossy_rx.try_recv().expect("first lossy frame queued");
        assert_eq!(received_root, root);
        assert_eq!(status_seq(&received_frame), Some(1));
        assert!(
            lossy_rx.try_recv().is_err(),
            "full lossy channel should keep later frames in overflow"
        );
        let overflow_batch = lossy_overflow.drain();
        assert_eq!(overflow_batch.len(), 1);
        assert_eq!(overflow_batch[0].1, root);
        assert_eq!(status_seq(&overflow_batch[0].2), Some(2));

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
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(1);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");

        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(7, 1)]));

        let routes = HashMap::new();
        let started = Instant::now();
        let result = fan_out_lossy_push_frame(
            &writer_tx,
            &metrics,
            &routes,
            &root_channels,
            &root,
            &status_frame(1),
        );
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
        routes.insert(route_key(9, 1), identity.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(9, 1)]));
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(1);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");

        let result = fan_out_reliable_push_frame(
            &writer_tx,
            &metrics,
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
        assert_eq!(
            retry_buffer.get(&route_key(9, 1)).map(VecDeque::len),
            Some(1)
        );
        assert_eq!(&retry_buffer[&route_key(9, 1)][0].0, &key);

        let queued = writer_rx.try_recv().expect("prefilled frame");
        assert_eq!(queued.header.ty, FrameType::Ping);
        assert_eq!(
            drain_retry_buffer_for_channel(
                &writer_tx,
                &metrics,
                route_key(9, 1),
                &mut retry_buffer
            ),
            1
        );
        let retried = writer_rx.try_recv().expect("retried reliable push");
        assert_eq!(retried.header.ty, FrameType::Push);
        assert_eq!(retried.header.channel, 9);
        assert_eq!(push_frame_task_id(&retried).as_deref(), Some("retry-task"));
        assert!(!retry_buffer.contains_key(&route_key(9, 1)));
    }

    #[test]
    fn reliable_push_fifo_gates_new_frames_behind_retry_buffer() {
        let (_root_dir, root) = test_root("subc-retry-fifo-root");
        let identity = route_identity(&root, "session-1");
        let mut routes = HashMap::new();
        routes.insert(route_key(9, 1), identity.clone());
        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(9, 1)]));
        let mut session_identity = HashMap::new();
        remember_session_identity(&mut session_identity, &identity);
        let mut retry_buffer = HashMap::new();
        let mut push_buffer = HashMap::new();
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(1);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");

        let first = completion_frame("fifo-1");
        let second = completion_frame("fifo-2");
        let _ = fan_out_reliable_push_frame(
            &writer_tx,
            &metrics,
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
            &metrics,
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
        let queued_tasks: Vec<_> = retry_buffer[&route_key(9, 1)]
            .iter()
            .filter_map(|(_, frame)| completion_task(frame))
            .collect();
        assert_eq!(queued_tasks, vec!["fifo-1", "fifo-2"]);

        assert_eq!(
            drain_retry_buffer_for_channel(
                &writer_tx,
                &metrics,
                route_key(9, 1),
                &mut retry_buffer
            ),
            1
        );
        let first_sent = writer_rx.try_recv().expect("first reliable push");
        assert_eq!(push_frame_task_id(&first_sent).as_deref(), Some("fifo-1"));
        assert_eq!(
            drain_retry_buffer_for_channel(
                &writer_tx,
                &metrics,
                route_key(9, 1),
                &mut retry_buffer
            ),
            1
        );
        let second_sent = writer_rx.try_recv().expect("second reliable push");
        assert_eq!(push_frame_task_id(&second_sent).as_deref(), Some("fifo-2"));
        assert!(!retry_buffer.contains_key(&route_key(9, 1)));
    }

    #[test]
    fn replay_buffered_push_frames_drains_incrementally_on_backpressure() {
        let (_root_dir, root) = test_root("subc-incremental-replay-root");
        let key = ReplayKey {
            root,
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
        };
        let (writer_tx, mut writer_rx) = mpsc::channel::<WriterFrame>(2);
        let metrics = DispatchPathMetrics::new();
        writer_tx
            .try_send(WriterFrame::plain(
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            ))
            .expect("prefill writer queue");
        let mut push_buffer = HashMap::new();
        for task in ["replay-1", "replay-2", "replay-3"] {
            buffer_push_frame(&mut push_buffer, key.clone(), completion_frame(task));
        }

        assert_eq!(
            replay_buffered_push_frames(
                &writer_tx,
                &metrics,
                route_key(4, 1),
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
                &metrics,
                route_key(4, 1),
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
            route_key(5, 1),
            key.clone(),
            completion_frame("migrated-task"),
        );
        let mut push_buffer = HashMap::new();

        assert_eq!(
            migrate_retry_buffer_to_push_buffer(
                &mut retry_buffer,
                route_key(5, 1),
                &mut push_buffer
            ),
            1
        );

        assert!(!retry_buffer.contains_key(&route_key(5, 1)));
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
        let (writer_tx, writer_rx) = mpsc::channel::<WriterFrame>(1);
        let metrics = DispatchPathMetrics::new();
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
                &metrics,
                route_key(4, 1),
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
            route_key(4, 1),
            key,
            completion_frame("closed-retry"),
        );
        assert_eq!(
            drain_retry_buffer_for_channel(
                &writer_tx,
                &metrics,
                route_key(4, 1),
                &mut retry_buffer
            ),
            0
        );
        assert!(!retry_buffer.contains_key(&route_key(4, 1)));
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
        let channel = route_key(7, 1);
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
        let channel = route_key(8, 1);
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
        let channel = route_key(9, 1);
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
}
