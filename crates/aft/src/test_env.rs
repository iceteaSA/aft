//! Shared helpers for tests that need process-global environment isolation.
//!
//! `HOME`, `USERPROFILE`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, and
//! `GIT_CONFIG_SYSTEM` are process-global. The libtest runner executes unit tests
//! concurrently within one process, so any test that mutates or depends on these
//! variables must serialize on the SAME lock — module-local mutexes only protect
//! against siblings in the same file, not against an env-mutating test in another
//! module running in parallel.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn process_env_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

thread_local! {
    static PROCESS_ENV_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
    static PROCESS_ENV_LOCK_GUARD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// Reentrant guard for the process-wide env-mutation lock.
///
/// Some tests already hold the shared env lock for `HOME` / `XDG_*` isolation and
/// then need to install hermetic git-config vars inside the same thread. A plain
/// `MutexGuard` would deadlock on that nested acquisition, so this guard keeps the
/// underlying mutex in thread-local storage and only releases it when the outermost
/// acquisition drops.
pub(crate) struct ProcessEnvLockGuard;

impl Drop for ProcessEnvLockGuard {
    fn drop(&mut self) {
        PROCESS_ENV_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "process env lock depth underflow");
            let next = current.saturating_sub(1);
            depth.set(next);
            if next == 0 {
                PROCESS_ENV_LOCK_GUARD.with(|slot| {
                    slot.borrow_mut().take();
                });
            }
        });
    }
}

/// Acquire the process-wide env-mutation lock. Poison is ignored: a panicking
/// test already restored (or failed to restore) the env; letting the next test
/// proceed keeps one failure from cascading into every env-dependent test.
pub(crate) fn process_env_lock() -> ProcessEnvLockGuard {
    PROCESS_ENV_LOCK_DEPTH.with(|depth| {
        if depth.get() == 0 {
            let guard = process_env_mutex()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            PROCESS_ENV_LOCK_GUARD.with(|slot| {
                *slot.borrow_mut() = Some(guard);
            });
        }
        depth.set(depth.get() + 1);
    });
    ProcessEnvLockGuard
}

struct ScopedEnvVar {
    key: &'static str,
    previous: Option<OsString>,
}

impl ScopedEnvVar {
    fn set(key: &'static str, value: &'static OsStr) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            unsafe { std::env::set_var(self.key, previous) };
        } else {
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

#[cfg(windows)]
const HERMETIC_GIT_CONFIG_PATH: &str = "NUL";
#[cfg(not(windows))]
const HERMETIC_GIT_CONFIG_PATH: &str = "/dev/null";

/// Test-only git env overrides that suppress user/system config reads.
#[allow(dead_code)]
pub(crate) fn hermetic_git_env() -> [(&'static str, &'static OsStr); 2] {
    [
        ("GIT_CONFIG_GLOBAL", OsStr::new(HERMETIC_GIT_CONFIG_PATH)),
        ("GIT_CONFIG_SYSTEM", OsStr::new(HERMETIC_GIT_CONFIG_PATH)),
    ]
}

/// Apply hermetic git-config env vars to one child process.
#[allow(dead_code)]
pub(crate) fn apply_hermetic_git_env(command: &mut Command) -> &mut Command {
    command.envs(hermetic_git_env())
}

/// Hold hermetic git-config env vars for the current test thread.
#[allow(dead_code)]
pub(crate) struct HermeticGitEnvGuard {
    _lock: ProcessEnvLockGuard,
    _global: ScopedEnvVar,
    _system: ScopedEnvVar,
}

/// Install hermetic git-config env vars for in-process git executions during a
/// test. This is for code-under-test paths that shell `git` internally and
/// therefore cannot receive per-command env injection from the caller.
#[allow(dead_code)]
pub(crate) fn hermetic_git_env_guard() -> HermeticGitEnvGuard {
    let lock = process_env_lock();
    let [(global_key, global_value), (system_key, system_value)] = hermetic_git_env();
    HermeticGitEnvGuard {
        _lock: lock,
        _global: ScopedEnvVar::set(global_key, global_value),
        _system: ScopedEnvVar::set(system_key, system_value),
    }
}
