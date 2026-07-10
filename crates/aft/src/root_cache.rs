//! These files coordinate safe access to a project-root cache: a writer lease
//! ensures only one process updates the cache at a time, and read-marker files
//! let cleanup see which readers are still using the cache.
//!
//! Writer leases are stored at `<storage>/callgraph/<artifact_cache_key>/writer.lease`
//! and `<storage>/inspect/<project_scope_key>/writer.lease`. They use the
//! `fs_lock` JSON format with a `writer_epoch` nonce so a writer can detect if
//! another process has taken over before publishing changes or starting SQLite
//! write transactions.
//!
//! Read markers track active SQLite readers so cache cleanup can tell when it is
//! safe to remove old data. They are stored under
//! `<cache-domain>/readers/<generation-label>/<pid>.<hostname>.<created_at_ms>.<seq>.json`;
//! the JSON records the process identity and creation time, mtime is used as a
//! heartbeat for cleanup across hosts, and the PID is used for cleanup on the
//! same host. Marker files are created `0600` so they do not expose checkout
//! activity or let another local user delete a protected marker.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

static MARKER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Read-marker heartbeats refresh no more often than the filesystem lock
/// heartbeat. Active readers piggyback this on normal read paths instead of
/// spawning a thread per connection.
pub const READ_MARKER_TOUCH_INTERVAL_MS: u64 = fs_lock::HEARTBEAT_INTERVAL_MS;
/// Cross-host markers cannot use local PID liveness, so they expire after the
/// same conservative 5x stale-heartbeat window used by filesystem locks.
pub const READ_MARKER_CROSS_HOST_STALE_MS: u64 = fs_lock::STALE_HEARTBEAT_MS * 5;
// Process start timestamps are not always millisecond-precise across OS APIs.
// A one-second grace keeps a marker created immediately after process launch
// attached to that process while still identifying clear PID reuse.
const PROCESS_START_TIME_GRACE_MS: u64 = 1_000;

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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WriterLeaseAcquisitionKey {
    domain: RootCacheDomain,
    key: String,
    project_root: PathBuf,
}

static PROCESS_LEASES: OnceLock<Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>>> =
    OnceLock::new();
// Same-root callers share this short-lived gate so only one thread performs the
// filesystem lease attempt, while different roots do not wait on the registry
// mutex during stat/probe/create/heartbeat work.
static PROCESS_LEASE_ACQUISITIONS: OnceLock<Mutex<HashMap<ProcessLeaseKey, Weak<Mutex<()>>>>> =
    OnceLock::new();
static WRITER_LEASE_ACQUISITION_COUNTS: OnceLock<Mutex<HashMap<WriterLeaseAcquisitionKey, usize>>> =
    OnceLock::new();
static WRITER_LEASE_ACQUISITION_COUNTER_ENABLED: AtomicBool = AtomicBool::new(false);
static CONFIGURED_ARTIFACT_ACCESS: OnceLock<Mutex<HashMap<PathBuf, ArtifactAccess>>> =
    OnceLock::new();
static WARNED_BORROW_ONLY_WRITES: OnceLock<Mutex<HashSet<(PathBuf, PathBuf)>>> = OnceLock::new();

/// Root-scoped capability that distinguishes shared repository artifacts from
/// mutable state private to one checkout.
#[derive(Clone, Debug)]
pub struct ArtifactAccess {
    project_root: PathBuf,
    shared_key: String,
    private_key: String,
    borrow_only_shared: bool,
}

impl ArtifactAccess {
    fn configured(project_root: &Path, shared_key: &str, borrow_only_shared: bool) -> Self {
        let project_root = canonical_root(project_root);
        Self {
            private_key: crate::path_identity::project_scope_key(&project_root),
            project_root,
            shared_key: shared_key.to_string(),
            borrow_only_shared,
        }
    }

    /// Resolve the capability registered during configure, probing Git only for
    /// direct artifact API callers that have not configured an app context.
    pub fn for_root(project_root: &Path) -> Self {
        let project_root = canonical_root(project_root);
        if let Some(access) = configured_artifact_access()
            .lock()
            .ok()
            .and_then(|access| access.get(&project_root).cloned())
        {
            return access;
        }
        let shared_key = crate::search_index::artifact_cache_key(&project_root);
        Self::configured(
            &project_root,
            &shared_key,
            detect_linked_worktree(&project_root),
        )
    }

