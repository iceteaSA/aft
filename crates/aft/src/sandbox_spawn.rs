//! Policy and process wiring for agent-provided shell commands.
//!
//! Every agent bash process reaches [`resolve_sandbox_spawn`] and carries the
//! resulting [`SpawnPlan`] into one of the two process-creation primitives:
//! detached pipes or PTY. Foreground orchestration uses the detached registry
//! too, so it does not create a third process-creation path.
//!
//! AFT also starts processes for its own implementation. Those are outside this
//! seam because they do not execute an agent command: external formatters,
//! linters, and type checkers in `format`; LSP servers and Windows LSP cleanup in
//! `lsp::client` and `lsp::child_registry`; git probes in `search_index`,
//! `readonly_artifacts`, `commands::configure`, and `commands::conflicts`; login
//! shell PATH discovery in `effective_path`; and Windows process liveness or
//! termination helpers in `artifact_owner`, `fs_lock`, and
//! `bash_background::process`. Image and PDF handling in `commands::read` is
//! in-process and creates no child. Keeping this inventory here makes the
//! agent-command boundary explicit without accidentally applying agent policy to
//! AFT's internal tooling.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::{DirBuilder, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use crate::context::AppContext;
use crate::sandbox_profile::SandboxProfile;

pub const SANDBOX_UNAVAILABLE_EXIT_CODE: i32 = 78;

/// Server-authenticated trust classification for a route bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalTrust {
    FirstParty,
    Untrusted,
}

/// Principal data supplied by the server-side transport, never by a bash body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticatedPrincipal {
    /// Standalone NDJSON and first-party plugin bindings have no route identity.
    FirstParty,
    /// Identity captured from an authenticated subc route bind.
    RouteBind {
        trust: PrincipalTrust,
        route_channel: u16,
        route_epoch: u32,
        project_root: PathBuf,
        harness: String,
        session_id: String,
        /// Server principal label (`direct`, `reserved:<module>`, or
        /// `unverified`). `None` preserves an absent principal for future
        /// fail-closed policy instead of silently treating it as first-party.
        principal_id: Option<String>,
    },
}

/// Sandbox tier requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedSandboxTier {
    Disabled,
    Native,
    Host,
}

pub(crate) type ChildEnvironment = BTreeMap<OsString, OsString>;

#[cfg(unix)]
const ESCALATION_GRANT_TTL: Duration = Duration::from_secs(120);
#[cfg(unix)]
const ESCALATION_DIGEST_TAG: &[u8] = b"aft-escalation-v1";
#[cfg(unix)]
const ESCALATION_TIER: &[u8] = b"host";

#[cfg(unix)]
#[derive(Debug, Clone)]
struct EscalationGrant {
    principal: AuthenticatedPrincipal,
    root: PathBuf,
    digest: blake3::Hash,
    expires_at: Instant,
    consumed: bool,
    environment: ChildEnvironment,
}

#[cfg(unix)]
#[derive(Debug, Default)]
pub(crate) struct EscalationGrantStore {
    grants: HashMap<String, EscalationGrant>,
}

#[cfg(unix)]
impl EscalationGrantStore {
    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.grants.len()
    }
}

#[derive(Debug, Clone)]
pub struct HostEscalationAttempt {
    pub grant_id: String,
    pub command: Vec<u8>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub shell_path: PathBuf,
    pub environment: ChildEnvironment,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationRefusal {
    Expired,
    Consumed,
    DigestMismatch,
    WrongPrincipal,
}

#[cfg(unix)]
impl EscalationRefusal {
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Consumed => "consumed",
            Self::DigestMismatch => "digest_mismatch",
            Self::WrongPrincipal => "wrong_principal",
        }
    }
}

/// Agent-command path that is about to create a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTaskKind {
    BashForeground,
    BashBackground,
    BashPty,
}

/// Complete process-launch decision consumed by a spawn primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnPlan {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
        environment: ChildEnvironment,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
    Refused {
        code: &'static str,
        message: String,
        mismatch_class: Option<&'static str>,
    },
}

