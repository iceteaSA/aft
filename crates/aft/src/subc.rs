//! subc daemon attach — transport edge (P5a).
//!
//! When AFT is launched as `aft --subc <connection-file>`, it does NOT run the
//! standalone NDJSON-over-stdin loop. Instead it connects to a running subc
//! daemon over loopback TCP, authenticates with the pre-envelope HMAC handshake
//! (`subc-transport`), then speaks the subc frame protocol (`subc-protocol`):
//! ModuleHello → HelloAck (register as a tool provider), then a channel-0
//! control loop (Ping/Pong, RouteBind) plus route-channel tool calls.
//!
//! Concurrency: this is the P5a SERIAL spike. tokio runs ONLY here, on a
//! current-thread runtime; the sync command core is reached by calling the
//! `dispatch` callback INLINE (one call at a time). `AppContext` is `!Send` and
//! that is fine — a current-thread runtime never moves the future across
//! threads. The concurrent per-actor executor is P5b; this proves the wire.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

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

/// Handshake budget. subc binds-before-spawn, so a reachable daemon authenticates
/// well within this; an unreachable/socket-stale daemon fails loud rather than
/// silently downgrading to standalone (the --subc contract).
const AUTH_DEADLINE: Duration = Duration::from_secs(5);

/// Correlation id for the initial ModuleHello (channel 0).
const HELLO_CORR: u64 = 1;

/// Sync command dispatch, passed in from `main` (the binary owns the command
/// table). Called inline for tool calls and the RouteBind-time configure.
pub type DispatchFn = fn(RawRequest, &AppContext) -> Response;

/// Entry point for `aft --subc <connection-file>`. Synchronous on the outside;
/// owns an isolated current-thread tokio runtime for the async transport.
/// Returns `Err` (fail-loud) on any connect/auth/protocol failure — we never
/// fall back to the standalone loop, to avoid split-brain index state.
pub fn run_subc_mode(
    connection_file_path: &Path,
    ctx: &AppContext,
    dispatch: DispatchFn,
) -> Result<(), SubcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SubcError::Runtime)?;

    runtime.block_on(async move {
        let stream = connect_and_authenticate(connection_file_path).await?;
        log::info!(
            "subc attach: authenticated to daemon via {}",
            connection_file_path.display()
        );
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        run_module_loop(&mut read_half, &mut write_half, ctx, dispatch).await
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
/// the connection (EOF) or sends a channel-0 Goodbye.
async fn run_module_loop<R, W>(
    read: &mut R,
    write: &mut W,
    ctx: &AppContext,
    dispatch: DispatchFn,
) -> Result<(), SubcError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
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
    write_frame(write, &hello_frame)
        .await
        .map_err(SubcError::FrameIo)?;

    // Expect HelloAck (registered) or a channel-0 Error (manifest/version reject).
    match read_frame(read).await.map_err(SubcError::FrameIo)? {
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

    // Control + route loop.
    loop {
        let Some(frame) = read_frame(read).await.map_err(SubcError::FrameIo)? else {
            log::info!("subc attach: daemon closed connection");
            return Ok(());
        };

        match frame.header.ty {
            FrameType::Ping if frame.header.channel == 0 => {
                let pong = Frame::build_with_version(
                    frame.header.ver,
                    FrameType::Pong,
                    frame.header.flags,
                    0,
                    frame.header.corr,
                    Vec::new(),
                )
                .map_err(SubcError::FrameBuild)?;
                write_frame(write, &pong)
                    .await
                    .map_err(SubcError::FrameIo)?;
            }
            FrameType::Goodbye if frame.header.channel == 0 => {
                log::info!("subc attach: received channel-0 Goodbye");
                return Ok(());
            }
            FrameType::Goodbye => {
                // Route teardown — no per-route state to drop in the serial spike.
                log::debug!("subc attach: route {} torn down", frame.header.channel);
            }
            FrameType::Request if frame.header.channel == 0 => {
                handle_control_request(write, &frame, ctx, dispatch).await?;
            }
            FrameType::Request => {
                handle_tool_call(write, &frame, ctx, dispatch).await?;
            }
            // Cancel/Push/etc. are ignored in the serial spike (no in-flight set).
            _ => {}
        }
    }
}

/// channel-0 control request — currently only RouteBind. Reconciles the route's
/// RootConfig via `configure` and replies RouteBindAck (Response lane) or an
/// ErrorBody (Error lane) on divergence/failure.
async fn handle_control_request<W>(
    write: &mut W,
    frame: &Frame,
    ctx: &AppContext,
    dispatch: DispatchFn,
) -> Result<(), SubcError>
where
    W: AsyncWrite + Unpin,
{
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(SubcError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            target: _,
            identity,
            config,
        } => {
            // Reconcile RootConfig: build a configure request from the bind
            // identity + the forwarded config tiers and run it through the sync
            // core. Success => RouteBindAck; failure => config_divergence Error.
            let config_tiers: Vec<Value> = config
                .iter()
                .map(|t| json!({ "tier": t.tier, "source": t.source, "doc": t.doc }))
                .collect();
            let configure_json = json!({
                "id": format!("subc-bind-{route_channel}"),
                "command": "configure",
                "project_root": identity.project_root,
                "harness": identity.harness,
                "config": config_tiers,
            });
            let reconciled = match serde_json::from_value::<RawRequest>(configure_json) {
                Ok(req) => dispatch(req, ctx),
                Err(error) => {
                    return send_route_bind_error(
                        write,
                        frame,
                        "config_divergence",
                        &format!("failed to build configure request: {error}"),
                    )
                    .await;
                }
            };
            let reconciled_ok = serde_json::to_value(&reconciled)
                .ok()
                .and_then(|v| v.get("success").and_then(Value::as_bool))
                .unwrap_or(false);

            if !reconciled_ok {
                let message = serde_json::to_value(&reconciled)
                    .ok()
                    .and_then(|v| v.get("message").and_then(|m| m.as_str().map(String::from)))
                    .unwrap_or_else(|| "configure failed during route bind".to_string());
                return send_route_bind_error(write, frame, "config_divergence", &message).await;
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
            write_frame(write, &response)
                .await
                .map_err(SubcError::FrameIo)?;
            log::info!("subc attach: route {route_channel} bound");
            Ok(())
        }
    }
}

async fn send_route_bind_error<W>(
    write: &mut W,
    frame: &Frame,
    code: &str,
    message: &str,
) -> Result<(), SubcError>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(SubcError::Json)?;
    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Error,
        control_flags(),
        0,
        frame.header.corr,
        body,
    )
    .map_err(SubcError::FrameBuild)?;
    write_frame(write, &response)
        .await
        .map_err(SubcError::FrameIo)?;
    log::warn!("subc attach: route bind rejected ({code}): {message}");
    Ok(())
}

