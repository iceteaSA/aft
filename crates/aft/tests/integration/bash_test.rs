use super::helpers::{user_config, AftProcess};

#[cfg(unix)]
fn shell_quote_path(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_executable_shim(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
fn wait_for_terminal_status(aft: &mut AftProcess, task_id: &str) -> serde_json::Value {
    let started = std::time::Instant::now();
    loop {
        let status = aft.send(
            &serde_json::json!({
                "id": format!("status-{task_id}"),
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if matches!(
            status["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return status;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "timed out waiting for terminal bash status: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    if !output.status.success() {
        return false;
    }
    !String::from_utf8_lossy(&output.stdout).contains('Z')
}

#[cfg(unix)]
fn wait_until_process_exits(pid: i32) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(2) {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[test]
fn bash_streams_progress_and_returns_final_response() {
    let mut aft = AftProcess::spawn();

    let response = aft.send(r#"{"id":"bash-1","method":"bash","params":{"command":"echo hello"}}"#);
    assert_eq!(response["id"], "bash-1");
    assert_eq!(response["success"], true);
    assert_eq!(response["status"], "running");

    let task_id = response["task_id"].as_str().unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        let status = aft.send(
            &serde_json::json!({
                "id": "bash-1-status",
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if status["status"] == "completed" {
            break status;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(
        status["output_preview"]
            .as_str()
            .unwrap()
            .replace("\r\n", "\n"),
        "hello\n"
    );
    assert_eq!(status["exit_code"], 0);
    assert!(status["duration_ms"].is_u64());

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn bash_rejects_blocked_env_vars() {
    let mut aft = AftProcess::spawn();

    let response = aft.send(
        &serde_json::json!({
            "id": "bash-blocked-env",
            "method": "bash",
            "params": {
                "command": "echo should-not-run",
                "env": { "LD_PRELOAD": "foo" }
            }
        })
        .to_string(),
    );

    assert_eq!(response["success"], false, "response: {response:?}");
    assert_eq!(response["code"], "blocked_env_var");
    assert!(response["message"].as_str().unwrap().contains("LD_PRELOAD"));

    assert!(aft.shutdown().success());
}

#[test]
fn bash_rejects_invalid_pty_dimensions() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let configure = aft.send(
        &serde_json::json!({
            "id": "cfg-bg",
            "command": "configure",
            "harness": "opencode",
            "project_root": dir.path(),
            "storage_dir": dir.path().join("storage"),
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
        })
        .to_string(),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let cases = [
        (
            "pty-rows-too-large",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_rows": 61,
            }),
            "ptyRows must be an integer between 1 and 60",
        ),
        (
            "pty-cols-too-large",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_cols": 141,
            }),
            "ptyCols must be an integer between 1 and 140",
        ),
        (
            "pty-rows-float",
            serde_json::json!({
                "command": "echo nope",
                "background": true,
                "pty": true,
                "pty_rows": 1.5,
            }),
            "invalid params",
        ),
    ];

    for (id, params, message) in cases {
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": params
            })
            .to_string(),
        );
        assert_eq!(response["success"], false, "case {id}: {response:?}");
        assert_eq!(
            response["code"], "invalid_request",
            "case {id}: {response:?}"
        );
        assert!(
            response["message"].as_str().unwrap().contains(message),
            "case {id}: expected message containing {message:?}, got {response:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_piped_runner_exit_status_is_not_hidden() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    write_executable_shim(
        &bin_dir.join("cargo"),
        "#!/bin/sh\nprintf 'fake cargo line\\n'\nexit 0\n",
    );
    write_executable_shim(
        &bin_dir.join("pytest"),
        "#!/bin/sh\nprintf 'fake pytest line\\n'\nexit 0\n",
    );
    let path_prefix = shell_quote_path(&bin_dir);
    let cases = [
        (
            "grep-v-empty",
            format!("PATH={path_prefix}:$PATH cargo test | grep -v '^'"),
        ),
        (
            "awk-end-exit",
            format!("PATH={path_prefix}:$PATH cargo test | awk 'END{{exit 1}}'"),
        ),
        (
            "pytest-grep-sentinel",
            format!("PATH={path_prefix}:$PATH pytest -q | grep SENTINEL || exit 1"),
        ),
    ];

    for (id, command) in cases {
        let response = aft.send(
            &serde_json::json!({
                "id": id,
                "method": "bash",
                "params": { "command": command }
            })
            .to_string(),
        );
        assert_eq!(
            response["success"], true,
            "spawn failed for {id}: {response:?}"
        );
        assert_eq!(
            response["status"], "running",
            "unexpected spawn status for {id}: {response:?}"
        );
        let task_id = response["task_id"].as_str().unwrap();
        let status = wait_for_terminal_status(&mut aft, task_id);
        assert_eq!(
            status["status"], "failed",
            "{id} should preserve pipeline failure: {status:?}"
        );
        assert_eq!(
            status["exit_code"], 1,
            "{id} should report the shell pipeline exit code: {status:?}"
        );
    }

    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn bash_timeout_terminates_shell_process_group_grandchild() {
    let mut aft = AftProcess::spawn();
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sleep.pid");
    let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());

    let response = aft.send(
        &serde_json::json!({
            "id": "bash-timeout-pgroup",
            "method": "bash",
            "params": { "command": command, "timeout": 200 }
        })
        .to_string(),
    );

    assert_eq!(response["success"], true, "bash failed: {response:?}");
    assert_eq!(response["status"], "running");
    let task_id = response["task_id"].as_str().unwrap();
    let started = std::time::Instant::now();
    loop {
        let status = aft.send(
            &serde_json::json!({
                "id": "bash-timeout-pgroup-status",
                "method": "bash_status",
                "params": { "task_id": task_id }
            })
            .to_string(),
        );
        if status["status"] == "timed_out" {
            break;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_until_process_exits(pid),
        "grandchild sleep process {pid} survived foreground timeout"
    );

    assert!(aft.shutdown().success());
}
