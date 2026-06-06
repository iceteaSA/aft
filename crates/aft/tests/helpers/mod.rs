#![allow(dead_code)]

//! Shared test helpers for integration tests.
//!
//! Provides `AftProcess` — a handle to a running aft binary with piped I/O —
//! and `fixture_path` for resolving test fixture files.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

enum StdoutEvent {
    Json(serde_json::Value),
    Eof,
    IoError(String),
    ParseError { line: String, error: String },
}

/// A handle to a running aft process with piped I/O.
///
/// Uses a persistent `BufReader` over stdout so sequential reads
/// don't lose buffered data between calls.
pub struct AftProcess {
    child: Child,
    stdout_rx: Receiver<StdoutEvent>,
    stdout_eof: bool,
    pending_frames: VecDeque<serde_json::Value>,
    diag_enabled: bool,
    spawned_at: Instant,
    stdout_trace_log_path: Option<PathBuf>,
    stdout_trace_log: Option<Arc<Mutex<std::fs::File>>>,
    stdout_capture_thread: Option<JoinHandle<()>>,
    stderr_log_path: Option<PathBuf>,
    stderr_capture_thread: Option<JoinHandle<String>>,
    _cache_dir: tempfile::TempDir,
}

impl AftProcess {
    /// Spawn the aft binary with piped stdin/stdout/stderr.
    /// Sets AFT_CACHE_DIR to a temp path so tests don't pollute the user's cache.
    pub fn spawn() -> Self {
        Self::spawn_inner(&[], false)
    }

    /// Spawn the aft binary with additional environment variables.
    /// Stderr is suppressed by default. Use `spawn_with_stderr()` for tests
    /// that need to inspect stderr output.
    pub fn spawn_with_env(envs: &[(&str, &std::ffi::OsStr)]) -> Self {
        Self::spawn_inner(envs, false)
    }

    /// Spawn with stderr piped so tests can read it via `stderr_output()`.
    pub fn spawn_with_stderr() -> Self {
        Self::spawn_inner(&[], true)
    }

