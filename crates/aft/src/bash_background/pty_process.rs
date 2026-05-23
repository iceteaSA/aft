use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{CommandBuilder, PtySize};

use super::persistence::{atomic_write, ExitMarker, TaskPaths};
use super::pty_runtime::{CompletionCoordinator, PtyRuntime};

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pty_for_command(
    task_id: &str,
    session_id: &str,
    user_command: &str,
    paths: &TaskPaths,
    workdir: &Path,
    env: &HashMap<String, String>,
    rows: u16,
    cols: u16,
    wake_tx: crossbeam_channel::Sender<()>,
) -> Result<PtyRuntime, String> {
    #[cfg(unix)]
    {
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(user_command);
        command.cwd(workdir.as_os_str());
        for (key, value) in env {
            command.env(key, value);
        }
        try_spawn_pty(task_id, session_id, command, paths, rows, cols, wake_tx)
    }
    #[cfg(windows)]
    {
        let _ = (
            task_id,
            session_id,
            user_command,
            paths,
            workdir,
            env,
            rows,
            cols,
            wake_tx,
        );
        Err("PTY spawn on Windows is deferred to Phase 1b".to_string())
    }
}

#[allow(clippy::too_many_arguments)]
fn try_spawn_pty(
    task_id: &str,
    session_id: &str,
    command: CommandBuilder,
    paths: &TaskPaths,
    rows: u16,
    cols: u16,
    wake_tx: crossbeam_channel::Sender<()>,
) -> Result<PtyRuntime, String> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("open PTY failed: {error}"))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("spawn PTY command failed: {error}"))?;
    let child_pid = child.process_id();
    let killer = child.clone_killer();
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("clone PTY reader failed: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("take PTY writer failed: {error}"))?;

    let reader_done = Arc::new(AtomicBool::new(false));
    let exit_observed = Arc::new(AtomicBool::new(false));
    let was_killed = Arc::new(AtomicBool::new(false));
    let coordinator = Arc::new(CompletionCoordinator::new(
        task_id.to_string(),
        session_id.to_string(),
        wake_tx,
    ));

    spawn_reader(
        reader,
        paths.pty.clone(),
        Arc::clone(&reader_done),
        Arc::clone(&coordinator),
    );
    spawn_waiter(
        child,
        paths.exit.clone(),
        Arc::clone(&was_killed),
        Arc::clone(&exit_observed),
        Arc::clone(&coordinator),
    );

    Ok(PtyRuntime {
        master: Some(pair.master),
        writer: Arc::new(Mutex::new(writer)),
        killer,
        child_pid,
        reader_done,
        exit_observed,
        was_killed,
        coordinator,
    })
}

pub(crate) fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    spill_path: std::path::PathBuf,
    reader_done: Arc<AtomicBool>,
    coordinator: Arc<CompletionCoordinator>,
) {
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            if let Some(parent) = spill_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&spill_path)?;
            let mut buf = [0_u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        file.write_all(&buf[..n])?;
                        file.flush()?;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            crate::slog_warn!(
                "PTY reader for {}:{} stopped with error: {error}",
                coordinator.session_id,
                coordinator.task_id
            );
        }
        reader_done.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    });
}

pub(crate) fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    exit_path: std::path::PathBuf,
    was_killed: Arc<AtomicBool>,
    exit_observed: Arc<AtomicBool>,
    coordinator: Arc<CompletionCoordinator>,
) {
    thread::spawn(move || {
        let marker = loop {
            match child.wait() {
                Ok(status) => {
                    if was_killed.load(Ordering::SeqCst) {
                        break ExitMarker::Killed;
                    }
                    let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    break ExitMarker::Code(code);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    crate::slog_warn!(
                        "PTY waiter for {}:{} failed: {error}",
                        coordinator.session_id,
                        coordinator.task_id
                    );
                    break ExitMarker::Killed;
                }
            }
        };

        if let Err(error) = write_exit_marker(&exit_path, &marker, &coordinator.task_id) {
            crate::slog_warn!(
                "PTY waiter for {}:{} failed to write exit marker: {error}",
                coordinator.session_id,
                coordinator.task_id
            );
        }
        exit_observed.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    });
}

fn write_exit_marker(path: &Path, marker: &ExitMarker, task_id: &str) -> io::Result<()> {
    let content = match marker {
        ExitMarker::Code(code) => code.to_string(),
        ExitMarker::Killed => "killed".to_string(),
    };
    atomic_write(path, content.as_bytes(), task_id)
}
