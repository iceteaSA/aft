#![cfg(unix)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn configure_native_policy(
    aft: &mut AftProcess,
    project: &Path,
    storage: &Path,
    enabled: bool,
    write_allow: &[PathBuf],
    read_deny: &[PathBuf],
) -> Value {
    aft.send(
        &json!({
            "id": format!("configure-native-{enabled}"),
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "bash_permissions": true,
            "config": user_config(json!({
                "bash": { "background": true, "rewrite": true },
                "sandbox": {
                    "enabled": enabled,
                    "write_allow": write_allow,
                    "read_deny": read_deny,
                }
            })),
        })
        .to_string(),
    )
}

fn configure_native(aft: &mut AftProcess, project: &Path, storage: &Path, enabled: bool) -> Value {
    configure_native_policy(aft, project, storage, enabled, &[], &[])
}

fn foreground(aft: &mut AftProcess, id: &str, command: &str) -> Value {
    aft.send(
        &json!({
            "id": id,
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": command,
                "foreground_orchestrate": true,
                "permissions_requested": true,
                "compressed": false,
            },
        })
        .to_string(),
    )
}

fn status(aft: &mut AftProcess, task_id: &str, output_mode: Option<&str>) -> Value {
    let mut params = json!({ "task_id": task_id });
    if let Some(output_mode) = output_mode {
        params["output_mode"] = Value::String(output_mode.to_string());
    }
    aft.send(
        &json!({
            "id": format!("status-{task_id}"),
            "method": "bash_status",
            "session_id": "native-sandbox-session",
            "params": params,
        })
        .to_string(),
    )
}