impl SpawnPlan {
    pub(crate) fn refusal_code(&self) -> Option<&'static str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            Self::Unsandboxed | Self::Host { .. } | Self::Launcher { .. } => None,
        }
    }

    pub(crate) fn refusal_message(&self) -> Option<&str> {
        match self {
            Self::Refused { message, .. } => Some(message),
            Self::Unsandboxed | Self::Host { .. } | Self::Launcher { .. } => None,
        }
    }

    pub(crate) fn refusal_mismatch_class(&self) -> Option<&'static str> {
        match self {
            Self::Refused { mismatch_class, .. } => *mismatch_class,
            Self::Unsandboxed | Self::Host { .. } | Self::Launcher { .. } => None,
        }
    }

    pub(crate) fn is_native_launcher(&self) -> bool {
        matches!(self, Self::Launcher { .. })
    }

    pub(crate) fn host_environment(&self) -> Option<&ChildEnvironment> {
        match self {
            Self::Host { environment, .. } => Some(environment),
            Self::Unsandboxed | Self::Launcher { .. } | Self::Refused { .. } => None,
        }
    }

    #[cfg(unix)]
    pub(crate) fn host_shell_path(&self) -> Option<&Path> {
        match self {
            Self::Host { shell_path, .. } => Some(shell_path),
            Self::Unsandboxed | Self::Launcher { .. } | Self::Refused { .. } => None,
        }
    }

    pub(crate) fn temp_dir(&self) -> Option<&Path> {
        match self {
            Self::Launcher { profile, .. } => Some(&profile.temp_dir),
            Self::Unsandboxed | Self::Host { .. } | Self::Refused { .. } => None,
        }
    }

    pub(crate) fn cleanup_unspawned(&self) {
        let Some(temp_dir) = self.temp_dir() else {
            return;
        };
        if is_managed_task_temp_dir(temp_dir) {
            let _ = std::fs::remove_dir_all(temp_dir);
        }
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self {
        Self::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn refused_for_test(code: &'static str) -> Self {
        Self::Refused {
            code,
            message: format!("bash process creation refused by sandbox policy: {code}"),
            mismatch_class: None,
        }
    }
}

/// One resolver invocation captured by the project-keyed test seam.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpawnObservation {
    pub principal: AuthenticatedPrincipal,
    pub requested_tier: RequestedSandboxTier,
    pub task_kind: SandboxTaskKind,
}

static TEST_OBSERVATIONS: OnceLock<Mutex<HashMap<PathBuf, Vec<SandboxSpawnObservation>>>> =
    OnceLock::new();

thread_local! {
    static CURRENT_PRINCIPAL: RefCell<Option<AuthenticatedPrincipal>> = const { RefCell::new(None) };
    #[cfg(test)]
    static TEST_PLAN_OVERRIDE: RefCell<Option<SpawnPlan>> = const { RefCell::new(None) };
}

struct PrincipalScope(Option<AuthenticatedPrincipal>);