    fn spawn_inner(envs: &[(&str, &std::ffi::OsStr)], pipe_stderr: bool) -> Self {
        let binary = std::env::var_os("AFT_TEST_AFT_BINARY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_aft")));
        let diag_enabled =
            std::env::var_os("AFT_TEST_DIAG").as_deref() == Some(std::ffi::OsStr::new("1"));
        let cache_dir = tempfile::tempdir().expect("create aft test cache dir");
        let mut command = Command::new(binary);
        command
            .env("AFT_CACHE_DIR", cache_dir.path())
            // Integration tests often run from a linked git worktree, but their
            // fixture projects need to exercise main-checkout demand builds.
            .env("AFT_TEST_ALLOW_WORKTREE_STORE_BUILD", "1")
            // Callgraph store cold build is pure-async in production (returns
            // `Building`, agent retries). Fixture projects are tiny (build in
            // ~100ms), so default the test harness to a large inline-wait window
            // so the first callgraph op resolves to `Ready` synchronously. A
            // failed build disconnects the channel and returns immediately, so
            // this never hangs. Lifecycle tests override it to "0" to exercise
            // the real Building -> drain -> Ready path.
            .env("AFT_CALLGRAPH_BUILD_WAIT_MS", "30000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if pipe_stderr || diag_enabled {
                Stdio::piped()
            } else {
                Stdio::null()
            });

        for (key, value) in envs {
            command.env(key, value);
        }

        let mut child = command.spawn().expect("failed to spawn aft binary");
        let child_pid = child.id();

        let spawned_at = Instant::now();
        let (stdout_trace_log_path, stdout_trace_log, stderr_log_path, stderr_capture_thread) =
            if diag_enabled {
                let target_tmpdir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
                std::fs::create_dir_all(&target_tmpdir).expect("create CARGO_TARGET_TMPDIR");

                let stdout_trace_log_path =
                    target_tmpdir.join(format!("aft-test-stdout-trace-{child_pid}.log"));
                let stdout_trace_log = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&stdout_trace_log_path)
                    .expect("open aft stdout trace log");
                let stdout_trace_log = Arc::new(Mutex::new(stdout_trace_log));

                let stderr_log_path =
                    target_tmpdir.join(format!("aft-test-stderr-{child_pid}.log"));
                let stderr = child.stderr.take().expect("stderr handle");
                let stderr_capture_thread =
                    Some(spawn_stderr_capture_thread(stderr, stderr_log_path.clone()));

                (
                    Some(stdout_trace_log_path),
                    Some(stdout_trace_log),
                    Some(stderr_log_path),
                    stderr_capture_thread,
                )
            } else {
                (None, None, None, None)
            };

        let stdout = child.stdout.take().expect("stdout handle");
        let (stdout_rx, stdout_capture_thread) =
            spawn_stdout_capture_thread(stdout, stdout_trace_log.clone(), spawned_at);

        AftProcess {
            child,
            stdout_rx,
            stdout_eof: false,
            pending_frames: VecDeque::new(),
            diag_enabled,
            spawned_at,
            stdout_trace_log_path,
            stdout_trace_log,
            stdout_capture_thread: Some(stdout_capture_thread),
            stderr_log_path,
            stderr_capture_thread,
            _cache_dir: cache_dir,
        }
    }

    /// Send a raw line and read back the JSON response.
    pub fn send(&mut self, request: &str) -> serde_json::Value {
        self.send_with_timeout(request, DEFAULT_RESPONSE_TIMEOUT)
    }

    pub fn send_with_timeout(&mut self, request: &str, timeout: Duration) -> serde_json::Value {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");

        let request_id = serde_json::from_str::<serde_json::Value>(request)
            .ok()
            .and_then(|value| value["id"].as_str().map(str::to_string));
        loop {
            let value = self.read_json_line_timeout(timeout, "response line");
            if value.get("type").is_some() && value.get("id").is_none() {
                self.pending_frames.push_back(value);
                continue;
            }
            if request_id
                .as_deref()
                .is_none_or(|request_id| value["id"] == request_id)
            {
                return value;
            }
            return value;
        }
    }

    /// Read the next JSON line from stdout without writing a request first.
    pub fn read_next(&mut self) -> serde_json::Value {
        if let Some(value) = self.pending_frames.pop_front() {
            return value;
        }
        self.read_json_line()
    }

    /// Try to read one JSON line from stdout within a short timeout.
    pub fn try_read_next_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> Option<serde_json::Value> {
        if let Some(value) = self.pending_frames.pop_front() {
            return Some(value);
        }

        if self.stdout_eof {
            return None;
        }

        match self.stdout_rx.recv_timeout(timeout) {
            Ok(StdoutEvent::Json(value)) => Some(value),
            Ok(StdoutEvent::Eof) => {
                self.stdout_eof = true;
                None
            }
            Ok(StdoutEvent::IoError(error)) => panic!("read from stdout: {error}"),
            Ok(StdoutEvent::ParseError { line, error }) => {
                panic!("parse response JSON: {error}; line: {line}")
            }
            Err(RecvTimeoutError::Timeout) => {
                self.trace_event(&format!(
                    "STDOUT_POLL_TIMEOUT (no data within {}ms)",
                    timeout.as_millis()
                ));
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_eof = true;
                None
            }
        }
    }

    fn read_json_line(&mut self) -> serde_json::Value {
        self.read_json_line_timeout(DEFAULT_RESPONSE_TIMEOUT, "response line")
    }

    fn read_json_line_timeout(&mut self, timeout: Duration, context: &str) -> serde_json::Value {
        assert!(
            !self.stdout_eof,
            "expected {context} but stdout was already at EOF from aft"
        );
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(StdoutEvent::Json(value)) => value,
            Ok(StdoutEvent::Eof) => {
                self.stdout_eof = true;
                panic!("expected {context} but got EOF from aft");
            }
            Ok(StdoutEvent::IoError(error)) => panic!("read from stdout: {error}"),
            Ok(StdoutEvent::ParseError { line, error }) => {
                panic!("parse response JSON: {error}; line: {line}")
            }
            Err(RecvTimeoutError::Timeout) => {
                let child_status = match self.child.try_wait() {
                    Ok(Some(status)) => format!("exited with status {status}"),
                    Ok(None) => "still running".to_string(),
                    Err(error) => format!("try_wait error: {error}"),
                };
                panic!(
                    "timed out after {}s waiting for {context} from aft stdout (child {child_status})",
                    timeout.as_secs()
                );
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_eof = true;
                panic!("expected {context} but stdout reader disconnected");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn queue_pending_frame_for_test(&mut self, frame: serde_json::Value) {
        self.pending_frames.push_back(frame);
    }

    /// Send a configure command with project_root.
    pub fn configure(&mut self, project_root: &std::path::Path) -> serde_json::Value {
        // Build via serde_json so Windows paths (with backslashes) are
        // escaped correctly in the wire format. Hand-formatted JSON would
        // turn `C:\Users\...` into invalid escape sequences.
        let request = serde_json::json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project_root.to_string_lossy(),
        });
        self.send(&request.to_string())
    }

    /// Wait for and consume a `configure_warnings` push frame, returning its
    /// `warnings` array merged into the original configure response.
    ///
    /// Configure now defers the file-walk + missing-binary detection to a
    /// background thread (so it can return in <100 ms even on huge directories).
    /// Tests that previously relied on synchronous warnings should call this
    /// helper after `configure` to merge the async results back in:
    ///
    /// ```rust,ignore
    /// let configure = aft.send(json!({"id":"cfg",...}).to_string().as_str());
    /// let configure = aft.merge_configure_warnings(configure);
    /// // configure["warnings"] now contains the async warnings
    /// ```
    pub fn merge_configure_warnings(
        &mut self,
        mut configure: serde_json::Value,
    ) -> serde_json::Value {
        let frame = self.wait_for_configure_warnings_frame();
        let warnings = frame
            .get("warnings")
            .and_then(|warnings| warnings.as_array())
            .cloned()
            .unwrap_or_default();
        configure["warnings"] = serde_json::Value::Array(warnings);
        if let Some(count) = frame.get("source_file_count").cloned() {
            configure["source_file_count"] = count;
        }
        if let Some(exceeds) = frame.get("source_file_count_exceeds_max").cloned() {
            configure["source_file_count_exceeds_max"] = exceeds;
        }
        configure["warnings_pending"] = serde_json::Value::Bool(false);
        configure
    }

    /// Read frames until a `configure_warnings` push frame arrives, then
    /// return it. Panics if a non-frame response (one with an `id`) arrives
    /// before the frame, or if EOF is hit, or if no frame arrives within 60s.
    fn wait_for_configure_warnings_frame(&mut self) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        let poll_interval = Duration::from_millis(100);
        loop {
            while let Some(value) = self.pending_frames.pop_front() {
                if value.get("type").and_then(|kind| kind.as_str()) == Some("configure_warnings") {
                    return value;
                }
                // Other push frames (progress, bash_completed) are skipped silently.
            }

            let now = Instant::now();
            if now >= deadline {
                if self.diag_enabled {
                    self.panic_configure_warnings_timeout();
                } else {
                    panic!(
                        "timed out waiting for configure_warnings push frame after 60s — \
                         background configure-warnings worker either crashed or progress_sender \
                         was not installed"
                    );
                }
            }
            let remaining = deadline - now;
            let timeout = std::cmp::min(remaining, poll_interval);
            if let Some(value) = self.try_read_next_timeout(timeout) {
                if value.get("type").and_then(|kind| kind.as_str()) == Some("configure_warnings") {
                    return value;
                }
                // Other push frames (progress, bash_completed) are skipped silently.
                continue;
            }
            // No data within poll_interval — loop and re-check deadline.
        }
    }

    /// Send a raw line that should produce no response (e.g. empty line).
    /// Verifies the process is still alive by sending a follow-up ping.
    pub fn send_silent(&mut self, request: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");
    }

    /// Send a raw line and collect response lines until `predicate` returns true.
    pub fn send_until<F>(&mut self, request: &str, mut predicate: F) -> Vec<serde_json::Value>
    where
        F: FnMut(&serde_json::Value) -> bool,
    {
        let stdin = self.child.stdin.as_mut().expect("stdin handle");
        writeln!(stdin, "{}", request).expect("write to stdin");
        stdin.flush().expect("flush stdin");

        let mut responses = Vec::new();
        loop {
            let value = self.read_next();
            let done = predicate(&value);
            responses.push(value);
            if done {
                return responses;
            }
        }
    }

    /// Close stdin and wait for the process to exit. Returns the exit status.
    pub fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.child.stdin.take());
        let status = self.wait_for_child_exit(SHUTDOWN_TIMEOUT, "shutdown after stdin close");
        if let Some(handle) = self.stdout_capture_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_capture_thread.take() {
            // Join the AFT_TEST_DIAG=1 capture thread before returning so
            // parallel test processes cannot leave detached stderr writers
            // interleaving with later tests.
            let _ = handle.join();
        }
        status
    }

    /// Return the PID of the spawned aft process.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Path to the stderr capture log when `AFT_TEST_DIAG=1` is enabled.
    pub fn stderr_log_path(&self) -> Option<&Path> {
        self.stderr_log_path.as_deref()
    }

    /// Path to the stdout trace log when `AFT_TEST_DIAG=1` is enabled.
    pub fn stdout_trace_log_path(&self) -> Option<&Path> {
        self.stdout_trace_log_path.as_deref()
    }

    /// Read stderr contents after process exits.
    pub fn stderr_output(mut self) -> (std::process::ExitStatus, String) {
        drop(self.child.stdin.take());
        let status = self.wait_for_child_exit(SHUTDOWN_TIMEOUT, "stderr_output after stdin close");
        if let Some(handle) = self.stdout_capture_thread.take() {
            let _ = handle.join();
        }
        let mut stderr_content = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            use std::io::Read;
            stderr.read_to_string(&mut stderr_content).ok();
        } else if let Some(stderr_capture_thread) = self.stderr_capture_thread.take() {
            stderr_content = stderr_capture_thread.join().unwrap_or_default();
        }
        (status, stderr_content)
    }

    fn wait_for_child_exit(
        &mut self,
        timeout: Duration,
        context: &str,
    ) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!(
                        "timed out after {}s waiting for aft process exit during {context}; child was killed",
                        timeout.as_secs()
                    );
                }
                Err(error) => panic!("wait for process exit during {context}: {error}"),
            }
        }
    }

    fn trace_event(&mut self, event: &str) {
        if !self.diag_enabled {
            return;
        }
        write_trace_event(&self.stdout_trace_log, self.spawned_at, event);
    }

    fn panic_configure_warnings_timeout(&mut self) -> ! {
        let stderr_log_path = self
            .stderr_log_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<disabled>".to_string());
        let stdout_trace_log_path = self
            .stdout_trace_log_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<disabled>".to_string());

        let child_status = match self.child.try_wait() {
            Ok(Some(status)) => format!("exited with status {status}"),
            Ok(None) => "still running".to_string(),
            Err(error) => format!("try_wait error: {error}"),
        };

        let pending_frames = describe_pending_frames(&self.pending_frames);
        let grace_read_result = self.grace_read_result(Duration::from_secs(5));

        let stderr_capture = read_log_file_tail(self.stderr_log_path.as_deref(), 64 * 1024);
        let stdout_trace = read_log_file_tail(self.stdout_trace_log_path.as_deref(), 64 * 1024);

        eprintln!("===== aft child stderr capture ({stderr_log_path}) =====\n{stderr_capture}");
        eprintln!("===== aft stdout trace ({stdout_trace_log_path}) =====\n{stdout_trace}");

        panic!(
            "timed out waiting for configure_warnings push frame after 60s\n \
             ↳ child status: {child_status}\n \
             ↳ pending_frames queue ({} entries):\n{}\n \
             ↳ stdout trace log: {stdout_trace_log_path}\n \
             ↳ stderr capture log: {stderr_log_path}\n \
             ↳ grace 5s read result: {grace_read_result}\n \
             ↳ full stderr capture:\n{stderr_capture}\n \
             ↳ full stdout trace:\n{stdout_trace}",
            self.pending_frames.len(),
            pending_frames
        );
    }

    fn grace_read_result(&mut self, timeout: Duration) -> String {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.try_read_next_timeout(timeout)
        })) {
            Ok(Some(value)) => format!("frame arrived: {}", value),
            Ok(None) => "timeout".to_string(),
            Err(_) => "io error: try_read_next_timeout panicked; see stdout trace".to_string(),
        }
    }
}

