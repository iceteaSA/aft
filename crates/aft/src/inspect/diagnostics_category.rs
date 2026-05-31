use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::job::{InspectSnapshot, JobOutcome, JobScope};
use crate::config::Config;
use crate::context::AppContext;
use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use crate::lsp::manager::{
    EnsureServerOutcomes, PullFileOutcome, PullFileResult, ServerAttemptResult,
};
use crate::lsp::registry::servers_for_file;
use crate::lsp::roots::ServerKey;
use crate::lsp::tsconfig_membership::TsconfigMembershipCache;

/// How long a SCOPED diagnostics pull waits for the LSP server to become ready
/// and publish before reporting `pending`. Only the scoped (active-pull) path
/// uses this — no-scope warm reads never wait.
///
/// 1s was too short for a cold language server: `ensure_server_for_file_detailed`
/// spawns the server asynchronously, so the first scoped call on a fresh bridge
/// almost always hit the deadline before the server finished initializing and
/// returned `pending`, forcing the agent to re-run. When an agent explicitly
/// scopes to a file it is asking "what's wrong with this" — it should get the
/// answer in one call. 8s covers typical tsserver/rust-analyzer cold start while
/// staying well under the 30s bridge transport timeout. The wait is bounded and
/// only the FIRST scoped call per server pays the cold-start cost (subsequent
/// calls hit a warm server). Tradeoff: diagnostics run on the single-threaded
/// dispatch loop (the LSP manager is `!Send`), so this wait blocks other requests
/// on the same bridge for its duration — acceptable because it is bounded and
/// cold-start-only. A genuinely slow/unresponsive server still falls back to an
/// honest `pending` at the deadline. For a directory scope the budget is shared
/// across files, so the first cold file warms the server and the rest resolve
/// within the remaining budget (or are reported truncated, honestly).
const INSPECT_DIAGNOSTICS_DEADLINE: Duration = Duration::from_secs(8);
const SCOPED_FILE_CAP: usize = 200;

#[derive(Default)]
struct DiagnosticsCollection {
    diagnostics: Vec<StoredDiagnostic>,
    server_ran: bool,
    servers_pending: BTreeSet<String>,
    servers_not_installed: BTreeSet<String>,
    scope_truncated: bool,
    /// Number of scoped files whose extension has NO registered LSP server.
    /// These files will never produce diagnostics — distinct from "pending"
    /// (a server is running but hasn't reported yet). Without this, a scope of
    /// only unsupported file types reported `status: "pending"` forever,
    /// implying results were still coming when none ever would.
    files_without_server: usize,
}

/// Main-thread implementation for the `diagnostics` inspect category.
///
/// The LSP manager is owned by `AppContext` behind a `RefCell`, so this category
/// must never be dispatched through the rayon inspect worker pool. `handle_inspect`
/// calls this directly, alongside the Tier-1 reads, while Tier-2 categories keep
/// using the cache/worker path.
pub(crate) fn run_diagnostics_category(
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    scope_was_provided: bool,
) -> JobOutcome {
    let collection = if scope_was_provided {
        collect_scoped_diagnostics(ctx, snapshot, scope)
    } else {
        collect_warm_working_set(ctx, snapshot)
    };

    JobOutcome::Fresh {
        payload: collection.into_payload(snapshot),
    }
}

fn collect_warm_working_set(ctx: &AppContext, snapshot: &InspectSnapshot) -> DiagnosticsCollection {
    let mut collection = DiagnosticsCollection::default();
    let mut tsconfig_membership = TsconfigMembershipCache::new();
    {
        let mut lsp = ctx.lsp();
        // No-scope inspect is intentionally cheap: drain already queued LSP
        // events, then read only the warm diagnostics store. It does not open
        // files or spawn servers.
        lsp.drain_events();
        collection.server_ran = lsp.has_any_diagnostic_reports();
        if !collection.server_ran {
            collection.servers_pending.extend(
                lsp.active_server_keys()
                    .into_iter()
                    .map(|key| server_id(&key)),
            );
        }
        collection.diagnostics = lsp.get_all_diagnostics().into_iter().cloned().collect();
    }

    collection.diagnostics.retain(|diagnostic| {
        diagnostic.file.starts_with(&snapshot.project_root)
            && !tsconfig_membership.should_skip_diagnostics(&diagnostic.file)
    });
    collection.sort_and_dedup();
    collection
}

