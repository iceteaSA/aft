use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
static TAIL_READS: OnceLock<Mutex<std::collections::HashMap<PathBuf, usize>>> = OnceLock::new();

pub const DISK_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRead {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskTruncation {
    pub stdout_prefix_bytes: u64,
    pub stderr_prefix_bytes: u64,
    pub combined_prefix_bytes: u64,
}

impl DiskTruncation {
    pub fn total_prefix_bytes(self) -> u64 {
        self.stdout_prefix_bytes
            .saturating_add(self.stderr_prefix_bytes)
            .saturating_add(self.combined_prefix_bytes)
    }
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
        #[cfg(test)]
        bump_tail_read_count(self);
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => read_two_file_tails(stdout_path, stderr_path, max_bytes),
            Self::Pty { combined_path } => match read_file_tail(combined_path, max_bytes) {
                Ok((bytes, truncated)) => (String::from_utf8_lossy(&bytes).into_owned(), truncated),
                Err(_) => (String::new(), false),
            },
        }
    }

    pub fn read_combined_head_tail(
        &self,
        max_bytes: usize,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> BoundedRead {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => {
                read_two_file_head_tail(stdout_path, stderr_path, max_bytes, head_bytes, tail_bytes)
            }
            Self::Pty { combined_path } => {
                read_single_file_head_tail(combined_path, max_bytes, head_bytes, tail_bytes)
                    .unwrap_or_else(|_| BoundedRead {
                        text: String::new(),
                        truncated: false,
                        total_bytes: 0,
                    })
            }
        }
    }

    pub fn read_stream_bounded(&self, stream: StreamKind, max_bytes: usize) -> BoundedRead {
        let path = match (self, stream) {
            (Self::Pipes { stdout_path, .. }, StreamKind::Stdout) => Some(stdout_path),
            (Self::Pipes { stderr_path, .. }, StreamKind::Stderr) => Some(stderr_path),
            (Self::Pty { combined_path }, _) => Some(combined_path),
        };
        path.and_then(|path| read_file_bounded(path, max_bytes).ok())
            .unwrap_or_else(|| BoundedRead {
                text: String::new(),
                truncated: false,
                total_bytes: 0,
            })
    }

    pub fn stream_len(&self, stream: StreamKind) -> u64 {
        let path = match (self, stream) {
            (Self::Pipes { stdout_path, .. }, StreamKind::Stdout) => Some(stdout_path),
            (Self::Pipes { stderr_path, .. }, StreamKind::Stderr) => Some(stderr_path),
            (Self::Pty { combined_path }, _) => Some(combined_path),
        };
        path.and_then(|path| path.metadata().ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0)
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
            // PTY output skips token accounting because the raw terminal stream
            // can contain control sequences produced by terminal emulation rather
            // than the original command text.
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

    pub fn enforce_terminal_cap(&mut self) -> DiskTruncation {
        match self {
            Self::Pipes {
                stdout_path,
                stderr_path,
            } => DiskTruncation {
                stdout_prefix_bytes: truncate_front(stdout_path, DISK_LIMIT_BYTES).unwrap_or(0),
                stderr_prefix_bytes: truncate_front(stderr_path, DISK_LIMIT_BYTES).unwrap_or(0),
                combined_prefix_bytes: 0,
            },
            Self::Pty { combined_path } => DiskTruncation {
                stdout_prefix_bytes: 0,
                stderr_prefix_bytes: 0,
                combined_prefix_bytes: truncate_front(combined_path, DISK_LIMIT_BYTES).unwrap_or(0),
            },
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

#[cfg(test)]
fn tail_reads() -> &'static Mutex<std::collections::HashMap<PathBuf, usize>> {
    TAIL_READS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
fn tail_read_key(buffer: &BgBuffer) -> &Path {
    match buffer {
        BgBuffer::Pipes { stdout_path, .. } => stdout_path,
        BgBuffer::Pty { combined_path } => combined_path,
    }
}

#[cfg(test)]
fn bump_tail_read_count(buffer: &BgBuffer) {
    if let Ok(mut reads) = tail_reads().lock() {
        *reads
            .entry(tail_read_key(buffer).to_path_buf())
            .or_default() += 1;
    }
}

#[cfg(test)]
pub(crate) fn reset_tail_read_count(path: &Path) {
    if let Ok(mut reads) = tail_reads().lock() {
        reads.remove(path);
    }
}

#[cfg(test)]
pub(crate) fn tail_read_count(path: &Path) -> usize {
    tail_reads()
        .lock()
        .ok()
        .and_then(|reads| reads.get(path).copied())
        .unwrap_or_default()
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
    let truncated = len > max_bytes as u64;
    if truncated {
        bytes = align_start_to_utf8(bytes);
    }
    Ok((bytes, truncated))
}

fn read_file_bounded(path: &Path, max_bytes: usize) -> io::Result<BoundedRead> {
    let metadata = path.metadata()?;
    let total_bytes = metadata.len();
    if total_bytes > max_bytes as u64 {
        if max_bytes == 0 {
            return Ok(BoundedRead {
                text: String::new(),
                truncated: true,
                total_bytes,
            });
        }
        return read_single_file_head_tail(
            path,
            max_bytes,
            max_bytes / 2,
            max_bytes - max_bytes / 2,
        );
    }
    let bytes = fs::read(path)?;
    Ok(BoundedRead {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: false,
        total_bytes,
    })
}

fn read_single_file_head_tail(
    path: &Path,
    max_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
) -> io::Result<BoundedRead> {
    let total_bytes = path.metadata()?.len();
    if total_bytes <= max_bytes as u64 {
        let bytes = fs::read(path)?;
        return Ok(BoundedRead {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated: false,
            total_bytes,
        });
    }

    let head_len = head_bytes.min(max_bytes) as u64;
    let tail_len = tail_bytes.min(max_bytes.saturating_sub(head_len as usize)) as u64;
    let head = read_file_range(path, 0, head_len)?;
    let tail_start = total_bytes.saturating_sub(tail_len);
    let tail = read_file_range(path, tail_start, tail_len)?;
    Ok(BoundedRead {
        text: join_head_tail_bytes(head, tail, total_bytes.saturating_sub(head_len + tail_len)),
        truncated: true,
        total_bytes,
    })
}

fn read_two_file_head_tail(
    first: &Path,
    second: &Path,
    max_bytes: usize,
    head_bytes: usize,
    tail_bytes: usize,
) -> BoundedRead {
    let first_len = first.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let second_len = second
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let total_bytes = first_len.saturating_add(second_len);

    if total_bytes <= max_bytes as u64 {
        let mut bytes = Vec::with_capacity(total_bytes as usize);
        if let Ok(first_bytes) = fs::read(first) {
            bytes.extend_from_slice(&first_bytes);
        }
        if let Ok(second_bytes) = fs::read(second) {
            bytes.extend_from_slice(&second_bytes);
        }
        return BoundedRead {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated: false,
            total_bytes,
        };
    }

    let head_budget = head_bytes.min(max_bytes);
    let (first_head, second_head) = split_stream_budget(first_len, second_len, head_budget);
    let tail_budget = tail_bytes.min(max_bytes.saturating_sub(first_head + second_head));
    let first_remaining = first_len.saturating_sub(first_head as u64);
    let second_remaining = second_len.saturating_sub(second_head as u64);
    let (first_tail, second_tail) =
        split_stream_budget(first_remaining, second_remaining, tail_budget);

    let first_read =
        read_single_file_head_tail(first, first_head + first_tail, first_head, first_tail)
            .unwrap_or_else(|_| BoundedRead {
                text: String::new(),
                truncated: false,
                total_bytes: first_len,
            });
    let second_read =
        read_single_file_head_tail(second, second_head + second_tail, second_head, second_tail)
            .unwrap_or_else(|_| BoundedRead {
                text: String::new(),
                truncated: false,
                total_bytes: second_len,
            });

    BoundedRead {
        text: combine_streams(&first_read.text, &second_read.text),
        truncated: true,
        total_bytes,
    }
}

fn read_two_file_tails(first: &Path, second: &Path, max_bytes: usize) -> (String, bool) {
    let first_len = first.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let second_len = second
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let total_bytes = first_len.saturating_add(second_len);
    if total_bytes <= max_bytes as u64 {
        let first_bytes = fs::read(first).unwrap_or_default();
        let second_bytes = fs::read(second).unwrap_or_default();
        return (
            combine_streams(
                String::from_utf8_lossy(&first_bytes).as_ref(),
                String::from_utf8_lossy(&second_bytes).as_ref(),
            ),
            false,
        );
    }

    let (first_budget, second_budget) = split_stream_budget(first_len, second_len, max_bytes);
    let (first_bytes, first_truncated) = read_file_tail(first, first_budget)
        .unwrap_or_else(|_| (Vec::new(), first_len > first_budget as u64));
    let (second_bytes, second_truncated) = read_file_tail(second, second_budget)
        .unwrap_or_else(|_| (Vec::new(), second_len > second_budget as u64));
    (
        combine_streams(
            String::from_utf8_lossy(&first_bytes).as_ref(),
            String::from_utf8_lossy(&second_bytes).as_ref(),
        ),
        first_truncated || second_truncated || total_bytes > max_bytes as u64,
    )
}

fn split_stream_budget(first_len: u64, second_len: u64, total_budget: usize) -> (usize, usize) {
    if total_budget == 0 {
        return (0, 0);
    }
    match (first_len > 0, second_len > 0) {
        (false, false) => (0, 0),
        (true, false) => (total_budget, 0),
        (false, true) => (0, total_budget),
        (true, true) => {
            let mut first_budget = total_budget / 2;
            let mut second_budget = total_budget - first_budget;
            redistribute_unused_budget(first_len, &mut first_budget, &mut second_budget);
            redistribute_unused_budget(second_len, &mut second_budget, &mut first_budget);
            (first_budget, second_budget)
        }
    }
}

fn redistribute_unused_budget(len: u64, own_budget: &mut usize, other_budget: &mut usize) {
    let needed = len.min(usize::MAX as u64) as usize;
    if needed < *own_budget {
        let spare = own_budget.saturating_sub(needed);
        *own_budget = needed;
        *other_budget = other_budget.saturating_add(spare);
    }
}

fn read_file_range(path: &Path, start: u64, len: u64) -> io::Result<Vec<u8>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut limited = file.take(len);
    let mut bytes = Vec::with_capacity(len as usize);
    limited.read_to_end(&mut bytes)?;
    if start > 0 {
        bytes = align_start_to_utf8(bytes);
    }
    bytes = align_end_to_utf8(bytes);
    Ok(bytes)
}

fn join_head_tail_bytes(head: Vec<u8>, tail: Vec<u8>, truncated_bytes: u64) -> String {
    let mut output = String::from_utf8_lossy(&head).into_owned();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&String::from_utf8_lossy(&tail));
    output
}

fn truncate_front(path: &Path, retain_bytes: u64) -> io::Result<u64> {
    let len = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if len <= retain_bytes {
        return Ok(0);
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::End(-(retain_bytes as i64)))?;
    let mut tail = Vec::with_capacity(retain_bytes as usize);
    file.read_to_end(&mut tail)?;
    let tail = align_start_to_utf8(tail);
    let retained_bytes = tail.len() as u64;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("out")
    ));
    fs::write(&tmp, tail)?;
    fs::rename(&tmp, path)?;
    Ok(len.saturating_sub(retained_bytes))
}