    /// Return whether this root may write the keyed artifact, logging the first
    /// denial for each concrete path so read-only degradation stays observable.
    pub fn allows_write(&self, artifact_key: &str, write_path: &Path) -> bool {
        let writes_keyed_dir = write_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == artifact_key);
        if !self.borrow_only_shared
            || artifact_key != self.shared_key
            || artifact_key == self.private_key
            || !writes_keyed_dir
        {
            return true;
        }
        let warning_key = (self.project_root.clone(), write_path.to_path_buf());
        let should_warn = WARNED_BORROW_ONLY_WRITES
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .map(|mut warned| {
                if warned.len() >= 4_096 {
                    warned.clear();
                }
                warned.insert(warning_key)
            })
            .unwrap_or(false);
        if should_warn {
            crate::slog_warn!(
                "borrow-only worktree denied shared artifact write at {}",
                write_path.display()
            );
        }
        false
    }
}

fn configured_artifact_access() -> &'static Mutex<HashMap<PathBuf, ArtifactAccess>> {
    CONFIGURED_ARTIFACT_ACCESS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the worktree topology already detected by configure so artifact
/// APIs can enforce it without repeating a Git subprocess on every write path.
pub fn configure_artifact_access(project_root: &Path, shared_key: &str, borrow_only_shared: bool) {
    let access = ArtifactAccess::configured(project_root, shared_key, borrow_only_shared);
    if let Ok(mut configured) = configured_artifact_access().lock() {
        if configured.len() >= 4_096 && !configured.contains_key(&access.project_root) {
            configured.clear();
        }
        configured.insert(access.project_root.clone(), access);
    }
}

fn canonical_root(project_root: &Path) -> PathBuf {
    std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf())
}

fn detect_linked_worktree(project_root: &Path) -> bool {
    if std::env::var_os("AFT_TEST_ALLOW_WORKTREE_STORE_BUILD").is_some() {
        return false;
    }
    let Ok(output) = crate::effective_path::new_command("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(git_dir), Some(common_dir)) = (lines.next(), lines.next()) else {
        return false;
    };
    canonical_root(Path::new(git_dir)) != canonical_root(Path::new(common_dir))
}

fn process_leases() -> &'static Mutex<HashMap<ProcessLeaseKey, Weak<WriterLease>>> {
    PROCESS_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn process_lease_acquisitions() -> &'static Mutex<HashMap<ProcessLeaseKey, Weak<Mutex<()>>>> {
    PROCESS_LEASE_ACQUISITIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn writer_lease_acquisition_counts() -> &'static Mutex<HashMap<WriterLeaseAcquisitionKey, usize>> {
    WRITER_LEASE_ACQUISITION_COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_writer_lease_acquisition(domain: RootCacheDomain, key: &str, project_root: &Path) {
    if !WRITER_LEASE_ACQUISITION_COUNTER_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let acquisition_key = WriterLeaseAcquisitionKey {
        domain,
        key: key.to_string(),
        project_root,
    };
    if let Ok(mut counts) = writer_lease_acquisition_counts().lock() {
        *counts.entry(acquisition_key).or_default() += 1;
    }
}

#[doc(hidden)]
pub fn reset_writer_lease_acquisition_counts_for_test() {
    WRITER_LEASE_ACQUISITION_COUNTER_ENABLED.store(true, Ordering::Relaxed);
    if let Ok(mut counts) = writer_lease_acquisition_counts().lock() {
        counts.clear();
    }
}

#[doc(hidden)]
pub fn writer_lease_acquisition_count_for_test(
    domain: RootCacheDomain,
    key: &str,
    project_root: &Path,
) -> usize {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    writer_lease_acquisition_counts()
        .lock()
        .ok()
        .and_then(|counts| {
            counts
                .get(&WriterLeaseAcquisitionKey {
                    domain,
                    key: key.to_string(),
                    project_root,
                })
                .copied()
        })
        .unwrap_or(0)
}

