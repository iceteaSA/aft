use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const SANDBOX_PROFILE_VERSION: u32 = 2;

/// Versioned policy transferred to `aft sandbox-launch` by descriptor or private task file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProfile {
    pub v: u32,
    pub writable_roots: Vec<PathBuf>,
    /// Mandatory write-deny paths for backends that support nested exclusions.
    #[serde(default)]
    pub write_deny: Vec<PathBuf>,
    pub write_deny_nested: Vec<PathBuf>,
    /// Existing paths that Landlock may grant read access beneath.
    #[serde(default)]
    pub read_allow: Vec<PathBuf>,
    pub read_deny: Vec<PathBuf>,
    pub socket_deny: Vec<PathBuf>,
    pub cache_roots: Vec<PathBuf>,
    pub temp_dir: PathBuf,
}

impl SandboxProfile {
    /// Build a profile whose existing paths are canonical before serialization.
    ///
    /// Writable roots, read grants, cache roots, and the task temp directory must already
    /// exist. Missing deny targets are normalized through their nearest
    /// existing ancestor so resources created after launch remain enforceable.
    pub fn build(
        writable_roots: Vec<PathBuf>,
        write_deny: Vec<PathBuf>,
        write_deny_nested: Vec<PathBuf>,
        read_allow: Vec<PathBuf>,
        read_deny: Vec<PathBuf>,
        socket_deny: Vec<PathBuf>,
        cache_roots: Vec<PathBuf>,
        temp_dir: PathBuf,
    ) -> Result<Self, SandboxProfileError> {
        Ok(Self {
            v: SANDBOX_PROFILE_VERSION,
            writable_roots: canonicalize_required_dirs(writable_roots, "writable_roots")?,
            write_deny: canonicalize_optional_paths(write_deny, "write_deny")?,
            write_deny_nested: canonicalize_optional_paths(write_deny_nested, "write_deny_nested")?,
            read_allow: canonicalize_required_paths(read_allow, "read_allow")?,
            read_deny: canonicalize_optional_paths(read_deny, "read_deny")?,
            socket_deny: canonicalize_optional_paths(socket_deny, "socket_deny")?,
            cache_roots: canonicalize_required_dirs(cache_roots, "cache_roots")?,
            temp_dir: canonicalize_required_dir(temp_dir, "temp_dir")?,
        })
    }

    /// Revalidate an inherited profile and canonicalize it in the launcher.
    ///
    /// Missing deny targets remain enforceable by resolving their nearest
    /// existing ancestor. Every path that grants write access must exist.
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
            write_deny: canonicalize_optional_paths(self.write_deny, "write_deny")?,
            write_deny_nested: canonicalize_optional_paths(
                self.write_deny_nested,
                "write_deny_nested",
            )?,
            read_allow: canonicalize_required_paths(self.read_allow, "read_allow")?,
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

fn canonicalize_required_paths(
    paths: Vec<PathBuf>,
    field: &str,
) -> Result<Vec<PathBuf>, SandboxProfileError> {
    let mut canonical = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute(&path, field)?;
        canonical.push(path.canonicalize().map_err(|error| {
            SandboxProfileError::new(format!(
                "{field} path does not exist: {}: {error}",
                path.display()
            ))
        })?);
    }
    canonical.sort_unstable();
    canonical.dedup();
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
                canonical.push(canonicalize_missing_path(path, field)?);
            }
            Err(error) => {
                return Err(canonicalization_error(&path, field, &error));
            }
        }
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

fn canonicalize_missing_path(path: PathBuf, field: &str) -> Result<PathBuf, SandboxProfileError> {
    let original = path.clone();
    let mut ancestor = path;
    let mut missing_tail = Vec::new();

    loop {
        match ancestor.canonicalize() {
            Ok(mut canonical) => {
                for component in missing_tail.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(_) => return Err(canonicalization_error(&original, field, &error)),
                    Err(probe_error) if probe_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(probe_error) => {
                        return Err(canonicalization_error(&original, field, &probe_error));
                    }
                }
                let Some(component) = ancestor.file_name().map(ToOwned::to_owned) else {
                    return Err(canonicalization_error(&original, field, &error));
                };
                missing_tail.push(component);
                if !ancestor.pop() {
                    return Err(canonicalization_error(&original, field, &error));
                }
            }
            Err(error) => return Err(canonicalization_error(&original, field, &error)),
        }
    }
}

