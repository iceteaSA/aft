use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashKillParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashKillParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_kill: invalid params: {e}"),
            );
        }
    };

    let Some(task_id) = params.task_id else {
        return Response::error(&req.id, "invalid_request", "bash_kill: missing task_id");
    };

    let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    let result = ctx
        .bash_background()
        .kill(&task_id, req.session())
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            {
                let config = ctx.config();
                let _ = if let Some(project_root) = config.project_root.as_deref() {
                    ctx.bash_background().replay_session_for_project(
                        &storage_dir,
                        req.session(),
                        project_root,
                    )
                } else {
                    ctx.bash_background()
                        .replay_session(&storage_dir, req.session())
                };
            }
            ctx.bash_background().kill(&task_id, req.session())
        })
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            let config = ctx.config();
            let Some(project_root) = config.project_root.as_deref() else {
                return Err(message);
            };
            ctx.bash_background()
                .kill_relaxed(&task_id, project_root, &storage_dir)
        });

    match result {
        Ok(snapshot) => Response::success(&req.id, json!(snapshot)),
        Err(message) if message.contains("not found") => {
            Response::error(&req.id, "task_not_found", message)
        }
        Err(message) => Response::error(&req.id, "kill_failed", message),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    use crate::config::Config;
    use crate::context::{App, AppContext};

    fn actor(app: &Arc<App>, project: &Path, storage: &Path) -> AppContext {
        let config = Config {
            project_root: Some(project.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..Config::default()
        };
        AppContext::from_app(Arc::clone(app), config)
    }

    fn write_running_project_task(storage: &Path, project: &Path, session: &str, task_id: &str) {
        let paths = task_paths(storage, session, task_id);
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            session.to_string(),
            "sleep 60".to_string(),
            project.to_path_buf(),
            Some(project.to_path_buf()),
            Some(30_000),
            true,
            true,
        );
        metadata.status = BgTaskStatus::Running;
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, "still running\n").unwrap();
        fs::write(&paths.stderr, "").unwrap();
    }

    fn kill_request(task_id: &str, session: &str) -> RawRequest {
        RawRequest {
            id: "kill-project-filter".to_string(),
            command: "bash_kill".to_string(),
            lsp_hints: None,
            session_id: Some(session.to_string()),
            params: json!({ "params": { "task_id": task_id } }),
        }
    }

    #[test]
    fn bash_kill_replay_filters_same_session_by_project_root() {
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let app = App::default_shared();
        let ctx_a = actor(&app, project_a.path(), storage.path());
        let ctx_b = actor(&app, project_b.path(), storage.path());
        let session = "shared-session";
        let task_id = "bash-project-a";
        write_running_project_task(storage.path(), project_a.path(), session, task_id);

        let miss = serde_json::to_value(handle(&kill_request(task_id, session), &ctx_b)).unwrap();
        assert_eq!(
            miss["success"], false,
            "wrong project killed task: {miss:?}"
        );
        assert_eq!(miss["code"], "task_not_found");

        let killed = serde_json::to_value(handle(&kill_request(task_id, session), &ctx_a)).unwrap();
        assert_eq!(
            killed["success"], true,
            "owning project kill failed: {killed:?}"
        );
        assert_eq!(killed["status"], "killed");
    }
}