impl Drop for PrincipalScope {
    fn drop(&mut self) {
        CURRENT_PRINCIPAL.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

/// Run dispatch with server-owned principal data installed for bash resolution.
pub(crate) fn with_authenticated_principal<R>(
    principal: AuthenticatedPrincipal,
    run: impl FnOnce() -> R,
) -> R {
    let previous = CURRENT_PRINCIPAL.with(|slot| slot.replace(Some(principal)));
    let _scope = PrincipalScope(previous);
    run()
}

/// Current dispatch principal. Standalone requests are first-party by construction.
pub(crate) fn current_authenticated_principal() -> AuthenticatedPrincipal {
    CURRENT_PRINCIPAL
        .with(|slot| slot.borrow().clone())
        .unwrap_or(AuthenticatedPrincipal::FirstParty)
}

pub(crate) fn principal_is_first_party(principal: &AuthenticatedPrincipal) -> bool {
    matches!(
        principal,
        AuthenticatedPrincipal::FirstParty
            | AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::FirstParty,
                ..
            }
    )
}

#[cfg(unix)]
pub(crate) fn capture_child_environment(overrides: &HashMap<String, String>) -> ChildEnvironment {
    let mut environment = std::env::vars_os().collect::<ChildEnvironment>();
    for (key, value) in overrides {
        environment.insert(OsString::from(key), OsString::from(value));
    }
    environment
}

#[cfg(unix)]
pub(crate) fn mint_host_escalation_grant(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<String, String> {
    mint_host_escalation_grant_at(
        ctx,
        principal,
        command,
        root,
        cwd,
        shell_path,
        environment,
        Instant::now(),
    )
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn mint_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    now: Instant,
) -> Result<String, String> {
    let mut store = ctx.escalation_grants().lock();
    let grant_id = loop {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| format!("failed to mint sandbox escalation grant: {error}"))?;
        let candidate = format!("esc_{}", hex_bytes(&random));
        if !store.grants.contains_key(&candidate) {
            break candidate;
        }
    };
    let grant = EscalationGrant {
        principal: principal.clone(),
        root: root.to_path_buf(),
        digest: escalation_digest(command, root, cwd, principal, shell_path, environment),
        expires_at: now + ESCALATION_GRANT_TTL,
        consumed: false,
        environment: environment.clone(),
    };
    store.grants.insert(grant_id.clone(), grant);
    Ok(grant_id)
}

#[cfg(unix)]
fn consume_host_escalation_grant_at(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    attempt: &HostEscalationAttempt,
    now: Instant,
) -> Result<ChildEnvironment, EscalationRefusal> {
    let mut store = ctx.escalation_grants().lock();
    let Some(grant) = store.grants.get_mut(&attempt.grant_id) else {
        return Err(EscalationRefusal::DigestMismatch);
    };
    if grant.principal != *principal {
        return Err(EscalationRefusal::WrongPrincipal);
    }
    if grant.root != attempt.root {
        grant.consumed = true;
        grant.environment.clear();
        return Err(EscalationRefusal::DigestMismatch);
    }
    if now >= grant.expires_at {
        grant.environment.clear();
        return Err(EscalationRefusal::Expired);
    }
    if grant.consumed {
        return Err(EscalationRefusal::Consumed);
    }
    let digest = escalation_digest(
        &attempt.command,
        &attempt.root,
        &attempt.cwd,
        principal,
        &attempt.shell_path,
        &attempt.environment,
    );
    if digest != grant.digest {
        grant.consumed = true;
        grant.environment.clear();
        return Err(EscalationRefusal::DigestMismatch);
    }
    grant.consumed = true;
    Ok(std::mem::take(&mut grant.environment))
}

#[cfg(unix)]
fn escalation_digest(
    command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, ESCALATION_DIGEST_TAG);
    hash_field(&mut hasher, command);
    hash_field(&mut hasher, &os_bytes(root.as_os_str()));
    hash_field(&mut hasher, &os_bytes(cwd.as_os_str()));
    hash_principal(&mut hasher, principal);
    hash_field(&mut hasher, &os_bytes(shell_path.as_os_str()));
    hash_field(&mut hasher, env!("CARGO_PKG_VERSION").as_bytes());
    hash_field(&mut hasher, ESCALATION_TIER);
    hasher.update(&(environment.len() as u64).to_be_bytes());
    for (key, value) in environment {
        hash_field(&mut hasher, &os_bytes(key));
        hash_field(&mut hasher, &os_bytes(value));
    }
    hasher.finalize()
}

