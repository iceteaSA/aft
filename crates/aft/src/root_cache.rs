//! Root-keyed cache lease and read-marker primitives.
//!
//! Writer leases live inside the root-keyed cache domain directory:
//! `<storage>/callgraph/<artifact_cache_key>/writer.lease` and
//! `<storage>/inspect/<project_scope_key>/writer.lease`. The lease file is the
//! `fs_lock` JSON with a `writer_epoch` fencing nonce; writers re-read that
//! nonce immediately before durable publish points and SQLite write transactions.
//!
//! Read markers are per SQLite connection and live below
//! `<cache-domain>/readers/<generation-label>/<pid>.<hostname>.<created_at_ms>.<seq>.json`.
//! The marker JSON format is intentionally small and frozen for the Lane-B GC:
//! `{ "pid": u32, "hostname": String, "created_at_ms": u64 }`.
//! The marker file's mtime is the heartbeat/touch signal for cross-host readers;
//! same-host protection is PID-authoritative in the GC lane. Marker files are
//! created `0600` to avoid leaking checkout activity or allowing another local
//! user to remove a protected reader marker.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

static MARKER_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RootCacheDomain {
    Callgraph,
    Inspect,
}

impl RootCacheDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            RootCacheDomain::Callgraph => "callgraph",
            RootCacheDomain::Inspect => "inspect",
        }
    }
}

pub struct WriterLease {
    domain: RootCacheDomain,
    key: String,
    path: PathBuf,
    epoch: String,
    guard: Mutex<fs_lock::LockGuard>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProcessLeaseKey {
    domain: RootCacheDomain,
    cache_dir: PathBuf,
}

static PROCESS_LEASES: OnceLock<Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>>> =
    OnceLock::new();

fn process_leases() -> &'static Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>> {
    PROCESS_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

impl std::fmt::Debug for WriterLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WriterLease")
            .field("domain", &self.domain)
            .field("key", &self.key)
            .field("path", &self.path)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl WriterLease {
    pub fn acquire_shared(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
    ) -> Result<Arc<Self>, fs_lock::AcquireError> {
        let registry_key = ProcessLeaseKey {
            domain,
            cache_dir: canonical_process_lease_dir(cache_dir),
        };
        {
            let mut leases = process_leases()
                .lock()
                .map_err(|_| fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned")))?;
            if let Some(existing) = leases.get(&registry_key).and_then(Weak::upgrade) {
                if existing.verify()? {
                    return Ok(existing);
                }
                leases.remove(&registry_key);
            }
        }

        let lease = Arc::new(Self::acquire(domain, cache_dir, key, Duration::ZERO)?);
        let mut leases = process_leases()
            .lock()
            .map_err(|_| fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned")))?;
        if let Some(existing) = leases.get(&registry_key).and_then(Weak::upgrade) {
            if existing.verify()? {
                return Ok(existing);
            }
        }
        leases.insert(registry_key, Arc::downgrade(&lease));
        Ok(lease)
    }

    pub fn acquire(
        domain: RootCacheDomain,
        cache_dir: &Path,
        key: &str,
        timeout: Duration,
    ) -> Result<Self, fs_lock::AcquireError> {
        if !storage_allows_root_keyed(cache_dir)? {
            return Err(fs_lock::AcquireError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing root-keyed {} writer lease on a network filesystem at {}",
                    domain.as_str(),
                    cache_dir.display()
                ),
            )));
        }
        if let Some(storage_root) = cache_dir.parent().and_then(Path::parent) {
            crate::legacy_partitions::guard_new_layout_write_path(
                storage_root,
                cache_dir,
                "root-keyed writer lease",
            )?;
        }
        fs::create_dir_all(cache_dir)?;
        let guard = fs_lock::try_acquire(&writer_lease_path(cache_dir), timeout)?;
        if !guard.verify_writer_epoch()? {
            return Err(fs_lock::AcquireError::Io(io::Error::other(
                "writer lease epoch changed immediately after acquisition",
            )));
        }
        let path = guard.path().to_path_buf();
        let epoch = guard.writer_epoch().to_string();
        Ok(Self {
            domain,
            key: key.to_string(),
            path,
            epoch,
            guard: Mutex::new(guard),
        })
    }

