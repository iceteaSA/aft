use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::bash_background::persistence::{
    task_bundle_files, task_paths, write_task, BgMode, PersistedTask, SCHEMA_VERSION,
};
use aft::bash_background::pty_runtime::CompletionCoordinator;
use aft::bash_background::{BgCompletion, BgTaskRegistry, BgTaskStatus};
use serde_json::json;

const SESSION: &str = "pty-phase-1a";

fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

fn base_task(
    storage: &std::path::Path,
    project: &std::path::Path,
    task_id: &str,
    mode: BgMode,
    status: BgTaskStatus,
) -> PersistedTask {
    let mut task = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "true".to_string(),
        project.to_path_buf(),
        Some(project.to_path_buf()),
        Some(30_000),
        true,
        false,
    );
    task.mode = mode;
    if status.is_terminal() {
        task.mark_terminal(status, Some(0), None);
        task.completion_delivered = false;
    } else {
        task.status = status;
        task.child_pid = Some(999_999);
        task.pgid = Some(999_999);
    }
    let paths = task_paths(storage, SESSION, task_id);
    write_task(&paths.json, &task).unwrap();
    fs::write(&paths.stdout, b"stdout").unwrap();
    fs::write(&paths.stderr, b"stderr").unwrap();
    fs::write(&paths.pty, b"pty").unwrap();
    task
}

fn wait_for_status(
    registry: &BgTaskRegistry,
    task_id: &str,
    status: BgTaskStatus,
) -> aft::bash_background::registry::BgTaskSnapshot {
    let started = Instant::now();
    loop {
        if let Some(snapshot) = registry.status(task_id, SESSION, None, None, 2048) {
            if snapshot.info.status == status {
                return snapshot;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for {status:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_completion(registry: &BgTaskRegistry, task_id: &str) -> BgCompletion {
    let started = Instant::now();
    loop {
        let completions = registry.drain_completions_for_session(Some(SESSION));
        if let Some(completion) = completions.into_iter().find(|c| c.task_id == task_id) {
            return completion;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timed out waiting for completion for {task_id}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn pty_spawn_echo_exit() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn_pty(
            "printf 'hello pty\\n'",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
            24,
            80,
        )
        .unwrap();

    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    assert_eq!(snapshot.info.mode, BgMode::Pty);
    let output_path = snapshot.output_path.expect("PTY output path");
    assert!(fs::read_to_string(output_path)
        .unwrap()
        .contains("hello pty"));
    assert_eq!(snapshot.stderr_path, None);
}

#[test]
fn pty_replay_marks_killed_when_running_no_marker() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    base_task(
        storage.path(),
        project.path(),
        "lost",
        BgMode::Pty,
        BgTaskStatus::Running,
    );

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let snapshot = registry.status("lost", SESSION, None, None, 1024).unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
    let persisted: PersistedTask = serde_json::from_str(
        &fs::read_to_string(task_paths(storage.path(), SESSION, "lost").json).unwrap(),
    )
    .unwrap();
    assert_eq!(
        persisted.status_reason.as_deref(),
        Some("pty_lost_on_bridge_restart")
    );
}

#[test]
fn pty_replay_keeps_terminal_when_already_terminal() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut task = base_task(
        storage.path(),
        project.path(),
        "terminal",
        BgMode::Pty,
        BgTaskStatus::Completed,
    );
    task.status_reason = Some("keep-me".to_string());
    write_task(&task_paths(storage.path(), SESSION, "terminal").json, &task).unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let snapshot = registry
        .status("terminal", SESSION, None, None, 1024)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    let persisted: PersistedTask = serde_json::from_str(
        &fs::read_to_string(task_paths(storage.path(), SESSION, "terminal").json).unwrap(),
    )
    .unwrap();
    assert_eq!(persisted.status_reason.as_deref(), Some("keep-me"));
}

#[test]
fn pty_replay_uses_exit_marker_when_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    base_task(
        storage.path(),
        project.path(),
        "marker",
        BgMode::Pty,
        BgTaskStatus::Running,
    );
    let paths = task_paths(storage.path(), SESSION, "marker");
    fs::write(&paths.exit, b"7").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let snapshot = registry
        .status("marker", SESSION, None, None, 1024)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    assert_eq!(snapshot.exit_code, Some(7));
}

#[test]
fn pty_replay_accepts_schema_version_2_as_piped() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "v2-piped");
    fs::create_dir_all(&paths.dir).unwrap();
    fs::write(
        &paths.json,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 2,
            "task_id": "v2-piped",
            "session_id": SESSION,
            "command": "true",
            "workdir": project.path(),
            "project_root": project.path(),
            "status": "completed",
            "started_at": 1,
            "finished_at": 2,
            "duration_ms": 1,
            "timeout_ms": null,
            "exit_code": 0,
            "child_pid": null,
            "pgid": null,
            "completion_delivered": false,
            "notify_on_completion": true,
            "compressed": false,
            "status_reason": null
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&paths.stdout, b"ok").unwrap();
    fs::write(&paths.stderr, b"").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let snapshot = registry
        .status("v2-piped", SESSION, None, None, 1024)
        .unwrap();
    assert_eq!(snapshot.info.mode, BgMode::Pipes);
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
}

#[cfg(unix)]
#[test]
fn pipes_unaffected_by_pty_changes() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "printf pipe-ok",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    assert_eq!(snapshot.info.mode, BgMode::Pipes);
    assert!(snapshot.output_path.unwrap().ends_with(".stdout"));
    assert!(snapshot.stderr_path.unwrap().ends_with(".stderr"));
    assert!(snapshot.output_preview.contains("pipe-ok"));
}

#[cfg(unix)]
#[test]
fn pty_waiter_writes_code_marker_on_natural_exit() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn_pty(
            "exit 3",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
            24,
            80,
        )
        .unwrap();
    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Failed);
    assert_eq!(snapshot.exit_code, Some(3));
    assert_eq!(
        fs::read_to_string(task_paths(storage.path(), SESSION, &task_id).exit)
            .unwrap()
            .trim(),
        "3"
    );
}

