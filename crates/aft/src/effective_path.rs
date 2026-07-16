//! Process-wide PATH enrichment for children spawned by AFT.
//!
//! Daemon launches can inherit a system-only PATH that misses the user's package
//! managers and version-manager shims. AFT initializes this module before any
//! helper threads start so later subprocesses inherit the same PATH a login
//! terminal would provide.

use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use serde::{Deserialize, Serialize};

#[cfg(not(unix))]
static EFFECTIVE_PATH: OnceLock<OsString> = OnceLock::new();

#[cfg(unix)]
#[derive(Clone, Debug)]
struct PathState {
    path: &'static OsStr,
    is_fallback: bool,
    last_probe_attempt: Option<Instant>,
}

#[cfg(unix)]
static EFFECTIVE_PATH_STATE: std::sync::Mutex<Option<PathState>> = std::sync::Mutex::new(None);

#[cfg(unix)]
const LOGIN_SHELL_PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Compute and export AFT's process PATH.
///
/// Call this during process startup, before AFT starts worker threads or async
/// executors. Mutating process environment variables while other threads may be
/// reading them is not safe on Unix, so later code should read the cached value
/// with [`effective_path`] instead of calling this initializer again.
pub fn initialize_process_path() -> &'static OsStr {
    let path = effective_path();

    #[cfg(unix)]
    {
        if path != OsStr::new("") && std::env::var_os("PATH").as_deref() != Some(path) {
            std::env::set_var("PATH", path);
        }
    }

    path
}

/// Create a new `Command` with the effective PATH set on Unix.
#[cfg(unix)]
pub fn new_command<S: AsRef<OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.env("PATH", effective_path());
    cmd
}

/// On Windows the process PATH is already correct (registry-backed
/// environment block); pass through without touching the child env.
#[cfg(not(unix))]
pub fn new_command<S: AsRef<OsStr>>(program: S) -> std::process::Command {
    std::process::Command::new(program)
}

/// Return the cached PATH that subprocesses should inherit.
///
/// On Windows this is the process PATH unchanged: Windows daemon environments
/// already receive PATH from the registry-backed environment block.
#[cfg(not(unix))]
pub fn effective_path() -> &'static OsStr {
    EFFECTIVE_PATH
        .get_or_init(compute_effective_path)
        .as_os_str()
}