fn spawn_stdout_capture_thread(
    stdout: std::process::ChildStdout,
    trace_log: Option<Arc<Mutex<std::fs::File>>>,
    spawned_at: Instant,
) -> (Receiver<StdoutEvent>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    write_trace_event(&trace_log, spawned_at, "STDOUT_EOF");
                    let _ = tx.send(StdoutEvent::Eof);
                    break;
                }
                Ok(_) => {
                    write_trace_event(
                        &trace_log,
                        spawned_at,
                        &format!("STDOUT_LINE: {}", truncate_for_trace(&line)),
                    );
                    match serde_json::from_str(line.trim()) {
                        Ok(value) => {
                            if tx.send(StdoutEvent::Json(value)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let rendered_line = truncate_for_trace(&line);
                            let _ = tx.send(StdoutEvent::ParseError {
                                line: rendered_line,
                                error: error.to_string(),
                            });
                            break;
                        }
                    }
                }
                Err(error) => {
                    write_trace_event(
                        &trace_log,
                        spawned_at,
                        &format!("STDOUT_READ_ERR: {:?}: {}", error.kind(), error),
                    );
                    let _ = tx.send(StdoutEvent::IoError(format!(
                        "{:?}: {}",
                        error.kind(),
                        error
                    )));
                    break;
                }
            }
        }
    });

    (rx, handle)
}

