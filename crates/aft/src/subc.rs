//! subc daemon attach — transport edge (P5a).
//!
//! When AFT is launched as `aft --subc <connection-file>`, it does NOT run the
//! standalone NDJSON-over-stdin loop. Instead it connects to a running subc
//! daemon over loopback TCP, authenticates with the pre-envelope HMAC handshake
//! (`subc-transport`), then speaks the subc frame protocol (`subc-protocol`):
//! ModuleHello → HelloAck (register as a tool provider), then a channel-0
//! control loop (Ping/Pong, RouteBind) plus route-channel tool calls.
//!
//! Concurrency: subc routes tool calls through the P5b executor. The tokio
//! edge never dispatches against `AppContext` inline; per-actor executor lanes
//! own the reader/mutator epoch, while a writer task serializes outbound frames.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::context::{App, AppContext, ProgressSender};
use crate::executor::{Executor, Lane};
use crate::path_identity::ProjectRootId;
use crate::protocol::{ProgressKind, PushFrame, RawRequest, Response};
use crate::runtime_drain;

use subc_protocol::manifest::{
    Bindings, Concurrency, ConfigBinding, ConfigSource, IdentityBinding, IdentityScope,
    ModuleManifest, ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
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

type RouteChannel = u32;

#[derive(Debug)]
struct RootMeta {
    maintenance_pending: bool,
    last_touched: Instant,
}

#[derive(Debug, Clone)]
struct RouteIdentity {
    root: ProjectRootId,
    harness: String,
    session: String,
}

impl RootMeta {
    fn new(now: Instant) -> Self {
        Self {
            maintenance_pending: false,
            last_touched: now,
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

fn progress_sender_for_root(
    push_tx: mpsc::Sender<(ProjectRootId, PushFrame)>,
    root_id: ProjectRootId,
) -> ProgressSender {
    Arc::new(Box::new(move |frame: PushFrame| {
        // Emitters can run on executor workers, maintenance jobs, watcher drains,
        // semantic refresh workers, or bg-bash watchdog threads. Never block any
        // of them on subc routing/backpressure; the loop coalesces lossy bursts.
        let _ = push_tx.try_send((root_id.clone(), frame));
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

fn fan_out_push_frame(
    writer_tx: &mpsc::Sender<Frame>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    root: &ProjectRootId,
    frame: &PushFrame,
) -> usize {
    let Some(channels) = root_channels.get(root) else {
        return 0;
    };
    let body = match serde_json::to_vec(frame) {
        Ok(body) => body,
        Err(error) => {
            log::warn!("subc attach: failed to serialize PushFrame for fan-out: {error}");
            return 0;
        }
    };

    let mut sent = 0;
    for &channel in channels {
        let Ok(route_channel) = u16::try_from(channel) else {
            log::warn!("subc attach: invalid route channel {channel} for Push fan-out");
            continue;
        };
        let push_frame = match Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Push,
            control_flags(),
            route_channel,
            0,
            body.clone(),
        ) {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("subc attach: failed to build Push frame: {error}");
                continue;
            }
        };
        if writer_tx.try_send(push_frame).is_ok() {
            sent += 1;
        }
    }
    sent
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
) -> Result<(), SubcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SubcError::Runtime)?;

    runtime.block_on(async move {
        let shared_app = ctx.app();
        drop(ctx);
        let stream = connect_and_authenticate(connection_file_path).await?;
        log::info!(
            "subc attach: authenticated to daemon via {}",
            connection_file_path.display()
        );
        let (read_half, write_half) = tokio::io::split(stream);
        run_module_loop(read_half, write_half, shared_app, executor, dispatch).await
    })
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
) -> Result<(), SubcError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // ModuleHello: register as a tool provider. control_ops:None = full baseline.
    let hello = ModuleHelloBody {
        manifest: build_manifest(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
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
    let (push_tx, mut push_rx) = mpsc::channel::<(ProjectRootId, PushFrame)>(1024);
    let mut routes: HashMap<RouteChannel, RouteIdentity> = HashMap::new();
    let mut root_channels: HashMap<ProjectRootId, HashSet<RouteChannel>> = HashMap::new();
    let mut live_roots: HashMap<ProjectRootId, RootMeta> = HashMap::new();

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
                        if let Some(identity) = remove_route_channel(&mut routes, &mut root_channels, channel) {
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
                            log::debug!("subc attach: unbound route {} torn down", frame.header.channel);
                        }
                    }
                    FrameType::Request if frame.header.channel == 0 => {
                        if let Err(error) = handle_control_request(
                            &writer_tx,
                            &frame,
                            &shared_app,
                            &executor,
                            &mut routes,
                            &mut root_channels,
                            &mut live_roots,
                            &push_tx,
                            &shutdown,
                            dispatch,
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
                            &mut live_roots,
                            &executor,
                            &shutdown,
                            dispatch,
                        )
                        .await
                        {
                            break Err(error);
                        }
                    }
                    // Cancel/Push/etc. are ignored until Phase 3 cancellation.
                    _ => {}
                }
            }
            Some((root_id, frame)) = push_rx.recv() => {
                // Drain the currently queued burst in one loop turn so lossy
                // status/progress classes coalesce before reaching subc's shared
                // egress queue.
                let mut batch = vec![(root_id, frame)];
                while let Ok(item) = push_rx.try_recv() {
                    batch.push(item);
                }

                for (root, frame) in coalesce_push_batch(batch) {
                    let _ = fan_out_push_frame(&writer_tx, &root_channels, &root, &frame);
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
    tx.send(frame).await.map_err(|_| SubcError::WriterClosed)
}

/// channel-0 control request — currently only RouteBind. Reconciles the route's
/// RootConfig through the executor's Mutating lane and replies RouteBindAck
/// (Response lane) or an ErrorBody (Error lane) on divergence/failure.
async fn handle_control_request(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    shared_app: &Arc<App>,
    executor: &Arc<Executor>,
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    root_channels: &mut HashMap<ProjectRootId, HashSet<RouteChannel>>,
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    push_tx: &mpsc::Sender<(ProjectRootId, PushFrame)>,
    shutdown: &Arc<Notify>,
    dispatch: DispatchFn,
) -> Result<(), SubcError> {
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(SubcError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            target: _,
            identity,
            config,
        } => {
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
            let config_tiers: Vec<Value> = config
                .iter()
                .map(|t| json!({ "tier": t.tier, "source": t.source, "doc": t.doc }))
                .collect();
            let configure_json = json!({
                "id": request_id,
                "command": "configure",
                "project_root": identity.project_root,
                "harness": identity.harness,
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
                    push_tx.clone(),
                    bind_root_id.clone(),
                )));
                let inserted =
                    executor.register_actor(bind_root_id.clone(), Arc::clone(&actor_ctx));
                drop(actor_ctx);
                live_roots.insert(bind_root_id.clone(), RootMeta::new(Instant::now()));
                log::debug!(
                    "subc attach: registered actor for route {} root {}",
                    route_channel,
                    bind_root_id.as_path().display()
                );
                inserted
            };

            let configure_request_id = configure_req.id.clone();
            let configure_rx = executor.submit_async(
                bind_root_id.clone(),
                Lane::Mutating,
                configure_request_id.clone(),
                Box::new(move |ctx| dispatch(configure_req, ctx)),
            );
            let configure_response =
                await_executor_response(configure_rx, configure_request_id.clone()).await;

            if !configure_response.success {
                if !root_was_live {
                    live_roots.remove(&bind_root_id);
                    if inserted_new_actor {
                        executor.remove_actor(&bind_root_id);
                    }
                }
                let message =
                    response_message(&configure_response, "configure failed during route bind");
                send_route_bind_error(tx, frame, "config_divergence", &message).await?;
                if response_is_internal_error(&configure_response) {
                    signal_fatal_teardown(
                        tx,
                        Some(route_channel),
                        frame.header.ver,
                        frame.header.corr,
                        shutdown,
                    )
                    .await;
                }
                return Ok(());
            }

            if !root_was_live {
                let drain_request_id = format!("subc-bind-drain-{route_channel}");
                let drain_response_id = drain_request_id.clone();
                let drain_rx = executor.submit_async(
                    bind_root_id.clone(),
                    Lane::Mutating,
                    drain_request_id.clone(),
                    Box::new(move |ctx| {
                        runtime_drain::drain_build_completions(ctx);
                        Response::success(drain_response_id, json!({ "drained": true }))
                    }),
                );
                let drain_response = await_executor_response(drain_rx, drain_request_id).await;
                if !drain_response.success {
                    let message = response_message(
                        &drain_response,
                        "build-completion drain failed during route bind",
                    );
                    send_route_bind_error(tx, frame, "config_divergence", &message).await?;
                    if response_is_internal_error(&drain_response) {
                        signal_fatal_teardown(
                            tx,
                            Some(route_channel),
                            frame.header.ver,
                            frame.header.corr,
                            shutdown,
                        )
                        .await;
                    }
                    return Ok(());
                }
            }

            insert_route_channel(
                routes,
                root_channels,
                route_key(route_channel),
                RouteIdentity {
                    root: bind_root_id.clone(),
                    harness: identity.harness,
                    session: identity.session,
                },
            );
            if let Some(meta) = live_roots.get_mut(&bind_root_id) {
                meta.touch();
            }

            let ack = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {})
                .map_err(SubcError::Json)?;
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                control_flags(),
                0,
                frame.header.corr,
                ack,
            )
            .map_err(SubcError::FrameBuild)?;
            send_frame(tx, response).await?;
            log::info!(
                "subc attach: route {} bound to root {}",
                route_channel,
                bind_root_id.as_path().display()
            );
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

async fn send_route_bind_error(
    tx: &mpsc::Sender<Frame>,
    frame: &Frame,
    code: &str,
    message: &str,
) -> Result<(), SubcError> {
    let response = build_error_frame(
        frame.header.ver,
        0,
        frame.header.corr,
        control_flags(),
        code,
        message,
    )?;
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
    live_roots: &mut HashMap<ProjectRootId, RootMeta>,
    executor: &Arc<Executor>,
    shutdown: &Arc<Notify>,
    dispatch: DispatchFn,
) -> Result<(), SubcError> {
    let Some(identity) = routes.get(&route_key(frame.header.channel)).cloned() else {
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

    // Build a RawRequest: {id, command: name, ...arguments}.
    let request_id = format!("subc-{}-{}", frame.header.channel, frame.header.corr);
    let command = call.name;
    let lane = command_lane(&command);
    let command_for_finalize = command.clone();
    let session_for_finalize = identity.session.clone();
    let mut map = call.arguments.as_object().cloned().unwrap_or_default();
    map.insert("id".to_string(), json!(request_id.clone()));
    map.insert("command".to_string(), json!(command));
    // Transport session from RouteBind identity; authoritative over any stray arg.
    map.insert("session_id".to_string(), json!(identity.session.clone()));

    let raw_req = match serde_json::from_value::<RawRequest>(Value::Object(map)) {
        Ok(req) => req,
        Err(error) => {
            let response = Response::error(
                request_id.clone(),
                "invalid_request",
                format!("failed to build request from tool call: {error}"),
            );
            let response_frame = build_tool_response_frame(
                frame.header.ver,
                frame.header.channel,
                frame.header.corr,
                frame.header.flags,
                &response,
            )?;
            return send_frame(tx, response_frame).await;
        }
    };

    let rx = executor.submit_async(
        identity.root,
        lane,
        request_id.clone(),
        Box::new(move |ctx| {
            let mut response = dispatch(raw_req, ctx);
            crate::response_finalize::attach_bg_completions(
                &mut response,
                ctx,
                &session_for_finalize,
                &command_for_finalize,
            );
            crate::response_finalize::attach_status_bar(&mut response, ctx, &command_for_finalize);
            response
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
        let fatal = is_mutating && response_is_internal_error(&response);
        match build_tool_response_frame(ver, route_channel, corr, flags, &response) {
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
            runtime_drain::drain_build_completions(ctx);
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
    response: &Response,
) -> Result<Frame, SubcError> {
    let response_value = serde_json::to_value(response).map_err(SubcError::Json)?;
    let is_error = response_value
        .get("success")
        .and_then(Value::as_bool)
        .map(|ok| !ok)
        .unwrap_or(true);
    let result = json!({
        "content": [{ "type": "text", "text": response_value.to_string() }],
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
            let _ = tx.send(frame).await;
        }
    }
    if let Ok(frame) = build_goodbye_frame(ver, 0, 0) {
        let _ = tx.send(frame).await;
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

        // Lazy reads mutate parser/terminal/url caches on a miss, but Phase 2b
        // classifies them onto the reader pool; install races are handled at the
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

/// AFT's subc-mode capability manifest. BARE tool names (the gateway owns the
/// `aft_` prefix); ModuleManaged concurrency (AFT schedules internally);
/// FirstParty trust. Minimal-but-conformant tool set for the spike — the full
/// bare set is locked before the gateway fronts AFT.
fn build_manifest() -> ModuleManifest {
    let tool = |name: &str, mutates: bool| Tool {
        name: name.to_string(),
        mutates,
        schema: json!({ "type": "object" }),
    };
    ModuleManifest {
        module_id: "aft".to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![
                tool("status", false),
                tool("read", false),
                tool("grep", false),
                tool("search", false),
                tool("outline", false),
                tool("inspect", false),
                tool("edit", true),
                tool("write", true),
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
            config: ConfigBinding {
                source: ConfigSource::SubcMediated,
                tiers: vec!["user".to_string(), "project".to_string()],
                expansion: std::collections::BTreeMap::new(),
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
        BashCompletedFrame, BashLongRunningFrame, ProgressFrame, StatusChangedFrame,
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
        PushFrame::StatusChanged(StatusChangedFrame {
            frame_type: "status_changed",
            session_id: None,
            snapshot: json!({ "seq": seq }),
        })
    }

    fn completion_frame(task_id: &str) -> PushFrame {
        PushFrame::BashCompleted(BashCompletedFrame {
            frame_type: "bash_completed",
            task_id: task_id.to_string(),
            session_id: "session-1".to_string(),
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
        PushFrame::BashLongRunning(BashLongRunningFrame {
            frame_type: "bash_long_running",
            task_id: task_id.to_string(),
            session_id: "session-1".to_string(),
            command: format!("sleep {elapsed_ms}"),
            elapsed_ms,
        })
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
    fn progress_sender_drops_when_push_funnel_is_full_without_blocking() {
        let (_root_dir, root) = test_root("subc-push-full-root");
        let (push_tx, mut push_rx) = mpsc::channel::<(ProjectRootId, PushFrame)>(1);
        let sender = progress_sender_for_root(push_tx, root.clone());

        let started = Instant::now();
        sender(status_frame(1));
        sender(status_frame(2));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "saturated push sender must return immediately"
        );

        let (received_root, received_frame) = push_rx.try_recv().expect("first frame queued");
        assert_eq!(received_root, root);
        assert_eq!(status_seq(&received_frame), Some(1));
        assert!(
            push_rx.try_recv().is_err(),
            "second frame should be dropped"
        );
    }

    #[test]
    fn fan_out_push_frame_drops_when_writer_is_full_without_blocking() {
        let (_root_dir, root) = test_root("subc-writer-full-root");
        let (writer_tx, mut writer_rx) = mpsc::channel::<Frame>(1);
        writer_tx
            .try_send(Frame::build(FrameType::Ping, control_flags(), 0, 1, Vec::new()).unwrap())
            .expect("prefill writer queue");

        let mut root_channels = HashMap::new();
        root_channels.insert(root.clone(), HashSet::from([route_key(7)]));

        let started = Instant::now();
        let sent = fan_out_push_frame(&writer_tx, &root_channels, &root, &status_frame(1));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "saturated writer fan-out must return immediately"
        );
        assert_eq!(sent, 0);

        let queued = writer_rx
            .try_recv()
            .expect("prefilled frame remains queued");
        assert_eq!(queued.header.ty, FrameType::Ping);
        assert!(
            writer_rx.try_recv().is_err(),
            "push should be dropped on full writer"
        );
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