fn canonicalization_error(path: &Path, field: &str, error: &std::io::Error) -> SandboxProfileError {
    SandboxProfileError::new(format!(
        "failed to canonicalize {field} path {}: {error}",
        path.display()
    ))
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
        std::fs::write(project.join("readable.txt"), b"readable").expect("readable file");
        let missing = root.path().join("missing-secret");
        let canonical_missing = root
            .path()
            .canonicalize()
            .expect("canonical root")
            .join("missing-secret");

        let profile = SandboxProfile::build(
            vec![project.clone()],
            Vec::new(),
            Vec::new(),
            vec![project.join("readable.txt")],
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
        assert_eq!(
            profile.read_allow,
            vec![project.join("readable.txt").canonicalize().unwrap()]
        );
        assert_eq!(profile.cache_roots, vec![cache.canonicalize().unwrap()]);
        assert_eq!(profile.temp_dir, temp.canonicalize().unwrap());
        assert_eq!(profile.read_deny, vec![canonical_missing]);
    }

    #[test]
    fn mixed_profile_versions_fail_closed() {
        #[derive(Debug, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyProfile {
            v: u32,
            writable_roots: Vec<PathBuf>,
            #[serde(default)]
            write_deny: Vec<PathBuf>,
            write_deny_nested: Vec<PathBuf>,
            read_deny: Vec<PathBuf>,
            socket_deny: Vec<PathBuf>,
            cache_roots: Vec<PathBuf>,
            temp_dir: PathBuf,
        }

        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let legacy_json = serde_json::json!({
            "v": 1,
            "writable_roots": [root],
            "write_deny": [],
            "write_deny_nested": [],
            "read_deny": [],
            "socket_deny": [],
            "cache_roots": [],
            "temp_dir": root,
        });
        let legacy: SandboxProfile =
            serde_json::from_value(legacy_json).expect("v1 shape remains parseable");
        let error = legacy
            .canonicalize_for_launch()
            .expect_err("new launcher must reject a v1 profile");
        assert!(error
            .to_string()
            .contains("unsupported sandbox profile version 1; expected 2"));

        let current = SandboxProfile::build(
            vec![root.clone()],
            Vec::new(),
            Vec::new(),
            vec![root.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            root,
        )
        .expect("current profile");
        let error = serde_json::from_value::<LegacyProfile>(
            serde_json::to_value(current).expect("serialize current profile"),
        )
        .expect_err("v1 launcher shape must reject read_allow");
        assert!(error.to_string().contains("unknown field `read_allow`"));
    }

    #[test]
    fn launch_validation_retains_normalized_missing_deny_paths() {
        let root = tempfile::tempdir().expect("temp root");
        let root = root.path().canonicalize().expect("canonical root");
        let profile = SandboxProfile {
            v: SANDBOX_PROFILE_VERSION,
            writable_roots: vec![root.clone()],
            write_deny: vec![root.join("missing-write-deny")],
            write_deny_nested: vec![root.join("missing-nested")],
            read_allow: vec![root.clone()],
            read_deny: vec![root.join("missing-secret")],
            socket_deny: vec![root.join("missing.sock")],
            cache_roots: Vec::new(),
            temp_dir: root.clone(),
        }
        .canonicalize_for_launch()
        .expect("validate profile");

        assert_eq!(profile.write_deny, vec![root.join("missing-write-deny")]);
        assert_eq!(profile.write_deny_nested, vec![root.join("missing-nested")]);
        assert_eq!(profile.read_deny, vec![root.join("missing-secret")]);
        assert_eq!(profile.socket_deny, vec![root.join("missing.sock")]);
    }
}