#[cfg(unix)]
pub fn effective_path() -> &'static OsStr {
    // Test seam: integration tests construct exact PATHs (e.g. to simulate a
    // missing formatter binary); probing and enrichment would re-add real tool
    // dirs from the host and break that isolation. Checked at runtime because
    // the spawned test binary is a production build.
    // "0" reads as unset so the PATH feature's own integration tests can
    // opt back in to probing under a test harness that defaults the seam on.
    if std::env::var_os("AFT_TEST_RAW_PATH").is_some_and(|v| v != "0" && !v.is_empty()) {
        static RAW: OnceLock<OsString> = OnceLock::new();
        return RAW
            .get_or_init(|| std::env::var_os("PATH").unwrap_or_default())
            .as_os_str();
    }
    let mut guard = EFFECTIVE_PATH_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state_opt = guard.clone();
    if let Some(state) = state_opt {
        if state.is_fallback {
            let now = Instant::now();
            let should_retry = state
                .last_probe_attempt
                .map(|last| now.duration_since(last) >= Duration::from_secs(60))
                .unwrap_or(true);

            if should_retry {
                let last_attempt = Some(now);
                let mut temp_state = state.clone();
                temp_state.last_probe_attempt = last_attempt;
                *guard = Some(temp_state);
                drop(guard);

                let new_path_opt = probe_login_shell_path();

                let mut guard = EFFECTIVE_PATH_STATE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(new_path) = new_path_opt {
                    write_login_path_memo(&new_path);
                    let current = std::env::var_os("PATH").unwrap_or_default();
                    let merged = merge_login_and_current_path(&new_path, &current);
                    let home = std::env::var_os("HOME");
                    let enriched =
                        append_missing_standard_dirs(&merged, home.as_deref(), |dir| dir.is_dir());
                    let leaked: &'static OsStr = Box::leak(enriched.into_boxed_os_str());
                    let new_state = PathState {
                        path: leaked,
                        is_fallback: false,
                        last_probe_attempt: None,
                    };
                    *guard = Some(new_state);
                    return leaked;
                } else {
                    let memo_path_opt = read_login_path_memo();
                    if let Some(mut current_state) = guard.clone() {
                        current_state.last_probe_attempt = Some(Instant::now());
                        if let Some(memo_path) = memo_path_opt {
                            let current = std::env::var_os("PATH").unwrap_or_default();
                            let merged = merge_login_and_current_path(&memo_path, &current);
                            let home = std::env::var_os("HOME");
                            let enriched =
                                append_missing_standard_dirs(&merged, home.as_deref(), |dir| {
                                    dir.is_dir()
                                });
                            let leaked: &'static OsStr = Box::leak(enriched.into_boxed_os_str());
                            current_state.path = leaked;
                            current_state.is_fallback = false;
                        }
                        *guard = Some(current_state);
                        return guard.as_ref().unwrap().path;
                    }
                }
            }
        }
        return state.path;
    }

    let current = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME");

    let mut is_fallback = false;
    let mut last_probe_attempt = None;

    let path = if !path_is_impoverished(&current, home.as_deref(), |dir| dir.is_dir()) {
        current.to_os_string()
    } else {
        if let Some(login_path) = probe_login_shell_path() {
            write_login_path_memo(&login_path);
            merge_login_and_current_path(&login_path, &current)
        } else if let Some(memo_path) = read_login_path_memo() {
            merge_login_and_current_path(&memo_path, &current)
        } else {
            is_fallback = true;
            last_probe_attempt = Some(Instant::now());
            current.to_os_string()
        }
    };

    let enriched = append_missing_standard_dirs(&path, home.as_deref(), |dir| dir.is_dir());
    let leaked: &'static OsStr = Box::leak(enriched.into_boxed_os_str());
    *guard = Some(PathState {
        path: leaked,
        is_fallback,
        last_probe_attempt,
    });
    leaked
}

#[cfg(windows)]
fn compute_effective_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

#[cfg(not(any(unix, windows)))]
fn compute_effective_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_default()
}

#[cfg(unix)]
fn path_is_impoverished<D>(current: &OsStr, home: Option<&OsStr>, mut dir_exists: D) -> bool
where
    D: FnMut(&Path) -> bool,
{
    let current_entries: Vec<PathBuf> = std::env::split_paths(current).collect();
    core_standard_path_dirs(home).into_iter().any(|dir| {
        dir_exists(&dir)
            && !current_entries
                .iter()
                .any(|entry| entry.as_path() == dir.as_path())
    })
}

/// Core tool dirs whose absence from PATH signals an impoverished daemon
/// environment worth paying a login-shell probe for. Deliberately excludes
/// the interactive-gated extras below: those are appended unconditionally by
/// `append_missing_standard_dirs`, so missing them alone never justifies a
/// probe.
#[cfg(unix)]
fn core_standard_path_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = home {
        let home = PathBuf::from(home);
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

/// All dirs merged into every constructed PATH when present on disk. Includes
/// installers that only amend interactive shell rc blocks (bun, pnpm, mise,
/// deno, volta), which even a successful login-shell probe cannot see.
#[cfg(unix)]
fn user_standard_path_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut dirs = core_standard_path_dirs(home);
    if let Some(home) = home {
        let home = PathBuf::from(home);
        dirs.push(home.join(".bun/bin"));
        dirs.push(home.join("Library/pnpm"));
        dirs.push(home.join(".local/share/pnpm"));
        dirs.push(home.join(".local/share/mise/shims"));
        dirs.push(home.join(".deno/bin"));
        dirs.push(home.join(".volta/bin"));
    }
    dirs
}

