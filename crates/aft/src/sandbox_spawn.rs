//! Policy seam for processes that execute agent-provided shell commands.
//!
//! Every agent bash process reaches [`resolve_sandbox_spawn`] and carries the
//! resulting [`SpawnPlan`] into one of the two process-creation primitives:
//! detached pipes or PTY. Foreground orchestration still uses the same detached
//! registry, so it does not create a third path. This module deliberately keeps
//! policy disabled for now: production resolution returns
//! [`SpawnPlan::Unsandboxed`] on every platform, and Windows will continue to do
//! so when native sandbox policy is introduced.
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
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use portable_pty::CommandBuilder;

use crate::context::AppContext;
use crate::sandbox_profile::SandboxProfile;

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

/// Sandbox tier requested by the caller. Native policy is wired in a later step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedSandboxTier {
    Disabled,
    Native,
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
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
    },
    Refused {
        code: &'static str,
    },
}

impl SpawnPlan {
    pub(crate) fn refusal_code(&self) -> Option<&'static str> {
        match self {
            Self::Refused { code } => Some(code),
            Self::Unsandboxed | Self::Launcher { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self {
        Self::Launcher {
            profile,
            launcher_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn refused_for_test(code: &'static str) -> Self {
        Self::Refused { code }
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

/// Resolve policy for an agent-command process.
///
/// Resolution is total. [`SpawnPlan::Refused`] is available for future policy
/// that cannot establish a containment backend or authenticated principal. This
/// plumbing step intentionally returns [`SpawnPlan::Unsandboxed`] for all
/// production callers, including every Windows caller.
pub fn resolve_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
) -> SpawnPlan {
    note_test_observation(ctx, principal, requested_tier, task_kind);

    #[cfg(windows)]
    {
        return SpawnPlan::Unsandboxed;
    }

    #[cfg(not(windows))]
    {
        #[cfg(test)]
        if let Some(plan) = TEST_PLAN_OVERRIDE.with(|slot| slot.borrow().clone()) {
            return plan;
        }

        let _ = (principal, requested_tier, task_kind);
        SpawnPlan::Unsandboxed
    }
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
) -> Result<Command, String> {
    let (program, args) = command_argv_for_plan(plan, program, args, task_marker)?;
    let mut command = crate::effective_path::new_command(program);
    command.args(args);
    Ok(command)
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
    let (program, args) = command_argv_for_plan(plan, program, args, task_marker)?;
    let mut command = CommandBuilder::new(program);
    for arg in args {
        command.arg(arg);
    }
    command.cwd(workdir.as_os_str());
    for (key, value) in env {
        command.env(key, value);
    }
    Ok(command)
}

fn command_argv_for_plan(
    plan: &SpawnPlan,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
) -> Result<(OsString, Vec<OsString>), String> {
    match plan {
        SpawnPlan::Unsandboxed => Ok((program.to_os_string(), args.to_vec())),
        SpawnPlan::Refused { code } => Err((*code).to_string()),
        SpawnPlan::Launcher {
            profile,
            launcher_path,
        } => launcher_argv(profile, launcher_path, program, args, task_marker),
    }
}

#[cfg(unix)]
fn launcher_argv(
    profile: &SandboxProfile,
    launcher_path: &Path,
    program: &OsStr,
    args: &[OsString],
    task_marker: &Path,
) -> Result<(OsString, Vec<OsString>), String> {
    let profile_path = write_profile_file(profile, task_marker)?;
    let mut wrapped = vec![
        OsString::from("-c"),
        OsString::from(
            r#"profile_path=$1
shift
exec 9<"$profile_path" || exit 78
rm -f -- "$profile_path"
exec "$@""#,
        ),
        OsString::from("aft-sandbox-spawn"),
        profile_path.into_os_string(),
        launcher_path.as_os_str().to_os_string(),
        OsString::from("--profile-fd"),
        OsString::from("9"),
        OsString::from("--"),
        program.to_os_string(),
    ];
    wrapped.extend_from_slice(args);
    Ok((OsString::from("/bin/sh"), wrapped))
}

#[cfg(not(unix))]
fn launcher_argv(
    _profile: &SandboxProfile,
    _launcher_path: &Path,
    _program: &OsStr,
    _args: &[OsString],
    _task_marker: &Path,
) -> Result<(OsString, Vec<OsString>), String> {
    Err("sandbox_unavailable".to_string())
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