fn shared_process_lease(
    registry_key: &ProcessLeaseKey,
) -> Result<Option<Arc<WriterLease>>, fs_lock::AcquireError> {
    let mut leases = process_leases().lock().map_err(|_| {
        fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned"))
    })?;
    if let Some(existing) = leases.get(registry_key).and_then(Weak::upgrade) {
        if existing.verify()? {
            return Ok(Some(existing));
        }
        leases.remove(registry_key);
    }
    Ok(None)
}

fn process_lease_acquisition_lock(
    registry_key: &ProcessLeaseKey,
) -> Result<Arc<Mutex<()>>, fs_lock::AcquireError> {
    let mut acquisitions = process_lease_acquisitions().lock().map_err(|_| {
        fs_lock::AcquireError::Io(io::Error::other(
            "process lease acquisition registry poisoned",
        ))
    })?;
    if let Some(existing) = acquisitions.get(registry_key).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    if acquisitions.len() > 1024 {
        acquisitions.retain(|_, lock| lock.strong_count() > 0);
    }
    let lock = Arc::new(Mutex::new(()));
    acquisitions.insert(registry_key.clone(), Arc::downgrade(&lock));
    Ok(lock)
}

#[cfg(test)]
type AcquireSharedHook = Arc<dyn Fn(RootCacheDomain, &Path, &str) + Send + Sync + 'static>;

#[cfg(test)]
static ACQUIRE_SHARED_HOOK: OnceLock<Mutex<Option<AcquireSharedHook>>> = OnceLock::new();

#[cfg(test)]
fn set_acquire_shared_hook_for_test(hook: Option<AcquireSharedHook>) {
    *ACQUIRE_SHARED_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("acquire shared hook mutex") = hook;
}

#[cfg(test)]
fn run_acquire_shared_hook_for_test(domain: RootCacheDomain, cache_dir: &Path, key: &str) {
    let hook = ACQUIRE_SHARED_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("acquire shared hook mutex")
        .clone();
    if let Some(hook) = hook {
        hook(domain, cache_dir, key);
    }
}