#[cfg(unix)]
fn append_missing_standard_dirs<D>(
    path: &OsStr,
    home: Option<&OsStr>,
    mut dir_exists: D,
) -> OsString
where
    D: FnMut(&Path) -> bool,
{
    let mut entries: Vec<PathBuf> = std::env::split_paths(path).collect();
    let mut seen: HashSet<PathBuf> = entries.iter().cloned().collect();

    for dir in user_standard_path_dirs(home) {
        if dir_exists(&dir) && seen.insert(dir.clone()) {
            entries.push(dir);
        }
    }

    std::env::join_paths(entries).unwrap_or_else(|_| path.to_os_string())
}

#[cfg(unix)]
fn probe_login_shell_path() -> Option<OsString> {
    let candidates = login_shell_candidates();
    for shell in candidates {
        let Some(path) = probe_login_shell_path_once(&shell, LOGIN_SHELL_PATH_PROBE_TIMEOUT) else {
            continue;
        };
        if login_path_is_acceptable(&path) {
            return Some(path);
        }
    }
    None
}

#[cfg(unix)]
fn login_shell_candidates() -> Vec<PathBuf> {
    if cfg!(test) {
        if let Some(val) = std::env::var_os("AFT_TEST_LOGIN_SHELL_CANDIDATES") {
            return std::env::split_paths(&val).collect();
        }
    }
    let mut candidates = Vec::new();
    if let Some(shell) = std::env::var_os("SHELL").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(shell));
    }
    let zsh = PathBuf::from("/bin/zsh");
    let bash = PathBuf::from("/bin/bash");
    if !candidates.contains(&zsh) {
        candidates.push(zsh);
    }
    if !candidates.contains(&bash) {
        candidates.push(bash);
    }
    candidates
}

#[cfg(unix)]
fn set_nonblocking<F: AsRawFd>(file: &F) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn probe_login_shell_path_once(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut command = Command::new(shell);
    let is_fish = shell
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.eq_ignore_ascii_case("fish"))
        .unwrap_or(false);

    let cmd_str = if is_fish {
        r#"printf %s (string join : $PATH)"#
    } else {
        r#"printf %s "$PATH""#
    };

    command
        .arg("-l")
        .arg("-c")
        .arg(cmd_str)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Run the probe in its own session so the timeout can kill login-shell
    // startup helpers as well as the shell process itself.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let _ = set_nonblocking(&stdout);
    let mut output_bytes = Vec::new();
    let mut buf = [0u8; 1024];

    let deadline = Instant::now() + timeout;
    loop {
        use std::io::Read;
        match stdout.read(&mut buf) {
            Ok(0) => {
                let wait_deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if Instant::now() >= wait_deadline => {
                            kill_login_shell_probe(&mut child);
                            break;
                        }
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => {
                            kill_login_shell_probe(&mut child);
                            break;
                        }
                    }
                }
                return Some(OsString::from_vec(output_bytes));
            }
            Ok(n) => {
                output_bytes.extend_from_slice(&buf[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data available right now
            }
            Err(_) => {
                kill_login_shell_probe(&mut child);
                return None;
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => {
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => output_bytes.extend_from_slice(&buf[..n]),
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let _ = child.wait();
                return Some(OsString::from_vec(output_bytes));
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_login_shell_probe(&mut child);
                return None;
            }
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                kill_login_shell_probe(&mut child);
                return None;
            }
        }
    }
}

