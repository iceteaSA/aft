//! subc daemon attach — transport edge (P5a).
//!
//! When AFT is launched as `aft --subc <connection-file>`, it does NOT run the
//! standalone NDJSON-over-stdin loop. Instead it connects to a running subc
//! daemon over loopback TCP and authenticates with the pre-envelope HMAC
//! handshake (both provided by the published `subc-transport` crate).
//!
//! This is the P5a connect+auth half. The frame-level control loop (ModuleHello
//! → HelloAck → route.bind → tool-call serving) is wired once the frame codec
//! (`Frame` + `read_frame`/`write_frame`) ships in subc-protocol 0.3.0 /
//! subc-transport 0.2.0. Until then this module proves the async-subc ↔
//! sync-AFT boundary: tokio runs ONLY here, at the transport edge; AFT's
//! command core stays synchronous and is reached (later) over a channel,
//! exactly as the standalone stdin-reader thread feeds dispatch today.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use subc_transport::{authenticate_client, connection_file, AuthError, ConnectionFileError};
use tokio::net::TcpStream;

/// Handshake budget. subc binds-before-spawn, so a reachable daemon authenticates
/// well within this; an unreachable/socket-stale daemon fails loud rather than
/// silently downgrading to standalone (the SUBC_SOCKET/--subc contract).
const AUTH_DEADLINE: Duration = Duration::from_secs(5);

/// Entry point for `aft --subc <connection-file>`. Synchronous on the outside
/// (called from `main`); owns an isolated current-thread tokio runtime for the
/// async transport. Returns `Err` (fail-loud) on any connect/auth failure — we
/// never fall back to the standalone loop, to avoid split-brain index state.
pub fn run_subc_mode(connection_file_path: &Path) -> Result<(), SubcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SubcError::Runtime)?;

    runtime.block_on(async move {
        let stream = connect_and_authenticate(connection_file_path).await?;
        // P5a connect+auth half proven: authenticated session established.
        // The frame-level handshake (Hello onward) lands with the published
        // codec. For now, hold the authenticated stream open briefly so the
        // daemon observes a clean authenticated connection, then drop it.
        log::info!(
            "subc attach: authenticated to daemon via {}",
            connection_file_path.display()
        );
        drop(stream);
        Ok(())
    })
}

/// Mirror of the reference `fake-aft-stub::connect_to_subc`: read the connection
/// file → resolve the first endpoint → TCP connect → HMAC handshake.
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

#[derive(Debug)]
pub enum SubcError {
    Runtime(std::io::Error),
    ConnectionFile {
        path: PathBuf,
        source: ConnectionFileError,
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
        source: AuthError,
    },
}

impl fmt::Display for SubcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "failed to build subc tokio runtime: {e}"),
            Self::ConnectionFile { path, source } => {
                write!(
                    f,
                    "failed to read subc connection file {:?}: {source}",
                    path
                )
            }
            Self::NoEndpoint { path } => {
                write!(f, "subc connection file {:?} has no endpoints", path)
            }
            Self::InvalidEndpoint { path, endpoint } => write!(
                f,
                "subc connection file {:?} has invalid endpoint {endpoint}",
                path
            ),
            Self::Connect { endpoint, source } => {
                write!(f, "failed to connect to subc endpoint {endpoint}: {source}")
            }
            Self::Auth { endpoint, source } => {
                write!(
                    f,
                    "failed to authenticate to subc endpoint {endpoint}: {source}"
                )
            }
        }
    }
}

impl std::error::Error for SubcError {}