#[cfg(unix)]
#[test]
fn pty_reader_drains_before_completion_fires() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn_pty(
            "head -c 102400 /dev/zero | tr '\\0' A",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
            24,
            80,
        )
        .unwrap();
    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    let output = fs::read(snapshot.output_path.unwrap()).unwrap();
    assert!(
        output.len() >= 100 * 1024,
        "PTY output drained only {} bytes",
        output.len()
    );
}

#[test]
fn pty_completion_coordinator_fires_only_when_both_done() {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let coordinator = CompletionCoordinator::new("task".to_string(), SESSION.to_string(), tx);
    coordinator.signal_one_done();
    assert!(rx.recv_timeout(Duration::from_millis(25)).is_err());
    coordinator.signal_one_done();
    rx.recv_timeout(Duration::from_millis(25)).unwrap();
}

#[cfg(unix)]
#[test]
fn pty_watchdog_wake_channel_triggers_immediate_completion() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let started = Instant::now();
    let task_id = registry
        .spawn_pty(
            "/bin/sh -c 'while [ ! -f wake-ready ]; do sleep 0.01; done; printf wake'",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
            24,
            80,
        )
        .unwrap();
    fs::write(project.path().join("wake-ready"), b"ready").unwrap();

    // Do not poll status here: status() calls poll_task directly and can
    // complete PTY tasks without the watchdog. Draining completions observes
    // the completion frame queued by the watchdog thread.
    let completion = wait_for_completion(&registry, &task_id);
    assert_eq!(completion.status, BgTaskStatus::Completed);

    // The command waits for this test to arm it after spawn_pty returns, so the
    // reader/waiter cannot signal before the task is registered. A completion
    // under 450ms is below the 500ms watchdog interval, leaving a 50ms guard at
    // the boundary while still proving the wake channel beat the periodic poll.
    assert!(started.elapsed() < Duration::from_millis(450));
}

#[test]
fn pty_task_bundle_files_includes_pty_spill() {
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "bundle");
    let files = task_bundle_files(&paths);
    assert!(files.iter().any(|path| path == &paths.pty));
}

#[test]
fn pty_v2_task_rehydrates_then_upgrades_to_current_schema_on_next_persist() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "v2-upgrade");
    fs::create_dir_all(&paths.dir).unwrap();
    fs::write(
        &paths.json,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 2,
            "task_id": "v2-upgrade",
            "session_id": SESSION,
            "command": "true",
            "mode": "pty",
            "workdir": project.path(),
            "project_root": project.path(),
            "status": "completed",
            "started_at": 1,
            "finished_at": 2,
            "duration_ms": 1,
            "timeout_ms": null,
            "exit_code": 0,
            "child_pid": null,
            "pgid": null,
            "completion_delivered": false,
            "notify_on_completion": true,
            "compressed": false,
            "status_reason": null
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&paths.pty, b"done").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let before: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&paths.json).unwrap()).unwrap();
    assert_eq!(before["schema_version"], 2);
    let acked = registry.ack_completions_for_session(Some(SESSION), &["v2-upgrade".to_string()]);
    assert_eq!(acked, vec!["v2-upgrade".to_string()]);
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&paths.json).unwrap()).unwrap();
    assert_eq!(after["schema_version"], SCHEMA_VERSION);
}

