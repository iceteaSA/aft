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
use std::os::unix::process::CommandExt;

static EFFECTIVE_PATH: OnceLock<OsString> = OnceLock::new();

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

/// Return the cached PATH that subprocesses should inherit.
///
/// On Windows this is the process PATH unchanged: Windows daemon environments
/// already receive PATH from the registry-backed environment block.
pub fn effective_path() -> &'static OsStr {
    EFFECTIVE_PATH
        .get_or_init(compute_effective_path)
        .as_os_str()
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
fn compute_effective_path() -> OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let home = std::env::var_os("HOME");
    compute_effective_path_with(
        &current,
        home.as_deref(),
        |dir| dir.is_dir(),
        probe_login_shell_path,
    )
}

#[cfg(unix)]
fn compute_effective_path_with<D, P>(
    current: &OsStr,
    home: Option<&OsStr>,
    dir_exists: D,
    mut probe_login_path: P,
) -> OsString
where
    D: FnMut(&Path) -> bool,
    P: FnMut() -> Option<OsString>,
{
    if !path_is_impoverished(current, home, dir_exists) {
        return current.to_os_string();
    }

    let Some(login_path) = probe_login_path().filter(|path| login_path_is_acceptable(path)) else {
        return current.to_os_string();
    };

    merge_login_and_current_path(&login_path, current)
}

#[cfg(unix)]
fn path_is_impoverished<D>(current: &OsStr, home: Option<&OsStr>, mut dir_exists: D) -> bool
where
    D: FnMut(&Path) -> bool,
{
    let current_entries: Vec<PathBuf> = std::env::split_paths(current).collect();
    user_standard_path_dirs(home).into_iter().any(|dir| {
        dir_exists(&dir)
            && !current_entries
                .iter()
                .any(|entry| entry.as_path() == dir.as_path())
    })
}

#[cfg(unix)]
fn user_standard_path_dirs(home: Option<&OsStr>) -> Vec<PathBuf> {
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
    if let Some(shell) = std::env::var_os("SHELL").filter(|value| !value.is_empty()) {
        return vec![PathBuf::from(shell)];
    }
    vec![PathBuf::from("/bin/zsh"), PathBuf::from("/bin/bash")]
}

#[cfg(unix)]
fn probe_login_shell_path_once(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut command = Command::new(shell);
    command
        .arg("-l")
        .arg("-c")
        .arg(r#"printf %s "$PATH""#)
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

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                return Some(OsString::from_vec(output.stdout));
            }
            Ok(None) if Instant::now() >= deadline => {
                kill_login_shell_probe(&mut child);
                return None;
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(25)));
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

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn impoverished_path_merges_login_first_and_keeps_current_entries() {
        let current = OsStr::new("/usr/bin:/bin:/home/alice/.cargo/bin:/custom/current");
        let login = OsString::from("/fake/login/bin:/usr/bin:/opt/homebrew/bin");

        let effective = compute_effective_path_with(
            current,
            Some(OsStr::new("/home/alice")),
            |_| true,
            || Some(login.clone()),
        );

        assert_eq!(
            effective,
            OsString::from(
                "/fake/login/bin:/usr/bin:/opt/homebrew/bin:/bin:/home/alice/.cargo/bin:/custom/current"
            )
        );
    }

    #[test]
    fn invalid_probe_paths_are_rejected() {
        let current = OsStr::new("/usr/bin:/bin");
        let rejected = vec![
            OsString::new(),
            OsString::from("/fake/login/bin\n/usr/bin"),
            OsString::from("/fake/login/bin:relative/bin"),
            OsString::from_vec(b"/fake/login/bin\0/usr/bin".to_vec()),
        ];

        for probe_path in rejected {
            let effective = compute_effective_path_with(
                current,
                Some(OsStr::new("/home/alice")),
                |_| true,
                || Some(probe_path.clone()),
            );
            assert_eq!(effective, current);
        }
    }

    #[test]
    fn rich_current_path_does_not_probe_login_shell() {
        let current = OsStr::new(
            "/opt/homebrew/bin:/usr/local/bin:/home/alice/.cargo/bin:/home/alice/.local/bin:/usr/bin:/bin",
        );

        let effective = compute_effective_path_with(
            current,
            Some(OsStr::new("/home/alice")),
            |_| true,
            || panic!("rich PATH must not invoke the login-shell probe"),
        );

        assert_eq!(effective, current);
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
}