fn write_trace_event(
    trace_log: &Option<Arc<Mutex<std::fs::File>>>,
    spawned_at: Instant,
    event: &str,
) {
    let Some(log) = trace_log else {
        return;
    };
    let elapsed_ms = spawned_at.elapsed().as_millis();
    let mut log = log.lock().expect("lock aft stdout trace log");
    writeln!(log, "[{}][{}] {}", iso_timestamp_now(), elapsed_ms, event)
        .expect("write aft stdout trace log");
    log.flush().expect("flush aft stdout trace log");
}

fn spawn_stderr_capture_thread(
    stderr: std::process::ChildStderr,
    stderr_log_path: PathBuf,
) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&stderr_log_path)
            .expect("open aft stderr capture log");
        let mut captured = Vec::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    log.write_all(&buffer)
                        .expect("write aft stderr capture log");
                    log.flush().expect("flush aft stderr capture log");
                    captured.extend_from_slice(&buffer);
                    eprint!("{}", String::from_utf8_lossy(&buffer));
                }
                Err(error) => {
                    let message = format!("[aft-test stderr capture read error: {error}]\n");
                    log.write_all(message.as_bytes())
                        .expect("write aft stderr capture error");
                    log.flush().expect("flush aft stderr capture error");
                    captured.extend_from_slice(message.as_bytes());
                    eprint!("{message}");
                    break;
                }
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    })
}