fn collect_scoped_diagnostics(
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    scope: &JobScope,
) -> DiagnosticsCollection {
    let deadline = Instant::now() + INSPECT_DIAGNOSTICS_DEADLINE;
    let config = ctx.config().clone();
    let mut tsconfig_membership = TsconfigMembershipCache::new();
    let scoped = scoped_lsp_files(snapshot, scope, &config, &mut tsconfig_membership);
    let files = scoped.files;
    let mut collection = DiagnosticsCollection {
        scope_truncated: scoped.truncated,
        files_without_server: scoped.explicit_files_without_server,
        ..DiagnosticsCollection::default()
    };

    for file in files {
        if Instant::now() >= deadline {
            collection.scope_truncated = true;
            break;
        }
        collect_scoped_file(ctx, &config, &file, deadline, &mut collection);
    }

    collection.diagnostics =
        scoped_warm_diagnostics(ctx, snapshot, scope, &mut tsconfig_membership);
    collection.sort_and_dedup();
    collection
}

fn collect_scoped_file(
    ctx: &AppContext,
    config: &Config,
    file: &Path,
    deadline: Instant,
    collection: &mut DiagnosticsCollection,
) {
    let canonical = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let outcomes: EnsureServerOutcomes = {
        let mut lsp = ctx.lsp();
        lsp.ensure_server_for_file_detailed(&canonical, config)
    };

    record_attempt_gaps(&outcomes, collection);
    if outcomes.only_inapplicable_root_markers() {
        // Every server registered for this file's extension failed the root
        // marker check (e.g. a `.ts` file in a project with no `.oxlintrc.json`
        // for oxlint). No applicable server will ever answer for this file, so
        // count it as a no-server file — otherwise the status falls through to
        // "pending" forever even after every truly-applicable server answered.
        collection.files_without_server += 1;
        return;
    }
    if outcomes.no_server_registered() || outcomes.successful.is_empty() {
        // No-server files are already excluded from the candidate set by
        // scoped_lsp_files (which counts explicit file-roots into
        // files_without_server); reaching here means the server exists but
        // isn't ready, which record_attempt_gaps already tracked.
        return;
    }

    let pre_push_snapshot = {
        let lsp = ctx.lsp();
        lsp.snapshot_pre_edit_state(&canonical)
    };

    let pull_results = {
        let mut lsp = ctx.lsp();
        match lsp.pull_file_diagnostics(&canonical, config) {
            Ok(results) => results,
            Err(err) => {
                crate::slog_warn!(
                    "[inspect:diagnostics] pull_file_diagnostics failed for {}: {err}",
                    canonical.display()
                );
                for key in &outcomes.successful {
                    collection.servers_pending.insert(server_id(key));
                }
                Vec::new()
            }
        }
    };

    let push_fallback_servers =
        record_pull_results(&outcomes.successful, &pull_results, collection);
    if push_fallback_servers.is_empty() {
        return;
    }

    if Instant::now() < deadline {
        let mut lsp = ctx.lsp();
        let _ = lsp.wait_for_file_diagnostics(&canonical, config, deadline);
    }

    let lsp = ctx.lsp();
    for key in push_fallback_servers {
        let pre = pre_push_snapshot.get(&key).copied().unwrap_or_default();
        if lsp.diagnostic_entry_is_fresh_for_document(&canonical, &key, pre)
            || lsp.has_diagnostic_report_for_server_file(&key, &canonical)
        {
            collection.server_ran = true;
        } else {
            collection.servers_pending.insert(server_id(&key));
        }
    }
}

