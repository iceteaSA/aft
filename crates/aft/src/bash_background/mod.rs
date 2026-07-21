//! Background bash task management: spawning detached tasks, the watchdog that
//! reaps them, output buffering/compression, and on-disk persistence so tasks
//! survive a bridge restart.

pub mod buffer;
pub mod output;
pub mod persistence;
pub mod process;
pub mod pty_process;
pub mod pty_runtime;
pub mod registry;
pub mod watchdog;
pub mod watches;

use crate::bash_permissions::PermissionAsk;
use crate::context::AppContext;
use crate::protocol::Response;
#[cfg(unix)]
use crate::sandbox_spawn::native_sandbox_enforced;
use crate::sandbox_spawn::{
    current_authenticated_principal, resolve_sandbox_spawn, HostEscalationAttempt,
    RequestedSandboxTier, SandboxTaskKind,
};
use persistence::BgMode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub use registry::{BgCompletion, BgTaskHealthCounts, BgTaskRegistry};

#[cfg(unix)]
pub(crate) fn resolved_shell_path(pty: bool) -> PathBuf {
    if pty {
        pty_process::resolve_posix_shell()
    } else {
        registry::resolve_posix_shell()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BgTaskInfo {
    pub task_id: String,
    pub status: BgTaskStatus,
    pub command: String,
    pub mode: BgMode,
    pub started_at: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BgTaskStatus {
    Starting,
    Running,
    Killing,
    Completed,
    Failed,
    Killed,
    TimedOut,
}

impl BgTaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BgTaskStatus::Completed
                | BgTaskStatus::Failed
                | BgTaskStatus::Killed
                | BgTaskStatus::TimedOut
        )
    }
}

/// Spawn a bash command in the background. Returns a task_id immediately.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    request_id: &str,
    session_id: &str,
    command: &str,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    timeout_ms: Option<u64>,
    ctx: &AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
    pty: bool,
    pty_rows: u16,
    pty_cols: u16,
    scanner_report: Vec<PermissionAsk>,
    host_escalation: Option<HostEscalationAttempt>,
) -> Response {
    if require_background_flag && !ctx.config().experimental_bash_background {
        return Response::error(
            request_id,
            "feature_disabled",
            "background bash is disabled; set `bash: { background: true }` (or `bash: true`) in aft.jsonc",
        );
    }

    let workdir = workdir.unwrap_or_else(|| {
        ctx.config().project_root.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
    });
    let storage_dir = task_storage_dir(ctx);
    let max_running = ctx.config().max_background_bash_tasks;
    let timeout = timeout_ms.map(Duration::from_millis);
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));

    let env = env.unwrap_or_default();
    let task_kind = if pty {
        SandboxTaskKind::BashPty
    } else if require_background_flag {
        SandboxTaskKind::BashBackground
    } else {
        SandboxTaskKind::BashForeground
    };
    let principal = current_authenticated_principal();
    let requested_tier = if host_escalation.is_some() {
        RequestedSandboxTier::Host
    } else if ctx.config().sandbox.enabled {
        RequestedSandboxTier::Native
    } else {
        RequestedSandboxTier::Disabled
    };
    let session_dir = persistence::session_tasks_dir(&storage_dir, session_id);
    #[cfg(unix)]
    let (spawn_plan, unregistered_task) = if native_sandbox_enforced(ctx, &principal)
        && host_escalation.is_none()
    {
        let task = match persistence::allocate_task_layout(&storage_dir, session_id) {
            Ok(task) => task,
            Err(error) => {
                return Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed to create the task artifact directory: {error}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                );
            }
        };
        let plan = resolve_sandbox_spawn(
            ctx,
            &principal,
            requested_tier,
            task_kind,
            &task.paths.io_dir,
            None,
        );
        if plan.refusal_code().is_some() {
            (plan, Some(task))
        } else {
            let shell_path = resolved_shell_path(pty);
            let root = project_root.as_deref().unwrap_or(&workdir);
            let environment = crate::sandbox_spawn::approved_environment_for_plan(&plan, &env);
            match crate::sandbox_spawn::prepare_task_payload(
                &task,
                command.as_bytes(),
                root,
                &workdir,
                &principal,
                &shell_path,
                &environment,
            ) {
                Ok(prepared) => (plan.with_prepared_task(prepared), Some(task)),
                Err(error) => {
                    let _ = persistence::delete_resolved_task(&task);
                    return Response::error(
                        request_id,
                        "sandbox_unavailable",
                        format!("native sandbox failed to materialize task payload: {error}"),
                    );
                }
            }
        }
    } else {
        (
            resolve_sandbox_spawn(
                ctx,
                &principal,
                requested_tier,
                task_kind,
                &session_dir,
                host_escalation.as_ref(),
            ),
            None,
        )
    };
    #[cfg(not(unix))]
    let spawn_plan = resolve_sandbox_spawn(
        ctx,
        &principal,
        requested_tier,
        task_kind,
        &session_dir,
        host_escalation.as_ref(),
    );
    if let Some(code) = spawn_plan.refusal_code() {
        #[cfg(unix)]
        if let Some(task) = unregistered_task.as_ref() {
            let _ = persistence::delete_resolved_task(task);
        }
        let message = spawn_plan
            .refusal_message()
            .unwrap_or("bash process creation refused by sandbox policy");
        return match spawn_plan.refusal_mismatch_class() {
            Some(class) => Response::error_with_data(
                request_id,
                code,
                message,
                json!({ "mismatch_class": class }),
            ),
            None => Response::error(request_id, code, message),
        };
    }

    let cleanup_plan = spawn_plan.clone();
    let spawn_result = if pty {
        ctx.bash_background().spawn_pty(
            spawn_plan,
            command,
            session_id.to_string(),
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
            pty_rows,
            pty_cols,
        )
    } else {
        ctx.bash_background().spawn(
            spawn_plan,
            command,
            session_id.to_string(),
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
        )
    };

    match spawn_result {
        Ok(task_id) => {
            if let Err(error) =
                ctx.bash_background()
                    .record_scanner_report(&task_id, session_id, scanner_report)
            {
                crate::slog_warn!("{error}");
            }
            Response::success(
                request_id,
                json!({
                    "task_id": task_id,
                    "status": BgTaskStatus::Running,
                    "mode": if pty { "pty" } else { "pipes" },
                }),
            )
        }
        Err(message) if message.contains("limit exceeded") => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(message) => {
            cleanup_plan.cleanup_unspawned();
            #[cfg(unix)]
            if let Some(task) = unregistered_task.as_ref() {
                let _ = persistence::delete_resolved_task(task);
            }
            if cleanup_plan.is_native_launcher() {
                Response::error(
                    request_id,
                    "sandbox_unavailable",
                    format!(
                        "native sandbox failed before command execution: {message}; set sandbox.enabled=false to disable native sandboxing"
                    ),
                )
            } else {
                Response::error(request_id, "execution_failed", message)
            }
        }
    }
}