fn align_start_to_utf8(mut bytes: Vec<u8>) -> Vec<u8> {
    let mut start = 0;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    if start > 0 {
        bytes.drain(..start);
    }
    bytes
}

fn align_end_to_utf8(mut bytes: Vec<u8>) -> Vec<u8> {
    while !bytes.is_empty() {
        let last = bytes.len() - 1;
        if bytes[last] < 0x80 {
            break;
        }
        let lead_pos = if (bytes[last] & 0xC0) == 0x80 {
            let mut pos = last;
            while pos > 0 && (bytes[pos] & 0xC0) == 0x80 {
                pos -= 1;
            }
            if (bytes[pos] & 0xC0) == 0xC0 {
                pos
            } else {
                bytes.pop();
                continue;
            }
        } else {
            last
        };
        let lead = bytes[lead_pos];
        debug_assert!(lead >= 0xC0, "lead byte must be >= 0xC0, got {lead:#x}");
        let expected = if lead < 0xE0 {
            1
        } else if lead < 0xF0 {
            2
        } else {
            3
        };
        if last - lead_pos >= expected {
            break;
        }
        bytes.truncate(lead_pos);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Regression tests for UTF-8 splitting at byte boundaries ---
    // CORRECT behavior: read_file_tail should not split UTF-8 characters.
    // These tests FAIL when the bug is present.

    #[test]
    fn read_file_tail_should_not_split_utf8_character() {
        // "AAAA€" = 7 bytes (4 ASCII + 3-byte €).
        // 2-byte tail reads bytes [5,6] = 0x82 0xAC - incomplete trailing
        // bytes of €. from_utf8_lossy produces U+FFFD.
        // CORRECT: no replacement character should appear.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        let (bytes, _truncated) = read_file_tail(&path, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_tail should not produce replacement characters, got: {:?}",
            text
        );
    }

    #[test]
    fn truncate_front_should_not_split_utf8_character() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        truncate_front(&path, 2).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "truncate_front should not produce replacement characters, got: {:?}",
            text
        );
    }

    #[test]
    fn read_file_tail_should_not_split_4byte_utf8() {
        // "AAAA😀" = 4 + 4 = 8 bytes. 2-byte tail reads bytes [6,7] = incomplete.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA😀".as_bytes()).unwrap();
        let (bytes, _truncated) = read_file_tail(&path, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_tail should not produce replacement characters for 4-byte chars, got: {:?}",
            text
        );
    }

    #[test]
    fn read_file_range_end_boundary_should_not_split_utf8() {
        // "AAAA€" = 7 bytes. read_file_range(path, 0, 5) reads bytes [0..5].
        // byte 4 = 0xE2 (lead of €), byte 5 = 0x82 (continuation) — not included.
        // End at byte 5 splits after the lead byte. align_end_to_utf8 should trim it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, "AAAA€".as_bytes()).unwrap();
        let bytes = read_file_range(&path, 0, 5).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_range should not produce replacement characters at end boundary, got: {:?}",
            text
        );
    }

    #[test]
    fn ascii_content_unaffected_by_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        let content = b"hello world\nline two\n";
        std::fs::write(&path, content).unwrap();
        let (bytes, truncated) = read_file_tail(&path, 10).unwrap();
        assert!(truncated);
        assert_eq!(bytes, b"\nline two\n");
    }

    #[test]
    fn read_file_range_start_boundary_should_not_split_utf8() {
        // "Hello€World" = 5 + 3 + 5 = 13 bytes.
        // read_file_range(path, 5, 4) reads bytes [5..9]:
        // bytes 5-7 = € (0xE2 0x82 0xAC), byte 8 = 'W'.
        // Start at byte 5 = 0xE2 (lead byte) — aligned, no split.
        // End at byte 9 = 'o' — aligned, no split.
        // But read_file_range(path, 6, 2) reads bytes [6..8]:
        // byte 6 = 0x82 (continuation), byte 7 = 0xAC (continuation).
        // Start at byte 6 splits inside €. align_start_to_utf8 should skip.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(&path, b"Hello\xe2\x82\xacWorld").unwrap();
        let bytes = read_file_range(&path, 6, 2).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains('\u{FFFD}'),
            "read_file_range with start>0 should not produce replacement characters, got: {:?}",
            text
        );
    }

    // --- Regression test for stdout/stderr interleaving ---
    // This test documents the limitation: stdout always comes before stderr
    // in the combined output, regardless of temporal write order.
    // It does not assert correct interleaving (that would require a redesign)
    // but verifies the current behavior is what we expect.

    #[test]
    fn read_tail_puts_stdout_before_stderr() {
        // Write stdout and stderr to separate files, then verify
        // the combined output has stdout content before stderr content.
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(&stdout_path, b"stdout-line\n").unwrap();
        std::fs::write(&stderr_path, b"stderr-line\n").unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);
        let (text, _) = buffer.read_tail(1024);
        let stdout_pos = text.find("stdout-line").unwrap();
        let stderr_pos = text.find("stderr-line").unwrap();
        assert!(
            stdout_pos < stderr_pos,
            "stdout should come before stderr in combined output"
        );
    }

    #[test]
    fn read_tail_preserves_each_stream_tail_when_combined_cap_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(
            &stdout_path,
            format!(
                "{}
error: stdout boom
",
                "stdout noise
"
                .repeat(20)
            ),
        )
        .unwrap();
        std::fs::write(
            &stderr_path,
            format!(
                "{}
stderr tail
",
                "stderr noise
"
                .repeat(200)
            ),
        )
        .unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);

        let (text, truncated) = buffer.read_tail(160);

        assert!(truncated);
        assert!(text.contains("error: stdout boom"));
        assert!(text.contains("stderr tail"));
    }

    #[test]
    fn read_combined_head_tail_preserves_each_stream_tail() {
        let dir = tempfile::tempdir().unwrap();
        let stdout_path = dir.path().join("stdout");
        let stderr_path = dir.path().join("stderr");
        std::fs::write(
            &stdout_path,
            format!(
                "stdout head
{}
ERROR: stdout final
",
                "x".repeat(512)
            ),
        )
        .unwrap();
        std::fs::write(
            &stderr_path,
            format!(
                "stderr head
{}
stderr final
",
                "y".repeat(2048)
            ),
        )
        .unwrap();
        let buffer = BgBuffer::new(stdout_path, stderr_path);

        let read = buffer.read_combined_head_tail(256, 64, 192);

        assert!(read.truncated);
        assert!(read.text.contains("ERROR: stdout final"));
        assert!(read.text.contains("stderr final"));
    }

    #[test]
    fn read_file_bounded_returns_head_and_tail_for_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(
            &path,
            format!(
                "HEAD
{}
TAIL",
                "x".repeat(256)
            ),
        )
        .unwrap();

        let read = read_file_bounded(&path, 64).unwrap();

        assert!(read.truncated);
        assert!(read.text.contains("HEAD"));
        assert!(read.text.contains("TAIL"));
        assert!(read.text.contains("...<truncated "));
    }

    #[test]
    fn truncate_front_reports_prefix_bytes_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout");
        std::fs::write(
            &path,
            b"early root cause
late tail
",
        )
        .unwrap();

        let removed = truncate_front(&path, 10).unwrap();
        let retained = std::fs::read_to_string(&path).unwrap();

        assert!(removed > 0);
        assert!(!retained.contains("early root cause"));
        assert!(retained.contains("late tail"));
    }
}
