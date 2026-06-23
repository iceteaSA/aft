//! Subc-mode local config read + wire-tier trust capping (subc edge only).
//!
//! Under the daemon there is no harness plugin to read the user's `aft.jsonc`
//! and relay it as wire tiers, so AFT reads its own config off disk from the
//! CortexKit config home. That on-disk read is a TRUSTED-LOCAL origin: AFT read
//! it itself, never over the wire, so its `user` tier is honored in full even
//! under an untrusted `mcp:*` front.
//!
//! Wire-relayed inline tiers, by contrast, are capped by the fronting harness:
//! an `mcp:*` (or otherwise un-vetted) front's `user` tier is downgraded to
//! `project` so the resolver's trust boundary strips its privileged fields. The
//! first-party fronts (`runner`, and the legacy `opencode`/`pi` plugins) keep
//! their tier labels.
//!
//! The capping is a label rewrite here at the subc edge — where the origin is
//! known from the code path — so `config_resolve` stays purely label-trusting.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::config_resolve::ConfigTier;
use crate::harness::Harness;

/// CortexKit user config home: `$XDG_CONFIG_HOME/cortexkit/aft.jsonc`, falling
/// back to `~/.config/cortexkit/aft.jsonc`. Matches the shared CortexKit
/// convention (`~/.config/cortexkit/<module>.jsonc`) alongside `subc.jsonc` and
/// `mcp.jsonc`. Pure over its env inputs so it is testable without mutating
/// process-global env vars (which race under the parallel test runner).
fn user_config_path_from(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg_config_home
        .map(PathBuf::from)
        // An unset-but-empty `$XDG_CONFIG_HOME` ("") is not absolute → fall back
        // to `~/.config`, per the XDG Base Directory spec.
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("cortexkit").join("aft.jsonc"))
}

/// Resolve the production CortexKit user config path from the process env. This
/// is the only env-reading entry; it is called once at the subc boundary and the
/// resolved path is threaded down, so the per-bind composition stays pure (and
/// the integration tests inject a path instead of mutating env, which races).
pub fn cortexkit_user_config_path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    user_config_path_from(xdg.as_deref(), home.as_deref())
}

/// CortexKit project config: `<root>/.cortexkit/aft.jsonc`.
fn cortexkit_project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".cortexkit").join("aft.jsonc")
}

/// Read the user + project config files into raw tiers. Pure over its path
/// inputs (no env, no fixed locations) so it is directly testable. Mirrors the
/// TS `readConfigTiers`: push `{tier, source, doc}` with the RAW file content as
/// `doc` (the resolver's `parse_tier` strips JSONC), skipping any missing or
/// unreadable file silently.
fn read_tiers_from(user_config_path: Option<&Path>, project_config_path: &Path) -> Vec<ConfigTier> {
    let mut tiers = Vec::new();

    if let Some(user_path) = user_config_path {
        if let Ok(doc) = std::fs::read_to_string(user_path) {
            tiers.push(ConfigTier {
                tier: "user".to_string(),
                source: user_path.to_string_lossy().into_owned(),
                doc,
            });
        }
    }

    if let Ok(doc) = std::fs::read_to_string(project_config_path) {
        tiers.push(ConfigTier {
            tier: "project".to_string(),
            source: project_config_path.to_string_lossy().into_owned(),
            doc,
        });
    }

    tiers
}

/// Read the CortexKit config home (user) + project config for a subc bind. These
/// tiers are TRUSTED-LOCAL origin and keep their labels. `user_config_path` is
/// resolved once at the subc boundary (`cortexkit_user_config_path`) and passed
/// in, keeping this pure for testing.
pub fn read_local_cortexkit_config_tiers(
    user_config_path: Option<&Path>,
    project_root: &Path,
) -> Vec<ConfigTier> {
    read_tiers_from(
        user_config_path,
        &cortexkit_project_config_path(project_root),
    )
}

/// True only for first-party fronts whose relayed tier labels are trusted as-is.
/// `mcp:*` and any unparseable harness fall through to capping (fail-safe toward
/// less privilege).
fn front_is_trusted(harness: Option<&Harness>) -> bool {
    matches!(
        harness,
        Some(Harness::Opencode | Harness::Pi | Harness::Runner)
    )
}

/// Cap wire-relayed inline tiers by the fronting harness's trust. For an
/// un-vetted front (`mcp:*` or unknown), a `user` tier on the wire is downgraded
/// to `project` so the resolver strips its privileged fields — an un-vetted agent
/// must never inject user-tier config over the wire. Returns the capped tiers and
/// whether any downgrade occurred (for warning surfacing).
fn cap_wire_tiers(wire: Vec<ConfigTier>, harness: Option<&Harness>) -> (Vec<ConfigTier>, bool) {
    if front_is_trusted(harness) {
        return (wire, false);
    }
    let mut downgraded = false;
    let capped = wire
        .into_iter()
        .map(|mut tier| {
            if tier.tier == "user" {
                tier.tier = "project".to_string();
                downgraded = true;
            }
            tier
        })
        .collect();
    (capped, downgraded)
}

