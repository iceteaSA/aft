use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DISK_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub enum BgBuffer {
    Pipes {
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
    Pty {
        combined_path: PathBuf,
    },
}

impl BgBuffer {
    pub fn new(stdout_path: PathBuf, stderr_path: PathBuf) -> Self {
        Self::Pipes {
            stdout_path,
            stderr_path,
        }
    }

    pub fn pty(combined_path: PathBuf) -> Self {
        Self::Pty { combined_path }
    }

    pub fn stdout_path(&self) -> Option<&Path> {
        match self {
            Self::Pipes { stdout_path, .. } => Some(stdout_path),
            Self::Pty { .. } => None,
        }
    }

    pub fn stderr_path(&self) -> Option<&Path> {
        match self {
            Self::Pipes { stderr_path, .. } => Some(stderr_path),
            Self::Pty { .. } => None,
        }
    }

    pub fn combined_path(&self) -> Option<&Path> {
        match self {
            Self::Pipes { .. } => None,
            Self::Pty { combined_path } => Some(combined_path),
        }
    }

    pub fn read_tail(&self, max_bytes: usize) -> (String, bool) {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => {
                let stdout = read_file_tail(stdout_path, max_bytes);
                let stderr = read_file_tail(stderr_path, max_bytes);
                match (stdout, stderr) {
                    (Ok((stdout, stdout_truncated)), Ok((stderr, stderr_truncated))) => {
                        let mut output =
                            Vec::with_capacity(stdout.len().saturating_add(stderr.len()));
                        output.extend_from_slice(&stdout);
                        output.extend_from_slice(&stderr);
                        let was_over_cap = output.len() > max_bytes;
                        if was_over_cap {
                            let keep_from = output.len().saturating_sub(max_bytes);
                            output.drain(..keep_from);
                        }
                        (
                            String::from_utf8_lossy(&output).into_owned(),
                            stdout_truncated || stderr_truncated || was_over_cap,
                        )
                    }
                    (Ok((stdout, stdout_truncated)), Err(_)) => (
                        String::from_utf8_lossy(&stdout).into_owned(),
                        stdout_truncated,
                    ),
                    (Err(_), Ok((stderr, stderr_truncated))) => (
                        String::from_utf8_lossy(&stderr).into_owned(),
                        stderr_truncated,
                    ),
                    (Err(_), Err(_)) => (String::new(), false),
                }
            }
            Self::Pty { combined_path } => match read_file_tail(combined_path, max_bytes) {
                Ok((bytes, truncated)) => (String::from_utf8_lossy(&bytes).into_owned(), truncated),
                Err(_) => (String::new(), false),
            },
        }
    }

    pub fn read_for_token_count(&self, max_bytes_per_stream: usize) -> TokenCountInput {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => {
                // Read up to `max_bytes_per_stream` bytes per stream rather than
                // refusing to tokenize anything when the file exceeds the cap.
                let stdout = read_file_tail(stdout_path, max_bytes_per_stream);
                let stderr = read_file_tail(stderr_path, max_bytes_per_stream);
                match (stdout, stderr) {
                    (Ok((stdout, _)), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                        String::from_utf8_lossy(&stdout).as_ref(),
                        String::from_utf8_lossy(&stderr).as_ref(),
                    )),
                    (Ok((stdout, _)), Err(_)) => TokenCountInput::Text(combine_streams(
                        String::from_utf8_lossy(&stdout).as_ref(),
                        "",
                    )),
                    (Err(_), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                        "",
                        String::from_utf8_lossy(&stderr).as_ref(),
                    )),
                    (Err(_), Err(_)) => TokenCountInput::Skipped,
                }
            }
            // PTY completions intentionally skip token accounting. The combined
            // stream can include terminal control sequences that Phase 2 renders
            // via xterm-headless instead of the text compressor.
            Self::Pty { .. } => TokenCountInput::Skipped,
        }
    }

    pub fn read_stream_tail(&self, stream: StreamKind, max_bytes: usize) -> (String, bool) {
        let path = match (self, stream) {
            (Self::Pipes { stdout_path, .. }, StreamKind::Stdout) => Some(stdout_path),
            (Self::Pipes { stderr_path, .. }, StreamKind::Stderr) => Some(stderr_path),
            (Self::Pty { combined_path }, _) => Some(combined_path),
        };
        match path.and_then(|path| read_file_tail(path, max_bytes).ok()) {
            Some((bytes, truncated)) => (String::from_utf8_lossy(&bytes).into_owned(), truncated),
            None => (String::new(), false),
        }
    }

    /// Path to the primary output spill file.
    pub fn output_path(&self) -> Option<PathBuf> {
        match self {
            Self::Pipes { stdout_path, .. } => Some(stdout_path.clone()),
            Self::Pty { combined_path } => Some(combined_path.clone()),
        }
    }

    pub fn enforce_terminal_cap(&mut self) {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => {
                let _ = truncate_front(stdout_path, DISK_LIMIT_BYTES);
                let _ = truncate_front(stderr_path, DISK_LIMIT_BYTES);
            }
            Self::Pty { combined_path } => {
                let _ = truncate_front(combined_path, DISK_LIMIT_BYTES);
            }
        }
    }

    pub fn cleanup(&self) {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => {
                let _ = fs::remove_file(stdout_path);
                let _ = fs::remove_file(stderr_path);
            }
            Self::Pty { combined_path } => {
                let _ = fs::remove_file(combined_path);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCountInput {
    Text(String),
    Skipped,
}

pub fn combine_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

pub(crate) fn read_file_tail(path: &Path, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    if max_bytes == 0 {
        return Ok((
            Vec::new(),
            path.metadata()
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false),
        ));
    }

    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let read_len = len.min(max_bytes as u64);
    if read_len > 0 {
        file.seek(SeekFrom::End(-(read_len as i64)))?;
    }
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.read_to_end(&mut bytes)?;
    Ok((bytes, len > max_bytes as u64))
}

fn truncate_front(path: &Path, retain_bytes: u64) -> io::Result<bool> {
    let len = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if len <= retain_bytes {
        return Ok(false);
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::End(-(retain_bytes as i64)))?;
    let mut tail = Vec::with_capacity(retain_bytes as usize);
    file.read_to_end(&mut tail)?;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("out")
    ));
    fs::write(&tmp, tail)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}
