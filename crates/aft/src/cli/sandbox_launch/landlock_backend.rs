use aft::sandbox_profile::SandboxProfile;
use landlock::{
    AccessFs, CompatLevel, Compatible, LandlockStatus, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, ABI,
};
use std::path::Path;

/// Highest ABI understood by the pinned landlock crate.
pub(super) const TARGET_ABI: ABI = ABI::V7;
/// V3 closes the `truncate(2)` gap in the write allowlist.
pub(super) const REQUIRED_WRITE_ABI: ABI = ABI::V3;

#[derive(Debug, Clone, Copy)]
pub(super) struct AppliedLandlock {
    pub effective_abi: ABI,
    pub partially_enforced: bool,
}

pub(super) fn apply(profile: &SandboxProfile) -> Result<AppliedLandlock, String> {
    apply_paths(profile.write_allow_roots())
}

pub(super) fn probe() -> Result<AppliedLandlock, String> {
    apply_paths(vec![Path::new("/")])
}

fn apply_paths(paths: Vec<&Path>) -> Result<AppliedLandlock, String> {
    // IoctlDev is not a pathname write and would break PTY/device use unless
    // `/dev` were also writable. Every filesystem mutation right through V3 is
    // handled, while BestEffort lets older supported kernels report degradation.
    let write_access = AccessFs::from_write(TARGET_ABI) & !AccessFs::IoctlDev;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(write_access)
        .map_err(|error| format!("failed to configure Landlock write access: {error}"))?
        .create()
        .map_err(|error| format!("failed to create Landlock ruleset: {error}"))?;

    for path in paths {
        let path_fd = PathFd::new(path)
            .map_err(|error| format!("failed to open Landlock path {}: {error}", path.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(path_fd, write_access))
            .map_err(|error| {
                format!(
                    "failed to grant Landlock writes beneath {}: {error}",
                    path.display()
                )
            })?;
    }

    let status = ruleset
        .restrict_self()
        .map_err(|error| format!("failed to restrict process with Landlock: {error}"))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("Landlock ruleset was not enforced".to_string());
    }

    let effective_abi = match status.landlock {
        LandlockStatus::Available { effective_abi, .. } => effective_abi,
        LandlockStatus::NotEnabled => return Err("Landlock is disabled by the kernel".to_string()),
        LandlockStatus::NotImplemented => {
            return Err("Landlock is not implemented by this kernel".to_string());
        }
    };

    Ok(AppliedLandlock {
        effective_abi,
        partially_enforced: status.ruleset == RulesetStatus::PartiallyEnforced,
    })
}

pub(super) fn abi_label(abi: ABI) -> String {
    if abi == ABI::Unsupported {
        "unsupported".to_string()
    } else {
        format!("V{abi}")
    }
}