fn spawn_pty_task(
    registry: &BgTaskRegistry,
    storage: &std::path::Path,
    project: &std::path::Path,
    command: &str,
    timeout: Duration,
) -> String {
    registry
        .spawn_pty(
            command,
            SESSION.to_string(),
            project.to_path_buf(),
            Default::default(),
            Some(timeout),
            storage.to_path_buf(),
            32,
            true,
            false,
            Some(project.to_path_buf()),
            24,
            80,
        )
        .unwrap()
}

fn read_pty_until(path: &std::path::Path, needle: &str, timeout: Duration) -> String {
    let started = Instant::now();
    loop {
        let output = fs::read_to_string(path).unwrap_or_default();
        if output.contains(needle) {
            return output;
        }
        assert!(
            started.elapsed() < timeout,
            "timed out waiting for {needle:?}; last output: {output:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn pty_write_to_cat() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "cat",
        Duration::from_secs(30),
    );
    let paths = task_paths(storage.path(), SESSION, &task_id);

    assert_eq!(
        registry.write_pty(&task_id, SESSION, b"hello\n").unwrap(),
        6
    );
    let output = read_pty_until(&paths.pty, "hello", Duration::from_secs(2));
    assert!(output.contains("hello"));
    registry.kill(&task_id, SESSION).unwrap();
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
}

#[cfg(windows)]
#[test]
fn pty_write_to_cmd_dir() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "cmd.exe",
        Duration::from_secs(30),
    );
    let paths = task_paths(storage.path(), SESSION, &task_id);

    registry
        .write_pty(&task_id, SESSION, b"dir & exit\r\n")
        .unwrap();
    let output = read_pty_until(&paths.pty, "Directory of", Duration::from_secs(5));
    assert!(output.contains("Directory of"));
    wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
}

#[test]
fn pty_write_python_repl_round_trip() {
    let python = if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
    {
        "python3 -q"
    } else if std::process::Command::new("python")
        .arg("--version")
        .output()
        .is_ok()
    {
        "python -q"
    } else {
        eprintln!("skipping: python not found");
        return;
    };
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        python,
        Duration::from_secs(30),
    );
    let paths = task_paths(storage.path(), SESSION, &task_id);

    registry
        .write_pty(&task_id, SESSION, b"print('pty-repl-ok')\n")
        .unwrap();
    let output = read_pty_until(&paths.pty, "pty-repl-ok", Duration::from_secs(5));
    assert!(output.contains("pty-repl-ok"));
    registry.kill(&task_id, SESSION).unwrap();
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
}

#[test]
fn pty_write_non_pty_task() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 1",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            32,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();

    assert_eq!(
        registry.write_pty(&task_id, SESSION, b"hello").unwrap_err(),
        "task_not_pty"
    );
    let _ = registry.kill(&task_id, SESSION);
}

#[test]
fn pty_write_exited_task() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "printf done",
        Duration::from_secs(30),
    );
    wait_for_status(&registry, &task_id, BgTaskStatus::Completed);

    assert_eq!(
        registry.write_pty(&task_id, SESSION, b"hello").unwrap_err(),
        "task_exited"
    );
}

#[test]
fn pty_write_too_large() {
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::RawRequest;

    let project = tempfile::tempdir().unwrap();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            experimental_bash_background: true,
            storage_dir: Some(project.path().join("storage")),
            ..Config::default()
        },
    );
    let req: RawRequest = serde_json::from_value(json!({
        "id": "write-large",
        "command": "bash_write",
        "params": { "task_id": "missing", "input": "x".repeat(1_048_577) }
    }))
    .unwrap();

    let response = aft::commands::bash_write::handle(&req, &ctx);
    assert!(!response.success);
    assert_eq!(response.data["code"], "input_too_large");
}

#[test]
fn pty_true_implies_background() {
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::RawRequest;

    let project = tempfile::tempdir().unwrap();
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            experimental_bash_background: true,
            storage_dir: Some(project.path().join("storage")),
            ..Config::default()
        },
    );
    let req: RawRequest = serde_json::from_value(json!({
        "id": "pty-implies-bg",
        "command": "bash",
        "params": { "command": "printf hi", "pty": true }
    }))
    .unwrap();

    let response = aft::commands::bash::handle(&req, &ctx);
    assert!(
        response.success,
        "pty:true should imply background: {response:?}"
    );
    assert_eq!(response.data["status"], "running");
    assert_eq!(response.data["mode"], "pty");
}