fn record_attempt_gaps(outcomes: &EnsureServerOutcomes, collection: &mut DiagnosticsCollection) {
    for attempt in &outcomes.attempts {
        match &attempt.result {
            ServerAttemptResult::Ok { .. } => {}
            ServerAttemptResult::BinaryNotInstalled { .. } => {
                collection
                    .servers_not_installed
                    .insert(attempt.server_id.clone());
            }
            ServerAttemptResult::SpawnFailed { .. } => {
                collection
                    .servers_not_installed
                    .insert(attempt.server_id.clone());
            }
            ServerAttemptResult::NoRootMarker { .. } => {
                // The server is registered for this file's extension but none of
                // its root markers exist in the project (e.g. oxlint registered
                // for `.ts` with no `.oxlintrc.json`). That's a filesystem fact
                // that never changes mid-scan — the server simply does not apply
                // to this project. It is NOT "pending" (results are never
                // coming), so treating it as a gap would leave scoped diagnostics
                // reporting `pending` forever even after every applicable server
                // answered. Ignore it. The all-not-applicable edge (a file whose
                // only registered servers all lack root markers) is handled by
                // the caller, which counts it into `files_without_server`.
            }
        }
    }
}

fn record_pull_results(
    expected_servers: &[ServerKey],
    pull_results: &[PullFileResult],
    collection: &mut DiagnosticsCollection,
) -> Vec<ServerKey> {
    let mut push_fallback_servers = Vec::new();

    for key in expected_servers {
        let Some(result) = pull_results.iter().find(|result| result.server_key == *key) else {
            collection.servers_pending.insert(server_id(key));
            continue;
        };

        match &result.outcome {
            PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged => {
                collection.server_ran = true;
            }
            PullFileOutcome::PullNotSupported => {
                push_fallback_servers.push(key.clone());
            }
            PullFileOutcome::RequestFailed { reason } if request_failure_needs_push(reason) => {
                push_fallback_servers.push(key.clone());
            }
            PullFileOutcome::PartialNotSupported | PullFileOutcome::RequestFailed { .. } => {
                collection.servers_pending.insert(server_id(key));
            }
        }
    }

    push_fallback_servers
}

fn request_failure_needs_push(reason: &str) -> bool {
    reason == "no_cache_for_unchanged" || reason.starts_with("pull_rejected_push_fallback:")
}

fn scoped_warm_diagnostics(
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    tsconfig_membership: &mut TsconfigMembershipCache,
) -> Vec<StoredDiagnostic> {
    let roots = if scope.roots().is_empty() {
        vec![snapshot.project_root.clone()]
    } else {
        scope.roots().to_vec()
    };

    let lsp = ctx.lsp();
    roots
        .iter()
        .flat_map(|root| {
            if root.is_file() {
                lsp.get_diagnostics_for_file(root)
            } else {
                lsp.get_diagnostics_for_directory(root)
            }
        })
        .filter(|diagnostic| {
            scope.contains(&diagnostic.file)
                && diagnostic.file.starts_with(&snapshot.project_root)
                && !tsconfig_membership.should_skip_diagnostics(&diagnostic.file)
        })
        .cloned()
        .collect()
}

struct ScopedLspFiles {
    files: Vec<PathBuf>,
    truncated: bool,
    /// Count of explicit file-roots in the scope that have no registered LSP
    /// server. Directory walks intentionally skip non-code files silently
    /// (you don't want a `.md` in a walked dir flagged), but a scope that names
    /// a specific file we cannot diagnose is a real "no server" signal the
    /// agent must see — otherwise the status reads "pending" forever.
    explicit_files_without_server: usize,
}

fn scoped_lsp_files(
    snapshot: &InspectSnapshot,
    scope: &JobScope,
    config: &Config,
    tsconfig_membership: &mut TsconfigMembershipCache,
) -> ScopedLspFiles {
    let roots = if scope.roots().is_empty() {
        vec![snapshot.project_root.clone()]
    } else {
        scope.roots().to_vec()
    };

    let mut files = BTreeSet::new();
    let mut truncated = false;
    let mut explicit_files_without_server = 0usize;
    for root in roots {
        if root.is_file() {
            if tsconfig_membership.should_skip_diagnostics(&root) {
                continue;
            }
            if servers_for_file(&root, config).is_empty() {
                explicit_files_without_server += 1;
                continue;
            }
            files.insert(std::fs::canonicalize(&root).unwrap_or(root));
            continue;
        }

        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(true)
            .add_custom_ignore_filename(".aftignore")
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | ".turbo"
                )
            })
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let path = entry.path();
            if tsconfig_membership.should_skip_diagnostics(path) {
                continue;
            }
            if servers_for_file(path, config).is_empty() {
                continue;
            }
            files.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
            if files.len() >= SCOPED_FILE_CAP {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }

    ScopedLspFiles {
        files: files.into_iter().collect(),
        truncated,
        explicit_files_without_server,
    }
}