#[cfg(unix)]
fn hash_principal(hasher: &mut blake3::Hasher, principal: &AuthenticatedPrincipal) {
    match principal {
        AuthenticatedPrincipal::FirstParty => hash_field(hasher, b"first_party"),
        AuthenticatedPrincipal::RouteBind {
            trust,
            route_channel,
            route_epoch,
            project_root,
            harness,
            session_id,
            principal_id,
        } => {
            hash_field(hasher, b"route_bind");
            hash_field(
                hasher,
                match trust {
                    PrincipalTrust::FirstParty => b"first_party",
                    PrincipalTrust::Untrusted => b"untrusted",
                },
            );
            hasher.update(&route_channel.to_be_bytes());
            hasher.update(&route_epoch.to_be_bytes());
            hash_field(hasher, &os_bytes(project_root.as_os_str()));
            hash_field(hasher, harness.as_bytes());
            hash_field(hasher, session_id.as_bytes());
            match principal_id {
                Some(id) => {
                    hasher.update(&[1]);
                    hash_field(hasher, id.as_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
    }
}

#[cfg(unix)]
fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(unix)]
fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Whether permission-scanner findings should be recorded instead of prompting.
pub(crate) fn native_sandbox_enforced(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
) -> bool {
    cfg!(unix) && ctx.config().sandbox.enabled && principal_is_first_party(principal)
}

/// Resolve policy for an agent-command process.
///
/// `task_bundle_dir` must be the already-created directory that owns the task's
/// capture files. The native builder creates a fresh private temp directory
/// beneath it and includes both directories in the profile.
pub fn resolve_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
) -> SpawnPlan {
    note_test_observation(ctx, principal, requested_tier, task_kind);

    // This check must remain ahead of every platform and production-policy
    // branch. Windows tests use it to avoid entering process paths that cannot
    // consume native launcher plans.
    #[cfg(test)]
    if let Some(plan) = TEST_PLAN_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return plan;
    }

    if requested_tier == RequestedSandboxTier::Disabled || !ctx.config().sandbox.enabled {
        return SpawnPlan::Unsandboxed;
    }

    if requested_tier == RequestedSandboxTier::Host {
        if !principal_is_first_party(principal) {
            return SpawnPlan::Refused {
                code: "sandbox_escalation_denied",
                message: "sandbox host escalation is unavailable to untrusted principals"
                    .to_string(),
                mismatch_class: Some("wrong_principal"),
            };
        }

        #[cfg(windows)]
        {
            warn_windows_unsupported_once();
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            return SpawnPlan::Unsandboxed;
        }

        #[cfg(unix)]
        {
            let Some(attempt) = host_escalation else {
                return escalation_refused(EscalationRefusal::DigestMismatch);
            };
            return match consume_host_escalation_grant_at(ctx, principal, attempt, Instant::now()) {
                Ok(environment) => SpawnPlan::Host {
                    shell_path: attempt.shell_path.clone(),
                    environment,
                },
                Err(refusal) => escalation_refused(refusal),
            };
        }

        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = (ctx, task_kind, task_bundle_dir, host_escalation);
            return SpawnPlan::Unsandboxed;
        }
    }

    if !principal_is_first_party(principal) {
        return SpawnPlan::Unsandboxed;
    }

    #[cfg(windows)]
    {
        warn_windows_unsupported_once();
        let _ = (ctx, task_kind, task_bundle_dir);
        SpawnPlan::Unsandboxed
    }

    #[cfg(unix)]
    {
        let profile = match build_native_profile(ctx, principal, task_bundle_dir) {
            Ok(profile) => profile,
            Err(error) => {
                return SpawnPlan::Refused {
                    code: "sandbox_unavailable",
                    message: format!(
                        "native sandbox setup failed: {error}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                    mismatch_class: None,
                };
            }
        };
        let launcher_path = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&profile.temp_dir);
                return SpawnPlan::Refused {
                    code: "sandbox_unavailable",
                    message: format!(
                        "native sandbox setup failed to locate the aft executable: {error}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                    mismatch_class: None,
                };
            }
        };
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(all(not(unix), not(windows)))]
    {
        let _ = (ctx, principal, task_kind, task_bundle_dir);
        SpawnPlan::Unsandboxed
    }
}

#[cfg(unix)]
fn escalation_refused(refusal: EscalationRefusal) -> SpawnPlan {
    let class = refusal.class();
    SpawnPlan::Refused {
        code: "sandbox_escalation_denied",
        message: format!("sandbox host escalation grant refused: {class}"),
        mismatch_class: Some(class),
    }
}

#[cfg(windows)]
fn warn_windows_unsupported_once() {
    let session = crate::log_ctx::current_session().unwrap_or_else(|| "<unknown-session>".into());
    if should_warn_windows_unsupported(&session) {
        crate::slog_warn!("sandbox.enabled is not supported on Windows");
    }
}

#[cfg(any(windows, test))]
fn should_warn_windows_unsupported(session: &str) -> bool {
    use std::collections::HashSet;

    static WARNED_SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WARNED_SESSIONS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .is_ok_and(|mut warned| warned.insert(session.to_string()))
}

