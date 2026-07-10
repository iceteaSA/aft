use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::unbounded;
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use crate::config::{Config, SemanticBackendConfig};
use crate::context::{
    AppContext, CallgraphStoreAccess, ConfigureMaintenanceJob, SemanticIndexEvent,
    SemanticIndexStatus, SemanticRefreshEvent, SemanticRefreshRequest, SemanticRefreshWorkerSlot,
};
use crate::harness::Harness;
use crate::log_ctx;
use crate::lsp::registry::{resolve_lsp_binary, servers_for_file, ServerKind};
use crate::parser::{detect_language, LangId, SharedSymbolCache};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, current_git_head, resolve_cache_dir_with_key,
    walk_project_files_bounded_matching, CacheLock, SearchIndex,
};
use crate::semantic_index::{is_semantic_indexed_extension, SemanticIndex, SemanticIndexLock};
use crate::watcher_filter::{self, WatcherFilterConfig, WatcherThreadHandle};
use crate::{slog_debug, slog_info, slog_warn};

static WATCHER_GENERATION: AtomicU64 = AtomicU64::new(0);

static SEMANTIC_STALE_GENERATION_DISCARDS: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub fn reset_semantic_stale_generation_discards_for_test() {
    SEMANTIC_STALE_GENERATION_DISCARDS.store(0, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn semantic_stale_generation_discards_for_test() -> usize {
    SEMANTIC_STALE_GENERATION_DISCARDS.load(Ordering::SeqCst)
}

const SEMANTIC_REFRESH_QUIET_WINDOW_MS: u64 = 250;
const SEMANTIC_REFRESH_MAX_BATCH_PATHS: usize = 50;

#[cfg(test)]
static CONFIGURE_REPLAY_SESSION_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn sleep_from_env_ms(name: &str) {
    let Some(delay) = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|delay| *delay > 0)
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay));
}

#[cfg(not(test))]
fn sleep_from_env_ms(_name: &str) {}

fn delay_configure_deferred_walk_for_test() {
    sleep_from_env_ms("AFT_TEST_CONFIGURE_DEFERRED_WALK_DELAY_MS");
}

#[cfg(test)]
fn signal_configure_deferred_walk_start_for_test() {
    let Some(path) = std::env::var_os("AFT_TEST_CONFIGURE_DEFERRED_WALK_START_FILE") else {
        return;
    };
    fs::write(path, "started\n").expect("write deferred walk start signal");
}

#[cfg(not(test))]
fn signal_configure_deferred_walk_start_for_test() {}

#[cfg(test)]
fn run_configure_deferred_walk_synchronously_for_test() -> bool {
    std::env::var_os("AFT_TEST_CONFIGURE_FORCE_SYNCHRONOUS_DEFERRED_WALK").is_some()
}

#[cfg(not(test))]
fn run_configure_deferred_walk_synchronously_for_test() -> bool {
    false
}

fn delay_configure_deferred_maintenance_for_test() {
    sleep_from_env_ms("AFT_TEST_CONFIGURE_DEFERRED_MAINTENANCE_DELAY_MS");
}

#[cfg(test)]
fn reset_configure_replay_session_calls_for_test() {
    CONFIGURE_REPLAY_SESSION_CALLS.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn configure_replay_session_calls_for_test() -> u64 {
    CONFIGURE_REPLAY_SESSION_CALLS.load(Ordering::SeqCst)
}

fn resolve_home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

fn create_project_watcher(
    root_path: PathBuf,
    extra_watch_paths: Vec<PathBuf>,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root_path, RecursiveMode::Recursive)?;
    for path in extra_watch_paths {
        if path.exists() {
            watcher.watch(&path, RecursiveMode::NonRecursive)?;
        }
    }
    Ok(watcher)
}

fn external_ignore_watch_paths(ctx: &AppContext, root_path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
        if global_ignore.is_file() {
            paths.push(global_ignore);
        }
    }
    let info_exclude = ctx
        .git_common_dir()
        .unwrap_or_else(|| root_path.join(".git"))
        .join("info")
        .join("exclude");
    if info_exclude.is_file() {
        paths.push(info_exclude);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn install_project_watcher_with<W, E, F>(
    ctx: &AppContext,
    root_path: &Path,
    extra_watch_paths: Vec<PathBuf>,
    attach: F,
) where
    W: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(PathBuf, Vec<PathBuf>, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>
        + Send
        + 'static,
{
    // Stop the previous watcher/filter runtime before replacing it
    // (re-configure). The OS watcher itself is owned by that runtime thread, so
    // shutting it down here drops the recursive watch and prevents stale filter
    // threads from accumulating across reconfigures.
    ctx.stop_watcher_runtime();

    let generation = WATCHER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let (dispatch_tx, dispatch_rx) = watcher_filter::watcher_dispatch_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);

    let root_path = root_path.to_path_buf();
    let filter_config = WatcherFilterConfig::new(root_path.clone(), ctx.git_common_dir());
    let shared_gitignore = ctx.shared_gitignore();
    let gitignore_generation = ctx.gitignore_generation();
    let session_id_for_bg = log_ctx::current_session();
    let sync_start = file_watcher_sync_start_for_test();
    let (start_tx, start_rx) = mpsc::channel::<Result<(), String>>();
    let start_tx = sync_start.then_some(start_tx);

    let join = thread::spawn(move || {
        log_ctx::with_session(session_id_for_bg, || {
            let attach_with_start =
                move |root: PathBuf,
                      extra_watch_paths: Vec<PathBuf>,
                      tx: mpsc::Sender<notify::Result<notify::Event>>| {
                    let result = attach(root, extra_watch_paths, tx);
                    if let Some(start_tx) = start_tx {
                        let _ = start_tx.send(
                            result
                                .as_ref()
                                .map(|_| ())
                                .map_err(|error| format!("watcher init failed: {error}")),
                        );
                    }
                    result
                };
            watcher_filter::run_watcher_thread(
                filter_config,
                extra_watch_paths,
                shared_gitignore,
                gitignore_generation,
                dispatch_tx,
                thread_shutdown,
                attach_with_start,
            );
        });
    });

    ctx.install_watcher_runtime(dispatch_rx, WatcherThreadHandle::new(shutdown, join));

    if sync_start {
        match start_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => slog_warn!("{error}"),
            Err(error) => slog_warn!(
                "timed out waiting for watcher startup for generation {generation}: {error}"
            ),
        }
    }
}

fn file_watcher_sync_start_for_test() -> bool {
    std::env::var("AFT_TEST_SYNC_FILE_WATCHER_START").is_ok_and(|value| value == "1")
}

/// Harness-only seam: when `AFT_TEST_DISABLE_FILE_WATCHER=1`, `configure` skips
/// installing the OS file watcher entirely. The integration suite spawns ~600
/// `aft` processes; under that concurrent load the macOS FSEvents `watch()` call
/// probabilistically hangs (it never returns and never delivers events for a
/// fraction of processes), which flaked any test waiting on watcher-driven
/// invalidation. The vast majority of tests mutate files through AFT's own tools
/// (which invalidate caches directly, not via the watcher), so they need no
/// watcher at all. The test helper disables it by default; the dedicated
/// `watcher_integration` test binary (which runs alone, with no concurrent load)
/// opts back in. Never set in production.
fn file_watcher_disabled_for_test() -> bool {
    std::env::var("AFT_TEST_DISABLE_FILE_WATCHER").is_ok_and(|value| value == "1")
}

fn install_project_watcher(ctx: &AppContext, root_path: &Path) {
    if file_watcher_disabled_for_test() {
        ctx.stop_watcher_runtime();
        return;
    }
    let extra_watch_paths = external_ignore_watch_paths(ctx, root_path);
    install_project_watcher_with(ctx, root_path, extra_watch_paths, create_project_watcher);
}

/// Restore the watcher after an idle root released its runtime. Artifact stores
/// are lazy by design, so the first request is the natural point to reattach
/// external-change invalidation as well.
pub(crate) fn ensure_project_watcher(ctx: &AppContext) {
    if ctx.watcher_runtime_active() {
        return;
    }
    let Some(root_path) = ctx.canonical_cache_root_opt() else {
        return;
    };
    if root_path.exists() {
        install_project_watcher(ctx, &root_path);
    }
}

/// Backoff for build-level retries when the embedding backend is unreachable.
/// Ramps 15s -> 30s -> 60s then holds at 60s. Keeps the retry cadence cheap
/// (the build re-walks files each attempt) while recovering within a minute of
/// the backend returning.
fn semantic_build_retry_backoff(attempt: usize) -> Duration {
    // Test seam: shrink the schedule to a fixed small interval so recovery
    // integration tests don't wait real 15s+ windows. Not a user-facing knob.
    if let Ok(raw) = std::env::var("AFT_SEMANTIC_RETRY_BACKOFF_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }
    const SCHEDULE_SECS: [u64; 3] = [15, 30, 60];
    let secs = SCHEDULE_SECS
        .get(attempt)
        .copied()
        .unwrap_or(*SCHEDULE_SECS.last().unwrap());
    Duration::from_secs(secs)
}

fn spawn_semantic_refresh_worker(
    project_root: PathBuf,
    mut index: SemanticIndex,
    mut model: crate::semantic_index::EmbeddingModel,
    max_batch_size: usize,
    max_files: usize,
    request_rx: crossbeam_channel::Receiver<SemanticRefreshRequest>,
    event_tx: crossbeam_channel::Sender<SemanticRefreshEvent>,
    session_id: Option<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            while let Ok(first_request) = request_rx.recv() {
                let mut paths = Vec::new();
                let mut corpus_requested = false;
                match first_request {
                    SemanticRefreshRequest::Files {
                        paths: request_paths,
                    } => {
                        paths.extend(request_paths);
                    }
                    SemanticRefreshRequest::Corpus => {
                        corpus_requested = true;
                    }
                }

                let mut disconnected = false;
                let quiet_window = Duration::from_millis(SEMANTIC_REFRESH_QUIET_WINDOW_MS);
                let mut deadline = Instant::now() + quiet_window;

                while !corpus_requested && paths.len() < SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match request_rx.recv_timeout(remaining) {
                        Ok(SemanticRefreshRequest::Files {
                            paths: request_paths,
                        }) => {
                            paths.extend(request_paths);
                            if paths.len() >= SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                                break;
                            }
                            deadline = Instant::now() + quiet_window;
                        }
                        Ok(SemanticRefreshRequest::Corpus) => {
                            paths.clear();
                            corpus_requested = true;
                            break;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if disconnected {
                    break;
                }

                if corpus_requested {
                    let mut current_files = match walk_semantic_project_files_bounded(
                        &project_root,
                        max_files,
                    ) {
                        Ok(files) => files,
                        Err(observed) => {
                            let error = format!(
                                "too many files (>{}) for semantic indexing (max {})",
                                max_files, max_files
                            );
                            slog_warn!(
                                "skipping semantic corpus refresh: more than {} files exceeds limit of {}. \
                                 Raise semantic.max_files or open a specific project directory.",
                                observed.saturating_sub(1),
                                max_files
                            );
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusFailed { error })
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                    };
                    current_files.sort();
                    current_files.dedup();
                    if current_files.len() > max_files {
                        let error = format!(
                            "too many files (>{}) for semantic indexing (max {})",
                            max_files, max_files
                        );
                        let _ = event_tx.send(SemanticRefreshEvent::CorpusFailed { error });
                        continue;
                    }
                    if event_tx
                        .send(SemanticRefreshEvent::CorpusStarted {
                            files: current_files.len(),
                        })
                        .is_err()
                    {
                        break;
                    }

                    let mut embed = |texts: Vec<String>| model.embed(texts);
                    let mut progress = |_done: usize, _total: usize| {};
                    match index.refresh_stale_files(
                        &project_root,
                        &current_files,
                        &mut embed,
                        max_batch_size,
                        &mut progress,
                    ) {
                        Ok(summary) => {
                            if !summary.is_noop() {
                                slog_info!(
                                    "semantic corpus refresh: {} changed, {} new, {} deleted, {} total processed",
                                    summary.changed,
                                    summary.added,
                                    summary.deleted,
                                    summary.total_processed,
                                );
                            }
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusCompleted {
                                    index: index.clone(),
                                    changed: summary.changed,
                                    added: summary.added,
                                    deleted: summary.deleted,
                                    total_processed: summary.total_processed,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            slog_warn!("semantic corpus refresh failed: {}", error);
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusFailed { error })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    continue;
                }

                paths.sort();
                paths.dedup();
                if paths.is_empty() {
                    continue;
                }

                if event_tx
                    .send(SemanticRefreshEvent::Started {
                        paths: paths.clone(),
                    })
                    .is_err()
                {
                    break;
                }

                let mut embed = |texts: Vec<String>| model.embed(texts);
                let mut progress = |_done: usize, _total: usize| {};
                match index.refresh_invalidated_files(
                    &project_root,
                    &paths,
                    &mut embed,
                    max_batch_size,
                    max_files,
                    &mut progress,
                ) {
                    Ok(update) => {
                        if !update.summary.is_noop() {
                            slog_info!(
                                "semantic refresh: {} changed, {} new, {} deleted, {} total processed",
                                update.summary.changed,
                                update.summary.added,
                                update.summary.deleted,
                                update.summary.total_processed,
                            );
                        }
                        if event_tx
                            .send(SemanticRefreshEvent::Completed {
                                added_entries: update.added_entries,
                                updated_metadata: update.updated_metadata,
                                completed_paths: update.completed_paths,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        slog_warn!(
                            "semantic refresh failed for {} file(s): {}",
                            paths.len(),
                            error
                        );
                        if event_tx
                            .send(SemanticRefreshEvent::Failed { paths, error })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    })
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    normalized
}

fn validate_storage_dir(raw: &str) -> Result<PathBuf, String> {
    let storage_dir = PathBuf::from(raw);
    if !storage_dir.is_absolute() {
        return Err("configure: storage_dir must be an absolute path".to_string());
    }

    let normalized = normalize_absolute_path(&storage_dir);
    if normalized
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("configure: storage_dir must not escape via '..' traversal".to_string());
    }

    Ok(normalized)
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn detect_worktree_bridge(ctx: &AppContext, project_root: &Path) -> (bool, Option<PathBuf>) {
    if std::env::var_os("AFT_TEST_ALLOW_WORKTREE_STORE_BUILD").is_some() {
        return (false, None);
    }
    if let Some(result) = ctx.cached_worktree_bridge(project_root) {
        return result;
    }

    // This is intentionally separate from artifact-cache-key memoization. The
    // cache key must validate the current HEAD, while worktree topology is
    // stable until the root's `.git` marker changes.
    #[cfg(test)]
    ctx.record_worktree_bridge_probe_spawn_for_test();
    let output = crate::effective_path::new_command("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output();
    let Ok(output) = output else {
        return (false, None);
    };
    if !output.status.success() {
        return (false, None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(git_dir) = lines.next().map(PathBuf::from) else {
        return (false, None);
    };
    let Some(common_dir) = lines.next().map(PathBuf::from) else {
        return (false, None);
    };
    let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    let is_worktree_bridge = git_dir != common_dir;
    ctx.cache_worktree_bridge(project_root, is_worktree_bridge, common_dir.clone());
    (is_worktree_bridge, Some(common_dir))
}

fn semantic_fingerprint_config_changed(
    previous: &SemanticBackendConfig,
    next: &SemanticBackendConfig,
) -> bool {
    previous.backend != next.backend
        || previous.model != next.model
        || previous.base_url != next.base_url
}

fn should_clear_failed_spawns(
    previous: &Config,
    next: &Config,
    equivalent_warm_config: bool,
) -> bool {
    !equivalent_warm_config
        || previous.lsp_paths_extra != next.lsp_paths_extra
        || previous.lsp_auto_install_binaries != next.lsp_auto_install_binaries
        || previous.lsp_inflight_installs != next.lsp_inflight_installs
}

fn workspace_manifest_fingerprint(project_root: &Path) -> String {
    let mut parts = Vec::new();
    push_manifest_fingerprint(&mut parts, project_root.join("package.json"));
    let packages_dir = project_root.join("packages");
    if let Ok(entries) = fs::read_dir(packages_dir) {
        let mut manifests = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("package.json"))
            .collect::<Vec<_>>();
        manifests.sort();
        for manifest in manifests {
            push_manifest_fingerprint(&mut parts, manifest);
        }
    }
    parts.join("|")
}

fn push_manifest_fingerprint(parts: &mut Vec<String>, path: PathBuf) {
    if let Ok(metadata) = fs::metadata(&path) {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
            .unwrap_or((0, 0));
        parts.push(format!(
            "{}:{}:{}:{}",
            path.display(),
            metadata.len(),
            modified.0,
            modified.1
        ));
    }
}

/// Parse the `lsp_paths_extra` config param: an array of absolute directory
/// paths the plugin wants AFT to search when resolving LSP binaries (used
/// for the auto-install cache, e.g.
/// `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`).
///
/// Rejects non-array values, non-string entries, empty strings, relative paths,
/// parent traversal, and existing paths that do not resolve to directories.
/// Non-existent paths are accepted silently — the resolver tolerates them and
/// falls through to the next candidate.
fn parse_lsp_paths_extra(value: &Value) -> Result<Vec<PathBuf>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "configure: lsp_paths_extra must be an array of strings".to_string())?;

    let mut paths = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("configure: lsp_paths_extra[{index}] must be a string"))?;
        if raw.is_empty() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not be empty"
            ));
        }
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must be an absolute path: {raw}"
            ));
        }
        if has_parent_component(&path) {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not contain '..' traversal: {raw}"
            ));
        }

        match std::fs::canonicalize(&path) {
            Ok(canonical) => {
                if has_parent_component(&canonical) {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] resolved path must not contain '..' traversal: {}",
                        canonical.display()
                    ));
                }
                if !canonical.is_dir() {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] must resolve to a directory: {}",
                        canonical.display()
                    ));
                }
                paths.push(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                paths.push(path);
            }
            Err(error) => {
                return Err(format!(
                    "configure: lsp_paths_extra[{index}] could not be resolved: {error}"
                ));
            }
        }
    }
    Ok(paths)
}