/// Route-channel tool call: `{name, arguments}` → dispatch to the sync command
/// core → wrap the structured Response in a CallToolResult `{content, isError}`.
/// v1 mapping: the whole `{success, ...}` Response serialized into ONE text
/// block; `isError` carries `success == false`.
async fn handle_tool_call<W>(
    write: &mut W,
    frame: &Frame,
    ctx: &AppContext,
    dispatch: DispatchFn,
) -> Result<(), SubcError>
where
    W: AsyncWrite + Unpin,
{
    let call = serde_json::from_slice::<ToolCallRequest>(&frame.body).map_err(SubcError::Json)?;

    // Build a RawRequest: {id, command: name, ...arguments}.
    let mut map = call.arguments.as_object().cloned().unwrap_or_default();
    map.insert(
        "id".to_string(),
        json!(format!(
            "subc-{}-{}",
            frame.header.channel, frame.header.corr
        )),
    );
    map.insert("command".to_string(), json!(call.name));

    let response = match serde_json::from_value::<RawRequest>(Value::Object(map)) {
        Ok(req) => dispatch(req, ctx),
        Err(error) => Response::error(
            &format!("subc-{}-{}", frame.header.channel, frame.header.corr),
            "invalid_request",
            format!("failed to build request from tool call: {error}"),
        ),
    };

    let response_value = serde_json::to_value(&response).map_err(SubcError::Json)?;
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

    let response_frame = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        frame.header.flags,
        frame.header.channel,
        frame.header.corr,
        body,
    )
    .map_err(SubcError::FrameBuild)?;
    write_frame(write, &response_frame)
        .await
        .map_err(SubcError::FrameIo)?;
    Ok(())
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
