use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES;
use crate::bash_background::registry::BgTaskSnapshot;
use crate::bash_background::BgTaskStatus;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::response_finalize::{DispatchOutcome, PendingResponse, PendingResponsePoll};

const TEST_FOREGROUND_WAIT_ENV: &str = "AFT_TEST_FOREGROUND_WAIT_MS";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BashOrchestrateParams {
    foreground_orchestrate: bool,
    block_to_completion: bool,
    background: bool,
    pty: bool,
    timeout: Option<u64>,
}

/// Port of `packages/aft-bridge/src/bash-format.ts` `formatForegroundResult` (lines 8-25).
pub fn format_foreground_result(snapshot: &BgTaskSnapshot) -> String {
    let mut rendered = snapshot.output_preview.clone();
    if snapshot.output_truncated {
        if let Some(output_path) = snapshot.output_path.as_deref() {
            rendered.push_str(&format!(
                "\n[output truncated; full output at {output_path}]"
            ));
        }
    }
    if snapshot.info.status == BgTaskStatus::TimedOut {
        rendered.push_str("\n[command timed out]");
    }
    if let Some(exit) = snapshot.exit_code.filter(|exit| *exit != 0) {
        rendered.push_str(&format!("\n[exit code: {exit}]"));
    }
    rendered
}

