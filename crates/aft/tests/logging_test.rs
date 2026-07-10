use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[test]
fn spawned_aft_writes_durable_log_under_aft_cache_dir() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_aft"))
        .env("AFT_CACHE_DIR", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"{\"id\":\"logging-smoke\",\"command\":\"echo\",\"message\":\"ok\"}\n")
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_path = temp
        .path()
        .join("aft")
        .join("logs")
        .join(format!("aft-{pid}.log"));
    for _ in 0..20 {
        if std::fs::read_to_string(&log_path)
            .is_ok_and(|contents| contents.contains("started, pid"))
        {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("durable log was not written at {}", log_path.display());
}