impl DiagnosticsCollection {
    fn into_payload(mut self, snapshot: &InspectSnapshot) -> Value {
        self.sort_and_dedup();
        let (errors, warnings, info, hints) = severity_counts(&self.diagnostics);
        let complete = self.server_ran
            && self.servers_pending.is_empty()
            && self.servers_not_installed.is_empty()
            && !self.scope_truncated;
        let status = diagnostics_status(
            complete,
            self.scope_truncated,
            &self.servers_not_installed,
            &self.servers_pending,
            self.files_without_server,
        );
        let items = self
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic_item(snapshot, diagnostic))
            .collect::<Vec<_>>();

        serde_json::json!({
            "errors": errors,
            "warnings": warnings,
            "info": info,
            "hints": hints,
            "server_ran": self.server_ran,
            "complete": complete,
            "status": status,
            "servers_pending": self.servers_pending.into_iter().collect::<Vec<_>>(),
            "servers_not_installed": self.servers_not_installed.into_iter().collect::<Vec<_>>(),
            "files_without_server": self.files_without_server,
            "items": items,
        })
    }

    fn sort_and_dedup(&mut self) {
        self.diagnostics.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
                .then(left.end_line.cmp(&right.end_line))
                .then(left.end_column.cmp(&right.end_column))
                .then(left.severity.as_str().cmp(right.severity.as_str()))
                .then(left.message.cmp(&right.message))
                .then(left.source.cmp(&right.source))
        });
        self.diagnostics.dedup_by(|left, right| {
            left.file == right.file
                && left.line == right.line
                && left.column == right.column
                && left.end_line == right.end_line
                && left.end_column == right.end_column
                && left.severity == right.severity
                && left.message == right.message
                && left.source == right.source
        });
    }
}

fn diagnostics_status(
    complete: bool,
    scope_truncated: bool,
    servers_not_installed: &BTreeSet<String>,
    servers_pending: &BTreeSet<String>,
    files_without_server: usize,
) -> Option<&'static str> {
    if complete {
        None
    } else if scope_truncated || !servers_not_installed.is_empty() {
        // Bounded gap: truncated scope, or a server exists but isn't installed.
        Some("incomplete")
    } else if !servers_pending.is_empty() {
        // A registered server is running but hasn't reported yet — results are
        // genuinely still coming.
        Some("pending")
    } else if files_without_server > 0 {
        // No registered server matched the scoped file type(s). Nothing will
        // ever arrive — report that honestly instead of "pending" forever.
        Some("no_server")
    } else {
        // Not complete, but no pending server and no unsupported files either:
        // treat as still-settling rather than asserting completeness.
        Some("pending")
    }
}

fn severity_counts(diagnostics: &[StoredDiagnostic]) -> (usize, usize, usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    let mut info = 0;
    let mut hints = 0;

    for diagnostic in diagnostics {
        match diagnostic.severity {
            DiagnosticSeverity::Error => errors += 1,
            DiagnosticSeverity::Warning => warnings += 1,
            DiagnosticSeverity::Information => info += 1,
            DiagnosticSeverity::Hint => hints += 1,
        }
    }

    (errors, warnings, info, hints)
}

fn diagnostic_item(snapshot: &InspectSnapshot, diagnostic: &StoredDiagnostic) -> Value {
    serde_json::json!({
        "file": display_path(snapshot, &diagnostic.file),
        "line": diagnostic.line,
        "column": diagnostic.column,
        "severity": diagnostic.severity.as_str(),
        "message": diagnostic.message,
        "source": diagnostic.source.as_deref().unwrap_or("lsp"),
    })
}

fn display_path(snapshot: &InspectSnapshot, path: &Path) -> String {
    path.strip_prefix(&snapshot.project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn server_id(key: &ServerKey) -> String {
    key.kind.id_str().to_string()
}
