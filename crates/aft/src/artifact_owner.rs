use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fs_lock;

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ArtifactOwnerManifest {
    pub schema_version: u32,
    pub project_scope_key: String,
    pub checkout_path: String,
    pub git_common_dir: Option<String>,
    pub pid: u32,
    pub hostname: String,
    pub created_at_ms: u64,
    pub heartbeat_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOwnerMode {
    Owner,
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArtifactOwnerStatus {
    pub mode: ArtifactOwnerMode,
    pub project_key: String,
    pub manifest_path: String,
    pub owner_project_scope_key: String,
    pub owner_checkout_path: String,
    pub note: Option<String>,
}

#[derive(Debug)]
pub struct ArtifactOwnerLease {
    path: PathBuf,
    manifest: ArtifactOwnerManifest,
    last_heartbeat_ms: u64,
}

#[derive(Debug)]
pub struct ArtifactOwnerClaim {
    pub status: ArtifactOwnerStatus,
    pub lease: Option<ArtifactOwnerLease>,
}

pub fn claim_or_open_read_only(
    storage_dir: Option<&Path>,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
    git_common_dir: Option<&Path>,
) -> io::Result<ArtifactOwnerClaim> {
    let manifest_dir = resolve_manifest_dir(storage_dir, project_root, project_key);
    fs::create_dir_all(&manifest_dir)?;
    let path = manifest_dir.join("owner.json");
    let checkout_path = project_root.display().to_string();
    let git_common_dir = git_common_dir.map(|path| path.display().to_string());

    loop {
        match read_manifest(&path) {
            Ok(existing) => {
                let same_checkout = existing.project_scope_key == project_scope_key;
                let linked_worktree = existing.git_common_dir.is_some()
                    && existing.git_common_dir == git_common_dir
                    && git_common_dir.is_some();
                if same_checkout || linked_worktree {
                    return write_owner_manifest(
                        &path,
                        project_key,
                        project_scope_key,
                        &checkout_path,
                        git_common_dir.as_deref(),
                    );
                }

                if manifest_owner_alive(&existing) {
                    let note = format!(
                        "shared artifacts opened read-only: cache key {project_key} is owned by checkout {} (scope {}, pid {})",
                        existing.checkout_path, existing.project_scope_key, existing.pid
                    );
                    return Ok(ArtifactOwnerClaim {
                        status: ArtifactOwnerStatus {
                            mode: ArtifactOwnerMode::ReadOnly,
                            project_key: project_key.to_string(),
                            manifest_path: path.display().to_string(),
                            owner_project_scope_key: existing.project_scope_key,
                            owner_checkout_path: existing.checkout_path,
                            note: Some(note),
                        },
                        lease: None,
                    });
                }

                if reclaim_manifest_if_unchanged(&path, &existing)? {
                    continue;
                }
            }
            Err(ReadManifestError::NotFound) => {
                match create_owner_manifest(
                    &path,
                    project_key,
                    project_scope_key,
                    &checkout_path,
                    git_common_dir.as_deref(),
                ) {
                    Ok(claim) => return Ok(claim),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(ReadManifestError::Malformed) => {
                let _ = fs::remove_file(&path);
                continue;
            }
            Err(ReadManifestError::Io(error)) => return Err(error),
        }
    }
}

impl ArtifactOwnerLease {
    pub fn heartbeat_if_due(&mut self) {
        let now = now_ms();
        if now.saturating_sub(self.last_heartbeat_ms) < fs_lock::HEARTBEAT_INTERVAL_MS {
            return;
        }
        self.manifest.heartbeat_at_ms = now;
        if atomic_write_manifest(&self.path, &self.manifest).is_ok() {
            self.last_heartbeat_ms = now;
        }
    }
}

fn create_owner_manifest(
    path: &Path,
    project_key: &str,
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> io::Result<ArtifactOwnerClaim> {
    let manifest = new_manifest(project_scope_key, checkout_path, git_common_dir);
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    write_manifest_to_file(&mut file, &manifest)?;
    file.sync_all()?;
    sync_parent(path);
    Ok(owner_claim(path, project_key, manifest))
}

fn write_owner_manifest(
    path: &Path,
    project_key: &str,
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> io::Result<ArtifactOwnerClaim> {
    let manifest = new_manifest(project_scope_key, checkout_path, git_common_dir);
    atomic_write_manifest(path, &manifest)?;
    Ok(owner_claim(path, project_key, manifest))
}

fn owner_claim(
    path: &Path,
    project_key: &str,
    manifest: ArtifactOwnerManifest,
) -> ArtifactOwnerClaim {
    let last_heartbeat_ms = manifest.heartbeat_at_ms;
    ArtifactOwnerClaim {
        status: ArtifactOwnerStatus {
            mode: ArtifactOwnerMode::Owner,
            project_key: project_key.to_string(),
            manifest_path: path.display().to_string(),
            owner_project_scope_key: manifest.project_scope_key.clone(),
            owner_checkout_path: manifest.checkout_path.clone(),
            note: None,
        },
        lease: Some(ArtifactOwnerLease {
            path: path.to_path_buf(),
            manifest,
            last_heartbeat_ms,
        }),
    }
}

fn new_manifest(
    project_scope_key: &str,
    checkout_path: &str,
    git_common_dir: Option<&str>,
) -> ArtifactOwnerManifest {
    let now = now_ms();
    ArtifactOwnerManifest {
        schema_version: SCHEMA_VERSION,
        project_scope_key: project_scope_key.to_string(),
        checkout_path: checkout_path.to_string(),
        git_common_dir: git_common_dir.map(str::to_string),
        pid: std::process::id(),
        hostname: current_hostname(),
        created_at_ms: now,
        heartbeat_at_ms: now,
    }
}

fn manifest_owner_alive(manifest: &ArtifactOwnerManifest) -> bool {
    let now = now_ms();
    let since_heartbeat = now.saturating_sub(manifest.heartbeat_at_ms);
    if manifest.hostname != current_hostname() {
        return since_heartbeat <= fs_lock::STALE_HEARTBEAT_MS.saturating_mul(5);
    }
    process_alive(manifest.pid)
}

fn reclaim_manifest_if_unchanged(path: &Path, judged: &ArtifactOwnerManifest) -> io::Result<bool> {
    match read_manifest(path) {
        Ok(current)
            if current.pid == judged.pid
                && current.hostname == judged.hostname
                && current.created_at_ms == judged.created_at_ms =>
        {
            fs::remove_file(path)?;
            sync_parent(path);
            Ok(true)
        }
        Ok(_) | Err(ReadManifestError::NotFound) | Err(ReadManifestError::Malformed) => Ok(false),
        Err(ReadManifestError::Io(error)) => Err(error),
    }
}

enum ReadManifestError {
    NotFound,
    Io(io::Error),
    Malformed,
}

fn read_manifest(path: &Path) -> Result<ArtifactOwnerManifest, ReadManifestError> {
    let bytes = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReadManifestError::NotFound
        } else {
            ReadManifestError::Io(error)
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|_| ReadManifestError::Malformed)
}

fn atomic_write_manifest(path: &Path, manifest: &ArtifactOwnerManifest) -> io::Result<()> {
    let tmp = temp_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp)?;
        write_manifest_to_file(&mut file, manifest)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_parent(path);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

fn write_manifest_to_file(file: &mut File, manifest: &ArtifactOwnerManifest) -> io::Result<()> {
    serde_json::to_writer(&mut *file, manifest).map_err(io::Error::other)?;
    file.write_all(b"\n")
}

fn temp_path(path: &Path) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();
    path.with_extension(format!("json.tmp.{}.{}", std::process::id(), now))
}

fn resolve_manifest_dir(
    storage_dir: Option<&Path>,
    project_root: &Path,
    project_key: &str,
) -> PathBuf {
    if let Some(override_dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(override_dir)
            .join("artifact-owners")
            .join(project_key);
    }
    if let Some(dir) = storage_dir {
        return dir.join("artifact-owners").join(project_key);
    }
    crate::search_index::resolve_cache_dir(project_root, None)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join("artifact-owners")
        .join(project_key)
}

fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(windows)]
fn current_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(not(any(unix, windows)))]
fn current_hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    !stdout.contains("No tasks are running") && stdout.contains(&format!("\"{pid}\""))
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
pub(crate) fn write_synthetic_manifest_for_test(
    storage_dir: &Path,
    project_root: &Path,
    project_key: &str,
    project_scope_key: &str,
    pid: u32,
    heartbeat_at_ms: u64,
) {
    let dir = resolve_manifest_dir(Some(storage_dir), project_root, project_key);
    fs::create_dir_all(&dir).unwrap();
    let now = now_ms();
    let manifest = ArtifactOwnerManifest {
        schema_version: SCHEMA_VERSION,
        project_scope_key: project_scope_key.to_string(),
        checkout_path: project_root.display().to_string(),
        git_common_dir: None,
        pid,
        hostname: current_hostname(),
        created_at_ms: now,
        heartbeat_at_ms,
    };
    atomic_write_manifest(&dir.join("owner.json"), &manifest).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_checkout_opens_read_only_while_owner_is_alive() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("owner");
        let sibling = temp.path().join("sibling");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let key = "shared-key";

        let first =
            claim_or_open_read_only(Some(temp.path()), &owner, key, "owner-scope", None).unwrap();
        assert_eq!(first.status.mode, ArtifactOwnerMode::Owner);
        assert!(first.lease.is_some());

        let second =
            claim_or_open_read_only(Some(temp.path()), &sibling, key, "sibling-scope", None)
                .unwrap();
        assert_eq!(second.status.mode, ArtifactOwnerMode::ReadOnly);
        assert!(second.status.note.unwrap().contains("read-only"));
        assert!(second.lease.is_none());
    }

    #[test]
    fn same_checkout_reconfigure_reclaims_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let key = "shared-key";

        let first = claim_or_open_read_only(Some(temp.path()), &root, key, "scope", None).unwrap();
        let second = claim_or_open_read_only(Some(temp.path()), &root, key, "scope", None).unwrap();

        assert_eq!(first.status.mode, ArtifactOwnerMode::Owner);
        assert_eq!(second.status.mode, ArtifactOwnerMode::Owner);
        assert!(second.lease.is_some());
    }

    #[test]
    fn dead_owner_is_reclaimed_by_different_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("owner");
        let sibling = temp.path().join("sibling");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let key = "shared-key";
        write_synthetic_manifest_for_test(temp.path(), &owner, key, "owner-scope", 0, 0);

        let claim =
            claim_or_open_read_only(Some(temp.path()), &sibling, key, "sibling-scope", None)
                .unwrap();

        assert_eq!(claim.status.mode, ArtifactOwnerMode::Owner);
        assert_eq!(claim.status.owner_project_scope_key, "sibling-scope");
        assert!(claim.lease.is_some());
    }

    #[test]
    fn linked_worktree_common_dir_is_not_forced_read_only_by_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let owner = temp.path().join("owner");
        let linked = temp.path().join("linked");
        let common = temp.path().join("common.git");
        fs::create_dir_all(&owner).unwrap();
        fs::create_dir_all(&linked).unwrap();
        fs::create_dir_all(&common).unwrap();
        let key = "shared-key";

        claim_or_open_read_only(Some(temp.path()), &owner, key, "owner-scope", Some(&common))
            .unwrap();
        let claim = claim_or_open_read_only(
            Some(temp.path()),
            &linked,
            key,
            "linked-scope",
            Some(&common),
        )
        .unwrap();

        assert_eq!(claim.status.mode, ArtifactOwnerMode::Owner);
    }
}