fn wait_for_terminal(aft: &mut AftProcess, task_id: &str, output_mode: Option<&str>) -> Value {
    let started = Instant::now();
    loop {
        let response = status(aft, task_id, output_mode);
        if matches!(
            response["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return response;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "native sandbox task did not finish: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn native_sandbox_enforces_writes_temp_cache_and_reports_scanner_findings() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let outside = home.join("outside");
    let extra_write = home.join("explicit-write-allow");
    let npm_cache = home.join(".npm");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&extra_write).unwrap();
    std::fs::create_dir_all(&npm_cache).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        std::slice::from_ref(&extra_write),
        &[],
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let project_file = project.join("project-write.txt");
    let project_write = foreground(
        &mut aft,
        "native-project-write",
        &format!("printf project-ok > {}", quote(&project_file)),
    );
    assert_eq!(
        project_write["status"], "completed",
        "project write should succeed: {project_write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&project_file).unwrap(),
        "project-ok"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        project_write["output"]
            .as_str()
            .unwrap_or_default()
            .matches("sandbox-launch: unenforced=")
            .count(),
        1,
        "Linux enforcement warning must appear once in stderr capture: {project_write:?}"
    );

    let explicitly_allowed_file = extra_write.join("allowed.txt");
    let explicit_write = foreground(
        &mut aft,
        "native-explicit-write",
        &format!("printf explicit-ok > {}", quote(&explicitly_allowed_file)),
    );
    assert_eq!(
        explicit_write["status"], "completed",
        "explicit write: {explicit_write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&explicitly_allowed_file).unwrap(),
        "explicit-ok"
    );

    let outside_file = outside.join("must-not-write.txt");
    let outside_write = foreground(
        &mut aft,
        "native-outside-write",
        &format!("printf denied > {}", quote(&outside_file)),
    );
    assert_eq!(
        outside_write["status"], "failed",
        "outside write should be denied: {outside_write:?}"
    );
    assert!(!outside_file.exists());

    let temp_path_file = project.join("task-temp-path.txt");
    let temp_probe = foreground(
        &mut aft,
        "native-temp-write",
        &format!(
            "printf '%s' \"$TMPDIR\" > {}; printf temp-ok > \"$TMPDIR/probe.txt\"",
            quote(&temp_path_file)
        ),
    );
    assert_eq!(
        temp_probe["status"], "completed",
        "temp probe: {temp_probe:?}"
    );
    let task_temp = PathBuf::from(std::fs::read_to_string(&temp_path_file).unwrap());
    let canonical_storage = storage.canonicalize().unwrap();
    assert!(
        task_temp.starts_with(&canonical_storage),
        "task temp must live in the task bundle: {}",
        task_temp.display()
    );
    assert_eq!(
        std::fs::read_to_string(task_temp.join("probe.txt")).unwrap(),
        "temp-ok"
    );

    let cache_file = npm_cache.join("native-cache-write.txt");
    let cache_write = foreground(
        &mut aft,
        "native-cache-write",
        &format!("printf cache-ok > {}", quote(&cache_file)),
    );
    assert_eq!(
        cache_write["status"], "completed",
        "cache write: {cache_write:?}"
    );
    assert_eq!(std::fs::read_to_string(&cache_file).unwrap(), "cache-ok");

    let scanner = foreground(&mut aft, "native-scanner", "echo native-scanner");
    assert_eq!(
        scanner["success"], true,
        "native scanner must not request permission: {scanner:?}"
    );
    let task_id = scanner["task_id"].as_str().expect("scanner task id");
    let scanner_status = status(&mut aft, task_id, None);
    assert!(
        scanner_status["scanner_report"]
            .as_array()
            .is_some_and(|report| !report.is_empty()),
        "scanner findings must be retained in task metadata: {scanner_status:?}"
    );

    let disabled = configure_native(&mut aft, &project, &storage, false);
    assert_eq!(
        disabled["success"], true,
        "disable configure failed: {disabled:?}"
    );
    let permission = foreground(&mut aft, "disabled-scanner", "echo needs-permission");
    assert_eq!(
        permission["success"], false,
        "disabled scanner response: {permission:?}"
    );
    assert_eq!(permission["code"], "permission_required");

    assert!(aft.shutdown().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_sandbox_denies_credentials_and_nested_git_writes_on_macos() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let ssh = home.join(".ssh");
    let secret = ssh.join("id_test");
    let configured_secret = home.join("extra-secret.txt");
    let git = project.join(".git");
    let git_config = git.join("config");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(&secret, "credential").unwrap();
    std::fs::write(&configured_secret, "configured-secret").unwrap();
    std::fs::write(&git_config, "safe").unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        &[],
        std::slice::from_ref(&configured_secret),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let read = foreground(
        &mut aft,
        "native-secret-read",
        &format!("cat {}", quote(&secret)),
    );
    assert_eq!(
        read["status"], "failed",
        "secret read should fail: {read:?}"
    );
    assert!(!read["output"]
        .as_str()
        .unwrap_or_default()
        .contains("credential"));

    let configured_read = foreground(
        &mut aft,
        "native-configured-secret-read",
        &format!(
            "/bin/sh -c 'cat \"$1\"' native-read {}",
            quote(&configured_secret)
        ),
    );
    assert_eq!(
        configured_read["status"], "failed",
        "configured secret read should fail: {configured_read:?}"
    );

    let git_write = foreground(
        &mut aft,
        "native-git-write",
        &format!("printf corrupted > {}", quote(&git_config)),
    );
    assert_eq!(
        git_write["status"], "failed",
        "nested git write should fail: {git_write:?}"
    );
    assert_eq!(std::fs::read_to_string(&git_config).unwrap(), "safe");

    assert!(aft.shutdown().success());
}

#[test]
fn native_sandbox_pty_denies_outside_write_and_renders_screen() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let outside = home.join("outside");
    let outside_file = outside.join("pty-must-not-write.txt");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native(&mut aft, &project, &storage, true);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let launch = aft.send(
        &json!({
            "id": "native-pty",
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": format!(
                    "printf denied > {}; printf '\npty-screen-rendered\n'",
                    quote(&outside_file)
                ),
                "pty": true,
                "permissions_requested": true,
                "compressed": false,
                "pty_rows": 24,
                "pty_cols": 80,
            },
        })
        .to_string(),
    );
    assert_eq!(launch["success"], true, "PTY launch failed: {launch:?}");
    let task_id = launch["task_id"].as_str().expect("PTY task id");
    let terminal = wait_for_terminal(&mut aft, task_id, Some("screen"));
    assert!(!outside_file.exists());
    assert!(
        terminal["pty_screen"]
            .as_str()
            .unwrap_or_default()
            .contains("pty-screen-rendered"),
        "PTY screen should render after the denied write: {terminal:?}"
    );

    #[cfg(target_os = "linux")]
    {
        let output = terminal["pty_screen"].as_str().unwrap_or_default();
        assert_eq!(
            output.matches("sandbox-launch: unenforced=").count(),
            1,
            "Linux enforcement warning must be captured once: {terminal:?}"
        );
    }

    assert!(aft.shutdown().success());
}
