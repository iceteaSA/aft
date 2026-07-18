use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const SANDBOX_PROFILE_VERSION: u32 = 1;

/// Versioned policy transferred to `aft sandbox-launch` through an inherited file descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    pub v: u32,
    pub writable_roots: Vec<PathBuf>,
    pub write_deny_nested: Vec<PathBuf>,
    pub read_deny: Vec<PathBuf>,
    pub socket_deny: Vec<PathBuf>,
    pub cache_roots: Vec<PathBuf>,
    pub temp_dir: PathBuf,
}

impl SandboxProfile {
    /// Build a profile whose existing paths are canonical before serialization.
    ///
    /// Writable roots, cache roots, and the task temp directory must already
    /// exist. Missing deny targets are retained as normalized absolute paths so
    /// callers can describe optional resources that may appear before launch.
    pub fn build(
        writable_roots: Vec<PathBuf>,
        write_deny_nested: Vec<PathBuf>,
        read_deny: Vec<PathBuf>,
        socket_deny: Vec<PathBuf>,
        cache_roots: Vec<PathBuf>,
        temp_dir: PathBuf,
    ) -> Result<Self, SandboxProfileError> {
        Ok(Self {
            v: SANDBOX_PROFILE_VERSION,
            writable_roots: canonicalize_required_dirs(writable_roots, "writable_roots")?,
            write_deny_nested: canonicalize_optional_paths(write_deny_nested, "write_deny_nested")?,
            read_deny: canonicalize_optional_paths(read_deny, "read_deny")?,
            socket_deny: canonicalize_optional_paths(socket_deny, "socket_deny")?,
            cache_roots: canonicalize_required_dirs(cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(temp_dir, "temp_dir")?,
        })
    }

    /// Revalidate an inherited profile and canonicalize it in the launcher.
    ///
    /// Missing deny targets remain normalized in the profile and platform
    /// appliers skip them. Every path that grants write access must exist.
    pub fn canonicalize_for_launch(self) -> Result<Self, SandboxProfileError> {
        if self.v != SANDBOX_PROFILE_VERSION {
            return Err(SandboxProfileError::new(format!(
                "unsupported sandbox profile version {}; expected {SANDBOX_PROFILE_VERSION}",
                self.v
            )));
        }

        Ok(Self {
            v: self.v,
            writable_roots: canonicalize_required_dirs(self.writable_roots, "writable_roots")?,
            write_deny_nested: canonicalize_optional_paths(
                self.write_deny_nested,
                "write_deny_nested",
            )?,
            read_deny: canonicalize_optional_paths(self.read_deny, "read_deny")?,
            socket_deny: canonicalize_optional_paths(self.socket_deny, "socket_deny")?,
            cache_roots: canonicalize_required_dirs(self.cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(self.temp_dir, "temp_dir")?,
        })
    }

    pub fn write_allow_roots(&self) -> Vec<&Path> {
        let mut roots: Vec<&Path> = self
            .writable_roots
            .iter()
            .chain(&self.cache_roots)
            .map(PathBuf::as_path)
            .collect();
        roots.push(self.temp_dir.as_path());
        roots.sort_unstable();
        roots.dedup();
        roots
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfileError {
    message: String,
}

impl SandboxProfileError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SandboxProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxProfileError {}

fn canonicalize_required_dirs(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    paths
        .into_iter()
        .map(|path| canonicalize_required_dir(path, field))
        .collect()
}

fn canonicalize_required_dir(path: PathBuf, field: &str) -> Result<PathBuf, SandboxProfileError> {
    validate_absolute(&path, field)?;
    let canonical = path.canonicalize().map_err(|error| {
        SandboxProfileError::new(format!(
            "{field} path is not an existing directory: {}: {error}",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(SandboxProfileError::new(format!(
            "{field} path is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn canonicalize_optional_paths(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute(&path, field)?;
        match path.canonicalize() {
            Ok(path) => canonical.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                validate_normalized(&path, field)?;
                canonical.push(path);
            }
            Err(error) => {
                return Err(SandboxProfileError::new(format!(
                    "failed to canonicalize {field} path {}: {error}",
                    path.display()
                )));
            }
        }
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

fn validate_absolute(path: &Path, field: &str) -> Result<(), SandboxProfileError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(SandboxProfileError::new(format!(
            "{field} paths must be absolute: {}",
            path.display()
        )))
    }
}

fn validate_normalized(path: &Path, field: &str) -> Result<(), SandboxProfileError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxProfileError::new(format!(
            "nonexistent {field} paths must be normalized: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_canonicalizes_write_paths_and_retains_missing_denies() {
        let root = tempfile::tempdir().expect("temp root");
        let project = root.path().join("project");
        let cache = root.path().join("cache");
        let temp = root.path().join("temp");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::create_dir_all(&temp).expect("temp");
        let missing = root.path().join("missing-secret");

        let profile = SandboxProfile::build(
            vec![project.clone()],
            Vec::new(),
            vec![missing.clone()],
            Vec::new(),
            vec![cache.clone()],
            temp.clone(),
        )
        .expect("build profile");

        assert_eq!(
            profile.writable_roots,
            vec![project.canonicalize().unwrap()]
        );
        assert_eq!(profile.cache_roots, vec![cache.canonicalize().unwrap()]);
        assert_eq!(profile.temp_dir, temp.canonicalize().unwrap());
        assert_eq!(profile.read_deny, vec![missing]);
    }

    #[test]
    fn launch_validation_retains_normalized_missing_deny_paths() {
        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let profile = SandboxProfile {
            v: SANDBOX_PROFILE_VERSION,
            writable_roots: vec![root.clone()],
            write_deny_nested: vec![root.join("missing-nested")],
            read_deny: vec![root.join("missing-secret")],
            socket_deny: vec![root.join("missing.sock")],
            cache_roots: Vec::new(),
            temp_dir: root.clone(),
        }
        .canonicalize_for_launch()
        .expect("validate profile");

        assert_eq!(profile.write_deny_nested, vec![root.join("missing-nested")]);
        assert_eq!(profile.read_deny, vec![root.join("missing-secret")]);
        assert_eq!(profile.socket_deny, vec![root.join("missing.sock")]);
    }
}