#[cfg(unix)]
fn build_native_profile(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    task_bundle_dir: &Path,
) -> Result<SandboxProfile, String> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "HOME is not set, so credential and cache paths cannot be resolved".to_string()
        })?;
    if !home.is_absolute() {
        return Err(format!("HOME must be absolute: {}", home.display()));
    }

    let mut project_roots = Vec::new();
    if let Some(root) = &ctx.config().project_root {
        project_roots.push(root.clone());
    }
    if let AuthenticatedPrincipal::RouteBind { project_root, .. } = principal {
        if !project_roots.contains(project_root) {
            project_roots.push(project_root.clone());
        }
    }
    if project_roots.is_empty() {
        project_roots.push(
            std::env::current_dir()
                .map_err(|error| format!("failed to resolve the current project root: {error}"))?,
        );
    }

    for root in &project_roots {
        if !root.is_dir() {
            return Err(format!(
                "project root is not an existing directory: {}",
                root.display()
            ));
        }
    }
    if !task_bundle_dir.is_dir() {
        return Err(format!(
            "task artifact directory is not an existing directory: {}",
            task_bundle_dir.display()
        ));
    }

    let temp_dir = create_task_temp_dir(task_bundle_dir)?;
    let result = {
        let mut writable_roots = project_roots.clone();
        writable_roots.push(task_bundle_dir.to_path_buf());
        writable_roots.extend(
            ctx.config()
                .sandbox
                .write_allow
                .iter()
                .map(|path| expand_home(path, &home)),
        );

        let mut write_deny_nested = Vec::new();
        let mut read_deny = vec![
            home.join(".ssh"),
            home.join(".aws"),
            home.join(".gnupg"),
            home.join(".config/gcloud"),
            home.join(".azure"),
            home.join(".config/cortexkit"),
        ];
        for root in &project_roots {
            write_deny_nested.push(root.join(".git"));
            write_deny_nested.push(root.join(".cortexkit"));
            read_deny.push(root.join(".git/hooks"));
        }
        read_deny.extend(
            ctx.config()
                .sandbox
                .read_deny
                .iter()
                .map(|path| expand_home(path, &home)),
        );

        let mut cache_roots = vec![
            home.join(".cargo/registry"),
            home.join(".cargo/git"),
            home.join(".rustup/downloads"),
            home.join(".npm"),
            home.join(".bun/install/cache"),
            home.join(".cache/pip"),
            home.join(".cache/uv"),
            home.join(".cache/go-build"),
            home.join(".gradle/caches"),
            home.join(".m2/repository"),
        ];
        #[cfg(target_os = "macos")]
        cache_roots.extend([
            home.join("Library/Caches/pip"),
            home.join("Library/Caches/uv"),
            home.join("Library/Caches/go-build"),
        ]);
        cache_roots.retain(|path| path.is_dir());

        let mut socket_deny = vec![PathBuf::from("/var/run/docker.sock")];
        if let Some(agent_socket) =
            std::env::var_os("SSH_AUTH_SOCK").filter(|value| !value.is_empty())
        {
            socket_deny.push(PathBuf::from(agent_socket));
        }

        SandboxProfile::build(
            writable_roots,
            write_deny_nested,
            read_deny,
            socket_deny,
            cache_roots,
            temp_dir.clone(),
        )
        .map_err(|error| error.to_string())
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    result
}

#[cfg(unix)]
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let mut components = path.components();
    if components
        .next()
        .is_some_and(|component| component.as_os_str() == "~")
    {
        return components.fold(home.to_path_buf(), |resolved, component| {
            resolved.join(component.as_os_str())
        });
    }
    path.to_path_buf()
}

#[cfg(unix)]
fn create_task_temp_dir(task_bundle_dir: &Path) -> Result<PathBuf, String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = task_bundle_dir.join(format!(".sandbox-tmp-{}-{nonce}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                return path.canonicalize().map_err(|error| {
                    format!(
                        "failed to canonicalize task temp directory {}: {error}",
                        path.display()
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create task temp directory {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("failed to allocate a fresh task temp directory after 32 attempts".to_string())
}

pub(crate) fn is_managed_task_temp_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with(".sandbox-tmp-"))
}

fn note_test_observation(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
) {
    let Some(observations) = TEST_OBSERVATIONS.get() else {
        return;
    };
    let Some(project_root) = ctx.config().project_root.clone() else {
        return;
    };
    let project_root = observation_key(&project_root);
    if let Some(project) = observations
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .get_mut(&project_root)
    {
        project.push(SandboxSpawnObservation {
            principal: principal.clone(),
            requested_tier,
            task_kind,
        });
    }
}

/// Start recording resolver calls for one project root.
#[doc(hidden)]
pub fn install_sandbox_spawn_test_seam(project_root: PathBuf) {
    TEST_OBSERVATIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("sandbox spawn test observation mutex poisoned")
        .insert(observation_key(&project_root), Vec::new());
}

/// Snapshot resolver calls recorded for one project root.
#[doc(hidden)]
pub fn sandbox_spawn_test_observations(project_root: &Path) -> Vec<SandboxSpawnObservation> {
    TEST_OBSERVATIONS
        .get()
        .and_then(|observations| {
            observations
                .lock()
                .expect("sandbox spawn test observation mutex poisoned")
                .get(&observation_key(project_root))
                .cloned()
        })
        .unwrap_or_default()
}

/// Remove one project-root resolver test seam.
#[doc(hidden)]
pub fn clear_sandbox_spawn_test_seam(project_root: &Path) {
    if let Some(observations) = TEST_OBSERVATIONS.get() {
        observations
            .lock()
            .expect("sandbox spawn test observation mutex poisoned")
            .remove(&observation_key(project_root));
    }
}

fn observation_key(project_root: &Path) -> PathBuf {
    std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf())
}

