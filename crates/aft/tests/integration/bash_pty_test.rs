use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::bash_background::persistence::{
    task_bundle_files, task_paths, write_task, BgMode, PersistedTask,
};
use aft::bash_background::pty_runtime::CompletionCoordinator;
use aft::bash_background::{BgTaskRegistry, BgTaskStatus};
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
            "printf wake",
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
    let _snapshot = wait_for_status(&registry, &task_id, BgTaskStatus::Completed);
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn pty_task_bundle_files_includes_pty_spill() {
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "bundle");
    let files = task_bundle_files(&paths);
    assert!(files.iter().any(|path| path == &paths.pty));
}

#[test]
fn pty_v2_task_rehydrates_then_upgrades_to_v3_on_next_persist() {
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
    assert_eq!(after["schema_version"], 3);
}