fn parse_string_set(
    value: &Value,
    field: &str,
) -> Result<std::collections::HashSet<String>, String> {
    let Some(entries) = value.as_array() else {
        return Err(format!("configure: {field} must be an array of strings"));
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry
                .as_str()
                .map(|value| value.to_string())
                .ok_or_else(|| format!("configure: {field}[{index}] must be a string"))
        })
        .collect()
}

fn is_custom_server(kind: &ServerKind) -> bool {
    matches!(kind, ServerKind::Custom(_))
}

fn lsp_missing_hint(binary: &str) -> String {
    crate::format::install_hint(binary)
}

fn lang_key(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => "typescript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
    }
}

fn has_project_config(project_root: Option<&Path>, filenames: &[&str]) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    filenames.iter().any(|file| root.join(file).exists())
}

fn has_pyproject_tool(project_root: Option<&Path>, tool_name: &str) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return false;
    }
    std::fs::read_to_string(pyproject)
        .map(|content| content.contains(&format!("[tool.{tool_name}]")))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct ConfigureToolCandidate {
    tool: String,
    source: String,
    required: bool,
}

fn configure_tool_candidate(tool: &str, source: &str, required: bool) -> ConfigureToolCandidate {
    ConfigureToolCandidate {
        tool: tool.to_string(),
        source: source.to_string(),
        required,
    }
}

fn explicit_formatter_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "biome" | "oxfmt" | "prettier" | "deno" | "ruff" | "black" | "rustfmt" | "goimports"
        | "gofmt" => {
            vec![configure_tool_candidate(name, "formatter config", true)]
        }
        _ => Vec::new(),
    }
}

fn explicit_checker_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "tsc" | "tsgo" | "cargo" | "go" | "biome" | "pyright" | "ruff" | "staticcheck" => {
            vec![configure_tool_candidate(name, "checker config", true)]
        }
        _ => Vec::new(),
    }
}