#[cfg(test)]
pub(crate) fn with_spawn_plan_for_test<R>(plan: SpawnPlan, run: impl FnOnce() -> R) -> R {
    struct PlanScope(Option<SpawnPlan>);

    impl Drop for PlanScope {
        fn drop(&mut self) {
            TEST_PLAN_OVERRIDE.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }

    let previous = TEST_PLAN_OVERRIDE.with(|slot| slot.replace(Some(plan)));
    let _scope = PlanScope(previous);
    run()
}

/// Build a detached `Command` while enforcing the required launch plan.
///
/// Windows detached spawns route through the shell-candidate ladder, which
/// enforces the plan inline, so this helper is Unix-only.
#[cfg(unix)]
pub(crate) fn detached_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    exit_marker: &Path,
) -> Result<Command, String> {
    let (program, args) =
        command_argv_for_plan(plan, program, args, task_marker, Some(exit_marker))?;
    let mut command = crate::effective_path::new_command(program);
    command.args(args);
    Ok(command)
}

#[cfg(unix)]
pub(crate) fn apply_sandbox_environment(plan: &SpawnPlan, command: &mut Command) {
    if let Some(environment) = plan.host_environment() {
        command.env_clear().envs(environment);
    } else if let Some(temp_dir) = plan.temp_dir() {
        command.env("TMPDIR", temp_dir).env("TEMP", temp_dir);
    }
}

/// Build a PTY `CommandBuilder` while enforcing the required launch plan.
pub(crate) fn pty_command_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    workdir: &Path,
    env: &HashMap<String, String>,
) -> Result<CommandBuilder, String> {
    let (program, args) = command_argv_for_plan(plan, program, args, task_marker, None)?;
    let mut command = CommandBuilder::new(program);
    for arg in args {
        command.arg(arg);
    }
    command.cwd(workdir.as_os_str());
    if let Some(environment) = plan.host_environment() {
        command.env_clear();
        for (key, value) in environment {
            command.env(key, value);
        }
    } else {
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(temp_dir) = plan.temp_dir() {
            command.env("TMPDIR", temp_dir);
            command.env("TEMP", temp_dir);
        }
    }
    Ok(command)
}

fn command_argv_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    exit_marker: Option<&Path>,
) -> Result<(OsString, Vec<OsString>), String> {
    match plan {
        SpawnPlan::Unsandboxed | SpawnPlan::Host { .. } => {
            Ok((program.to_os_string(), args.to_vec()))
        }
        SpawnPlan::Refused { code, .. } => Err((*code).to_string()),
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => launcher_argv(
            profile,
            launcher_path,
            program,
            args,
            task_marker,
            exit_marker,
        ),
    }
}

#[cfg(unix)]
fn launcher_argv(
    profile: &SandboxProfile,
    launcher_path: &Path,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
    exit_marker: Option<&Path>,
) -> Result<(OsString, Vec<OsString>), String> {
    let profile_path = write_profile_file(profile, task_marker)?;
    let failure_marker = sandbox_failure_marker_path(task_marker)?;
    if let Some(exit_marker) = exit_marker {
        let mut wrapped = vec![
            OsString::from("-c"),
            OsString::from(
                r#"umask 077
launcher=$1
profile=$2
exit_marker=$3
failure_marker=$4
shift 4
"$launcher" sandbox-launch --profile-file "$profile" --failure-marker "$failure_marker" -- "$@"
code=$?
if [ "$code" -eq 78 ] && [ ! -e "$exit_marker" ]; then
  if [ ! -e "$failure_marker" ]; then
    printf "%s" sandbox_unavailable > "$failure_marker.tmp.$$" && mv -f "$failure_marker.tmp.$$" "$failure_marker"
  fi
  printf "%s" "$code" > "$exit_marker.tmp.$$" && mv -f "$exit_marker.tmp.$$" "$exit_marker"
fi
exit "$code""#,
            ),
            OsString::from("aft-sandbox-supervisor"),
            launcher_path.as_os_str().to_os_string(),
            profile_path.into_os_string(),
            exit_marker.as_os_str().to_os_string(),
            failure_marker.into_os_string(),
            program.to_os_string(),
        ];
        wrapped.extend_from_slice(args);
        return Ok((OsString::from("/bin/sh"), wrapped));
    }

    let mut wrapped = vec![
        OsString::from("sandbox-launch"),
        OsString::from("--profile-file"),
        profile_path.into_os_string(),
        OsString::from("--failure-marker"),
        failure_marker.into_os_string(),
        OsString::from("--"),
        program.to_os_string(),
    ];
    wrapped.extend_from_slice(args);
    Ok((launcher_path.as_os_str().to_os_string(), wrapped))
}