#[cfg(not(test))]
fn run_acquire_shared_hook_for_test(_domain: RootCacheDomain, _cache_dir: &Path, _key: &str) {}

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
        project_root: &Path,
    ) -> Result<Option<Arc<Self>>, fs_lock::AcquireError> {
        let access = ArtifactAccess::for_root(project_root);
        if !access.allows_write(key, &writer_lease_path(cache_dir)) {
            return Ok(None);
        }
        let registry_key = ProcessLeaseKey {
            domain,
            cache_dir: canonical_process_lease_dir(cache_dir),
        };
        if let Some(existing) = shared_process_lease(&registry_key)? {
            record_writer_lease_acquisition(domain, key, project_root);
            return Ok(Some(existing));
        }

        let acquisition_lock = process_lease_acquisition_lock(&registry_key)?;
        let _acquisition_guard = acquisition_lock.lock().map_err(|_| {
            fs_lock::AcquireError::Io(io::Error::other("process lease acquisition poisoned"))
        })?;

        if let Some(existing) = shared_process_lease(&registry_key)? {
            record_writer_lease_acquisition(domain, key, project_root);
            return Ok(Some(existing));
        }

        run_acquire_shared_hook_for_test(domain, cache_dir, key);

        let lease = Arc::new(Self::acquire(domain, cache_dir, key, Duration::ZERO)?);
        process_leases()
            .lock()
            .map_err(|_| {
                fs_lock::AcquireError::Io(io::Error::other("process lease registry poisoned"))
            })?
            .insert(registry_key, Arc::downgrade(&lease));
        record_writer_lease_acquisition(domain, key, project_root);
        Ok(Some(lease))
    }

    fn acquire(
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
    last_touched_at_ms: AtomicU64,
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
        let last_touched_at_ms = AtomicU64::new(metadata.created_at_ms);
        Ok(Self {
            path,
            metadata,
            last_touched_at_ms,
        })
    }

    pub fn touch(&self) -> io::Result<()> {
        write_marker_file(&self.path, &self.metadata)?;
        self.last_touched_at_ms.store(now_ms(), Ordering::Relaxed);
        Ok(())
    }

    pub fn touch_if_due(&self) -> io::Result<()> {
        let now = now_ms();
        let last = self.last_touched_at_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < READ_MARKER_TOUCH_INTERVAL_MS {
            return Ok(());
        }
        self.touch()
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadMarkerSweep {
    pub protected: bool,
    pub removed_stale: usize,
}

pub fn protected_read_marker_exists(cache_dir: &Path, generation_label: &str) -> bool {
    read_marker_protection(cache_dir, generation_label, false).protected
}

pub fn sweep_read_markers(cache_dir: &Path, generation_label: &str) -> ReadMarkerSweep {
    read_marker_protection(cache_dir, generation_label, true)
}

fn read_marker_protection(
    cache_dir: &Path,
    generation_label: &str,
    remove_stale: bool,
) -> ReadMarkerSweep {
    let dir = read_marker_dir(cache_dir, generation_label);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ReadMarkerSweep::default(),
        Err(_) => {
            return ReadMarkerSweep {
                protected: true,
                removed_stale: 0,
            };
        }
    };

    let hostname = current_hostname();
    let now = now_ms();
    let mut sweep = ReadMarkerSweep::default();
    for entry in entries.flatten() {
        let path = entry.path();
        match marker_file_is_protected(&path, now, &hostname) {
            MarkerProtection::Protected => sweep.protected = true,
            MarkerProtection::Stale | MarkerProtection::Malformed => {
                if remove_stale && fs::remove_file(&path).is_ok() {
                    fs_lock::sync_parent(&path);
                    sweep.removed_stale += 1;
                }
            }
        }
    }
    sweep
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerProtection {
    Protected,
    Stale,
    Malformed,
}

fn marker_file_is_protected(path: &Path, now: u64, current_host: &str) -> MarkerProtection {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return MarkerProtection::Stale,
        Err(_) => return MarkerProtection::Protected,
    };
    let metadata: ReadMarkerMetadata = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(_) => return MarkerProtection::Malformed,
    };
    if metadata.hostname != current_host {
        let Ok(file_metadata) = fs::metadata(path) else {
            return MarkerProtection::Protected;
        };
        let mtime_ms = file_metadata
            .modified()
            .ok()
            .map(system_time_ms)
            .unwrap_or(now);
        let age_ms = now.saturating_sub(mtime_ms);
        return if age_ms <= READ_MARKER_CROSS_HOST_STALE_MS {
            MarkerProtection::Protected
        } else {
            MarkerProtection::Stale
        };
    }

    if !fs_lock::process_alive(metadata.pid) {
        return MarkerProtection::Stale;
    }
    if marker_matches_live_process_instance(&metadata) {
        MarkerProtection::Protected
    } else {
        MarkerProtection::Stale
    }
}

fn marker_matches_live_process_instance(metadata: &ReadMarkerMetadata) -> bool {
    // Same-host PID liveness is authoritative for the process instance. When the
    // OS can tell us the live PID started after this marker was created, the PID
    // has been reused and the marker belongs to a dead prior process; otherwise
    // a live PID protects the marker without consulting marker mtime.
    process_start_time_ms(metadata.pid)
        .map(|started_at_ms| {
            started_at_ms
                <= metadata
                    .created_at_ms
                    .saturating_add(PROCESS_START_TIME_GRACE_MS)
        })
        .unwrap_or(true)
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
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let normalized = lexical_normalize(path);
    let mut missing_components = Vec::new();
    let mut current = normalized.as_path();
    while !current.exists() {
        let Some(name) = current.file_name() else {
            return normalized;
        };
        missing_components.push(name.to_os_string());
        let Some(parent) = current.parent() else {
            return normalized;
        };
        current = parent;
    }

    let mut canonical = std::fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    for component in missing_components.iter().rev() {
        canonical.push(component);
    }
    canonical
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
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

#[cfg(test)]
static PROCESS_START_TIME_OVERRIDES: OnceLock<Mutex<HashMap<u32, Option<u64>>>> = OnceLock::new();

#[cfg(test)]
fn set_process_start_time_for_test(pid: u32, started_at_ms: Option<u64>) {
    PROCESS_START_TIME_OVERRIDES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("process start override mutex")
        .insert(pid, started_at_ms);
}

#[cfg(test)]
fn clear_process_start_time_for_test(pid: u32) {
    if let Some(overrides) = PROCESS_START_TIME_OVERRIDES.get() {
        overrides
            .lock()
            .expect("process start override mutex")
            .remove(&pid);
    }
}

#[cfg(test)]
fn process_start_time_override(pid: u32) -> Option<Option<u64>> {
    PROCESS_START_TIME_OVERRIDES
        .get()
        .and_then(|overrides| overrides.lock().ok()?.get(&pid).copied())
}

#[cfg(target_os = "linux")]
fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    let boot_time_secs = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse::<u64>().ok())?;
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    let ticks_per_second = ticks_per_second as u64;
    Some(
        boot_time_secs
            .saturating_mul(1_000)
            .saturating_add(start_ticks.saturating_mul(1_000) / ticks_per_second),
    )
}