fn formatter_candidates(
    lang: LangId,
    config: &crate::config::Config,
) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.formatter.get(lang_key(lang)) {
        return explicit_formatter_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(
                project_root,
                &[".oxfmtrc.json", ".oxfmtrc.jsonc", "oxfmt.config.ts"],
            ) {
                vec![configure_tool_candidate("oxfmt", "oxfmt config", true)]
            } else if has_project_config(
                project_root,
                &[
                    ".prettierrc",
                    ".prettierrc.json",
                    ".prettierrc.yml",
                    ".prettierrc.yaml",
                    ".prettierrc.js",
                    ".prettierrc.cjs",
                    ".prettierrc.mjs",
                    ".prettierrc.toml",
                    "prettier.config.js",
                    "prettier.config.cjs",
                    "prettier.config.mjs",
                ],
            ) {
                vec![configure_tool_candidate(
                    "prettier",
                    "Prettier config",
                    true,
                )]
            } else if has_project_config(project_root, &["deno.json", "deno.jsonc"]) {
                vec![configure_tool_candidate("deno", "deno.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else if has_pyproject_tool(project_root, "black") {
                vec![configure_tool_candidate("black", "pyproject.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("rustfmt", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("goimports", "go.mod", false),
                    configure_tool_candidate("gofmt", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn checker_candidates(lang: LangId, config: &crate::config::Config) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.checker.get(lang_key(lang)) {
        return explicit_checker_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(project_root, &["tsconfig.json"]) {
                vec![configure_tool_candidate("tsc", "tsconfig.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["pyrightconfig.json"])
                || has_pyproject_tool(project_root, "pyright")
            {
                vec![configure_tool_candidate("pyright", "pyright config", true)]
            } else if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("cargo", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("staticcheck", "go.mod", false),
                    configure_tool_candidate("go", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl
        | LangId::Pascal
        | LangId::R
        | LangId::Groovy
        | LangId::ObjC => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn resolve_tool_cached(
    tool: &str,
    project_root: Option<&Path>,
    cache: &mut HashMap<String, bool>,
) -> bool {
    if let Some(is_available) = cache.get(tool) {
        return *is_available;
    }

    let is_available = crate::format::tool_available_for_missing_warning(tool, project_root);
    cache.insert(tool.to_string(), is_available);
    is_available
}

fn should_warn_missing_formatters(config: &crate::config::Config, lang: LangId) -> bool {
    config.format_on_edit || config.formatter.contains_key(lang_key(lang))
}

fn should_warn_missing_checkers(config: &crate::config::Config, lang: LangId) -> bool {
    let mode = config.validate_on_edit.as_deref().unwrap_or("off");
    (mode == "syntax" || mode == "full") || config.checker.contains_key(lang_key(lang))
}

fn missing_tool_warning(
    kind: &str,
    language: &str,
    candidate: &ConfigureToolCandidate,
    project_root: Option<&Path>,
    tool_cache: &mut HashMap<String, bool>,
) -> Option<crate::format::MissingTool> {
    if !candidate.required || resolve_tool_cached(&candidate.tool, project_root, tool_cache) {
        return None;
    }

    Some(crate::format::MissingTool {
        kind: kind.to_string(),
        language: language.to_string(),
        tool: candidate.tool.clone(),
        // GitHub issue #47: word this so the user understands the tool may
        // be installed but missing from AFT's PATH (common with GUI-launched
        // editors that don't inherit a login shell). format.rs has the same
        // wording in `configured_tool_hint`.
        hint: format!(
            "{} is configured in {} but was not found on PATH or in common install locations. {}",
            candidate.tool,
            candidate.source,
            crate::format::install_hint(&candidate.tool)
        ),
    })
}

fn detect_missing_tools_for_languages(
    languages: &HashSet<LangId>,
    config: &crate::config::Config,
) -> Vec<crate::format::MissingTool> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut tool_cache = HashMap::new();

    for &lang in languages {
        let language = lang_key(lang);

        if should_warn_missing_formatters(config, lang) {
            for candidate in formatter_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "formatter_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }

        if should_warn_missing_checkers(config, lang) {
            for candidate in checker_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "checker_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }
    }

    warnings.sort_by(|left, right| {
        (&left.kind, &left.language, &left.tool).cmp(&(&right.kind, &right.language, &right.tool))
    });
    warnings
}

fn detect_missing_lsp_binaries(files: &[PathBuf], config: &crate::config::Config) -> Vec<Value> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut resolved_binaries = HashSet::new();
    let mut missing_binaries = HashSet::new();

    let project_root = config.project_root.as_deref();
    let extra_paths = &config.lsp_paths_extra;

    for file in files {
        for server in servers_for_file(&file, config) {
            if is_custom_server(&server.kind)
                || !seen.insert((server.kind.id_str().to_string(), server.binary.clone()))
            {
                continue;
            }

            if !config.lsp_auto_install_binaries.contains(&server.binary) {
                continue;
            }

            if config.lsp_inflight_installs.contains(&server.binary) {
                continue;
            }

            if !resolved_binaries.contains(&server.binary) {
                if resolve_lsp_binary(&server.binary, project_root, extra_paths).is_some() {
                    resolved_binaries.insert(server.binary.clone());
                } else {
                    missing_binaries.insert(server.binary.clone());
                }
            }

            if missing_binaries.contains(&server.binary) {
                warnings.push(json!({
                    "kind": "lsp_binary_missing",
                    "server": server.binary,
                    "binary": server.binary,
                    "hint": lsp_missing_hint(&server.binary),
                }));
            }
        }
    }

    for server in &config.lsp_servers {
        // A blank binary means "partial built-in override, inherit the built-in
        // binary" — the resolvable binary is already covered by the built-in
        // pass above, so skip the missing-binary probe here (probing "" never
        // resolves and would emit a bogus warning).
        if server.binary.is_empty() {
            continue;
        }

        if server.disabled || !seen.insert((server.id.clone(), server.binary.clone())) {
            continue;
        }

        if config.lsp_inflight_installs.contains(&server.binary) {
            continue;
        }

        if !resolved_binaries.contains(&server.binary) {
            if resolve_lsp_binary(&server.binary, project_root, extra_paths).is_some() {
                resolved_binaries.insert(server.binary.clone());
            } else {
                missing_binaries.insert(server.binary.clone());
            }
        }

        if missing_binaries.contains(&server.binary) {
            warnings.push(json!({
                "kind": "lsp_binary_missing",
                "server": server.id,
                "binary": server.binary,
                "hint": lsp_missing_hint(&server.binary),
            }));
        }
    }

    warnings.sort_by_key(|warning| warning.to_string());
    warnings
}

type SearchIndexSymbolFile = (PathBuf, SystemTime);

fn search_index_symbol_files(index: &SearchIndex) -> Vec<SearchIndexSymbolFile> {
    index
        .files
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .map(|entry| (entry.path.clone(), entry.modified))
        .collect()
}

fn spawn_symbol_cache_prewarm(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
    session_id: Option<String>,
) {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            prewarm_symbol_cache_from_search_files(
                root,
                symbol_cache,
                symbol_storage,
                symbol_project_key,
                symbol_cache_generation,
                symbol_files,
                is_worktree_bridge,
            );
        });
    });
}

fn prewarm_symbol_cache_from_search_files(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
) {
    #[cfg(debug_assertions)]
    delay_symbol_prewarm_for_debug();

    let mut warmed_files = 0usize;
    let mut skipped_files = 0usize;
    if let Ok(mut cache) = symbol_cache.write() {
        if !cache.set_project_root_for_generation(symbol_cache_generation, root.clone()) {
            slog_info!("skipping stale symbol cache prewarm after reconfigure");
            return;
        }
        if let Some(storage_dir) = symbol_storage.as_deref() {
            let loaded_count = cache.load_from_disk_for_generation(
                symbol_cache_generation,
                storage_dir,
                &symbol_project_key,
                &root,
            );
            slog_info!("loaded symbol cache from disk: {} files", loaded_count);
        }
    } else {
        return;
    }

    let mut parser = crate::parser::FileParser::with_symbol_cache_generation(
        symbol_cache.clone(),
        Some(symbol_cache_generation),
    );
    for (path, modified) in &symbol_files {
        let cached = symbol_cache
            .read()
            .map(|cache| cache.contains_path_with_mtime(path, *modified))
            .unwrap_or(false);
        if cached {
            skipped_files += 1;
            continue;
        }
        if parser.extract_symbols(path).is_ok() {
            warmed_files += 1;
        }
    }

    let total_files = symbol_cache.read().map(|cache| cache.len()).unwrap_or(0);
    if !is_worktree_bridge {
        if let Some(storage_dir) = symbol_storage.as_deref() {
            if let Ok(cache) = symbol_cache.read() {
                if cache.generation() != symbol_cache_generation {
                    slog_info!("skipping stale symbol cache persistence after reconfigure");
                    return;
                }
                match crate::symbol_cache_disk::write_to_disk(
                    &cache,
                    storage_dir,
                    &symbol_project_key,
                ) {
                    Ok(()) => {
                        slog_info!("persisted symbol cache: {} files", cache.len());
                    }
                    Err(error) => {
                        slog_warn!("failed to persist symbol cache: {}", error);
                    }
                }
            }
        }
    }
    slog_info!(
        "pre-warmed symbol cache: {} new, {} cached, {} files total",
        warmed_files,
        skipped_files,
        total_files
    );
}

#[cfg(debug_assertions)]
fn delay_symbol_prewarm_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SYMBOL_PREWARM_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

fn walk_semantic_project_files_bounded(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    let filters = build_path_filters(&[], &[]).unwrap_or_default();
    walk_project_files_bounded_matching(root, &filters, max_files, is_semantic_indexed_extension)
}

#[cfg(debug_assertions)]
fn delay_search_rebuild_publish_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

#[cfg(not(debug_assertions))]
fn delay_search_rebuild_publish_for_debug() {}

#[cfg(debug_assertions)]
fn mark_search_rebuild_spawn_for_debug() {
    let Some(path) = std::env::var_os("AFT_TEST_SEARCH_REBUILD_THREAD_MARKER") else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"spawned");
}

/// Parse the optional `config: [{tier, source, doc}]` raw-tier array from
/// configure params. Returns `None` when absent or empty, so the resolver only
/// runs when tiers are actually supplied. Tiers with
/// a missing/invalid `tier` or `doc` are skipped (the `source` is optional
/// metadata). The `tier` label is trusted as stamped by the plugin (by config
/// source path) — never re-derived from `doc` content.
fn parse_config_tiers(
    params: &serde_json::Value,
) -> Option<Vec<crate::config_resolve::ConfigTier>> {
    let arr = params.get("config")?.as_array()?;
    let tiers: Vec<crate::config_resolve::ConfigTier> = arr
        .iter()
        .filter_map(|entry| {
            let tier = entry.get("tier")?.as_str()?.to_string();
            let doc = entry.get("doc")?.as_str()?.to_string();
            let source = entry
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(crate::config_resolve::ConfigTier { tier, source, doc })
        })
        .collect();
    (!tiers.is_empty()).then_some(tiers)
}

fn parse_cortexkit_user_config_path(params: &serde_json::Value) -> Result<Option<PathBuf>, String> {
    let Some(raw) = params.get("cortexkit_user_config_path") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let Some(value) = raw.as_str() else {
        return Err("configure: cortexkit_user_config_path must be a string".to_string());
    };
    if value.trim().is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err("configure: cortexkit_user_config_path must be an absolute path".to_string());
    }
    Ok(Some(path))
}

fn find_config_tier(
    tiers: &[crate::config_resolve::ConfigTier],
    tier_name: &str,
) -> Option<crate::config_resolve::ConfigTier> {
    tiers.iter().find(|tier| tier.tier == tier_name).cloned()
}

fn resolve_config_tiers_for_configure(
    params: &serde_json::Value,
    project_root: &Path,
) -> Result<Vec<crate::config_resolve::ConfigTier>, String> {
    let wire_tiers = parse_config_tiers(params).unwrap_or_default();
    let user_config_path = parse_cortexkit_user_config_path(params)?;
    let file_tiers = crate::subc_config::read_local_cortexkit_config_tiers(
        user_config_path.as_deref(),
        project_root,
    );

    let mut tiers = Vec::new();
    for tier_name in ["user", "project"] {
        if let Some(tier) = find_config_tier(&file_tiers, tier_name) {
            tiers.push(tier);
        } else if let Some(tier) = find_config_tier(&wire_tiers, tier_name) {
            tiers.push(tier);
        }
    }
    Ok(tiers)
}

fn configure_fingerprint(
    canonical_root: &Path,
    harness: &Harness,
    session_id: &str,
    config: &Config,
) -> Value {
    let mut effective_config =
        serde_json::to_value(config).unwrap_or_else(|_| serde_json::Value::Null);
    if let Some(fields) = effective_config.as_object_mut() {
        // Root and harness have canonical, explicit fingerprint fields below.
        fields.remove("project_root");
        fields.remove("harness");
    }
    json!({
        "canonical_root": canonical_root,
        "harness": harness,
        "session_id": session_id,
        "effective_config": effective_config,
        // These process-state fields are intentionally omitted from Config's
        // serialized form, so include them explicitly in configure identity.
        "foreground_wait_window_ms": config.foreground_wait_window_ms,
        "diagnostics_on_edit": config.diagnostics_on_edit,
    })
}

fn configure_warm_key(
    canonical_root: &Path,
    config: &Config,
    home_match: bool,
    is_worktree_bridge: bool,
    shared_artifacts_read_only: bool,
) -> String {
    format!(
        "root={:?};storage={:?};home={};worktree={};readonly={};search={}:{};semantic={}:{:?};callgraph={}:{};inspect={};manifests={}",
        canonical_root,
        config.storage_dir,
        home_match,
        is_worktree_bridge,
        shared_artifacts_read_only,
        config.search_index,
        config.search_index_max_file_size,
        config.semantic_search,
        config.semantic,
        config.callgraph_store,
        config.callgraph_chunk_size,
        config.inspect.enabled,
        workspace_manifest_fingerprint(canonical_root),
    )
}

/// Handle a `configure` request.
///
/// Expects `project_root` (string, required) — absolute path to the project root.
/// Sets the project root on `Config`, initializes the `CallGraph` with that root,
/// spawns a file watcher for live invalidation, and returns success with the
/// configured path.
///
/// Stderr log: `[aft] project root set: <path>`
/// Stderr log: `[aft] watcher started: <path>`
pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = req.params.get("params").unwrap_or(&req.params);
    let harness = match params.get("harness") {
        Some(raw) => match serde_json::from_value::<Harness>(raw.clone()) {
            Ok(harness) => harness,
            Err(_) => {
                // A malformed fed fingerprint gets its own machine code: at
                // bind time it means either a federation-module bug or
                // fingerprint-format drift between the fed spec and ours, and
                // a typed code lets the fed side detect that without parsing
                // prose.
                let is_fed_shaped = raw.as_str().is_some_and(|s| s.starts_with("fed:"));
                if is_fed_shaped {
                    return Response::error(
                        &req.id,
                        "bad_harness_fingerprint",
                        "configure payload invalid field 'harness'; fed fingerprint must be 32-64 lowercase hex characters",
                    );
                }
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "configure payload invalid field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'",
                );
            }
        },
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure payload missing required field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'",
            );
        }
    };
    let root = match params.get("project_root").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: missing required param 'project_root'",
            );
        }
    };

    let root_path = PathBuf::from(root);
    if !root_path.is_absolute() {
        return Response::error(
            &req.id,
            "invalid_request",
            "project_root must be an absolute path",
        );
    }
    if !root_path.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("configure: project_root is not a directory: {}", root),
        );
    }
    let canonical_cache_root =
        std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
    debug_assert!(canonical_cache_root.is_absolute());
    let (is_worktree_bridge, git_common_dir) = detect_worktree_bridge(ctx, &canonical_cache_root);

    let previous_config = ctx.config();
    let previous_project_root = previous_config.project_root.clone();
    let previous_canonical_cache_root = ctx.canonical_cache_root_opt();
    let project_root_changed =
        previous_canonical_cache_root.as_deref() != Some(canonical_cache_root.as_path());
    let mut next_config = previous_config.as_ref().clone();
    next_config.project_root = Some(root_path.clone());
    next_config.harness = Some(harness.clone());

    // P1 config relocation: plugins send raw config tiers
    // (`config: [{tier, source, doc}]`), and AFT-core resolves the merge +
    // trust-boundary itself (crate::config_resolve). The CortexKit
    // config files are authoritative per tier: the user file wins over the wire user tier,
    // and the project file independently wins over the wire project tier. The
    // wire tiers remain as a one-release fallback for cached old plugins and for
    // tiers that have not migrated yet.
    // ALWAYS resolve, even when no tiers are supplied. `resolve_config_onto`
    // uses reset-onto-default semantics, so an absent/empty config must still
    // run it to reset the core-domain config to defaults — otherwise a bind with
    // no tiers would keep the PREVIOUS bind's resolved core config (the
    // cross-bind escalation: a later low-trust bind inheriting an earlier
    // high-trust bind's capability by simply omitting tiers).
    let tiers = match resolve_config_tiers_for_configure(params, &root_path) {
        Ok(tiers) => tiers,
        Err(error) => return Response::error(&req.id, "invalid_request", error),
    };
    let config_dropped_keys: Vec<crate::config_resolve::DroppedKey> =
        crate::config_resolve::resolve_config_onto(&tiers, &mut next_config);

    // NO configure-time SSRF guard on semantic.base_url — deliberate (config
    // relocation posture). The original guard existed to stop UNTRUSTED *project*
    // config from pointing the embedding backend at an internal IP (SSRF). The tier
    // resolver now drops project-tier `semantic.base_url` outright (SEMANTIC_SECRET_REASON),
    // so the only base_url that can reach `next_config` is USER-tier — the same trust
    // level as the binary itself. A user pointing AFT at their own self-hosted
    // embedding server on the LAN/WAN (homelab Ollama/LMStudio at 192.168.x / 10.x, or
    // a remote box) is legitimate and must be allowed; the SSRF threat model (attacker-
    // controlled URL) does not apply to user-trusted config. The `validate_base_url_no_ssrf`
    // primitive remains for any future less-trusted source. See semantic_test::
    // configure_accepts_user_private_base_url.

    // Parse and validate process-state configure fields into a temporary config first.
    // AppContext is mutated only after this phase succeeds, so an invalid late
    // field cannot leave the bridge half-configured. Core AftConfig fields are
    // resolved exclusively from `config: [{tier, source, doc}]` above.
    if let Some(v) = params
        .get("aft_search_registered")
        .and_then(|v| v.as_bool())
    {
        next_config.aft_search_registered = v;
    }
    if let Some(v) = params.get("bash_permissions").and_then(|v| v.as_bool()) {
        next_config.bash_permissions = v;
    }
    if let Some(v) = params.get("lsp_paths_extra") {
        next_config.lsp_paths_extra = match parse_lsp_paths_extra(v) {
            Ok(paths) => paths,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params.get("lsp_auto_install_binaries") {
        next_config.lsp_auto_install_binaries =
            match parse_string_set(v, "lsp_auto_install_binaries") {
                Ok(binaries) => binaries,
                Err(error) => return Response::error(&req.id, "invalid_request", error),
            };
    }
    if let Some(v) = params.get("lsp_inflight_installs") {
        next_config.lsp_inflight_installs = match parse_string_set(v, "lsp_inflight_installs") {
            Ok(binaries) => binaries,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params
        .get("search_index_max_file_size")
        .and_then(|v| v.as_u64())
    {
        next_config.search_index_max_file_size = v;
    }
    if let Some(raw) = params.get("storage_dir") {
        let Some(value) = raw.as_str() else {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: storage_dir must be a string",
            );
        };
        next_config.storage_dir = match validate_storage_dir(value) {
            Ok(path) => Some(path),
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if next_config.storage_dir.is_none() {
        // Plugin-less consumers (the daemon-supervised module, MCP hosts)
        // never send the plugin-computed storage_dir param. Resolve the
        // shared CortexKit storage root here so every artifact lane keys off
        // one concrete path. Leaving this None made lanes that gate writes on
        // `Some(storage_dir)` (semantic persistence) silently RAM-only under
        // the daemon while lanes with their own fallback (trigram) persisted,
        // splitting the storage universe by transport.
        next_config.storage_dir = Some(crate::bash_background::storage_dir(None));
    }
    if let Some(raw) = params.get("max_background_bash_tasks") {
        let parsed = raw.as_u64().filter(|v| *v >= 1);
        match parsed.and_then(|v| usize::try_from(v).ok()) {
            Some(v) => next_config.max_background_bash_tasks = v,
            None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "max_background_bash_tasks must be a positive integer (>= 1); got {}",
                        raw
                    ),
                );
            }
        }
    }

    // Detect "this is not really a project root" scenarios before any walks
    // that traverse `project_root`.
    let mut degraded_reasons: Vec<String> = Vec::new();
    let home_match = resolve_home_dir().is_some_and(|home| home == canonical_cache_root);
    if home_match {
        degraded_reasons.push("home_root".to_string());
    }

    // `_bypass_size_limits` (set by `aft warmup --force`) lifts the semantic
    // `max_files` cap so a very large repo is fully embedded for measurement.
    // Internal benchmarking escape hatch, not a user-facing knob. (The search
    // index has no file-count cap — its disk-backed design is RAM-bounded.)
    let bypass_size_limits = params
        .get("_bypass_size_limits")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if bypass_size_limits {
        const UNCAPPED: usize = 1_000_000_000;
        next_config.semantic.max_files = next_config.semantic.max_files.max(UNCAPPED);
    }

    let search_disabled_for_home = home_match && next_config.search_index;
    let semantic_disabled_for_home = home_match && next_config.semantic_search;
    if search_disabled_for_home {
        next_config.search_index = false;
    }
    if semantic_disabled_for_home {
        next_config.semantic_search = false;
    }

    let requested_fingerprint =
        configure_fingerprint(&canonical_cache_root, &harness, req.session(), &next_config);
    let current_harness = ctx.harness_opt();
    let current_fingerprint = previous_canonical_cache_root
        .as_deref()
        .zip(current_harness.as_ref())
        .map(|(canonical_root, current_harness)| {
            configure_fingerprint(
                canonical_root,
                current_harness,
                req.session(),
                previous_config.as_ref(),
            )
        });
    let effective_configure_changed = current_fingerprint.as_ref() != Some(&requested_fingerprint);
    let preflight_warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        home_match,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
    );
    if ctx.configure_generation() > 0
        && current_fingerprint.as_ref() == Some(&requested_fingerprint)
        && ctx.configure_warm_key_matches(&preflight_warm_key)
        && ctx.is_worktree_bridge() == is_worktree_bridge
        && ctx.git_common_dir() == git_common_dir
    {
        let first_session_bind = ctx.note_configure_session_binding(
            canonical_cache_root.clone(),
            req.session().to_string(),
        );
        let generation = ctx.configure_generation();
        if first_session_bind {
            let storage_root =
                crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
            ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
                generation,
                root_path: root_path.clone(),
                canonical_cache_root: canonical_cache_root.clone(),
                harness: harness.clone(),
                storage_root: storage_root.clone(),
                harness_dir: storage_root.join(harness.storage_segment()),
                session_id: req.session().to_string(),
                home_match,
                format_tool_cache_clear_needed: false,
                run_bash_replay: true,
                refresh_project_runtime: false,
                sync_bash_compress_flag: false,
                reset_filter_registry: false,
                clear_failed_spawns: false,
                warm_callgraph_store: false,
            });
            slog_debug!(
                "equivalent configure registered session {} for generation {}",
                req.session(),
                generation
            );
        } else {
            slog_debug!(
                "equivalent configure no-op for session {} at generation {}",
                req.session(),
                generation
            );
        }

        let artifact_owner_status = ctx.artifact_owner_status();
        let search_index_cache_reused = next_config.search_index
            && ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
        return Response::success(
            &req.id,
            json!({
                "project_root": root_path.display().to_string(),
                "warnings": [],
                "warnings_pending": false,
                "search_index_cache_reused": search_index_cache_reused,
                "artifact_owner": artifact_owner_status
                    .as_ref()
                    .map(|status| serde_json::to_value(status).unwrap_or(serde_json::Value::Null)),
                "config_dropped_keys": config_dropped_keys
                    .iter()
                    .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    if search_disabled_for_home {
        slog_warn!(
            "search_index auto-disabled: project root is the user home directory \
             ({}). Open a project subdirectory for full features.",
            canonical_cache_root.display()
        );
    }
    if semantic_disabled_for_home {
        slog_warn!(
            "semantic_search auto-disabled: project root is the user home directory \
             ({}). Open a project subdirectory for full features.",
            canonical_cache_root.display()
        );
    }

    let format_tool_cache_clear_needed = previous_project_root.as_ref() != Some(&root_path);

    let storage_root = crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    let artifact_key_needed = !home_match
        && (next_config.search_index || next_config.semantic_search || next_config.callgraph_store);
    let project_key = if artifact_key_needed {
        Some(
            match ctx.memoized_artifact_cache_key_for_configure(
                &root_path,
                &canonical_cache_root,
                &storage_root,
                git_common_dir.as_deref(),
            ) {
                Ok(key) => key,
                Err(error) => {
                    return Response::error_with_data(
                        &req.id,
                        "cache_key_probe_failed",
                        error.to_string(),
                        json!({
                            "retryable": true,
                            "root": error.root().display().to_string(),
                            "detail": error.detail(),
                        }),
                    );
                }
            },
        )
    } else {
        None
    };
    let project_scope_key = crate::path_identity::project_scope_key(&canonical_cache_root);
    let artifact_owner_claim = if let Some(project_key) = project_key.as_ref() {
        if is_worktree_bridge {
            Some(crate::artifact_owner::open_read_only_borrow(
                next_config.storage_dir.as_deref(),
                &canonical_cache_root,
                project_key,
                &project_scope_key,
            ))
        } else {
            match crate::artifact_owner::claim_or_open_read_only(
                next_config.storage_dir.as_deref(),
                &canonical_cache_root,
                project_key,
                &project_scope_key,
                git_common_dir.as_deref(),
            ) {
                Ok(claim) => Some(claim),
                Err(error) => {
                    return Response::error(
                        &req.id,
                        "artifact_owner_unavailable",
                        format!("failed to claim artifact owner manifest: {error}"),
                    );
                }
            }
        }
    } else {
        None
    };
    let project_key = project_key.unwrap_or_default();
    let artifact_owner_status = artifact_owner_claim
        .as_ref()
        .map(|claim| claim.status.clone());
    let artifact_owner_read_only = artifact_owner_status
        .as_ref()
        .is_some_and(|status| status.mode == crate::artifact_owner::ArtifactOwnerMode::ReadOnly);
    if artifact_owner_read_only {
        degraded_reasons.push("artifact_owner_read_only".to_string());
        if let Some(note) = artifact_owner_status
            .as_ref()
            .and_then(|status| status.note.as_ref())
        {
            slog_warn!("{}", note);
        }
    }
    let root_cache_storage_ok = match crate::root_cache::storage_allows_root_keyed(&storage_root) {
        Ok(true) => true,
        Ok(false) => {
            degraded_reasons.push("root_cache_network_fs".to_string());
            slog_warn!(
                "root-keyed callgraph/inspect writers disabled: storage directory appears to be on a network filesystem ({})",
                storage_root.display()
            );
            false
        }
        Err(error) => {
            degraded_reasons.push("root_cache_fs_probe_failed".to_string());
            slog_warn!(
                "root-keyed callgraph/inspect writers disabled: failed to probe storage filesystem {}: {}",
                storage_root.display(),
                error
            );
            false
        }
    };
    let callgraph_writer_capability =
        root_cache_storage_ok && !is_worktree_bridge && !artifact_owner_read_only && !home_match;
    let inspect_writer_capability = root_cache_storage_ok && !home_match;
    let heavy_root_work_allowed =
        !home_match && !degraded_reasons.iter().any(|reason| reason == "home_root");

    // Commit phase: no validation returns after this point.
    let semantic_fingerprint_generation =
        if semantic_fingerprint_config_changed(&previous_config.semantic, &next_config.semantic) {
            ctx.advance_semantic_fingerprint_generation()
        } else {
            ctx.semantic_fingerprint_generation()
        };
    ctx.set_config(next_config.clone());
    ctx.set_harness(harness.clone());
    {
        let mut backup = ctx.backup().lock();
        backup.set_policy(crate::backup::BackupPolicy {
            enabled: next_config.backup.enabled.unwrap_or(true),
            max_depth: next_config
                .backup
                .max_depth
                .unwrap_or(crate::backup::DEFAULT_MAX_UNDO_DEPTH),
            max_file_size: next_config.backup.max_file_size,
        });
        backup.set_db_harness(harness.clone());
    }
    ctx.set_canonical_cache_root(canonical_cache_root.clone());
    crate::root_cache::configure_artifact_access(
        &canonical_cache_root,
        &project_key,
        is_worktree_bridge,
    );
    ctx.set_cache_role(is_worktree_bridge, git_common_dir);
    let artifact_owner_lease = artifact_owner_claim.and_then(|claim| claim.lease);
    ctx.set_artifact_owner(artifact_owner_status.clone(), artifact_owner_lease);
    ctx.set_cache_writer_capabilities(callgraph_writer_capability, inspect_writer_capability);
    // Snapshot degraded-mode state once at configure time so every later
    // heavy-work entry point reads the same cheap gate instead of re-deriving
    // home-root logic independently.
    ctx.set_degraded_reasons(degraded_reasons.clone());
    ctx.set_heavy_root_work_allowed(heavy_root_work_allowed);
    let warm_key = configure_warm_key(
        &canonical_cache_root,
        &next_config,
        home_match,
        is_worktree_bridge,
        ctx.shared_artifacts_read_only(),
    );
    let (configure_generation, equivalent_warm_config) = ctx.note_configure_warm_key(warm_key);
    let first_session_bind =
        ctx.note_configure_session_binding(canonical_cache_root.clone(), req.session().to_string());
    let semantic_cold_seed_generation = if !equivalent_warm_config {
        ctx.reset_tier2_refresh_scheduler();
        let semantic_cold_seed_generation = ctx.reset_semantic_cold_seed_gate_for_configure();
        if next_config.semantic_search && !ctx.shared_artifacts_read_only() && !home_match {
            ctx.schedule_semantic_cold_seed_gate_for_configure();
        }
        semantic_cold_seed_generation
    } else {
        configure_generation
    };
    // Project root (and thus tsconfig resolution) may have changed; drop the
    // status-bar membership cache so the next bar count re-resolves from disk.
    // Equivalent rebinds keep this hot cache because no tsconfig input changed.
    if !equivalent_warm_config || project_root_changed {
        ctx.clear_tsconfig_membership_cache();
    }
    ctx.backup()
        .lock()
        .set_db_project_key(crate::path_identity::project_scope_key(
            &canonical_cache_root,
        ));
    let search_index = ctx.config().search_index;
    let semantic_search = ctx.config().semantic_search;
    let search_index_max_file_size = ctx.config().search_index_max_file_size;
    let semantic_config = ctx.config().semantic.clone();
    let mut search_index_cache_reused = false;

    // Reconfigure is still the signal that workspace package metadata may have
    // changed, even when the warm-maintenance key is otherwise equivalent.
    crate::callgraph::clear_workspace_package_cache();

    let search_build_in_progress = ctx
        .search_index_rx()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    let semantic_build_in_progress = ctx.semantic_index_rx().lock().is_some();
    if equivalent_warm_config {
        // The zero-work rebind path keeps the live index serving; report that
        // honestly instead of implying the cache was dropped.
        search_index_cache_reused = search_index
            && ctx
                .search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
        if search_build_in_progress {
            slog_info!(
                "search index build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        if semantic_build_in_progress {
            slog_info!(
                "semantic index build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
        if ctx.callgraph_store_rx().lock().is_some() {
            slog_info!(
                "callgraph store warm build adopted by equivalent reconfigure (generation {})",
                configure_generation
            );
        }
    } else {
        // Note: We intentionally only WARN on rapid reconfigure (rather than tracking
        // JoinHandles to cancel old threads) because:
        //   1. Old thread results are dropped when ctx.search_index_rx() is reset
        //   2. Atomic tempfile writes via std::fs::rename are race-safe (last writer wins)
        //   3. Only CPU is wasted; no correctness issue
        //   4. Tracking handles would add complexity for negligible benefit
        // If reconfigure rate becomes a real problem, switch to a single
        // generation-counter + cancellation-token pattern.
        if search_build_in_progress {
            slog_warn!(
                "search index build cancelled (superseded by generation {})",
                configure_generation
            );
        }
        if semantic_build_in_progress {
            slog_warn!(
                "semantic index build cancelled (superseded by generation {})",
                configure_generation
            );
        }
        if ctx.callgraph_store_rx().lock().is_some() {
            slog_warn!(
                "callgraph store warm build cancelled (superseded by generation {})",
                configure_generation
            );
        }

        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *ctx.search_index_rx()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let symbol_cache_generation = ctx.reset_symbol_cache();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *ctx.semantic_index_rx().lock() = None;
        *ctx.callgraph_store()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if previous_project_root.as_ref() == Some(&root_path) {
            ctx.mark_callgraph_store_force_rebuild();
        }
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
        ctx.clear_semantic_refresh_worker();
        *ctx.semantic_embedding_model().lock() = None;
        ctx.clear_pending_index_updates();

        let storage_dir = ctx.config().storage_dir.clone();

        if search_index {
            let cache_dir = resolve_cache_dir_with_key(&project_key, storage_dir.as_deref());
            // Unlike worktree topology, HEAD is a cache-freshness input and may
            // change between equivalent rebinds, so this probe remains live.
            let current_head = current_git_head(&canonical_cache_root);

            let root_for_prewarm = canonical_cache_root.clone();
            let symbol_cache = ctx.symbol_cache();
            let symbol_storage = storage_dir.clone();
            let symbol_project_key = project_key.clone();
            let is_worktree_bridge_for_search = is_worktree_bridge;
            let shared_artifacts_read_only_for_search = ctx.shared_artifacts_read_only();
            let session_id_for_bg = log_ctx::current_session();
            let search_generation = configure_generation;
            let search_generation_flag = ctx.configure_generation_flag();

            if shared_artifacts_read_only_for_search {
                match crate::readonly_artifacts::open_search_index_read_only(
                    &canonical_cache_root,
                    storage_dir.as_deref(),
                ) {
                    crate::readonly_artifacts::ReadOnlyArtifact::Fresh(index) => {
                        search_index_cache_reused = true;
                        let symbol_files = search_index_symbol_files(&index);
                        *ctx.search_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                        spawn_symbol_cache_prewarm(
                            root_for_prewarm,
                            symbol_cache,
                            symbol_storage,
                            symbol_project_key,
                            symbol_cache_generation,
                            symbol_files,
                            true,
                            session_id_for_bg,
                        );
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Stale(stale) => {
                        slog_warn!(
                        "search index is read-only and stale for {} file(s); not repairing shared artifacts",
                        stale.drift_count
                    );
                    }
                    crate::readonly_artifacts::ReadOnlyArtifact::Absent => {
                        slog_warn!(
                            "search index is read-only but no shared artifact snapshot exists"
                        );
                    }
                }
            } else {
                let baseline = SearchIndex::read_from_disk(&cache_dir, &canonical_cache_root);
                search_index_cache_reused = baseline.is_some();
                match baseline {
                    Some(mut index) if index.stored_git_head() == current_head.as_deref() => {
                        // Install the cached index immediately as NOT-ready, then VERIFY
                        // it against disk on a BACKGROUND thread. `verify_against_disk`
                        // walks the project and content-hashes every cached file
                        // (verify_file_strict → blake3), which is O(repo) and MUST NOT
                        // run on the dispatch thread: configure is dispatched on the
                        // single request loop, so an inline verify blocks configure and
                        // every queued request (bash/read/edit) past the 30s transport
                        // timeout on a large repo. This regressed in v0.39.1 — v0.39.0
                        // verify was stat-only (mtime+size); content_hash was added to
                        // FileFreshness so verify now hashes all files. While ready=false
                        // grep/glob fall back to a walk, exactly like the cache-miss
                        // branch below. The drain installs the verified, ready index.
                        index.set_ready(false);
                        *ctx.search_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(index.clone());

                        let (tx, rx): (
                            crossbeam_channel::Sender<SearchIndex>,
                            crossbeam_channel::Receiver<SearchIndex>,
                        ) = unbounded();
                        *ctx.search_index_rx()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);

                        #[cfg(debug_assertions)]
                        mark_search_rebuild_spawn_for_debug();

                        let head_for_verify = current_head.clone();
                        thread::spawn(move || {
                            let session_id_for_prewarm = session_id_for_bg.clone();
                            log_ctx::with_session(session_id_for_bg, || {
                                let _permit = crate::cold_build_limiter::acquire_blocking(
                                    "search index warm verify",
                                );
                                let mut verified = index;
                                let _cache_lock = if is_worktree_bridge_for_search {
                                    None
                                } else {
                                    CacheLock::acquire(&cache_dir, &root_for_prewarm).ok()
                                };
                                verified.verify_against_disk(head_for_verify);
                                let symbol_files = search_index_symbol_files(&verified);
                                if search_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                                    == search_generation
                                {
                                    let _ = tx.send(verified);
                                } else {
                                    slog_info!(
                                    "search index build result discarded for stale generation {}",
                                    search_generation
                                );
                                    return;
                                }
                                spawn_symbol_cache_prewarm(
                                    root_for_prewarm,
                                    symbol_cache,
                                    symbol_storage,
                                    symbol_project_key,
                                    symbol_cache_generation,
                                    symbol_files,
                                    is_worktree_bridge_for_search,
                                    session_id_for_prewarm,
                                );
                            });
                        });
                    }
                    mut baseline => {
                        if let Some(index) = baseline.as_mut() {
                            index.set_ready(false);
                            *ctx.search_index()
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(index.clone());
                        }

                        let (tx, rx): (
                            crossbeam_channel::Sender<SearchIndex>,
                            crossbeam_channel::Receiver<SearchIndex>,
                        ) = unbounded();
                        *ctx.search_index_rx()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);

                        #[cfg(debug_assertions)]
                        mark_search_rebuild_spawn_for_debug();

                        let root_clone = canonical_cache_root.clone();
                        let search_generation_flag = Arc::clone(&search_generation_flag);
                        thread::spawn(move || {
                            let session_id_for_prewarm = session_id_for_bg.clone();
                            log_ctx::with_session(session_id_for_bg, || {
                                let _permit = crate::cold_build_limiter::acquire_blocking(
                                    "search index build",
                                );
                                let index = {
                                    let _cache_lock = if is_worktree_bridge_for_search {
                                        None
                                    } else {
                                        match CacheLock::acquire(&cache_dir, &root_clone) {
                                            Ok(lock) => Some(lock),
                                            Err(error) => {
                                                slog_warn!(
                                                    "failed to acquire search cache lock: {}",
                                                    error
                                                );
                                                None
                                            }
                                        }
                                    };
                                    let mut index = SearchIndex::rebuild_or_refresh(
                                        &root_clone,
                                        search_index_max_file_size,
                                        current_head,
                                        baseline,
                                        Some(&cache_dir),
                                    );
                                    delay_search_rebuild_publish_for_debug();
                                    if !is_worktree_bridge_for_search {
                                        let head = index.stored_git_head().map(str::to_owned);
                                        index.write_to_disk(&cache_dir, head.as_deref());
                                    }
                                    index
                                };

                                let symbol_files = search_index_symbol_files(&index);
                                if search_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                                    == search_generation
                                {
                                    let _ = tx.send(index);
                                } else {
                                    slog_info!(
                                    "search index build result discarded for stale generation {}",
                                    search_generation
                                );
                                    return;
                                }
                                spawn_symbol_cache_prewarm(
                                    root_clone,
                                    symbol_cache,
                                    symbol_storage,
                                    symbol_project_key,
                                    symbol_cache_generation,
                                    symbol_files,
                                    is_worktree_bridge_for_search,
                                    session_id_for_prewarm,
                                );
                            });
                        });
                    }
                }
            }
        }

        if semantic_search && ctx.shared_artifacts_read_only() {
            match crate::readonly_artifacts::open_semantic_index_read_only(
                &canonical_cache_root,
                storage_dir.as_deref(),
            ) {
                crate::readonly_artifacts::ReadOnlyArtifact::Fresh(index) => {
                    *ctx.semantic_index()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::ready();
                }
                crate::readonly_artifacts::ReadOnlyArtifact::Stale(stale) => {
                    // Serve the stale snapshot instead of refusing: read-only
                    // sessions (mason worktrees) cannot rebuild, and drifted
                    // embeddings still rank far better than no semantic lane.
                    // Same serve-with-disclosure posture as the trigram lane
                    // above and the cross-root borrow path.
                    crate::slog_warn!(
                        "semantic index is read-only and stale for {} file(s); serving stale snapshot without repairing shared artifacts",
                        stale.drift_count
                    );
                    *ctx.semantic_index()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stale.index);
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::ready();
                }
                crate::readonly_artifacts::ReadOnlyArtifact::Absent => {
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::Failed(
                            "semantic index is read-only but no shared artifact snapshot exists"
                                .to_string(),
                        );
                }
            }
        } else if semantic_search {
            let semantic_initial_stage = if previous_config.semantic_search
                && previous_project_root.as_deref() == Some(root_path.as_path())
                && semantic_fingerprint_config_changed(&previous_config.semantic, &semantic_config)
            {
                "fingerprint_change"
            } else {
                "initial"
            };
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SemanticIndexStatus::Building {
                    stage: semantic_initial_stage.to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
            let (tx, rx): (
                crossbeam_channel::Sender<SemanticIndexEvent>,
                crossbeam_channel::Receiver<SemanticIndexEvent>,
            ) = unbounded();
            *ctx.semantic_index_rx().lock() = Some(rx);

            let (refresh_tx, refresh_rx) = unbounded::<SemanticRefreshRequest>();
            let (refresh_event_tx, refresh_event_rx) = unbounded::<SemanticRefreshEvent>();
            let refresh_worker_slot: SemanticRefreshWorkerSlot = Arc::new(Mutex::new(None));
            ctx.install_semantic_refresh_worker(
                refresh_tx,
                refresh_event_rx,
                Arc::clone(&refresh_worker_slot),
            );

            let root_clone = canonical_cache_root.clone();
            let semantic_storage = storage_dir.clone();
            let semantic_project_key = project_key.clone();
            let semantic_config = semantic_config.clone();
            let tx_progress = tx.clone();
            let is_worktree_bridge_for_semantic = is_worktree_bridge;
            let semantic_cold_seed_active = ctx.semantic_cold_seed_active_flag();
            let semantic_cold_seed_generation_flag = ctx.semantic_cold_seed_generation_flag();
            let semantic_cold_seed_generation_for_worker = semantic_cold_seed_generation;
            let semantic_generation = configure_generation;
            let semantic_generation_flag = ctx.configure_generation_flag();
            let semantic_fingerprint_generation_flag = ctx.semantic_fingerprint_generation_flag();
            let session_id_for_bg2 = log_ctx::current_session();
            thread::spawn(move || {
                log_ctx::with_session(session_id_for_bg2, || {
                    // Cap file count to bound memory on huge project roots (e.g.,
                    // /home/user). The local fastembed model (~200MB) + embeddings +
                    // batch buffers can exceed memory on constrained systems when
                    // indexing tens of thousands of files. Configurable via
                    // `semantic.max_files` (default 20k); remote backends that embed
                    // server-side can raise it freely.
                    let max_semantic_files = semantic_config.max_files;
                    let mut semantic_retry_attempt: usize = 0;
                    let set_cold_seed_active = || {
                        if semantic_cold_seed_generation_flag
                            .load(std::sync::atomic::Ordering::SeqCst)
                            == semantic_cold_seed_generation_for_worker
                        {
                            semantic_cold_seed_active
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    };
                    let clear_cold_seed_active = || {
                        if semantic_cold_seed_generation_flag
                            .load(std::sync::atomic::Ordering::SeqCst)
                            == semantic_cold_seed_generation_for_worker
                        {
                            semantic_cold_seed_active
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                    };
                    let clear_cold_seed_gate_and_notify = || {
                        clear_cold_seed_active();
                        let _ = tx_progress.send(SemanticIndexEvent::ColdSeedGateCleared);
                    };

                    struct SemanticBuildReady {
                        index: SemanticIndex,
                        model: crate::semantic_index::EmbeddingModel,
                        persist_to_disk: bool,
                    }

                    let build_once = || -> Result<SemanticBuildReady, String> {
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "initializing_embedding_model".to_string(),
                            files: None,
                            entries_done: None,
                            entries_total: None,
                        });
                        let mut model =
                            crate::semantic_index::EmbeddingModel::from_config(&semantic_config)?;
                        let fingerprint = model.fingerprint(&semantic_config)?;
                        let fingerprint_key = fingerprint.as_string();
                        let _semantic_cache_lock = (!is_worktree_bridge_for_semantic)
                            .then(|| ())
                            .and_then(|_| semantic_storage.as_ref())
                            .and_then(|dir| {
                                match SemanticIndexLock::acquire(
                                    dir,
                                    &semantic_project_key,
                                    &root_clone,
                                ) {
                                    Ok(lock) => Some(lock),
                                    Err(error) => {
                                        slog_warn!(
                                            "failed to acquire semantic cache lock: {}",
                                            error
                                        );
                                        None
                                    }
                                }
                            });

                        if let Some(ref dir) = semantic_storage {
                            if let Some(cached) = SemanticIndex::read_from_disk(
                                dir,
                                &semantic_project_key,
                                &root_clone,
                                is_worktree_bridge_for_semantic,
                                Some(&fingerprint_key),
                            ) {
                                clear_cold_seed_gate_and_notify();
                                // Try incremental refresh: re-embed only changed/new files,
                                // drop entries for deleted files, keep everything else.
                                // This is the hot path for restart on a project with a
                                // handful of edits — avoids re-embedding 4000+ unchanged
                                // files just to pick up 10 changes.
                                let current_files = match walk_semantic_project_files_bounded(
                                    &root_clone,
                                    max_semantic_files,
                                ) {
                                    Ok(files) => files,
                                    Err(observed) => {
                                        slog_warn!(
                                            "skipping semantic index: more than {} files exceeds limit of {}. \
                                             Raise semantic.max_files or open a specific project directory.",
                                            observed.saturating_sub(1),
                                            max_semantic_files
                                        );
                                        return Err(format!(
                                            "too many files (>{}) for semantic indexing (max {})",
                                            max_semantic_files, max_semantic_files
                                        ));
                                    }
                                };

                                let mut cached = cached;
                                let mut embed = |texts: Vec<String>| model.embed(texts);
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "refreshing_stale_files".to_string(),
                                    files: None,
                                    entries_done: None,
                                    entries_total: None,
                                });
                                let mut progress = |done: usize, total: usize| {
                                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                        stage: "embedding_stale_symbols".to_string(),
                                        files: None,
                                        entries_done: Some(done),
                                        entries_total: Some(total),
                                    });
                                };

                                match cached.refresh_stale_files(
                                    &root_clone,
                                    &current_files,
                                    &mut embed,
                                    semantic_config.max_batch_size.max(1),
                                    &mut progress,
                                ) {
                                    Ok(summary) => {
                                        if summary.is_noop() {
                                            slog_info!(
                                                "semantic index: cached index is current ({} entries)",
                                                cached.entry_count(),
                                            );
                                        } else {
                                            slog_info!(
                                                "semantic index: refreshed incrementally — {} changed, {} new, {} deleted, {} total processed (kept {} cached)",
                                                summary.changed,
                                                summary.added,
                                                summary.deleted,
                                                summary.total_processed,
                                                cached.len(),
                                            );
                                            cached.set_fingerprint(fingerprint);
                                        }
                                        let persist_to_disk = !summary.is_noop();
                                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                            stage: "loaded_cached_index".to_string(),
                                            files: None,
                                            entries_done: Some(cached.entry_count()),
                                            entries_total: Some(cached.entry_count()),
                                        });
                                        return Ok(SemanticBuildReady {
                                            index: cached,
                                            model,
                                            persist_to_disk,
                                        });
                                    }
                                    Err(error) => {
                                        if crate::semantic_index::embedding_failure_is_transient(
                                            &error,
                                        ) {
                                            // TRANSIENT backend error (e.g. the embedding
                                            // server is overloaded by concurrent bridges, or
                                            // briefly unreachable). Do NOT drop the cache and
                                            // full-rebuild: a full corpus re-embed against an
                                            // already-overloaded backend amplifies the overload
                                            // and cascades to other bridges AND the main
                                            // session (every bridge's incremental refresh then
                                            // fails transiently and full-rebuilds too). Keep
                                            // serving the valid cached index; the handful of
                                            // changed files re-embed on a later refresh once the
                                            // backend recovers. Mirrors the watcher-refresh
                                            // self-heal in main.rs.
                                            let clean =
                                                crate::semantic_index::strip_transient_embedding_marker(
                                                    &error,
                                                );
                                            slog_warn!(
                                                "incremental refresh hit a transient backend error ({}); keeping the cached index instead of full-rebuilding",
                                                clean
                                            );
                                            return Ok(SemanticBuildReady {
                                                index: cached,
                                                model,
                                                persist_to_disk: false,
                                            });
                                        }
                                        // Permanent failure (dimension mismatch, etc.): the
                                        // cache is genuinely unusable, drop it and full-rebuild.
                                        slog_warn!(
                                            "incremental refresh failed ({}), falling back to full rebuild",
                                            error
                                        );
                                    }
                                }
                            }
                        }

                        set_cold_seed_active();

                        let files = match walk_semantic_project_files_bounded(
                            &root_clone,
                            max_semantic_files,
                        ) {
                            Ok(files) => {
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "scanned_project_files".to_string(),
                                    files: Some(files.len()),
                                    entries_done: None,
                                    entries_total: None,
                                });
                                files
                            }
                            Err(observed) => {
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "scanned_project_files".to_string(),
                                    files: Some(observed),
                                    entries_done: None,
                                    entries_total: None,
                                });
                                slog_warn!(
                                    "skipping semantic index: more than {} files exceeds limit of {}. \
                                     Raise semantic.max_files or open a specific project directory.",
                                    observed.saturating_sub(1),
                                    max_semantic_files
                                );
                                return Err(format!(
                                    "too many files (>{}) for semantic indexing (max {})",
                                    max_semantic_files, max_semantic_files
                                ));
                            }
                        };

                        let mut embed = |texts: Vec<String>| model.embed(texts);

                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "extracting_symbols".to_string(),
                            files: Some(files.len()),
                            entries_done: None,
                            entries_total: None,
                        });
                        let mut progress = |done: usize, total: usize| {
                            let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                stage: "embedding_symbols".to_string(),
                                files: Some(files.len()),
                                entries_done: Some(done),
                                entries_total: Some(total),
                            });
                        };
                        let index = SemanticIndex::build_with_progress(
                            &root_clone,
                            &files,
                            &mut embed,
                            semantic_config.max_batch_size.max(1),
                            &mut progress,
                        )?;
                        let mut index = index;
                        index.set_fingerprint(fingerprint);
                        slog_info!(
                            "built semantic index: {} files, {} entries",
                            files.len(),
                            index.len()
                        );
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "persisting_index".to_string(),
                            files: Some(files.len()),
                            entries_done: Some(index.len()),
                            entries_total: Some(index.len()),
                        });

                        Ok(SemanticBuildReady {
                            index,
                            model,
                            persist_to_disk: true,
                        })
                    };

                    // Build-level retry: if the embedding backend is unreachable or
                    // briefly failing (connection refused, timeout, 5xx/429), riding
                    // it out beats parking the index in `Failed` forever — a state
                    // nothing re-triggers short of a bridge restart. We keep retrying
                    // with capped backoff, surfacing an honest "waiting for backend"
                    // building-state so the sidebar shows recovery-in-progress, not a
                    // red failure. The moment the backend returns, the build
                    // succeeds and the index goes Ready.
                    //
                    // Permanent errors (dimension mismatch, too-many-files, 4xx auth)
                    // are NOT marked transient and fail fast with the real message.
                    //
                    // Supersession is automatic: a reconfigure replaces the bridge's
                    // semantic receiver, so the next `tx`/`tx_progress.send` returns
                    // Err (receiver dropped) and this thread exits without competing
                    // with the fresh build.
                    let build_result = loop {
                        let attempt_result = catch_unwind(AssertUnwindSafe(&build_once));
                        match attempt_result {
                            Ok(Err(ref error))
                                if crate::semantic_index::embedding_failure_is_transient(error) =>
                            {
                                let clean =
                                    crate::semantic_index::strip_transient_embedding_marker(error);
                                let backoff = semantic_build_retry_backoff(semantic_retry_attempt);
                                semantic_retry_attempt += 1;
                                slog_warn!(
                                "semantic index build: embedding backend unavailable ({}); retrying in {}s",
                                clean,
                                backoff.as_secs(),
                            );
                                // Surface "waiting for backend" as a building stage so
                                // the sidebar shows recovery-in-progress. If the
                                // receiver is gone (reconfigure superseded us), bail.
                                clear_cold_seed_active();
                                if tx_progress
                                    .send(SemanticIndexEvent::Progress {
                                        stage: format!("waiting_for_embedding_backend: {clean}"),
                                        files: None,
                                        entries_done: None,
                                        entries_total: None,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                if tx_progress
                                    .send(SemanticIndexEvent::ColdSeedGateCleared)
                                    .is_err()
                                {
                                    return;
                                }
                                thread::sleep(backoff);
                                continue;
                            }
                            other => break other,
                        }
                    };

                    enum SemanticBuildOutcome {
                        Ready(SemanticBuildReady),
                        Failed(String),
                    }

                    let outcome = match build_result {
                        Ok(Ok(ready)) => SemanticBuildOutcome::Ready(ready),
                        Ok(Err(error)) => {
                            slog_warn!("failed to build semantic index: {}", error);
                            SemanticBuildOutcome::Failed(error)
                        }
                        Err(_) => {
                            let error = "semantic index build panicked".to_string();
                            slog_warn!("{}", error);
                            SemanticBuildOutcome::Failed(error)
                        }
                    };

                    let persist_completed_index = |index: &SemanticIndex, reason: &str| {
                        if is_worktree_bridge_for_semantic {
                            return;
                        }
                        let Some(dir) = semantic_storage.as_ref() else {
                            // Should be unreachable now that configure defaults
                            // storage_dir; keep it loud in case a path regresses.
                            slog_warn!(
                                "semantic index persistence skipped for {reason}: no storage_dir resolved"
                            );
                            return;
                        };
                        if semantic_fingerprint_generation_flag
                            .load(std::sync::atomic::Ordering::SeqCst)
                            != semantic_fingerprint_generation
                        {
                            slog_info!(
                                "semantic index persistence skipped for {reason}: semantic fingerprint changed after generation {} started",
                                semantic_generation
                            );
                            return;
                        }
                        index.write_to_disk(dir, &semantic_project_key);
                    };

                    if semantic_generation_flag.load(std::sync::atomic::Ordering::SeqCst)
                        != semantic_generation
                    {
                        if let SemanticBuildOutcome::Ready(ready) = &outcome {
                            if ready.persist_to_disk {
                                persist_completed_index(&ready.index, "stale generation discard");
                            }
                        }
                        SEMANTIC_STALE_GENERATION_DISCARDS.fetch_add(1, Ordering::SeqCst);
                        slog_info!(
                            "semantic index build result discarded for stale generation {}",
                            semantic_generation
                        );
                        clear_cold_seed_active();
                        return;
                    }

                    let event = match outcome {
                        SemanticBuildOutcome::Ready(ready) => {
                            let SemanticBuildReady {
                                index,
                                model,
                                persist_to_disk,
                            } = ready;
                            if persist_to_disk {
                                persist_completed_index(&index, "current generation publish");
                            }
                            let worker_index = index.clone();
                            let worker_handle = spawn_semantic_refresh_worker(
                                root_clone.clone(),
                                worker_index,
                                model,
                                semantic_config.max_batch_size.max(1),
                                semantic_config.max_files,
                                refresh_rx,
                                refresh_event_tx,
                                log_ctx::current_session(),
                            );
                            if let Ok(mut slot) = refresh_worker_slot.lock() {
                                *slot = Some(worker_handle);
                            }
                            SemanticIndexEvent::Ready(index)
                        }
                        SemanticBuildOutcome::Failed(error) => SemanticIndexEvent::Failed(error),
                    };

                    if tx.send(event).is_err() {
                        clear_cold_seed_active();
                    }
                });
            });
        }

        // Clear the workspace package caches here because reconfigure can point AFT at a
        // different root; reset them before warming the callgraph store for the new project.
        crate::callgraph::clear_workspace_package_cache();
    }

    let refresh_project_runtime =
        !equivalent_warm_config || project_root_changed || effective_configure_changed;
    let sync_bash_compress_flag = !equivalent_warm_config
        || previous_config.experimental_bash_compress != next_config.experimental_bash_compress;
    let clear_failed_spawns =
        should_clear_failed_spawns(&previous_config, &next_config, equivalent_warm_config);
    ctx.enqueue_configure_maintenance(ConfigureMaintenanceJob {
        generation: configure_generation,
        root_path: root_path.clone(),
        canonical_cache_root: canonical_cache_root.clone(),
        harness: harness.clone(),
        storage_root: crate::bash_background::storage_dir(next_config.storage_dir.as_deref()),
        harness_dir: ctx.harness_dir(),
        session_id: req.session().to_string(),
        home_match,
        format_tool_cache_clear_needed,
        run_bash_replay: !equivalent_warm_config || first_session_bind,
        refresh_project_runtime,
        sync_bash_compress_flag,
        reset_filter_registry: !equivalent_warm_config,
        clear_failed_spawns,
        warm_callgraph_store: next_config.callgraph_store && !home_match && !equivalent_warm_config,
    });

    slog_info!("project root set: {}", root_path.display());

    let config_snapshot = ctx.config().clone();

    // Defer the full source-file walk + language detection +
    // formatter/checker/LSP missing-binary detection to a background thread.
    // On a normal project this finishes in <1 s and pushes a
    // `ConfigureWarningsFrame` for the plugin to surface; on a huge directory
    // it may take seconds-to-minutes, but configure itself returns now.
    let warnings_pending = !home_match && ctx.progress_sender_handle().is_some();
    if warnings_pending {
        let warning_tx = ctx.configure_warnings_sender();
        let warning_generation = configure_generation;
        let walk_root = root_path.clone();
        let project_root_display = root_path.display().to_string();
        let config_for_bg = config_snapshot.clone();
        let session_id_for_bg = log_ctx::current_session();
        let session_id_for_frame = session_id_for_bg.clone();
        let run_deferred_walk = move || {
            log_ctx::with_session(session_id_for_bg, || {
                delay_configure_deferred_walk_for_test();
                signal_configure_deferred_walk_start_for_test();
                let source_files: Vec<PathBuf> =
                    crate::callgraph::walk_project_files(&walk_root).collect();
                let detected_languages: HashSet<LangId> = source_files
                    .iter()
                    .filter_map(|path| detect_language(path))
                    .collect();
                let mut warnings =
                    detect_missing_tools_for_languages(&detected_languages, &config_for_bg)
                        .into_iter()
                        .map(|warning| json!(warning))
                        .collect::<Vec<_>>();
                warnings.extend(detect_missing_lsp_binaries(&source_files, &config_for_bg));

                let frame = crate::protocol::ConfigureWarningsFrame::new_with_session_id(
                    session_id_for_frame,
                    project_root_display,
                    warnings,
                );
                let _ = warning_tx.send((warning_generation, frame));
            });
        };
        if run_configure_deferred_walk_synchronously_for_test() {
            run_deferred_walk();
        } else {
            thread::spawn(run_deferred_walk);
        }
    }

    // Return the success response immediately so the plugin can mark the project as
    // configured. Missing-binary warnings are sent later in a `configure_warnings`
    // push frame.
    let response = Response::success(
        &req.id,
        json!({
            "project_root": root_path.display().to_string(),
            "warnings": [],
            "warnings_pending": warnings_pending,
            "search_index_cache_reused": search_index_cache_reused,
            "artifact_owner": artifact_owner_status
                .as_ref()
                .map(|status| serde_json::to_value(status).unwrap_or(serde_json::Value::Null)),
            "config_dropped_keys": config_dropped_keys
                .iter()
                .map(|d| json!({ "key": d.key, "tier": d.tier, "reason": d.reason }))
                .collect::<Vec<_>>(),
        }),
    );
    response
}

