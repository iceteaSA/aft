//! Background bash task management. Phase 0 stub; Phase 1 Track D fills in.

pub mod buffer;
pub mod output;
pub mod persistence;
pub mod process;
pub mod pty_process;
pub mod pty_runtime;
pub mod registry;
pub mod watchdog;
pub mod watches;

use crate::context::AppContext;
use crate::protocol::Response;
use persistence::BgMode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub use registry::{BgCompletion, BgTaskRegistry};

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
    let storage_dir = {
        let config = ctx.config();
        let root = storage_dir(config.storage_dir.as_deref());
        config
            .harness
            .map(|harness| root.join(harness.as_str()))
            .unwrap_or(root)
    };
    let max_running = ctx.config().max_background_bash_tasks;
    let timeout = timeout_ms.map(Duration::from_millis);
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));

    let env = env.unwrap_or_default();
    let spawn_result = if pty {
        ctx.bash_background().spawn_pty(
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
        Ok(task_id) => Response::success(
            request_id,
            json!({
                "task_id": task_id,
                "status": BgTaskStatus::Running,
                "mode": if pty { "pty" } else { "pipes" },
            }),
        ),
        Err(message) if message.contains("limit exceeded") => {
            Response::error(request_id, "background_task_limit_exceeded", message)
        }
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

pub fn storage_dir(configured: Option<&std::path::Path>) -> PathBuf {
    if let Some(dir) = configured {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(dir).join("aft");
    }
    // Fallback to the user's home directory. On Unix this is `$HOME`; on
    // Windows `HOME` is typically unset, so fall back to `USERPROFILE`
    // (which is always set in interactive sessions and in the env that
    // OpenCode/Pi pass through to plugin processes). If both are missing
    // (rare — embedded contexts, broken shells), fall back to a temp
    // directory rather than `"."` — a relative path makes bg-bash wrapper
    // commands like `move /Y .\.cache\aft\... ...` fail with "system
    // cannot find the path specified" once the working directory shifts.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("aft")
}

pub fn repair_legacy_root_tasks(storage_root: &std::path::Path, harness: crate::harness::Harness) {
    let root_tasks = storage_root.join("bash-tasks");
    if !dir_has_entries(&root_tasks) {
        return;
    }

    let harness_tasks = storage_root.join(harness.as_str()).join("bash-tasks");
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