#[test]
fn pty_status_output_mode_validation() {
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::RawRequest;

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    let req: RawRequest = serde_json::from_value(json!({
        "id": "bad-output-mode",
        "command": "bash_status",
        "params": { "task_id": "missing", "output_mode": "ansi" }
    }))
    .unwrap();

    let response = aft::commands::bash_status::handle(&req, &ctx);
    assert!(!response.success);
    assert_eq!(response.data["code"], "invalid_request");
}

#[test]
fn pty_status_accepts_output_modes() {
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::RawRequest;

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    for output_mode in ["screen", "raw", "both"] {
        let req: RawRequest = serde_json::from_value(json!({
            "id": format!("mode-{output_mode}"),
            "command": "bash_status",
            "params": { "task_id": "missing", "output_mode": output_mode }
        }))
        .unwrap();
        let response = aft::commands::bash_status::handle(&req, &ctx);
        assert!(!response.success);
        assert_eq!(response.data["code"], "task_not_found");
    }
}

#[cfg(unix)]
#[test]
fn pty_kill_terminates_sighup_ignoring_cat() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "trap '' TERM HUP; cat",
        Duration::from_secs(30),
    );

    let killed = registry.kill(&task_id, SESSION).unwrap();
    assert_eq!(killed.info.status, BgTaskStatus::Killing);
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
}

#[cfg(windows)]
#[test]
fn pty_kill_terminates_pwsh_infinite_loop() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "pwsh -NoProfile -Command while($true){Start-Sleep -Milliseconds 100}",
        Duration::from_secs(30),
    );

    registry.kill(&task_id, SESSION).unwrap();
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
}

#[test]
fn pty_waiter_writes_killed_marker_on_kill_via_killer_kill() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "cat",
        Duration::from_secs(30),
    );

    registry.kill(&task_id, SESSION).unwrap();
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
    let marker = fs::read_to_string(task_paths(storage.path(), SESSION, &task_id).exit).unwrap();
    assert_eq!(marker.trim(), "killed");
}

#[test]
fn pty_kill_with_clones_outstanding_still_terminates() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "cat",
        Duration::from_secs(30),
    );

    registry
        .write_pty(&task_id, SESSION, b"keep-clones-busy\n")
        .unwrap();
    registry.kill(&task_id, SESSION).unwrap();
    let started = Instant::now();
    wait_for_status(&registry, &task_id, BgTaskStatus::Killed);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn pty_timeout_kill_finalizes_as_timed_out_not_killed() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "sleep 5",
        Duration::from_millis(100),
    );

    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::TimedOut);
    assert_eq!(snapshot.exit_code, Some(124));
}

#[test]
fn pty_status_snapshot_skips_preview_and_uses_pty_path() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "printf preview",
        Duration::from_secs(30),
    );

    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    assert_eq!(snapshot.info.mode, BgMode::Pty);
    assert_eq!(snapshot.output_preview, "");
    assert!(!snapshot.output_truncated);
    assert!(snapshot.output_path.unwrap().ends_with(".pty"));
    assert_eq!(snapshot.stderr_path, None);
}

#[test]
fn pty_completion_preview_is_empty() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "printf completion",
        Duration::from_secs(30),
    );
    wait_for_status(&registry, &task_id, BgTaskStatus::Completed);

    let completion = wait_for_completion(&registry, &task_id);
    assert_eq!(completion.output_preview, "");
    assert!(!completion.output_truncated);
}

#[test]
fn pty_completion_token_counts_returns_skipped_sentinel() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "printf tokens",
        Duration::from_secs(30),
    );
    wait_for_status(&registry, &task_id, BgTaskStatus::Completed);

    let completion = wait_for_completion(&registry, &task_id);
    assert_eq!(completion.original_tokens, None);
    assert_eq!(completion.compressed_tokens, None);
    assert!(completion.tokens_skipped);
}

#[test]
fn pty_parallel_smoke_10_tasks() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let mut task_ids = Vec::new();
    for index in 0..10 {
        task_ids.push(spawn_pty_task(
            &registry,
            storage.path(),
            project.path(),
            &format!("printf pty-{index}"),
            Duration::from_secs(30),
        ));
    }

    for task_id in task_ids {
        let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
        let output = fs::read_to_string(snapshot.output_path.unwrap()).unwrap();
        assert!(output.contains("pty-"));
    }
}

#[cfg(windows)]
#[test]
fn pty_windows_wrapper_script_runs_utf8_command() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = spawn_pty_task(
        &registry,
        storage.path(),
        project.path(),
        "echo café-東京",
        Duration::from_secs(30),
    );

    let snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    let output = fs::read_to_string(snapshot.output_path.unwrap()).unwrap();
    assert!(output.contains("café-東京"), "output: {output:?}");
}
