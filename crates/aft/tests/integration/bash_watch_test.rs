use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::AftProcess;

fn configure_background(aft: &mut AftProcess) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let response = aft.send(
        &json!({
            "id": "cfg-watch-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "experimental_bash_background": true,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    dir
}

fn notify(aft: &mut AftProcess, task_id: &str, params: Value) -> Value {
    let mut params = params.as_object().unwrap().clone();
    params.insert("task_id".into(), json!(task_id));
    aft.send(
        &json!({
            "id": "notify-watch",
            "command": "bash_notify",
            "params": params,
        })
        .to_string(),
    )
}

fn spawn(aft: &mut AftProcess, command: &str) -> String {
    let spawn = aft.send(
        &json!({
            "id": "spawn-watch-bg",
            "command": "bash",
            "params": { "command": command, "background": true }
        })
        .to_string(),
    );
    assert_eq!(spawn["success"], true, "spawn failed: {spawn:?}");
    spawn["task_id"].as_str().unwrap().to_string()
}

#[cfg(windows)]
fn print_ready_after_complete_command() -> &'static str {
    "Write-Host -NoNewline READY-AFTER-COMPLETE"
}

#[cfg(not(windows))]
fn print_ready_after_complete_command() -> &'static str {
    "printf READY-AFTER-COMPLETE"
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(not(windows))]
fn release_gate_command(release: &Path, text: &str) -> String {
    format!(
        "while [ ! -f {} ]; do sleep 0.05; done; printf '%s\\n' {}",
        shell_quote(&release.display().to_string()),
        shell_quote(text)
    )
}

#[cfg(windows)]
fn release_gate_command(release: &Path, text: &str) -> String {
    format!(
        "while (-not (Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 50 }}; Write-Output {}",
        shell_quote(&release.display().to_string()),
        shell_quote(text)
    )
}

fn release_task(path: &Path) {
    fs::write(path, "go").expect("release watch task");
}

fn wait_for_pattern_frame(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["type"] == "bash_pattern_match" && frame["task_id"] == task_id {
                return frame;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for pattern frame"
        );
    }
}

#[test]
fn register_pattern_watch_returns_watch_id() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 1; echo READY");
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    assert!(response["watch_id"].as_str().unwrap().starts_with("watch-"));
    assert!(aft.shutdown().success());
}

#[test]
fn pattern_match_emits_push_frame() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("pattern-release");
    let command = release_gate_command(&release, "READY");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    release_task(&release);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["match_text"], "READY");
    assert_eq!(frame["once"], true);
    assert!(aft.shutdown().success());
}

#[test]
fn cap_8_watches_per_task_rejects_9th() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 2");
    for idx in 0..8 {
        let response = notify(&mut aft, &task_id, json!({ "pattern": format!("x{idx}") }));
        assert_eq!(
            response["success"], true,
            "notify {idx} failed: {response:?}"
        );
    }
    let ninth = notify(&mut aft, &task_id, json!({ "pattern": "x9" }));
    assert_eq!(ninth["success"], false);
    assert_eq!(ninth["code"], "too_many_watches");
    assert!(aft.shutdown().success());
}

#[test]
fn regex_pattern_matches_with_capture() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("regex-release");
    let command = release_gate_command(&release, "port 3000");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "regex": "port (\\d+)" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    release_task(&release);
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["match_text"], "port 3000");
    assert!(aft.shutdown().success());
}

#[test]
fn final_output_scan_emits_pattern_before_completion_on_exit_race() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("exit-race-release");
    let command = release_gate_command(&release, "ready-now");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "ready-now" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    release_task(&release);

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] == task_id {
                assert_eq!(
                    frame["type"], "bash_pattern_match",
                    "watch-controlled task completed before final pattern scan: {frame:?}"
                );
                assert_eq!(frame["match_text"], "ready-now");
                assert_eq!(frame["reason"], "pattern_match");
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for first terminal watch frame"
        );
    }
    assert!(aft.shutdown().success());
}

#[test]
fn watch_controlled_exit_emits_exit_safety_net_not_completion() {
    let mut aft = AftProcess::spawn();
    let dir = configure_background(&mut aft);
    let release = dir.path().join("exit-safety-release");
    let command = release_gate_command(&release, "never-matches-output");
    let task_id = spawn(&mut aft, &command);
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    release_task(&release);

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] != task_id {
                continue;
            }
            assert_eq!(
                frame["type"], "bash_pattern_match",
                "watch-controlled task emitted a background completion: {frame:?}"
            );
            assert_eq!(frame["reason"], "task_exit");
            assert!(frame["context"]
                .as_str()
                .unwrap()
                .contains("never-matches-output"));
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for exit safety-net frame"
        );
    }

    let drained = aft.send(
        &json!({
            "id": "drain-watch-exit",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|completion| completion["task_id"] != task_id),
        "watch-controlled task also queued a normal completion: {drained:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn registering_watch_after_completion_removes_completion_and_emits_one_watch_frame() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, print_ready_after_complete_command());

    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["task_id"] == task_id {
                assert_eq!(
                    frame["type"], "bash_completed",
                    "task should first complete normally before watch registration: {frame:?}"
                );
                break;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "timed out waiting for completion frame before watch registration"
        );
    }

    let response = notify(
        &mut aft,
        &task_id,
        json!({ "pattern": "READY-AFTER-COMPLETE" }),
    );
    assert_eq!(response["success"], true, "notify failed: {response:?}");

    let mut task_frames = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) || task_frames.is_empty() {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(100)) {
            if frame["task_id"] == task_id {
                task_frames.push(frame);
            }
        }
        if started.elapsed() > Duration::from_secs(6) {
            break;
        }
    }

    assert_eq!(
        task_frames.len(),
        1,
        "watch-after-completion should emit exactly one task frame: {task_frames:?}"
    );
    assert_eq!(task_frames[0]["type"], "bash_pattern_match");
    assert_eq!(task_frames[0]["reason"], "pattern_match");
    assert_eq!(task_frames[0]["match_text"], "READY-AFTER-COMPLETE");

    let drained = aft.send(
        &json!({
            "id": "drain-after-late-watch",
            "command": "bash_drain_completions"
        })
        .to_string(),
    );
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    assert!(
        drained["bg_completions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|completion| completion["task_id"] != task_id),
        "late watch should remove queued normal completion: {drained:?}"
    );
    assert!(aft.shutdown().success());
}