#[cfg(target_os = "macos")]
fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }

    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let info_size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            info_size,
        )
    };
    if bytes != info_size {
        return None;
    }
    Some(
        info.pbi_start_tvsec
            .saturating_mul(1_000)
            .saturating_add(info.pbi_start_tvusec / 1_000),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(test)]
    if let Some(override_value) = process_start_time_override(pid) {
        return override_value;
    }
    let _ = pid;
    None
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
    fn same_host_live_marker_ignores_stale_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let marker = ReadMarker::create(dir.path(), "current").unwrap();
        filetime::set_file_mtime(marker.path(), filetime::FileTime::from_unix_time(0, 0)).unwrap();

        assert!(protected_read_marker_exists(dir.path(), "current"));
    }

    #[test]
    fn sweep_removes_dead_same_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("dead.json");
        let metadata = ReadMarkerMetadata {
            pid: 0,
            hostname: current_hostname(),
            created_at_ms: now_ms(),
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_reused_pid_marker_when_created_at_predates_process_start() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("reused.json");
        let pid = std::process::id();
        let metadata = ReadMarkerMetadata {
            pid,
            hostname: current_hostname(),
            created_at_ms: 1_000,
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        set_process_start_time_for_test(pid, Some(10_000));

        let sweep = sweep_read_markers(dir.path(), "old");
        clear_process_start_time_for_test(pid);

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[test]
    fn sweep_removes_expired_cross_host_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker_path = read_marker_dir(dir.path(), "old").join("cross-host.json");
        let metadata = ReadMarkerMetadata {
            pid: 123,
            hostname: format!("other-{}", current_hostname()),
            created_at_ms: now_ms(),
        };
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        write_marker_file(&marker_path, &metadata).unwrap();
        let stale_time = SystemTime::now()
            .checked_sub(Duration::from_millis(
                READ_MARKER_CROSS_HOST_STALE_MS.saturating_add(1_000),
            ))
            .unwrap_or(UNIX_EPOCH);
        filetime::set_file_mtime(
            &marker_path,
            filetime::FileTime::from_system_time(stale_time),
        )
        .unwrap();

        let sweep = sweep_read_markers(dir.path(), "old");

        assert!(!sweep.protected);
        assert_eq!(sweep.removed_stale, 1);
        assert!(!marker_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn process_lease_dir_canonicalizes_existing_ancestor_before_cache_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let missing_cache_dir = link.join("inspect").join("project");
        let before_create = canonical_process_lease_dir(&missing_cache_dir);
        fs::create_dir_all(&missing_cache_dir).unwrap();
        let after_create = canonical_process_lease_dir(&missing_cache_dir);

        assert_eq!(before_create, after_create);
        assert!(before_create.starts_with(std::fs::canonicalize(&real).unwrap()));
    }

    #[test]
    fn borrow_only_root_never_receives_existing_shared_writer_capability() {
        let storage = tempfile::tempdir().unwrap();
        let parent_root = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let shared_key = "shared-artifact-key";
        let cache_dir = storage.path().join("callgraph").join(shared_key);
        configure_artifact_access(parent_root.path(), shared_key, false);
        configure_artifact_access(worktree_root.path(), shared_key, true);

        let parent_lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            shared_key,
            parent_root.path(),
        )
        .unwrap()
        .expect("parent writer lease");
        reset_writer_lease_acquisition_counts_for_test();

        let worktree_lease = WriterLease::acquire_shared(
            RootCacheDomain::Callgraph,
            &cache_dir,
            shared_key,
            worktree_root.path(),
        )
        .unwrap();

        assert!(worktree_lease.is_none());
        assert!(parent_lease.verify().unwrap());
        assert_eq!(
            writer_lease_acquisition_count_for_test(
                RootCacheDomain::Callgraph,
                shared_key,
                worktree_root.path(),
            ),
            0
        );
    }

    #[test]
    fn borrow_only_root_keeps_private_project_scope_writable() {
        let storage = tempfile::tempdir().unwrap();
        let worktree_root = tempfile::tempdir().unwrap();
        let shared_key = "shared-artifact-key";
        let private_key = crate::path_identity::project_scope_key(worktree_root.path());
        let cache_dir = storage.path().join("inspect").join(&private_key);
        configure_artifact_access(worktree_root.path(), shared_key, true);

        let lease = WriterLease::acquire_shared(
            RootCacheDomain::Inspect,
            &cache_dir,
            &private_key,
            worktree_root.path(),
        )
        .unwrap()
        .expect("private inspect writer lease");

        assert!(lease.verify().unwrap());
    }

    #[test]
    fn writer_lease_acquire_shared_does_not_serialize_different_roots() {
        let dir = tempfile::tempdir().unwrap();
        let blocked_cache_dir = dir.path().join("callgraph").join("blocked");
        let free_cache_dir = dir.path().join("callgraph").join("free");
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                set_acquire_shared_hook_for_test(None);
            }
        }

        set_acquire_shared_hook_for_test(Some(Arc::new(move |_, _, key| {
            if key == "blocked" {
                blocked_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
        })));
        let _hook_guard = HookGuard;

        let blocked_handle = std::thread::spawn(move || {
            WriterLease::acquire_shared(
                RootCacheDomain::Callgraph,
                &blocked_cache_dir,
                "blocked",
                &blocked_cache_dir,
            )
            .map_err(|error| error.to_string())
            .and_then(|lease| lease.ok_or_else(|| "writer lease unexpectedly denied".to_string()))
        });
        blocked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked root should reach acquisition hook");

        let (free_tx, free_rx) = std::sync::mpsc::channel();
        let free_handle = std::thread::spawn(move || {
            let result = WriterLease::acquire_shared(
                RootCacheDomain::Callgraph,
                &free_cache_dir,
                "free",
                &free_cache_dir,
            )
            .map_err(|error| error.to_string())
            .and_then(|lease| lease.ok_or_else(|| "writer lease unexpectedly denied".to_string()));
            free_tx.send(result).unwrap();
        });
        let free_lease = free_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("free root should not wait behind another root's acquisition")
            .expect("free root should acquire while another root is in acquisition");
        assert!(free_lease.verify().unwrap());
        free_handle
            .join()
            .expect("free root thread should not panic");

        release_tx.send(()).unwrap();
        let blocked_lease = blocked_handle
            .join()
            .expect("blocked root thread should not panic")
            .expect("blocked root should acquire after release");
        assert!(blocked_lease.verify().unwrap());
    }

    #[test]
    fn writer_lease_acquire_shared_reuses_single_process_lease_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("inspect").join("project");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let barrier = std::sync::Arc::clone(&barrier);
            let cache_dir = cache_dir.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                WriterLease::acquire_shared(
                    RootCacheDomain::Inspect,
                    &cache_dir,
                    "project",
                    &cache_dir,
                )
                .map_err(|error| error.to_string())
                .and_then(|lease| {
                    lease.ok_or_else(|| "writer lease unexpectedly denied".to_string())
                })
            }));
        }

        let leases = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let epoch = leases[0].epoch().to_string();
        let path = leases[0].path().to_path_buf();
        for lease in &leases {
            assert_eq!(lease.epoch(), epoch);
            assert_eq!(lease.path(), path.as_path());
            assert!(lease.verify().unwrap());
        }
    }

    #[test]
    fn nfs_guard_test_seam_fails_closed() {
        set_force_network_fs_for_test(true);
        assert!(!storage_allows_root_keyed(Path::new(".")).unwrap());
        set_force_network_fs_for_test(false);
    }
}