#[cfg(not(unix))]
fn launcher_argv(
    _profile: &SandboxProfile,
    _launcher_path: &Path,
    _program: &OsStr,
    _args: &[OsString],
    _task_marker: &Path,
    _exit_marker: Option<&Path>,
) -> Result<(OsString, Vec<OsString>), String> {
    Err("sandbox_unavailable".to_string())
}

#[cfg(unix)]
fn sandbox_failure_marker_path(task_marker: &Path) -> Result<PathBuf, String> {
    let parent = task_marker
        .parent()
        .ok_or_else(|| "sandbox task marker has no parent directory".to_string())?;
    let stem = task_marker
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("bash-task");
    Ok(parent.join(format!("{stem}.sandbox-unavailable")))
}

#[cfg(unix)]
fn write_profile_file(profile: &SandboxProfile, task_marker: &Path) -> Result<PathBuf, String> {
    let parent = task_marker
        .parent()
        .ok_or_else(|| "sandbox task marker has no parent directory".to_string())?;
    let stem = task_marker
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("bash-task");
    let path = parent.join(format!("{stem}.sandbox-profile.json"));
    let bytes = serde_json::to_vec(profile)
        .map_err(|error| format!("failed to serialize sandbox profile: {error}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("failed to create sandbox profile file: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("failed to write sandbox profile file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("failed to sync sandbox profile file: {error}"))?;
    std::fs::canonicalize(&path)
        .map_err(|error| format!("failed to canonicalize sandbox profile file: {error}"))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn host_plan_clears_inherited_environment_and_applies_snapshot() {
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let plan = SpawnPlan::Host {
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("test -z \"$SHOULD_DISAPPEAR\" && printf %s \"$APPROVED\"")
            .env("SHOULD_DISAPPEAR", "yes");
        apply_sandbox_environment(&plan, &mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"snapshot");
    }

    #[test]
    fn product_profile_file_is_private() {
        let root = tempfile::tempdir().unwrap();
        let task_dir = root.path().join("task");
        let project = root.path().join("project");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let profile = SandboxProfile::build(
            vec![project],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            temp,
        )
        .unwrap();
        let marker = task_dir.join("bash-private.json");

        let path = write_profile_file(&profile, &marker).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn context(project_root: PathBuf) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root),
                sandbox: crate::config::SandboxConfig {
                    enabled: true,
                    ..crate::config::SandboxConfig::default()
                },
                ..crate::config::Config::default()
            },
        )
    }

    #[test]
    fn untrusted_principal_never_enters_the_native_launcher() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-sandbox-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };

        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan, SpawnPlan::Unsandboxed);
    }

    #[cfg(unix)]
    fn grant_attempt(
        grant_id: String,
        project: &Path,
        command: &[u8],
        environment: ChildEnvironment,
    ) -> HostEscalationAttempt {
        HostEscalationAttempt {
            grant_id,
            command: command.to_vec(),
            root: project.to_path_buf(),
            cwd: project.to_path_buf(),
            shell_path: PathBuf::from("/bin/sh"),
            environment,
        }
    }

    #[cfg(unix)]
    fn mint_test_grant(
        ctx: &AppContext,
        principal: &AuthenticatedPrincipal,
        project: &Path,
        command: &[u8],
        environment: &ChildEnvironment,
        now: Instant,
    ) -> String {
        mint_host_escalation_grant_at(
            ctx,
            principal,
            command,
            project,
            project,
            Path::new("/bin/sh"),
            environment,
            now,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn refusal_class(plan: &SpawnPlan) -> Option<&'static str> {
        plan.refusal_mismatch_class()
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_exact_command_and_environment() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment = ChildEnvironment::from([
            (OsString::from("A"), OsString::from("one")),
            (OsString::from("B"), OsString::from("two")),
        ]);

        for (command, retry_environment) in [
            (b"printf approved!".as_slice(), environment.clone()),
            (
                b"printf approved".as_slice(),
                ChildEnvironment::from([
                    (OsString::from("A"), OsString::from("changed")),
                    (OsString::from("B"), OsString::from("two")),
                ]),
            ),
        ] {
            let grant_id = mint_test_grant(
                &ctx,
                &principal,
                project.path(),
                b"printf approved",
                &environment,
                Instant::now(),
            );
            let attempt = grant_attempt(grant_id, project.path(), command, retry_environment);
            let plan = resolve_sandbox_spawn(
                &ctx,
                &principal,
                RequestedSandboxTier::Host,
                SandboxTaskKind::BashForeground,
                project.path(),
                Some(&attempt),
            );
            assert_eq!(refusal_class(&plan), Some("digest_mismatch"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_is_single_use_and_expires() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::FirstParty;
        let environment =
            ChildEnvironment::from([(OsString::from("ONLY"), OsString::from("snapshot"))]);
        let grant_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let attempt = grant_attempt(grant_id, project.path(), b"true", environment.clone());
        let first = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert!(matches!(first, SpawnPlan::Host { .. }));
        let second = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&attempt),
        );
        assert_eq!(refusal_class(&second), Some("consumed"));

        let expired_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now() - ESCALATION_GRANT_TTL - Duration::from_millis(1),
        );
        let expired_attempt =
            grant_attempt(expired_id, project.path(), b"true", environment.clone());
        let expired = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&expired_attempt),
        );
        assert_eq!(refusal_class(&expired), Some("expired"));
    }

    #[cfg(unix)]
    #[test]
    fn escalation_grant_binds_principal_and_happy_path_reuses_snapshot() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 9,
            route_epoch: 2,
            project_root: project.path().to_path_buf(),
            harness: "opencode".to_string(),
            session_id: "session-x".to_string(),
            principal_id: Some("direct".to_string()),
        };
        let environment =
            ChildEnvironment::from([(OsString::from("APPROVED"), OsString::from("snapshot"))]);
        let wrong_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let wrong_attempt = grant_attempt(wrong_id, project.path(), b"true", environment.clone());
        let wrong = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&wrong_attempt),
        );
        assert_eq!(refusal_class(&wrong), Some("wrong_principal"));

        let happy_id = mint_test_grant(
            &ctx,
            &principal,
            project.path(),
            b"true",
            &environment,
            Instant::now(),
        );
        let happy_attempt = grant_attempt(happy_id, project.path(), b"true", environment.clone());
        let happy = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            Some(&happy_attempt),
        );
        match happy {
            SpawnPlan::Host {
                shell_path,
                environment: actual,
            } => {
                assert_eq!(shell_path, Path::new("/bin/sh"));
                assert_eq!(actual, environment);
            }
            other => panic!("expected host plan, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_host_request_refuses_without_minting_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let principal = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 7,
            route_epoch: 1,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "untrusted-escalation-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };
        let plan = resolve_sandbox_spawn(
            &ctx,
            &principal,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan.refusal_code(), Some("sandbox_escalation_denied"));
        assert!(ctx.escalation_grants().lock().grants.is_empty());
    }

    #[test]
    fn windows_unsupported_warning_is_once_per_session() {
        let suffix = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        let first = format!("windows-warning-first-{suffix}");
        let second = format!("windows-warning-second-{suffix}");
        assert!(should_warn_windows_unsupported(&first));
        assert!(!should_warn_windows_unsupported(&first));
        assert!(should_warn_windows_unsupported(&second));
    }

    #[cfg(windows)]
    #[test]
    fn enabled_host_request_is_unsandboxed_on_windows_without_a_grant() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Host,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan, SpawnPlan::Unsandboxed);
    }

    #[cfg(windows)]
    #[test]
    fn enabled_native_tier_is_unsandboxed_on_windows() {
        let project = tempfile::tempdir().unwrap();
        let ctx = context(project.path().to_path_buf());
        let plan = resolve_sandbox_spawn(
            &ctx,
            &AuthenticatedPrincipal::FirstParty,
            RequestedSandboxTier::Native,
            SandboxTaskKind::BashForeground,
            project.path(),
            None,
        );
        assert_eq!(plan, SpawnPlan::Unsandboxed);
    }
}