fn replay_configure_session(ctx: &AppContext, job: &ConfigureMaintenanceJob) {
    crate::bash_background::repair_legacy_root_tasks(&job.storage_root, job.harness.clone());
    #[cfg(test)]
    CONFIGURE_REPLAY_SESSION_CALLS.fetch_add(1, Ordering::SeqCst);
    if let Err(error) = ctx.bash_background().replay_session_for_project(
        &job.harness_dir,
        &job.session_id,
        &job.root_path,
    ) {
        slog_warn!("failed to replay background bash tasks: {error}");
    }
}

pub(crate) fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    for job in ctx.drain_configure_maintenance() {
        if ctx.configure_generation() != job.generation {
            slog_info!(
                "dropping stale configure maintenance for generation {} (current {})",
                job.generation,
                ctx.configure_generation()
            );
            // The superseding configure re-runs everything root-scoped, but
            // bash replay is per-(root, session) and gated on the first bind
            // of that session; forget the binding so the session's next bind
            // replays its tasks instead of losing them to the dropped job.
            if job.run_bash_replay {
                ctx.forget_configure_session_binding(&job.canonical_cache_root, &job.session_id);
            }
            continue;
        }

        let session_only = job.run_bash_replay
            && !job.format_tool_cache_clear_needed
            && !job.refresh_project_runtime
            && !job.sync_bash_compress_flag
            && !job.reset_filter_registry
            && !job.clear_failed_spawns
            && !job.warm_callgraph_store;
        if session_only {
            replay_configure_session(ctx, &job);
            continue;
        }

        delay_configure_deferred_maintenance_for_test();

        if job.format_tool_cache_clear_needed {
            crate::format::clear_tool_cache();
        }

        ctx.backup()
            .lock()
            .set_db_project_key(crate::path_identity::project_scope_key(
                &job.canonical_cache_root,
            ));

        if let Some(storage_dir) = ctx.config().storage_dir.clone() {
            // Ensure the storage root exists for persistence subsystems. This is
            // maintenance work: the configure ack only needs the accepted config
            // snapshot, while disk-backed stores can converge immediately after.
            if let Err(err) = fs::create_dir_all(&storage_dir) {
                slog_warn!(
                    "failed to create storage directory {}: {}",
                    storage_dir.display(),
                    err
                );
            }
            ctx.backup().lock().set_storage_dir_for_harness(
                storage_dir,
                job.harness.clone(),
                ctx.config().checkpoint_ttl_hours,
            );
        }

        if job.refresh_project_runtime {
            // Rebuild gitignore matcher used by the watcher event filter to honor
            // the user's `.gitignore` files instead of a hardcoded directory list.
            // Skipped entirely for home roots because that walk would traverse
            // `$HOME`.
            if !job.home_match {
                ctx.rebuild_gitignore();
            } else {
                ctx.clear_gitignore();
            }
        }

        match crate::url_fetch::cleanup_url_cache(&job.storage_root) {
            Ok(0) => {}
            Ok(n) => slog_info!("URL cache cleanup: removed {} stale entries", n),
            Err(err) => slog_warn!("URL cache cleanup failed: {}", err),
        }

        let db_path = job.storage_root.join("aft.db");
        match ctx.app().open_db(&db_path) {
            Ok(shared) => {
                ctx.backup().lock().set_db_pool(shared.clone());
                ctx.bash_background().set_db_pool(shared);
            }
            Err(err) => {
                // Do not clear the process-shared handle if another root is
                // already using it. A failed root configure must not close that
                // root's SQLite connection and WAL descriptors.
                ctx.app().clear_db_for_path(&db_path);
                ctx.backup().lock().clear_db_pool();
                ctx.bash_background().clear_db_pool();
                slog_warn!(
                    "failed to open aft.db at {}: {} — running with JSON-only persistence",
                    db_path.display(),
                    err
                );
            }
        }

        match crate::migrate_storage::cleanup_staging_dirs(&job.storage_root, job.harness.clone()) {
            Ok(0) => {}
            Ok(n) => slog_info!(
                "swept {} staging directory orphans from prior migrations",
                n
            ),
            Err(err) => slog_warn!(
                "staging cleanup failed: {} (will retry next configure)",
                err
            ),
        }

        let config = ctx.config();
        ctx.bash_background().configure_long_running_reminders(
            config.bash_long_running_reminder_enabled,
            config.bash_long_running_reminder_interval_ms,
        );
        drop(config);

        if job.run_bash_replay {
            replay_configure_session(ctx, &job);
        }

        if job.refresh_project_runtime {
            // FSEvents startup can synchronously wait for seconds on very large
            // roots; configure returns before this maintenance step attaches.
            if !job.home_match {
                install_project_watcher(ctx, &job.canonical_cache_root);
            } else {
                ctx.stop_watcher_runtime();
            }
        }

        if job.sync_bash_compress_flag {
            ctx.sync_bash_compress_flag();
        }
        if job.reset_filter_registry {
            ctx.reset_filter_registry();
        }

        if job.clear_failed_spawns {
            // Forget cached LSP spawn FAILURES when configure inputs changed. A
            // pure equivalent rebind keeps this cache hot; a real config/root
            // change lets the next file event retry previously missing servers.
            let cleared = ctx.lsp().clear_failed_spawns();
            if cleared > 0 {
                slog_debug!(
                    "configure: cleared {} cached LSP spawn failure(s) for retry",
                    cleared
                );
            }
        }

        if job.warm_callgraph_store {
            if ctx.semantic_cold_seed_active() {
                ctx.defer_callgraph_store_warm_for_semantic_cold_seed();
                slog_info!(
                    "callgraph store warm deferred until semantic cold seed gate clears or completes"
                );
            } else {
                match ctx.callgraph_store_for_ops() {
                    CallgraphStoreAccess::Ready(_) => {
                        slog_debug!("callgraph store ready at configure maintenance");
                    }
                    CallgraphStoreAccess::Building => {
                        slog_info!("callgraph store warm build scheduled by configure maintenance");
                    }
                    CallgraphStoreAccess::Unavailable => {
                        slog_info!(
                            "callgraph store unavailable at configure maintenance; dead_code will retry later"
                        );
                    }
                    CallgraphStoreAccess::Error(error) => {
                        slog_warn!("callgraph store configure warm failed: {}", error);
                    }
                }
            }
        }

        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::ffi::OsString;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    use super::{
        external_ignore_watch_paths, install_project_watcher_with, parse_lsp_paths_extra,
        semantic_build_retry_backoff, should_clear_failed_spawns, validate_storage_dir,
    };
    use crate::config::{Config, SemanticBackendConfig};
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use crate::protocol::{ConfigureWarningsFrame, PushFrame, RawRequest, Response};
    use std::process::Command;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    fn git_command(root: &std::path::Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        command
    }

    fn handle_configure_for_test(req: &RawRequest, ctx: &AppContext) -> Response {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        super::handle_configure(req, ctx)
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.take() {
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    fn wait_for_configure_warnings(
        ctx: &AppContext,
        generation: u64,
        timeout: Duration,
    ) -> ConfigureWarningsFrame {
        let deadline = Instant::now() + timeout;
        loop {
            for (frame_generation, frame) in ctx.drain_configure_warnings() {
                if frame_generation == generation {
                    return frame;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for configure warnings frame"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn semantic_build_retry_backoff_ramps_then_holds() {
        assert_eq!(semantic_build_retry_backoff(0), Duration::from_secs(15));
        assert_eq!(semantic_build_retry_backoff(1), Duration::from_secs(30));
        assert_eq!(semantic_build_retry_backoff(2), Duration::from_secs(60));
        // Holds at the cap for all later attempts.
        assert_eq!(semantic_build_retry_backoff(3), Duration::from_secs(60));
        assert_eq!(semantic_build_retry_backoff(99), Duration::from_secs(60));
    }

    fn configure_request(project_root: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "project_root": project_root, "harness": "opencode" }),
        }
    }

    fn configure_request_with_params(params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    fn configure_request_with_session(params: serde_json::Value, session_id: &str) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: Some(session_id.to_string()),
            params,
        }
    }

    fn user_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "user",
            "source": "/u/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn project_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "project",
            "source": "/p/.opencode/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn write_config(path: &std::path::Path, doc: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, doc).unwrap();
    }

    fn init_git_fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("tracked.rs"),
            format!("// fixture:{}\nfn tracked() {{}}\n", root.display()),
        )
        .unwrap();
        assert!(git_command(root)
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success());
        assert!(git_command(root)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(git_command(root)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn configure_without_storage_dir_defaults_to_shared_storage_root() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());

        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success);

        // Plugin-less consumers never send storage_dir; the resolved config
        // must still carry a concrete storage root so artifact lanes that
        // gate persistence on `Some(storage_dir)` (semantic index) write to
        // the same universe as lanes with their own fallback (trigram).
        let resolved = ctx.config().storage_dir.clone();
        assert_eq!(
            resolved,
            Some(crate::bash_background::storage_dir(None)),
            "configure must default storage_dir to the shared storage root"
        );
    }

    fn configure_with_storage(root: &std::path::Path, storage: &std::path::Path) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({ "search_index": true, "semantic_search": false }))],
        }))
    }

    fn configure_semantic_with_storage(
        root: &std::path::Path,
        storage: &std::path::Path,
        base_url: &str,
        semantic_search: bool,
    ) -> RawRequest {
        configure_request_with_params(json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": semantic_search,
                "callgraph_store": false,
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "counting-test-embedding",
                    "base_url": base_url,
                    "timeout_ms": 5_000,
                    "max_batch_size": 64,
                    "max_files": 1_000
                }
            }))],
        }))
    }

    fn owner_manifest_from_response(
        response: &Response,
    ) -> crate::artifact_owner::ArtifactOwnerManifest {
        let manifest_path = response.data["artifact_owner"]["manifest_path"]
            .as_str()
            .expect("artifact owner manifest path");
        let bytes = std::fs::read(manifest_path).expect("read owner manifest");
        serde_json::from_slice(&bytes).expect("parse owner manifest")
    }

    fn semantic_cache_file(
        storage: &std::path::Path,
        root: &std::path::Path,
    ) -> std::path::PathBuf {
        let project_key = crate::search_index::artifact_cache_key(root);
        storage
            .join("semantic")
            .join(project_key)
            .join("semantic.bin")
    }

    struct CountingEmbeddingServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl CountingEmbeddingServer {
        fn start(delay: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding mock");
            let addr = listener.local_addr().expect("embedding mock addr");
            listener
                .set_nonblocking(true)
                .expect("embedding mock nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop_for_thread = Arc::clone(&stop);
            let requests_for_thread = Arc::clone(&requests);
            let handle = std::thread::spawn(move || {
                while !stop_for_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests_for_thread);
                            std::thread::spawn(move || {
                                handle_counting_embedding_request(stream, delay, requests)
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("embedding mock accept failed: {error}"),
                    }
                }
            });
            Self {
                base_url: format!("http://{addr}"),
                stop,
                requests,
                handle: Some(handle),
            }
        }

        fn non_probe_input_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .filter(|text| text.as_str() != "semantic index fingerprint probe")
                .count()
        }
    }

    impl Drop for CountingEmbeddingServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect(self.base_url.trim_start_matches("http://"));
            if let Some(handle) = self.handle.take() {
                handle.join().expect("embedding mock joins");
            }
        }
    }

    fn handle_counting_embedding_request(
        mut stream: std::net::TcpStream,
        delay: Duration,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
    ) {
        stream
            .set_nonblocking(false)
            .expect("embedding request stream blocking mode");
        let mut reader = BufReader::new(stream.try_clone().expect("clone embedding stream"));
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read embedding header");
            if read == 0 || line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("read embedding body");
        let request_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let inputs = match request_body.get("input") {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>(),
            Some(serde_json::Value::String(value)) => vec![value.clone()],
            _ => vec![String::new()],
        };
        requests.lock().unwrap().push(inputs.clone());
        std::thread::sleep(delay);
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let base = index as f64 + 1.0;
                json!({
                    "embedding": [base, base + 0.1, base + 0.2],
                    "index": index,
                })
            })
            .collect::<Vec<_>>();
        let response_body = json!({ "data": data }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    }

    fn wait_for_semantic_build_ready(ctx: &AppContext, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            crate::runtime_drain::drain_build_completions(ctx);
            let ready = ctx
                .semantic_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if ready && ctx.semantic_index_rx().lock().is_none() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for semantic build"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn configure_user_file_wins_project_wire_falls_back_per_tier() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        write_config(&user_path, r#"{ "format_on_edit": false }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": true, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 3 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(!ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 3);
    }

    #[test]
    fn configure_project_file_wins_user_wire_falls_back_per_tier() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = temp.path().join(".cortexkit/aft.jsonc");
        write_config(&project_path, r#"{ "callgraph_chunk_size": 7 }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 4 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 7);
    }

    #[test]
    fn configure_both_files_ignore_wire_tiers() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let project_path = temp.path().join(".cortexkit/aft.jsonc");
        write_config(
            &user_path,
            r#"{ "format_on_edit": false, "url_fetch_allow_private": false }"#,
        );
        write_config(&project_path, r#"{ "callgraph_chunk_size": 9 }"#);
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": true, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 2 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(!ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 9);
    }

    #[test]
    fn configure_neither_file_uses_wire_tiers_for_old_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let user_path = temp.path().join("xdg/cortexkit/aft.jsonc");
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "cortexkit_user_config_path": user_path,
            "config": [
                user_tier(json!({ "format_on_edit": false, "url_fetch_allow_private": true })),
                project_tier(json!({ "callgraph_chunk_size": 11 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert!(ctx.config().url_fetch_allow_private);
        assert_eq!(ctx.config().callgraph_chunk_size, 11);
    }

    #[test]
    fn configure_accepts_old_plugin_wire_without_cortexkit_user_path() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [
                user_tier(json!({ "format_on_edit": false })),
                project_tier(json!({ "callgraph_chunk_size": 13 }))
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);
        assert!(!ctx.config().format_on_edit);
        assert_eq!(ctx.config().callgraph_chunk_size, 13);
    }

    #[test]
    fn configure_resolves_config_tiers_and_surfaces_dropped_keys() {
        // P1 receiver half: configure accepts raw config tiers, resolves them in
        // core (merge + trust boundary), and surfaces project-tier drops.
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [
                { "tier": "user", "source": "/u/aft.jsonc",
                  "doc": "{ \"restrict_to_project_root\": true, \"search_index\": true, \"backup\": { \"enabled\": false, \"max_depth\": 7 }, \"disabled_tools\": [\"aft_safety\"] }" },
                { "tier": "project", "source": "/p/.opencode/aft.jsonc",
                  "doc": "{ \"restrict_to_project_root\": false, \"semantic\": { \"api_key_env\": \"EVIL\" }, \"backup\": { \"enabled\": true, \"max_depth\": 1 }, \"disabled_tools\": [\"aft_safety\"] }" }
            ]
        }));

        let response = handle_configure_for_test(&req, &ctx);
        assert!(response.success, "configure failed: {:?}", response.data);

        // Core-resolved field applied: user search_index=true survived.
        assert!(ctx.config().search_index);
        // Trust boundary: project tried restrict=false over user restrict=true →
        // user value wins.
        assert!(ctx.config().restrict_to_project_root);
        // Project semantic.api_key_env never reached Config.
        assert!(ctx.config().semantic.api_key_env.is_none());
        assert_eq!(ctx.config().backup.enabled, Some(false));
        assert_eq!(ctx.config().backup.max_depth, Some(7));

        // Drops surfaced for the warning path.
        let dropped = response.data["config_dropped_keys"].as_array().unwrap();
        let keys: Vec<&str> = dropped.iter().filter_map(|d| d["key"].as_str()).collect();
        assert!(keys.contains(&"restrict_to_project_root"), "keys: {keys:?}");
        assert!(keys.contains(&"semantic.api_key_env"), "keys: {keys:?}");
        assert!(keys.contains(&"backup"), "keys: {keys:?}");
        assert!(
            keys.contains(&"disabled_tools.aft_safety"),
            "keys: {keys:?}"
        );
    }

    #[test]
    fn configure_without_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({ "project_root": temp.path() }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert_eq!(
            response.data["message"],
            "configure payload missing required field 'harness'; expected 'opencode', 'pi', 'runner', 'mcp:<client>', or 'fed:<fingerprint>'"
        );
    }

    #[test]
    fn configure_with_invalid_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "claude_code"
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn harness_set_on_appcontext_after_configure() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "pi"
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(ctx.harness(), crate::harness::Harness::Pi);
        assert_eq!(ctx.config().harness, Some(crate::harness::Harness::Pi));
    }

    #[test]
    fn handle_configure_rejects_relative_project_root() {
        let ctx = test_context();
        let req = configure_request(json!("relative/path"));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn handle_configure_populates_canonical_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request(json!(temp.path()));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(
            ctx.canonical_cache_root(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(ctx.cache_role(), "main");
    }

    #[test]
    fn configure_reuses_cached_worktree_probe_until_forced_to_reprobe() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let storage = temp.path().join("storage");
        init_git_fixture(&root);
        let ctx = test_context();
        let request = || {
            configure_request_with_params(json!({
                "project_root": root.clone(),
                "harness": "opencode",
                "storage_dir": storage.clone(),
                "config": [user_tier(json!({
                    "search_index": false,
                    "semantic_search": false,
                    "callgraph_store": false,
                }))],
            }))
        };

        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 1);
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            1,
            "an equivalent configure must reuse the successful git topology probe"
        );

        filetime::set_file_mtime(
            root.join(".git"),
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() + Duration::from_secs(5),
            ),
        )
        .expect("advance root git marker mtime");
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            2,
            "a changed root .git marker must invalidate the cached topology"
        );

        ctx.force_worktree_bridge_reprobe_for_test(true);
        assert!(handle_configure_for_test(&request(), &ctx).success);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 3);
        ctx.force_worktree_bridge_reprobe_for_test(false);
    }

    #[test]
    fn handle_configure_rejects_git_like_root_when_cache_key_probe_fails_without_memo() {
        let _probe_lock = crate::search_index::git_root_commit_probe_override_lock_for_test();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("create git marker");
        let storage = temp.path().join("storage");
        let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
        let _override =
            crate::search_index::force_git_root_commit_probe_transient_for_paths_for_test(
                vec![root.clone(), canonical_root],
                "spawn failed: Too many open files (os error 24)",
            );
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.clone(),
            "harness": "opencode",
            "storage_dir": storage.clone(),
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false,
            }))],
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(
            !response.success,
            "configure must reject ambiguous git identity"
        );
        assert_eq!(response.data["code"], "cache_key_probe_failed");
        assert_eq!(response.data["retryable"], true);
        let path_key = crate::search_index::artifact_path_identity_key_for_test(&root);
        assert!(!storage.join("index").join(&path_key).exists());
        assert!(!storage.join("semantic").join(&path_key).exists());
        assert!(!storage.join("callgraph").join(&path_key).exists());
    }

    #[test]
    fn sibling_clone_same_artifact_key_opens_shared_artifacts_read_only() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let owner = temp.path().join("owner");
        init_git_fixture(&owner);
        let sibling = temp.path().join("sibling");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&owner)
            .arg(&sibling)
            .status()
            .unwrap()
            .success());

        let owner_ctx = test_context();
        let owner_response =
            handle_configure_for_test(&configure_with_storage(&owner, &storage), &owner_ctx);
        assert!(owner_response.success);
        assert_eq!(owner_ctx.cache_role(), "main");

        let sibling_ctx = test_context();
        let sibling_response =
            handle_configure_for_test(&configure_with_storage(&sibling, &storage), &sibling_ctx);

        assert!(sibling_response.success);
        assert_eq!(sibling_ctx.cache_role(), "read_only");
        assert!(sibling_ctx.shared_artifacts_read_only());
        assert_eq!(
            sibling_response.data["artifact_owner"]["mode"],
            json!("read_only")
        );
        assert!(sibling_response.data["artifact_owner"]["note"]
            .as_str()
            .unwrap()
            .contains("shared artifacts opened read-only"));
        assert!(sibling_ctx.search_index_rx().read().unwrap().is_none());
    }

    #[test]
    fn detect_worktree_bridge_returns_common_dir_for_main_and_linked_worktree() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let canonical_main = std::fs::canonicalize(&main).unwrap();
        let canonical_worktree = std::fs::canonicalize(&worktree).unwrap();
        let ctx = test_context();
        let (main_is_worktree, main_common) = super::detect_worktree_bridge(&ctx, &canonical_main);
        let (linked_is_worktree, linked_common) =
            super::detect_worktree_bridge(&ctx, &canonical_worktree);

        assert!(!main_is_worktree);
        assert!(linked_is_worktree);
        let expected_common = canonical_main.join(".git");
        assert_eq!(main_common.as_deref(), Some(expected_common.as_path()));
        assert_eq!(linked_common, main_common);
        assert_eq!(ctx.worktree_bridge_probe_spawns_for_test(), 2);

        let repeated_main = super::detect_worktree_bridge(&ctx, &canonical_main);
        let repeated_worktree = super::detect_worktree_bridge(&ctx, &canonical_worktree);
        assert_eq!(repeated_main, (main_is_worktree, main_common));
        assert_eq!(repeated_worktree, (linked_is_worktree, linked_common));
        assert_eq!(
            ctx.worktree_bridge_probe_spawns_for_test(),
            2,
            "main and linked-worktree roots must each retain their own cached result"
        );
    }

    #[test]
    fn worktree_then_main_claim_sequence_ends_with_main_owner() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let worktree_ctx = test_context();
        let worktree_response =
            handle_configure_for_test(&configure_with_storage(&worktree, &storage), &worktree_ctx);
        assert!(worktree_response.success);
        assert_eq!(worktree_ctx.cache_role(), "worktree");
        assert!(worktree_ctx.shared_artifacts_read_only());
        let borrowed_manifest_path = worktree_response.data["artifact_owner"]["manifest_path"]
            .as_str()
            .unwrap();
        assert!(
            !std::path::Path::new(borrowed_manifest_path).exists(),
            "linked worktree must not create the family owner manifest"
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_ctx.cache_role(), "main");
        assert!(!main_ctx.shared_artifacts_read_only());
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        let manifest = owner_manifest_from_response(&main_response);
        assert_eq!(
            manifest.checkout_path,
            std::fs::canonicalize(&main).unwrap().display().to_string()
        );
    }

    #[test]
    fn main_then_worktree_keeps_main_owner_and_worktree_borrows_read_only() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        let main_manifest = owner_manifest_from_response(&main_response);

        let worktree_ctx = test_context();
        let worktree_response =
            handle_configure_for_test(&configure_with_storage(&worktree, &storage), &worktree_ctx);
        assert!(worktree_response.success);
        assert_eq!(worktree_ctx.cache_role(), "worktree");
        assert!(worktree_ctx.shared_artifacts_read_only());
        assert_eq!(
            worktree_response.data["artifact_owner"]["mode"],
            json!("read_only")
        );
        let manifest_after_worktree = owner_manifest_from_response(&main_response);
        assert_eq!(
            manifest_after_worktree.project_scope_key,
            main_manifest.project_scope_key
        );
        assert_eq!(
            manifest_after_worktree.checkout_path,
            main_manifest.checkout_path
        );
    }

    #[test]
    fn main_bind_self_heals_live_worktree_owner_manifest() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );
        let canonical_main = std::fs::canonicalize(&main).unwrap();
        let canonical_worktree = std::fs::canonicalize(&worktree).unwrap();
        let project_key = crate::search_index::artifact_cache_key(&canonical_main);
        let worktree_scope = crate::path_identity::project_scope_key(&canonical_worktree);
        let worktree_probe_ctx = test_context();
        let (_, common_dir) =
            super::detect_worktree_bridge(&worktree_probe_ctx, &canonical_worktree);
        let common_dir = common_dir.expect("linked worktree common dir");
        crate::artifact_owner::write_synthetic_manifest_with_git_common_dir_for_test(
            &storage,
            &canonical_worktree,
            &project_key,
            &worktree_scope,
            std::process::id(),
            0,
            Some(&common_dir),
        );

        let main_ctx = test_context();
        let main_response =
            handle_configure_for_test(&configure_with_storage(&main, &storage), &main_ctx);
        assert!(main_response.success);
        assert_eq!(main_response.data["artifact_owner"]["mode"], json!("owner"));
        assert_eq!(main_ctx.cache_role(), "main");
        let manifest = owner_manifest_from_response(&main_response);
        assert_eq!(manifest.checkout_path, canonical_main.display().to_string());
        assert_eq!(
            manifest.project_scope_key,
            crate::path_identity::project_scope_key(&canonical_main)
        );
        let common_dir_string = common_dir.display().to_string();
        assert_eq!(
            manifest.git_common_dir.as_deref(),
            Some(common_dir_string.as_str())
        );
    }

    #[test]
    fn stale_generation_semantic_build_persists_and_followup_refreshes_incrementally() {
        let _env_lock = home_env_mutex();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        super::reset_semantic_stale_generation_discards_for_test();
        let server = CountingEmbeddingServer::start(Duration::from_millis(50));
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let storage = temp.path().join("storage");
        std::fs::create_dir_all(project.join("src")).unwrap();
        for name in ["alpha", "beta", "gamma", "delta"] {
            std::fs::write(
                project.join("src").join(format!("{name}.rs")),
                format!("pub fn {name}_symbol() -> usize {{ 1 }}\n"),
            )
            .unwrap();
        }
        let semantic_file = semantic_cache_file(&storage, &project);

        let first_ctx = test_context();
        let first_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, true),
            &first_ctx,
        );
        assert!(
            first_response.success,
            "configure failed: {:?}",
            first_response.data
        );
        let disabled_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, false),
            &first_ctx,
        );
        assert!(
            disabled_response.success,
            "disable configure failed: {:?}",
            disabled_response.data
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if super::semantic_stale_generation_discards_for_test() > 0 && semantic_file.is_file() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for stale semantic build to persist"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let initial_non_probe_inputs = server.non_probe_input_count();
        assert!(
            initial_non_probe_inputs >= 4,
            "initial build should embed the corpus, saw {initial_non_probe_inputs} inputs"
        );

        std::fs::write(
            project.join("src").join("epsilon.rs"),
            "pub fn epsilon_symbol() -> usize { 5 }\n",
        )
        .unwrap();

        let before_followup_inputs = server.non_probe_input_count();
        let second_ctx = test_context();
        let second_response = handle_configure_for_test(
            &configure_semantic_with_storage(&project, &storage, &server.base_url, true),
            &second_ctx,
        );
        assert!(
            second_response.success,
            "second configure failed: {:?}",
            second_response.data
        );
        wait_for_semantic_build_ready(&second_ctx, Duration::from_secs(5));
        let followup_inputs = server
            .non_probe_input_count()
            .saturating_sub(before_followup_inputs);
        assert!(
            followup_inputs < initial_non_probe_inputs,
            "follow-up build should use the persisted index and embed only the delta; initial={initial_non_probe_inputs}, followup={followup_inputs}"
        );
        assert!(
            followup_inputs <= 2,
            "expected only the new file's semantic chunks to be embedded, got {followup_inputs}"
        );
        super::reset_semantic_stale_generation_discards_for_test();
    }

    #[test]
    fn linked_worktree_configure_schedules_no_cold_warm_builds() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        init_git_fixture(&main);
        let worktree = temp.path().join("worktree");
        let mut worktree_command = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(worktree_command.arg("-C").arg(&main))
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&worktree)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );

        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": worktree,
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": true,
                "callgraph_store": true
            }))]
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(response.success);
        assert_eq!(ctx.cache_role(), "worktree");
        assert!(ctx.search_index_rx().read().unwrap().is_none());
        assert!(ctx.semantic_index_rx().lock().is_none());
        assert!(ctx.callgraph_store_rx().lock().is_none());
    }

    #[test]
    fn configure_defers_large_tree_file_walk_until_after_ack() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let _delay_walk = EnvVarGuard::set("AFT_TEST_CONFIGURE_DEFERRED_WALK_DELAY_MS", "2000");
        let temp = tempfile::tempdir().unwrap();
        let walk_start_file = temp.path().join("deferred-walk-start");
        let _walk_start_signal = EnvVarGuard::set(
            "AFT_TEST_CONFIGURE_DEFERRED_WALK_START_FILE",
            walk_start_file.to_str().unwrap(),
        );
        init_git_fixture(temp.path());
        for dir in 0..10 {
            let dir_path = temp.path().join(format!("bulk-{dir}"));
            std::fs::create_dir_all(&dir_path).unwrap();
            for file in 0..40 {
                std::fs::write(dir_path.join(format!("file-{file}.rs")), "fn main() {}\n").unwrap();
            }
        }

        let ctx = test_context();
        ctx.set_progress_sender(Some(Arc::new(Box::new(|_frame: PushFrame| {}))));
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));

        let start = Instant::now();
        let response = handle_configure_for_test(&req, &ctx);
        let elapsed = start.elapsed();

        assert!(response.success);
        assert!(
            elapsed < Duration::from_secs(5),
            "configure acknowledgement exceeded its generous sanity ceiling: {elapsed:?}"
        );
        assert!(
            !walk_start_file.exists(),
            "configure acknowledgement was not observed before the deferred file walk started"
        );
        assert!(response.data.get("source_file_count").is_none());
        assert!(ctx.drain_configure_warnings().is_empty());

        let walk_start_deadline = Instant::now() + Duration::from_secs(5);
        while !walk_start_file.exists() {
            assert!(
                Instant::now() < walk_start_deadline,
                "timed out waiting for the deferred file walk to start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The deferred walk still completes and publishes its frame (for
        // missing-binary warnings); only the count field is gone.
        let frame =
            wait_for_configure_warnings(&ctx, ctx.configure_generation(), Duration::from_secs(5));
        assert_eq!(frame.frame_type, "configure_warnings");
    }

    #[test]
    fn equivalent_reconfigure_keeps_warm_work_adopted_and_idempotent() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));

        let first = handle_configure_for_test(&req, &ctx);
        assert!(first.success);
        super::drain_deferred_configure_maintenance(&ctx);

        let generation_after_first = ctx.configure_generation();
        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "initial configure maintenance must not inspect backup directories"
        );
        let tsconfig_clear_generation_after_first =
            ctx.tsconfig_membership_clear_generation_for_test();
        let filter_rebuilds_after_first = ctx.filter_registry_rebuild_count_for_test();
        let artifact_derivations_after_first = ctx.artifact_cache_key_derivation_count_for_test();
        assert_eq!(
            artifact_derivations_after_first, 0,
            "configure should not derive an unused artifact key when artifact-backed features are disabled"
        );
        for _ in 0..5 {
            let response = handle_configure_for_test(&req, &ctx);
            assert!(response.success);
            super::drain_deferred_configure_maintenance(&ctx);
        }

        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "equivalent configures must not inspect backup directories"
        );
        assert!(ctx.search_index_rx().read().unwrap().is_none());
        assert!(ctx.semantic_index_rx().lock().is_none());
        assert!(ctx.callgraph_store_rx().lock().is_none());
        assert_eq!(
            ctx.tsconfig_membership_clear_generation_for_test(),
            tsconfig_clear_generation_after_first,
            "equivalent rebind must keep the tsconfig-membership cache hot"
        );
        assert_eq!(
            ctx.filter_registry_rebuild_count_for_test(),
            filter_rebuilds_after_first,
            "equivalent rebind must not rebuild the TOML filter registry"
        );
        assert_eq!(
            ctx.artifact_cache_key_derivation_count_for_test(),
            artifact_derivations_after_first,
            "equivalent rebind must reuse the artifact cache key"
        );
        // Load-bearing: in-flight build workers publish only while the
        // generation flag equals their spawn generation. If equivalent
        // rebinds advanced it, every rebind during a long build would
        // silently discard the build's result at completion.
        assert_eq!(ctx.configure_generation(), generation_after_first);

        // A genuinely different warm config must still advance (this is what
        // cancels superseded in-flight builds).
        let changed = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "search_index": true,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        }));
        let response = handle_configure_for_test(&changed, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(ctx.configure_generation(), generation_after_first + 1);
        assert!(ctx.config().search_index, "changed config must apply fully");
        assert_eq!(
            ctx.tsconfig_membership_clear_generation_for_test(),
            tsconfig_clear_generation_after_first + 1
        );
        assert_eq!(
            ctx.filter_registry_rebuild_count_for_test(),
            filter_rebuilds_after_first + 1
        );
    }

    #[test]
    fn equivalent_reconfigure_replays_new_sessions_but_not_same_session_rebinds() {
        let _env_guard = home_env_mutex();
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _disable_watcher = EnvVarGuard::set("AFT_TEST_DISABLE_FILE_WATCHER", "1");
        super::reset_configure_replay_session_calls_for_test();
        let temp = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        init_git_fixture(temp.path());
        let ctx = test_context();
        let params = json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "storage_dir": storage.path(),
            "config": [user_tier(json!({
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false
            }))]
        });

        let session_a = configure_request_with_session(params.clone(), "session-a");
        let response = handle_configure_for_test(&session_a, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(super::configure_replay_session_calls_for_test(), 1);
        assert_eq!(ctx.backup().lock().disk_io_count_for_tests(), 0);

        let response = handle_configure_for_test(&session_a, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(
            super::configure_replay_session_calls_for_test(),
            1,
            "equivalent rebind for an already-bound session should skip replay"
        );

        let session_b = configure_request_with_session(params, "session-b");
        let response = handle_configure_for_test(&session_b, &ctx);
        assert!(response.success);
        super::drain_deferred_configure_maintenance(&ctx);
        assert_eq!(
            super::configure_replay_session_calls_for_test(),
            2,
            "a new session on an equivalent warm root still needs session replay"
        );
        assert_eq!(
            ctx.backup().lock().disk_io_count_for_tests(),
            0,
            "a fresh-session bind must not inspect backup directories"
        );
    }

    #[test]
    fn dead_artifact_owner_manifest_is_taken_over_on_configure() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let _artifact_guard = artifact_owner_test_mutex().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let owner = temp.path().join("owner");
        init_git_fixture(&owner);
        let sibling = temp.path().join("sibling");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&owner)
            .arg(&sibling)
            .status()
            .unwrap()
            .success());
        let key = crate::search_index::artifact_cache_key(&owner);
        crate::artifact_owner::write_synthetic_manifest_for_test(
            &storage,
            &owner,
            &key,
            "dead-owner",
            0,
            0,
        );

        let sibling_ctx = test_context();
        let sibling_response =
            handle_configure_for_test(&configure_with_storage(&sibling, &storage), &sibling_ctx);

        assert!(sibling_response.success);
        assert_eq!(sibling_ctx.cache_role(), "main");
        assert_eq!(
            sibling_response.data["artifact_owner"]["mode"],
            json!("owner")
        );
    }

    #[test]
    fn semantic_file_cap_counts_only_semantic_extensions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        for index in 0..5 {
            std::fs::write(
                temp.path().join(format!("asset-{index}.bin")),
                format!("asset {index}"),
            )
            .unwrap();
        }

        let files = super::walk_semantic_project_files_bounded(temp.path(), 1)
            .expect("one semantic file should be within cap");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src/lib.rs"));

        std::fs::write(temp.path().join("src/second.rs"), "pub fn two() {}\n").unwrap();
        assert!(super::walk_semantic_project_files_bounded(temp.path(), 1).is_err());
    }

    #[test]
    fn configure_missing_tools_warns_for_explicit_oxfmt_formatter() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "oxfmt".to_string());
        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[test]
    fn detect_missing_tools_skips_formatters_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("biome.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert!(
            warnings.is_empty(),
            "format_on_edit:false should suppress derived formatter warnings: {warnings:?}"
        );
    }

    #[test]
    fn detect_missing_tools_still_warns_explicit_formatter_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "biome".to_string());
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].tool, "biome");
    }

    #[test]
    fn configure_missing_tools_warns_for_oxfmt_project_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".oxfmtrc.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };

        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[cfg(unix)]
    #[test]
    fn configure_missing_tools_uses_shared_go_tool_resolution() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.test\ngo 1.21\n").unwrap();
        let bin_dir = temp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;

        let go = bin_dir.join("go");
        std::fs::write(
            &go,
            "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then exit 0; fi\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();

        let gofmt = bin_dir.join("gofmt");
        std::fs::write(
            &gofmt,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\ncat >/dev/null\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&gofmt, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut languages = std::collections::HashSet::new();
        languages.insert(crate::parser::LangId::Go);
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);

        assert!(
            warnings.is_empty(),
            "expected shared Go resolver to avoid false missing-tool warnings, got {warnings:?}"
        );
    }

    /// Serialize the home-root tests below on the process-wide env lock.
    /// They mutate process-global `HOME` / `USERPROFILE` env vars, and `cargo
    /// test` runs unit tests concurrently within the same process — without
    /// serialization a parallel `set_var("HOME", X)` in another env-mutating
    /// test (e.g. the gitignore-neutralizing tests in `context.rs`) can race
    /// `resolve_home_dir()` here and produce flaky failures. A module-local
    /// mutex is not enough: the lock must be shared with every other test
    /// that touches these variables.
    fn home_env_mutex() -> crate::test_env::ProcessEnvLockGuard {
        crate::test_env::process_env_lock()
    }

    /// Shared mutex serializing the watcher tests below. They install watcher
    /// runtimes on an `AppContext`, and each test must stop its runtime before
    /// the next one starts.
    fn watcher_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn artifact_owner_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn handle_configure_enters_degraded_mode_when_project_root_is_home() {
        let _guard = home_env_mutex();
        // Simulate the Desktop-launches-from-`~` case by pointing `HOME` at a
        // tempdir and using that same tempdir as `project_root`. The
        // canonical-equality check inside `handle_configure` is the same
        // mechanism that catches real `$HOME` regardless of HOME mutation.
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();

        // Save + restore HOME so we don't pollute other tests in the
        // same process (Rust runs tests in parallel by default but env
        // mutation is process-global). For Windows, USERPROFILE is the
        // var `resolve_home_dir` checks after HOME.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: env mutation is sound here — this is single-threaded test
        // setup. The matching restore at the bottom runs even on assertion
        // failure because Rust's panic path executes drop order, but we make
        // the env writes explicit for clarity.
        unsafe {
            std::env::set_var("HOME", &canonical);
            std::env::set_var("USERPROFILE", &canonical);
        }

        let ctx = test_context();
        // Supply search_index/semantic_search as a user config TIER (how real
        // configures carry core fields under reset resolution). The point of this
        // test is that the HOME-root degraded gate force-disables them EVEN WHEN
        // user config enables them — so they must actually be enabled by the tier
        // first, then asserted off below.
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "opencode",
            "config": [user_tier(json!({ "search_index": true, "semantic_search": true }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);

        // Restore env immediately so a later assertion failure doesn't leak.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }

        assert!(response.success);
        assert!(ctx.is_degraded(), "expected degraded mode for HOME root");
        assert!(
            !ctx.heavy_root_work_allowed(),
            "HOME root configure must close the heavy-root-work gate"
        );
        assert!(
            ctx.degraded_reasons().contains(&"home_root".to_string()),
            "expected `home_root` reason, got {:?}",
            ctx.degraded_reasons()
        );
        // Heavy subsystems must have been force-disabled regardless of user config.
        assert!(
            !ctx.config().search_index,
            "search_index must be auto-disabled at HOME root"
        );
        assert!(
            !ctx.config().semantic_search,
            "semantic_search must be auto-disabled at HOME root"
        );
    }

    #[test]
    fn handle_configure_stays_full_featured_for_subdirectory_of_home() {
        let _guard = home_env_mutex();
        // A real subdirectory of `$HOME` (the legitimate case: most projects
        // live under `~/Work`, `~/Documents`, etc.) must NOT trip the
        // degraded gate. We point HOME at a tempdir and configure against a
        // nested subdir to confirm subdirs pass through.
        let temp = tempfile::tempdir().unwrap();
        let subdir = temp.path().join("project");
        std::fs::create_dir(&subdir).unwrap();
        let canonical_home = std::fs::canonicalize(temp.path()).unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &canonical_home);
            std::env::set_var("USERPROFILE", &canonical_home);
        }

        let ctx = test_context();
        // search_index is a core-domain field that arrives via a config TIER on
        // every real configure (P1 relocation). With reset-onto-default resolution
        // it must be supplied as a tier, not seeded via update_config — a tier-less
        // configure correctly resets core fields to default.
        let req = configure_request_with_params(json!({
            "project_root": subdir,
            "harness": "opencode",
            "config": [user_tier(json!({ "search_index": true }))],
        }));
        let response = handle_configure_for_test(&req, &ctx);

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }

        assert!(response.success);
        assert!(
            !ctx.is_degraded(),
            "subdirectories of $HOME must not enter degraded mode"
        );
        assert!(
            ctx.heavy_root_work_allowed(),
            "subdirectories of $HOME must keep heavy root work enabled"
        );
        assert!(
            ctx.degraded_reasons().is_empty(),
            "expected no degraded reasons, got {:?}",
            ctx.degraded_reasons()
        );
        // User config preserved.
        assert!(ctx.config().search_index);
    }

    #[cfg(unix)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_dir(src, dst).unwrap();
    }

    #[cfg(unix)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_file(src, dst).unwrap();
    }

    #[test]
    fn validate_storage_dir_requires_absolute_paths() {
        assert!(validate_storage_dir("relative/cache").is_err());
    }

    #[test]
    fn validate_storage_dir_normalizes_safe_parents() {
        let base = std::env::temp_dir();
        let path = base.join("aft-config-test").join("..").join("cache");
        assert_eq!(
            validate_storage_dir(path.to_str().unwrap()).unwrap(),
            base.join("cache")
        );
    }

    #[test]
    fn validate_storage_dir_rejects_relative_with_dotdot() {
        // Relative paths with .. are rejected (not absolute)
        assert!(validate_storage_dir("../../../etc/passwd").is_err());
    }

    // Unix-only: on Windows, `\..\..\cache` isn't an absolute path (no
    // drive letter), so the dotdot-normalization-of-absolute-path
    // semantics this test asserts don't apply.
    #[cfg(unix)]
    #[test]
    fn validate_storage_dir_accepts_absolute_with_dotdot_that_normalizes() {
        // /../../cache normalizes to /cache which is a valid absolute path
        let mut path = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
        path.push("..");
        path.push("..");
        path.push("cache");
        assert!(validate_storage_dir(path.to_str().unwrap()).is_ok());
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_existing_directory_after_canonicalize() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cache").join("node_modules").join(".bin");
        std::fs::create_dir_all(&dir).unwrap();

        let paths = parse_lsp_paths_extra(&json!([dir])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&dir).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_nonexistent_directory_for_later_install() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("pending").join("node_modules").join(".bin");

        let paths = parse_lsp_paths_extra(&json!([missing])).unwrap();

        assert_eq!(paths, vec![missing]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "not a directory").unwrap();

        let error = parse_lsp_paths_extra(&json!([file])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let traversing = tmp.path().join("project").join("..").join("outside");

        let error = parse_lsp_paths_extra(&json!([traversing])).unwrap_err();

        assert!(error.contains("must not contain '..' traversal"));
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_symlink_to_directory_as_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-dir");
        let link = tmp.path().join("linked-dir");
        std::fs::create_dir_all(&target).unwrap();
        create_dir_symlink(&target, &link);

        let paths = parse_lsp_paths_extra(&json!([link])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&target).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_symlink_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-file");
        let link = tmp.path().join("linked-file");
        std::fs::write(&target, "not a directory").unwrap();
        create_file_symlink(&target, &link);

        let error = parse_lsp_paths_extra(&json!([link])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn watcher_attach_runs_off_configure_foreground_when_slow() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let attach_started = Arc::new(Barrier::new(2));
        let attach_started_for_thread = Arc::clone(&attach_started);

        let started = Instant::now();
        install_project_watcher_with(
            &ctx,
            root.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, _tx| {
                attach_started_for_thread.wait();
                std::thread::sleep(Duration::from_millis(250));
                Ok::<(), &'static str>(())
            },
        );

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "watcher installation should not wait for slow attach"
        );
        assert!(ctx.watcher_rx().lock().is_some());
        assert!(ctx.watcher().lock().is_none());

        attach_started.wait();
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn watcher_attach_failure_reports_error_on_receiver() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());

        install_project_watcher_with(
            &ctx,
            root.path(),
            Vec::new(),
            |_root, _extra_watch_paths, _tx| Err::<(), _>("no watcher backend"),
        );

        let event = ctx
            .watcher_rx()
            .lock()
            .as_ref()
            .expect("watcher receiver installed")
            .recv_timeout(Duration::from_secs(2))
            .expect("watcher error event");
        match event {
            crate::watcher_filter::WatcherDispatchEvent::Error(error) => {
                assert!(error.contains("no watcher backend"));
            }
            other => panic!("unexpected watcher event: {other:?}"),
        }
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn watcher_reconfigure_does_not_leak_filter_threads() {
        let _guard = watcher_test_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        struct FakeWatcher {
            _tx: mpsc::Sender<notify::Result<notify::Event>>,
            drops: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Drop for FakeWatcher {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let root1 = tempfile::tempdir().unwrap();
        let root2 = tempfile::tempdir().unwrap();
        let root3 = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root1.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root2.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "first watcher should be dropped on reconfigure"
        );

        let drops_for_watcher = Arc::clone(&drops);
        install_project_watcher_with(
            &ctx,
            root3.path(),
            Vec::new(),
            move |_root, _extra_watch_paths, tx| {
                Ok::<_, &'static str>(FakeWatcher {
                    _tx: tx,
                    drops: drops_for_watcher,
                })
            },
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            2,
            "second watcher should be dropped on reconfigure"
        );

        ctx.stop_watcher_runtime();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            3,
            "final watcher should be dropped on explicit shutdown"
        );
    }

    #[test]
    fn external_ignore_watch_paths_includes_git_common_info_exclude() {
        let root = tempfile::tempdir().unwrap();
        let common = tempfile::tempdir().unwrap();
        let info = common.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        std::fs::write(
            &exclude,
            "ignored/
",
        )
        .unwrap();

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.set_cache_role(false, Some(common.path().to_path_buf()));

        let paths = external_ignore_watch_paths(&ctx, root.path());

        assert!(paths.contains(&exclude));
    }

    #[test]
    fn invalid_late_configure_field_does_not_mutate_existing_context() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": first.path(),
            "harness": "opencode",
            "config": [user_tier(json!({ "format_on_edit": true }))]
        }));
        let first_response = handle_configure_for_test(&first_req, &ctx);
        assert!(first_response.success);
        let canonical_before = ctx.canonical_cache_root();

        let invalid_req = configure_request_with_params(json!({
            "project_root": second.path(),
            "harness": "pi",
            "max_background_bash_tasks": 0
        }));
        let invalid_response = handle_configure_for_test(&invalid_req, &ctx);

        assert!(!invalid_response.success);
        assert_eq!(invalid_response.data["code"], "invalid_request");
        assert_eq!(ctx.harness_opt(), Some(crate::harness::Harness::Opencode));
        assert_eq!(ctx.canonical_cache_root(), canonical_before);
        let config = ctx.config();
        assert_eq!(config.project_root.as_deref(), Some(first.path()));
        assert_eq!(config.harness, Some(crate::harness::Harness::Opencode));
        assert!(config.format_on_edit);
    }

    #[test]
    fn configure_replaces_formatter_and_checker_maps_when_present() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "formatter": { "typescript": "biome", "python": "ruff" },
                "checker": { "typescript": "tsc" }
            }))]
        }));
        assert!(handle_configure_for_test(&first_req, &ctx).success);

        let second_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "config": [user_tier(json!({
                "formatter": { "rust": "rustfmt" },
                "checker": { "go": "go" }
            }))]
        }));
        assert!(handle_configure_for_test(&second_req, &ctx).success);

        let config = ctx.config();
        assert_eq!(
            config.formatter.get("rust").map(String::as_str),
            Some("rustfmt")
        );
        assert!(!config.formatter.contains_key("typescript"));
        assert!(!config.formatter.contains_key("python"));
        assert_eq!(config.checker.get("go").map(String::as_str), Some("go"));
        assert!(!config.checker.contains_key("typescript"));
    }

    #[test]
    fn configure_rejects_invalid_process_state_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "max_background_bash_tasks": 0
        }));

        let response = handle_configure_for_test(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert!(ctx.config().project_root.is_none());
        assert!(ctx.harness_opt().is_none());
    }

    #[test]
    fn configure_generation_advances_only_after_successful_configure() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let invalid_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "max_background_bash_tasks": 0
        }));
        assert!(!handle_configure_for_test(&invalid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 0);

        let valid_req = configure_request(json!(root.path()));
        assert!(handle_configure_for_test(&valid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 1);
    }

    #[test]
    fn semantic_max_files_defaults_to_20k() {
        assert_eq!(SemanticBackendConfig::default().max_files, 20_000);
    }

    #[test]
    fn lsp_paths_extra_change_clears_failed_spawns_for_retry() {
        let previous = Config::default();
        let mut next = previous.clone();
        next.lsp_paths_extra.push(PathBuf::from("/cache/lsp/.bin"));

        assert!(should_clear_failed_spawns(&previous, &next, true));
        assert!(!should_clear_failed_spawns(&previous, &previous, true));
    }
}