fn truncate_for_trace(line: &str) -> String {
    const LIMIT: usize = 4096;
    let bytes = line.as_bytes();
    let mut rendered = if bytes.len() <= LIMIT {
        line.to_string()
    } else {
        let mut end = LIMIT;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}...[truncated at 4096B, total {}bytes]",
            &line[..end],
            bytes.len()
        )
    };
    if rendered.ends_with('\n') {
        rendered.pop();
        if rendered.ends_with('\r') {
            rendered.pop();
        }
    }
    rendered
}

fn read_log_file_tail(path: Option<&Path>, max_bytes: usize) -> String {
    const MAX: usize = 64 * 1024;
    let max_bytes = max_bytes.min(MAX);
    match path {
        Some(path) => {
            let result = (|| -> std::io::Result<String> {
                let mut file = std::fs::File::open(path)?;
                let len = file.metadata()?.len() as usize;
                if len <= max_bytes {
                    let mut s = String::new();
                    use std::io::Read;
                    file.read_to_string(&mut s)?;
                    return Ok(s);
                }

                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::End(-(max_bytes as i64)))?;
                let mut buf = Vec::with_capacity(max_bytes);
                file.read_to_end(&mut buf)?;
                let lossy = String::from_utf8_lossy(&buf).into_owned();
                // Drop the (potentially mid-multibyte) first line so output starts clean.
                let after_first_newline = lossy.find('\n').map(|i| i + 1).unwrap_or(0);
                Ok(format!(
                    "...[truncated to tail {max_bytes}B of total {len}B]\n{}",
                    &lossy[after_first_newline..]
                ))
            })();
            result.unwrap_or_else(|error| format!("<failed to read {}: {error}>", path.display()))
        }
        None => "<disabled>".to_string(),
    }
}