#[cfg(unix)]
fn kill_login_shell_probe(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    if pid > 0 {
        // Negative PID targets the process group created by setsid above.
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn login_path_is_acceptable(path: &OsStr) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty()
        || bytes
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
    {
        return false;
    }

    if bytes.contains(&b' ') {
        let mut abs_count = 0;
        for part in bytes.split(|&b| b == b' ') {
            if part.first() == Some(&b'/') {
                abs_count += 1;
            }
        }
        if abs_count > 1 {
            return false;
        }
    }

    bytes
        .split(|byte| *byte == b':')
        .all(|entry| entry.first() == Some(&b'/'))
}

#[cfg(unix)]
fn merge_login_and_current_path(login_path: &OsStr, current_path: &OsStr) -> OsString {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    for entry in std::env::split_paths(login_path).chain(std::env::split_paths(current_path)) {
        if seen.insert(entry.clone()) {
            merged.push(entry);
        }
    }

    std::env::join_paths(merged).unwrap_or_else(|_| current_path.to_os_string())
}

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
struct LoginPathMemo {
    login_path: String,
}

#[cfg(unix)]
fn read_login_path_memo() -> Option<OsString> {
    let memo_path = crate::bash_background::storage_dir(None).join("login-path-memo.json");
    let content = std::fs::read_to_string(&memo_path).ok()?;
    let memo: LoginPathMemo = serde_json::from_str(&content).ok()?;
    Some(OsString::from(memo.login_path))
}

#[cfg(unix)]
fn write_login_path_memo(path: &OsStr) {
    let memo_path = crate::bash_background::storage_dir(None).join("login-path-memo.json");
    if let Some(parent) = memo_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let memo = LoginPathMemo {
        login_path: path.to_string_lossy().into_owned(),
    };
    if let Ok(content) = serde_json::to_string(&memo) {
        let _ = std::fs::write(&memo_path, content);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct EnvVarGuard {
        key: &'static str,
        old_value: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old_value = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(val) = &self.old_value {
                std::env::set_var(self.key, val);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn reset_effective_path_state() {
        let mut guard = EFFECTIVE_PATH_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    #[test]
    fn impoverished_path_merges_login_first_and_keeps_current_entries() {
        let current = OsStr::new("/usr/bin:/bin:/home/alice/.cargo/bin:/custom/current");
        let login = OsString::from("/fake/login/bin:/usr/bin:/opt/homebrew/bin");

        let mut dir_exists = |_dir: &Path| true;
        let probe_login_path = || Some(login.clone());

        if !path_is_impoverished(current, Some(OsStr::new("/home/alice")), &mut dir_exists) {
            panic!("should be impoverished");
        }

        let login_path = probe_login_path()
            .filter(|path| login_path_is_acceptable(path))
            .unwrap();
        let effective = merge_login_and_current_path(&login_path, current);

        assert_eq!(
            effective,
            OsString::from(
                "/fake/login/bin:/usr/bin:/opt/homebrew/bin:/bin:/home/alice/.cargo/bin:/custom/current"
            )
        );
    }

    #[test]
    fn invalid_probe_paths_are_rejected() {
        let rejected = vec![
            OsString::new(),
            OsString::from("/fake/login/bin\n/usr/bin"),
            OsString::from("/fake/login/bin:relative/bin"),
            OsString::from_vec(b"/fake/login/bin\0/usr/bin".to_vec()),
            OsString::from("/usr/bin /bin /opt/homebrew/bin"), // fish-shaped space-joined
        ];

        for probe_path in rejected {
            assert!(!login_path_is_acceptable(&probe_path));
        }
    }

    #[test]
    fn rich_current_path_does_not_probe_login_shell() {
        let current = OsStr::new(
            "/opt/homebrew/bin:/usr/local/bin:/home/alice/.cargo/bin:/home/alice/.local/bin:/usr/bin:/bin",
        );

        assert!(!path_is_impoverished(
            current,
            Some(OsStr::new("/home/alice")),
            |_| true
        ));
    }

    #[test]
    fn login_shell_probe_times_out() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let shell = dir.path().join("slow-login-shell");
        fs::write(
            &shell,
            "#!/bin/sh\nsleep 10\nprintf '%s' '/fake/login/bin:/usr/bin:/bin'\n",
        )
        .expect("write fake shell");
        let mut permissions = fs::metadata(&shell)
            .expect("fake shell metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&shell, permissions).expect("chmod fake shell");

        let started = Instant::now();
        let probed = probe_login_shell_path_once(&shell, LOGIN_SHELL_PATH_PROBE_TIMEOUT);

        assert!(probed.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "login-shell PATH probe exceeded the 5s test budget"
        );
    }

    #[test]
    fn test_login_shell_candidates_includes_fallbacks() {
        let _guard = crate::test_env::process_env_lock();
        let _shell_guard = EnvVarGuard::set("SHELL", "/opt/zerobrew/bin/fish");
        let candidates = login_shell_candidates();
        assert_eq!(candidates[0], PathBuf::from("/opt/zerobrew/bin/fish"));
        assert!(candidates.contains(&PathBuf::from("/bin/zsh")));
        assert!(candidates.contains(&PathBuf::from("/bin/bash")));
    }

    #[test]
    fn test_memo_written_and_read() {
        let _guard = crate::test_env::process_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let _cache_guard = EnvVarGuard::set("AFT_CACHE_DIR", temp.path().to_str().unwrap());

        let test_path = OsStr::new("/opt/homebrew/bin:/usr/bin:/bin");
        write_login_path_memo(test_path);

        let read_path = read_login_path_memo().unwrap();
        assert_eq!(read_path, test_path);
    }

    #[test]
    fn test_bounded_re_probe() {
        let _guard = crate::test_env::process_env_lock();
        reset_effective_path_state();

        let temp = tempfile::tempdir().unwrap();
        let _cache_guard = EnvVarGuard::set("AFT_CACHE_DIR", temp.path().to_str().unwrap());

        // Force impoverished PATH
        let _path_guard = EnvVarGuard::set("PATH", "/usr/bin:/bin");

        // Set SHELL to a non-existent shell so probe fails
        let _shell_guard = EnvVarGuard::set("SHELL", "/nonexistent/shell");

        // Set candidates override to nonexistent shell to force probe failure
        let _candidates_guard =
            EnvVarGuard::set("AFT_TEST_LOGIN_SHELL_CANDIDATES", "/nonexistent/shell");

        // First call: probe fails, no memo, falls back to the impoverished
        // PATH (plus whatever standard dirs exist on the test machine, which
        // the unconditional append may add — assert the prefix, not equality).
        let path1 = effective_path();
        assert!(path1.to_string_lossy().starts_with("/usr/bin:/bin"));

        {
            let guard = EFFECTIVE_PATH_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = guard.as_ref().unwrap();
            assert!(state.is_fallback);
            assert!(state.last_probe_attempt.is_some());
        }

        // Second call immediately: should NOT retry probe (returns cached fallback)
        let path2 = effective_path();
        assert_eq!(path2, path1);

        // Simulate 61 seconds passing by modifying last_probe_attempt
        {
            let mut guard = EFFECTIVE_PATH_STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = guard.as_mut().unwrap();
            state.last_probe_attempt = Some(Instant::now() - Duration::from_secs(61));
        }

        // Now write a memo so the next probe/fallback can succeed
        write_login_path_memo(OsStr::new("/opt/homebrew/bin:/usr/bin:/bin"));

        // Third call: should retry probe (which still fails because SHELL is nonexistent),
        // but now it reads from the memo!
        let path3 = effective_path();
        assert!(path3.to_string_lossy().contains("/opt/homebrew/bin"));
    }

    #[test]
    fn test_append_missing_standard_dirs() {
        let home = OsStr::new("/home/alice");
        let path = OsStr::new("/usr/bin:/bin");

        // Mock dir_exists to return true only for ~/.bun/bin
        let dir_exists = |dir: &Path| dir == Path::new("/home/alice/.bun/bin");

        let enriched = append_missing_standard_dirs(path, Some(home), dir_exists);
        assert_eq!(
            enriched,
            OsString::from("/usr/bin:/bin:/home/alice/.bun/bin")
        );
    }

    #[test]
    fn test_append_missing_standard_dirs_dedup_and_order() {
        let home = OsStr::new("/home/alice");
        // ~/.bun/bin is already in the path, but /opt/homebrew/bin is missing
        let path = OsStr::new("/home/alice/.bun/bin:/usr/bin:/bin");

        let dir_exists = |dir: &Path| {
            dir == Path::new("/home/alice/.bun/bin") || dir == Path::new("/opt/homebrew/bin")
        };

        let enriched = append_missing_standard_dirs(path, Some(home), dir_exists);
        // /opt/homebrew/bin should be appended at the end, and ~/.bun/bin should not be duplicated
        assert_eq!(
            enriched,
            OsString::from("/home/alice/.bun/bin:/usr/bin:/bin:/opt/homebrew/bin")
        );
    }
}