/// Port of `packages/aft-bridge/src/bash-format.ts` `formatSeconds` (lines 3-6).
pub fn format_seconds(ms: u64) -> String {
    let mut seconds = format!("{:.1}", ms as f64 / 1000.0);
    if seconds.ends_with(".0") {
        seconds.truncate(seconds.len() - 2);
    }
    format!("{seconds}s")
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatPromotionMessage` (lines 603-614).
pub fn format_promotion_message(
    task_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
) -> String {
    let waited = timeout
        .map(|timeout| timeout.min(wait_window_ms))
        .unwrap_or(wait_window_ms);
    format!(
        "Foreground bash didn't finish within {} and was promoted to background: {task_id}. A completion reminder will be delivered automatically; use bash_status({{ taskId: \"{task_id}\" }}) to inspect output or bash_kill({{ taskId: \"{task_id}\" }}) to terminate.",
        format_seconds(waited)
    )
}

/// Port of OpenCode `packages/opencode-plugin/src/tools/bash.ts` `formatBackgroundLaunch` (lines 593-601).
pub fn format_background_launch(task_id: &str, pty: bool) -> String {
    if pty {
        return format!(
            "PTY task started: {task_id}. Use bash_status({{ taskId: \"{task_id}\", outputMode: \"screen\" }}) to see the visible terminal, bash_write({{ taskId: \"{task_id}\", input: ... }}) to send keystrokes. A completion reminder fires automatically when the task exits."
        );
    }
    format!(
        "Background task started: {task_id}. A completion reminder will be delivered automatically; don't poll bash_status."
    )
}

pub fn foreground_orchestrate_enabled(req: &RawRequest) -> bool {
    parse_params(req)
        .map(|params| params.foreground_orchestrate)
        .unwrap_or(false)
}

pub fn build_bash_outcome(
    req: &RawRequest,
    ctx: &AppContext,
    spawn_response: Response,
) -> DispatchOutcome {
    if !spawn_response.success {
        return DispatchOutcome::Immediate(spawn_response);
    }

    let params = parse_params(req).unwrap_or_default();
    let Some(task_id) = spawn_response
        .data
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return DispatchOutcome::Immediate(spawn_response);
    };
    if spawn_response.data.get("status").and_then(Value::as_str) != Some("running") {
        return DispatchOutcome::Immediate(spawn_response);
    }

    let mode = spawn_response
        .data
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("pipes");
    let is_pty = mode == "pty" || params.pty;
    if is_pty || params.background {
        return DispatchOutcome::Immediate(background_launch_response(&req.id, &task_id, is_pty));
    }

    let request_id = req.id.clone();
    let session_id = req.session().to_string();
    let attach_command = "bash".to_string();
    let wait_window_ms = resolve_foreground_wait_window_ms(ctx.config().foreground_wait_window_ms);
    let deadline = Instant::now() + Duration::from_millis(wait_window_ms);
    let block_to_completion = params.block_to_completion;
    let timeout = params.timeout;
    let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    let project_root = ctx.config().project_root.clone();
    let task_id_for_poll = task_id.clone();
    let request_id_for_poll = request_id.clone();
    let session_id_for_poll = session_id.clone();

    let mut poll: PendingResponsePoll = Box::new(move |ctx| {
        let Some(snapshot) = poll_bash_status(
            ctx,
            &task_id_for_poll,
            &session_id_for_poll,
            project_root.as_deref(),
            &storage_dir,
            RUNNING_OUTPUT_PREVIEW_BYTES,
        ) else {
            return Some(task_not_found_response(
                &request_id_for_poll,
                &task_id_for_poll,
            ));
        };

        match decide_bash_step(
            snapshot,
            deadline,
            block_to_completion,
            Instant::now(),
            &request_id_for_poll,
        ) {
            BashStep::Done(response) => Some(response),
            BashStep::Promote => Some(promote_bash(
                ctx,
                &task_id_for_poll,
                &session_id_for_poll,
                project_root.as_deref(),
                timeout,
                wait_window_ms,
                &request_id_for_poll,
            )),
            BashStep::Wait => None,
        }
    });

    if let Some(response) = poll(ctx) {
        return DispatchOutcome::Immediate(response);
    }

    DispatchOutcome::Deferred(PendingResponse {
        request_id,
        session_id,
        attach_command,
        poll,
    })
}

pub(crate) fn poll_bash_status(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    project_root: Option<&Path>,
    storage_dir: &Path,
    preview_bytes: usize,
) -> Option<BgTaskSnapshot> {
    ctx.bash_background().status(
        task_id,
        session_id,
        project_root,
        Some(storage_dir),
        preview_bytes,
    )
}

pub(crate) enum BashStep {
    Done(Response),
    Promote,
    Wait,
}

pub(crate) fn task_not_found_response(request_id: &str, task_id: &str) -> Response {
    Response::error(
        request_id,
        "task_not_found",
        format!("background task not found: {task_id}"),
    )
}

pub(crate) fn decide_bash_step(
    snapshot: BgTaskSnapshot,
    deadline: Instant,
    block_to_completion: bool,
    now: Instant,
    request_id: &str,
) -> BashStep {
    if snapshot.info.status.is_terminal() {
        BashStep::Done(foreground_result_response(request_id, snapshot))
    } else if !block_to_completion && now >= deadline {
        BashStep::Promote
    } else {
        BashStep::Wait
    }
}

pub(crate) fn promote_bash(
    ctx: &AppContext,
    task_id: &str,
    session_id: &str,
    _project_root: Option<&Path>,
    timeout: Option<u64>,
    wait_window_ms: u64,
    request_id: &str,
) -> Response {
    match ctx.bash_background().promote(task_id, session_id) {
        Ok(_) => promotion_response(request_id, task_id, timeout, wait_window_ms),
        Err(message) if message.contains("not found") => {
            Response::error(request_id, "task_not_found", message)
        }
        Err(message) => Response::error(request_id, "execution_failed", message),
    }
}

fn foreground_result_response(request_id: &str, snapshot: BgTaskSnapshot) -> Response {
    let output = format_foreground_result(&snapshot);
    let timed_out = snapshot.info.status == BgTaskStatus::TimedOut;
    Response::success(
        request_id,
        json!({
            "output": output,
            "task_id": snapshot.info.task_id,
            "status": snapshot.info.status,
            "mode": snapshot.info.mode,
            "exit_code": snapshot.exit_code,
            "output_preview": snapshot.output_preview,
            "output_truncated": snapshot.output_truncated,
            "truncated": snapshot.output_truncated,
            "output_path": snapshot.output_path,
            "timed_out": timed_out,
            "duration_ms": snapshot.info.duration_ms,
        }),
    )
}

fn background_launch_response(request_id: &str, task_id: &str, is_pty: bool) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format_background_launch(task_id, is_pty),
            "task_id": task_id,
            "status": "running",
            "mode": if is_pty { "pty" } else { "pipes" },
        }),
    )
}

fn promotion_response(
    request_id: &str,
    task_id: &str,
    timeout: Option<u64>,
    wait_window_ms: u64,
) -> Response {
    Response::success(
        request_id,
        json!({
            "output": format_promotion_message(task_id, timeout, wait_window_ms),
            "task_id": task_id,
            "status": "running",
        }),
    )
}

fn parse_params(req: &RawRequest) -> Option<BashOrchestrateParams> {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    serde_json::from_value::<BashOrchestrateParams>(raw_params).ok()
}

pub(crate) fn resolve_foreground_wait_window_ms(configured: u64) -> u64 {
    std::env::var(TEST_FOREGROUND_WAIT_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(configured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::BgMode;
    use crate::bash_background::registry::BgTaskSnapshot;
    use crate::bash_background::BgTaskInfo;

    fn snapshot(
        output_preview: &str,
        output_truncated: bool,
        output_path: Option<&str>,
        status: BgTaskStatus,
        exit_code: Option<i32>,
    ) -> BgTaskSnapshot {
        BgTaskSnapshot {
            info: BgTaskInfo {
                task_id: "bash-test".to_string(),
                status,
                command: "echo test".to_string(),
                mode: BgMode::Pipes,
                started_at: 0,
                duration_ms: Some(1),
            },
            exit_code,
            child_pid: None,
            workdir: "/tmp".to_string(),
            output_preview: output_preview.to_string(),
            output_truncated,
            output_path: output_path.map(str::to_string),
            stderr_path: None,
            pty_rows: None,
            pty_cols: None,
            pty_screen: None,
        }
    }

    #[test]
    fn decide_bash_step_returns_done_for_terminal_snapshot_even_at_deadline() {
        let snapshot = snapshot("done", false, None, BgTaskStatus::Completed, Some(0));
        let now = Instant::now();

        match decide_bash_step(snapshot, now, false, now, "req-terminal") {
            BashStep::Done(response) => {
                assert_eq!(response.id, "req-terminal");
                assert!(response.success);
                assert_eq!(response.data["status"], json!("completed"));
                assert_eq!(response.data["output"], json!("done"));
            }
            BashStep::Promote => panic!("terminal snapshot should not promote"),
            BashStep::Wait => panic!("terminal snapshot should not wait"),
        }
    }

    #[test]
    fn decide_bash_step_promotes_at_deadline_when_not_blocking() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(snapshot, now, false, now, "req-promote") {
            BashStep::Promote => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Wait => panic!("deadline should promote when not blocking"),
        }
    }

    #[test]
    fn decide_bash_step_waits_before_deadline() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(
            snapshot,
            now + Duration::from_millis(1),
            false,
            now,
            "req-wait",
        ) {
            BashStep::Wait => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Promote => panic!("snapshot should wait before the deadline"),
        }
    }

    #[test]
    fn decide_bash_step_never_promotes_when_blocking_to_completion() {
        let snapshot = snapshot("running", false, None, BgTaskStatus::Running, None);
        let now = Instant::now();

        match decide_bash_step(snapshot, now, true, now, "req-block") {
            BashStep::Wait => {}
            BashStep::Done(_) => panic!("running snapshot should not finish"),
            BashStep::Promote => panic!("block_to_completion should suppress promotion"),
        }
    }

    #[test]
    fn foreground_result_format_matches_typescript_order() {
        let snapshot = snapshot(
            "hello",
            true,
            Some("/tmp/aft-output.txt"),
            BgTaskStatus::TimedOut,
            Some(124),
        );

        assert_eq!(
            format_foreground_result(&snapshot),
            "hello\n[output truncated; full output at /tmp/aft-output.txt]\n[command timed out]\n[exit code: 124]"
        );
    }

    #[test]
    fn format_seconds_strips_integer_decimal_and_keeps_tenths() {
        assert_eq!(format_seconds(8_000), "8s");
        assert_eq!(format_seconds(5_500), "5.5s");
        assert_eq!(format_seconds(14_999), "15s");
    }

    #[test]
    fn promotion_message_matches_opencode_copy() {
        assert_eq!(
            format_promotion_message("bash-123", Some(5_500), 8_000),
            "Foreground bash didn't finish within 5.5s and was promoted to background: bash-123. A completion reminder will be delivered automatically; use bash_status({ taskId: \"bash-123\" }) to inspect output or bash_kill({ taskId: \"bash-123\" }) to terminate."
        );
    }

    #[test]
    fn background_launch_messages_match_opencode_copy() {
        assert_eq!(
            format_background_launch("bash-bg", false),
            "Background task started: bash-bg. A completion reminder will be delivered automatically; don't poll bash_status."
        );
        assert_eq!(
            format_background_launch("bash-pty", true),
            "PTY task started: bash-pty. Use bash_status({ taskId: \"bash-pty\", outputMode: \"screen\" }) to see the visible terminal, bash_write({ taskId: \"bash-pty\", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits."
        );
    }
}