    pub fn verify(&self) -> io::Result<bool> {
        self.guard
            .lock()
            .map_err(|_| io::Error::other("writer lease mutex poisoned"))?
            .verify_writer_epoch()
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn domain(&self) -> RootCacheDomain {
        self.domain
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn writer_lease_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("writer.lease")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ReadMarkerMetadata {
    pub pid: u32,
    pub hostname: String,
    pub created_at_ms: u64,
}

#[derive(Debug)]
pub struct ReadMarker {
    path: PathBuf,
    metadata: ReadMarkerMetadata,
}

impl ReadMarker {
    pub fn create(cache_dir: &Path, generation_label: &str) -> io::Result<Self> {
        let metadata = ReadMarkerMetadata {
            pid: std::process::id(),
            hostname: current_hostname(),
            created_at_ms: now_ms(),
        };
        let dir = read_marker_dir(cache_dir, generation_label);
        fs::create_dir_all(&dir)?;
        let seq = MARKER_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(
            "{}.{}.{}.{}.json",
            metadata.pid,
            sanitize_marker_component(&metadata.hostname),
            metadata.created_at_ms,
            seq
        ));
        write_marker_file(&path, &metadata)?;
        Ok(Self { path, metadata })
    }

    pub fn touch(&self) -> io::Result<()> {
        write_marker_file(&self.path, &self.metadata)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &ReadMarkerMetadata {
        &self.metadata
    }
}

impl Drop for ReadMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        fs_lock::sync_parent(&self.path);
    }
}

pub fn read_marker_dir(cache_dir: &Path, generation_label: &str) -> PathBuf {
    cache_dir.join("readers").join(generation_label)
}

pub fn protected_read_marker_exists(cache_dir: &Path, generation_label: &str) -> bool {
    let dir = read_marker_dir(cache_dir, generation_label);
    fs::read_dir(dir)
        .map(|mut entries| entries.any(|entry| entry.is_ok()))
        .unwrap_or(false)
}

fn write_marker_file(path: &Path, metadata: &ReadMarkerMetadata) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("reader"),
        std::process::id(),
        now_nanos()
    ));
    let result = (|| {
        let mut file = open_private_file(&tmp)?;
        serde_json::to_writer(&mut file, metadata).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs_lock::rename_over(&tmp, path)?;
        fs_lock::sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn open_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn sanitize_marker_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
static FORCE_NETWORK_FS_FOR_TEST: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub fn set_force_network_fs_for_test(enabled: bool) {
    FORCE_NETWORK_FS_FOR_TEST.store(enabled, Ordering::SeqCst);
}

pub fn storage_allows_root_keyed(path: &Path) -> io::Result<bool> {
    #[cfg(test)]
    if FORCE_NETWORK_FS_FOR_TEST.load(Ordering::SeqCst) {
        return Ok(false);
    }

    let probe = existing_ancestor(path);
    filesystem_is_local(&probe)
}

fn canonical_process_lease_dir(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return PathBuf::from(".");
        };
        current = parent;
    }
}

#[cfg(target_os = "macos")]
fn filesystem_is_local(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let nul = stat
        .f_fstypename
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(stat.f_fstypename.len());
    let fs_type = String::from_utf8_lossy(
        &stat.f_fstypename[..nul]
            .iter()
            .map(|byte| *byte as u8)
            .collect::<Vec<_>>(),
    )
    .to_ascii_lowercase();
    Ok(!matches!(
        fs_type.as_str(),
        "nfs" | "smbfs" | "afpfs" | "webdav" | "fusefs"
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn filesystem_is_local(path: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fs_type = stat.f_type as i64;
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const CIFS_MAGIC_NUMBER: i64 = 0xFF534D42;
    Ok(!matches!(
        fs_type,
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | CIFS_MAGIC_NUMBER
    ))
}

#[cfg(not(unix))]
fn filesystem_is_local(_path: &Path) -> io::Result<bool> {
    Ok(true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

#[cfg(unix)]
fn current_hostname() -> String {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result == 0 {
        let len = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        if len > 0 {
            return String::from_utf8_lossy(&buffer[..len]).into_owned();
        }
    }
    "unknown-host".to_string()
}

#[cfg(windows)]
fn current_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(all(not(unix), not(windows)))]
fn current_hostname() -> String {
    "unknown-host".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_marker_file_is_private_and_touchable() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        assert!(marker.path().is_file());
        marker.touch().unwrap();
        let bytes = fs::read(marker.path()).unwrap();
        let parsed: ReadMarkerMetadata = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.pid, std::process::id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(marker.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn nfs_guard_test_seam_fails_closed() {
        set_force_network_fs_for_test(true);
        assert!(!storage_allows_root_keyed(Path::new(".")).unwrap());
        set_force_network_fs_for_test(false);
    }
}
