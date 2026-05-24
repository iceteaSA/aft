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
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 1; echo READY");
    let response = notify(&mut aft, &task_id, json!({ "pattern": "READY" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
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
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 1; echo 'port 3000'");
    let response = notify(&mut aft, &task_id, json!({ "regex": "port (\\d+)" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");
    let frame = wait_for_pattern_frame(&mut aft, &task_id);
    assert_eq!(frame["match_text"], "port 3000");
    assert!(aft.shutdown().success());
}

#[test]
fn final_output_scan_emits_pattern_before_completion_on_exit_race() {
    let mut aft = AftProcess::spawn();
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 0.2; echo ready-now");
    let response = notify(&mut aft, &task_id, json!({ "pattern": "ready-now" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");

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
    let _dir = configure_background(&mut aft);
    let task_id = spawn(&mut aft, "sleep 0.2; echo never-matches-output");
    let response = notify(&mut aft, &task_id, json!({ "pattern": "not-present" }));
    assert_eq!(response["success"], true, "notify failed: {response:?}");

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