/// Compose the final tier list for a subc RouteBind: the AFT-read local config
/// (trusted-local origin) as the base, then the harness-capped wire tiers
/// refining on top. Later tiers override earlier ones for shared fields, so a
/// trusted front's relayed session config refines the on-disk config, while an
/// un-vetted front can only touch project-safe fields. Returns the composed
/// tiers and whether any wire `user` tier was downgraded (for warning).
pub fn compose_route_bind_tiers(
    user_config_path: Option<&Path>,
    project_root: &Path,
    wire: Vec<ConfigTier>,
    harness: Option<&Harness>,
) -> (Vec<ConfigTier>, bool) {
    let mut tiers = read_local_cortexkit_config_tiers(user_config_path, project_root);
    let (capped_wire, downgraded) = cap_wire_tiers(wire, harness);
    tiers.extend(capped_wire);
    (tiers, downgraded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config_resolve::resolve_config_onto;

    fn tier(tier: &str, doc: &str) -> ConfigTier {
        ConfigTier {
            tier: tier.to_string(),
            source: "wire".to_string(),
            doc: doc.to_string(),
        }
    }

    // ---- path resolution (pure, no env mutation) ----

    #[test]
    fn user_path_prefers_absolute_xdg_config_home() {
        // The XDG base must be absolute ON THE HOST OS: `/xdg/cfg` is absolute on
        // Unix but NOT on Windows (which needs a drive letter), and the production
        // filter correctly ignores a non-absolute XDG per the XDG spec. Build the
        // expected path the same way production does so the separator matches.
        let xdg = if cfg!(windows) {
            r"C:\xdg\cfg"
        } else {
            "/xdg/cfg"
        };
        let home = if cfg!(windows) {
            r"C:\home\u"
        } else {
            "/home/u"
        };
        let path = user_config_path_from(Some(OsStr::new(xdg)), Some(OsStr::new(home)));
        let expected = PathBuf::from(xdg).join("cortexkit").join("aft.jsonc");
        assert_eq!(path, Some(expected));
    }

    #[test]
    fn user_path_falls_back_to_home_config_when_xdg_unset() {
        let path = user_config_path_from(None, Some(OsStr::new("/home/u")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/u/.config/cortexkit/aft.jsonc"))
        );
    }

    #[test]
    fn user_path_treats_empty_xdg_as_unset() {
        let path = user_config_path_from(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/u/.config/cortexkit/aft.jsonc"))
        );
    }

    #[test]
    fn user_path_none_when_no_home_and_no_xdg() {
        assert_eq!(user_config_path_from(None, None), None);
    }

    // ---- local file read ----

    #[test]
    fn reads_user_and_project_with_raw_jsonc_docs() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user-aft.jsonc");
        let project = dir.path().join("project-aft.jsonc");
        // Comments preserved in the raw doc — the resolver strips JSONC.
        std::fs::write(&user, "{\n  // user\n  \"search_index\": true\n}").unwrap();
        std::fs::write(&project, "{ \"semantic_search\": false }").unwrap();

        let tiers = read_tiers_from(Some(&user), &project);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].tier, "user");
        assert!(tiers[0].doc.contains("// user"));
        assert_eq!(tiers[1].tier, "project");
        assert_eq!(tiers[1].source, project.to_string_lossy());
    }

    #[test]
    fn missing_files_yield_no_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let tiers = read_tiers_from(
            Some(&dir.path().join("nope-user.jsonc")),
            &dir.path().join("nope-project.jsonc"),
        );
        assert!(tiers.is_empty());
    }

    // ---- the security property: mcp:* cannot inject user-tier privilege ----

    const PRIVILEGED_DOC: &str = r#"{ "semantic": { "api_key_env": "SECRET_KEY" } }"#;

    #[test]
    fn mcp_front_user_tier_is_capped_and_privileged_field_dropped() {
        let (capped, downgraded) = cap_wire_tiers(
            vec![tier("user", PRIVILEGED_DOC)],
            Some(&Harness::Mcp {
                client: "claude-code".to_string(),
            }),
        );
        assert!(downgraded, "mcp user tier must be downgraded");
        assert_eq!(capped[0].tier, "project");

        let mut base = Config::default();
        let dropped = resolve_config_onto(&capped, &mut base);
        assert!(
            dropped.iter().any(|d| d.key == "semantic.api_key_env"),
            "capped privileged field must be dropped by the resolver"
        );
        assert!(
            base.semantic.api_key_env.is_none(),
            "dropped field must NOT reach the resolved config"
        );
    }

    #[test]
    fn runner_front_user_tier_is_trusted_and_privileged_field_survives() {
        let (capped, downgraded) =
            cap_wire_tiers(vec![tier("user", PRIVILEGED_DOC)], Some(&Harness::Runner));
        assert!(!downgraded, "runner user tier must not be downgraded");
        assert_eq!(capped[0].tier, "user");

        let mut base = Config::default();
        let dropped = resolve_config_onto(&capped, &mut base);
        assert!(
            !dropped.iter().any(|d| d.key == "semantic.api_key_env"),
            "trusted user-tier field must not be dropped"
        );
        assert_eq!(base.semantic.api_key_env.as_deref(), Some("SECRET_KEY"));
    }

    #[test]
    fn unknown_front_is_capped_like_mcp() {
        let (_capped, downgraded) = cap_wire_tiers(vec![tier("user", PRIVILEGED_DOC)], None);
        assert!(
            downgraded,
            "unparseable/unknown front must cap conservatively"
        );
    }

    #[test]
    fn project_tier_passes_through_uncapped_for_any_front() {
        // A project tier is already untrusted; capping is a no-op on it.
        let (capped, downgraded) = cap_wire_tiers(
            vec![tier("project", PRIVILEGED_DOC)],
            Some(&Harness::Mcp {
                client: "x".to_string(),
            }),
        );
        assert!(!downgraded);
        assert_eq!(capped[0].tier, "project");
    }
}