pub(crate) fn task_storage_dir(ctx: &AppContext) -> PathBuf {
    let config = ctx.config();
    let root = storage_dir(config.storage_dir.as_deref());
    config
        .harness
        .as_ref()
        .map(|harness| root.join(harness.storage_segment()))
        .unwrap_or(root)
}

pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    if let Some(dir) = configured {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(dir).join("aft");
    }
    // Default to the CortexKit shared data root — the SAME location the
    // plugins inject as `storage_dir` on every configure. Before this, the
    // fallback was the pre-migration `~/.cache/aft`, which only plugin-less
    // invocations ever hit; once the daemon-supervised module became such an
    // invocation it built a parallel storage universe there (duplicate
    // trigram/semantic/callgraph caches AND a separate artifact-owner
    // manifest, so cross-front leases could not see each other). Keep this
    // in sync with resolveCortexKitStorageRoot in the TS packages.
    cortexkit_data_root().join("cortexkit").join("aft")
}

fn cortexkit_data_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));
    }
    home.join(".local").join("share")
}

pub fn repair_legacy_root_tasks(storage_root: &std::path::Path, harness: crate::harness::Harness) {
    let root_tasks = storage_root.join("bash-tasks");
    if !dir_has_entries(&root_tasks) {
        return;
    }

    let harness_tasks = storage_root
        .join(harness.storage_segment())
        .join("bash-tasks");
    if dir_has_entries(&harness_tasks) {
        return;
    }
    if let Some(parent) = harness_tasks.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            crate::slog_warn!(
                "failed to create harness bash task dir {}: {}",
                parent.display(),
                error
            );
            return;
        }
    }
    if harness_tasks.exists() {
        let _ = std::fs::remove_dir(&harness_tasks);
    }

    match std::fs::rename(&root_tasks, &harness_tasks) {
        Ok(()) => crate::slog_info!(
            "moved legacy root bash tasks into harness namespace: {}",
            harness_tasks.display()
        ),
        Err(error) => {
            crate::slog_warn!(
                "failed to move legacy root bash tasks into {}: {}; trying child merge",
                harness_tasks.display(),
                error
            );
            if std::fs::create_dir_all(&harness_tasks).is_err() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(&root_tasks) {
                for entry in entries.flatten() {
                    let source = entry.path();
                    let target = harness_tasks.join(entry.file_name());
                    if !target.exists() {
                        let _ = std::fs::rename(source, target);
                    }
                }
            }
            let _ = std::fs::remove_dir(&root_tasks);
        }
    }
}

fn dir_has_entries(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod storage_root_tests {
    use std::path::PathBuf;

    // The plugins inject the CortexKit data root as `storage_dir` on every
    // configure; plugin-less invocations (daemon-supervised module, bare CLI,
    // warmup) hit the Rust fallback below instead. If the two ever diverge
    // again, every front pays duplicate cold indexes AND artifact-owner
    // manifests split across universes so cross-front ReadOnly leasing goes
    // blind (the v0.45.x daemon-module regression: the fallback still pointed
    // at pre-migration ~/.cache/aft). This locks all Rust fallback sites to
    // the same resolution the TS packages use (resolveCortexKitStorageRoot:
    // XDG_DATA_HOME || platform data dir, + cortexkit/aft).
    #[test]
    fn plugin_less_fallback_matches_plugin_injected_cortexkit_root() {
        let _guard = crate::test_env::process_env_lock();
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(PathBuf::from)
                    .expect("test environment provides a home directory");
                if cfg!(windows) {
                    std::env::var_os("LOCALAPPDATA")
                        .or_else(|| std::env::var_os("APPDATA"))
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.join("AppData").join("Local"))
                } else {
                    home.join(".local").join("share")
                }
            });
        let expected_plugin_injected_root = data_home.join("cortexkit").join("aft");

        let cache_dir_override_absent = std::env::var_os("AFT_CACHE_DIR").is_none();
        assert!(
            cache_dir_override_absent,
            "test requires AFT_CACHE_DIR unset to exercise the real fallback"
        );

        assert_eq!(
            super::storage_dir(None),
            expected_plugin_injected_root,
            "bash_background::storage_dir fallback diverged from the plugin-injected root"
        );

        let temp_project = tempfile::tempdir().expect("temp project");
        let resolved = crate::search_index::resolve_cache_dir(temp_project.path(), None);
        assert!(
            resolved.starts_with(&expected_plugin_injected_root),
            "search_index::resolve_cache_dir fallback diverged: {}",
            resolved.display()
        );
    }
}