fn describe_pending_frames(pending_frames: &VecDeque<serde_json::Value>) -> String {
    if pending_frames.is_empty() {
        return "    <empty>".to_string();
    }
    pending_frames
        .iter()
        .map(|frame| {
            let frame_type = frame
                .get("type")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let message = frame.get("message").cloned();
            match message {
                Some(message) => format!(
                    "    - {{ type: {}, id: {}, message: {} }}",
                    frame_type, id, message
                ),
                None => format!("    - {{ type: {}, id: {} }}", frame_type, id),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn iso_timestamp_now() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_seconds = duration.as_secs() as i64;
    let millis = duration.subsec_millis();
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

/// Resolve the crate manifest directory.
pub fn cargo_manifest_dir() -> PathBuf {
    normalize_embedded_manifest_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

#[cfg(not(windows))]
fn normalize_embedded_manifest_dir(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
fn normalize_embedded_manifest_dir(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    // The Windows VM verification path cross-compiles the test binary on macOS.
    // Cargo embeds a Unix-shaped manifest dir such as `/Users/.../crates/aft`;
    // on Windows that is drive-relative, not absolute, so AFT correctly rejects
    // it as a project root. Map that shape onto the current drive for the VM.
    let raw = path.to_string_lossy();
    let Some(stripped) = raw.strip_prefix('/').or_else(|| raw.strip_prefix('\\')) else {
        return path;
    };
    let Some(drive) = current_windows_drive() else {
        return path;
    };
    PathBuf::from(format!("{drive}:/{stripped}"))
}

#[cfg(windows)]
fn current_windows_drive() -> Option<char> {
    use std::path::{Component, Prefix};

    match std::env::current_dir().ok()?.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                Some((drive as char).to_ascii_uppercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Resolve a fixture file path relative to the project root.
pub fn fixture_path(name: &str) -> PathBuf {
    cargo_manifest_dir()
        .join("tests")
        .join("fixtures")
        .join(name)
}

// ---------------------------------------------------------------------------
// Shared warn-level log capture for integration tests
//
// All integration tests compile into ONE binary, and `log` allows exactly one
// global logger per process. Previously two test modules each installed their
// own logger via `log::set_boxed_logger(...).expect(...)`; whichever ran second
// panicked with `SetLoggerError`, and even without the panic only one module's
// logger could win — the other module's `take_logs()` would read an empty
// buffer because records routed to the winner's storage.
//
// Fix: a SINGLE process-global logger installed once, routing each record to a
// THREAD-LOCAL buffer. Because every `#[test]` runs on its own thread, capture
// is isolated per test and race-free under parallel execution. Tests call
// `init_test_logger()` (installs once, clears this thread's buffer) and
// `take_logs()` (drains this thread's buffer).
// ---------------------------------------------------------------------------

use std::sync::Once as StdOnce;

thread_local! {
    static THREAD_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

static SHARED_LOGGER_INIT: StdOnce = StdOnce::new();

struct ThreadLocalTestLogger;

impl log::Log for ThreadLocalTestLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let line = format!("{}", record.args());
            THREAD_LOGS.with(|logs| logs.borrow_mut().push(line));
        }
    }

    fn flush(&self) {}
}

/// Install the shared thread-local-capturing logger (once per process) and
/// clear the CURRENT thread's captured logs so each test starts clean.
pub fn init_test_logger() {
    SHARED_LOGGER_INIT.call_once(|| {
        // Ignore an already-installed logger: another harness (or a prior
        // `Once` in legacy code) may have installed one. We only need warn-level
        // capture, and a failed install must never panic the test binary.
        let _ = log::set_boxed_logger(Box::new(ThreadLocalTestLogger));
        log::set_max_level(log::LevelFilter::Warn);
    });
    THREAD_LOGS.with(|logs| logs.borrow_mut().clear());
}

/// Drain and return the warn-level logs captured on the CURRENT thread since
/// the last `init_test_logger()` / `take_logs()`.
pub fn take_logs() -> Vec<String> {
    THREAD_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
}
