use crate as aft;
use crate::callgraph_store::CallGraphStore;
use crate::context::{
    AppContext, SemanticIndexEvent, SemanticIndexStatus, SemanticRefreshEvent,
    SemanticRefreshRequest, WatcherDrainApplyPhase, WatcherDrainPhase, WatcherDrainSliceState,
};
use crate::log_ctx;
use crate::lsp::client::LspEvent;
use crate::protocol::PushFrame;
use crate::watcher_filter::{watcher_path_is_infra_skip, WatcherDispatchEvent};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainBatchOutcome {
    pub processed: usize,
    pub has_more: bool,
}

pub const WATCHER_PATH_DRAIN_BATCH_CAP: usize = 2_048;
pub const WATCHER_DRAIN_SLICE_BUDGET: Duration = Duration::from_millis(250);
const WATCHER_DRAIN_UNIT_WARN_AFTER: Duration = Duration::from_secs(5);
const WATCHER_DRAIN_UNIT_FINAL_AFTER: Duration = Duration::from_secs(30);
pub const LSP_EVENT_DRAIN_BATCH_CAP: usize = 256;

struct WatcherDrainUnitGuard<'a> {
    phase: &'static str,
    path: &'a Path,
    batch_len: usize,
    started: Instant,
}

impl<'a> WatcherDrainUnitGuard<'a> {
    fn start(phase: WatcherDrainApplyPhase, path: &'a Path) -> Self {
        Self {
            phase: watcher_drain_phase_name(phase),
            path,
            batch_len: 1,
            started: Instant::now(),
        }
    }

    fn start_batch(phase: WatcherDrainApplyPhase, path: &'a Path, batch_len: usize) -> Self {
        Self {
            phase: watcher_drain_phase_name(phase),
            path,
            batch_len,
            started: Instant::now(),
        }
    }
}

impl Drop for WatcherDrainUnitGuard<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        let (warn_after, final_after) = watcher_drain_unit_thresholds();
        if elapsed < warn_after {
            return;
        }
        let path = if self.batch_len == 1 {
            self.path.display().to_string()
        } else {
            format!("{} (+{} paths)", self.path.display(), self.batch_len - 1)
        };
        emit_watcher_drain_unit_log(format!(
            "watcher drain unit exceeded 5s: phase={} path={} elapsed={}ms",
            self.phase,
            path,
            elapsed.as_millis()
        ));
        if elapsed >= final_after {
            emit_watcher_drain_unit_log(format!(
                "watcher drain unit completed after 30s: phase={} path={} elapsed={}ms",
                self.phase,
                path,
                elapsed.as_millis()
            ));
        }
    }
}

fn watcher_drain_unit_thresholds() -> (Duration, Duration) {
    #[cfg(test)]
    if let Some(thresholds) = WATCHER_UNIT_TEST_THRESHOLDS.with(std::cell::Cell::get) {
        return thresholds;
    }
    (
        WATCHER_DRAIN_UNIT_WARN_AFTER,
        WATCHER_DRAIN_UNIT_FINAL_AFTER,
    )
}

fn emit_watcher_drain_unit_log(line: String) {
    log::warn!("{line}");
    #[cfg(test)]
    WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow_mut().push(line));
}

#[cfg(test)]
thread_local! {
    static WATCHER_UNIT_TEST_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    static WATCHER_UNIT_TEST_THRESHOLDS: std::cell::Cell<Option<(Duration, Duration)>> = const { std::cell::Cell::new(None) };
    static WATCHER_UNIT_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn delay_watcher_unit_for_test() {
    let delay = WATCHER_UNIT_TEST_DELAY.with(std::cell::Cell::get);
    if !delay.is_zero() {
        thread::sleep(delay);
    }
}

#[cfg(not(test))]
fn delay_watcher_unit_for_test() {}

pub fn drain_deferred_configure_maintenance(ctx: &AppContext) {
    crate::commands::configure::drain_deferred_configure_maintenance(ctx);
}

pub fn drain_configure_warning_events(ctx: &AppContext) {
    for (generation, frame) in ctx.drain_configure_warnings() {
        if ctx.configure_generation() != generation {
            aft::slog_info!(
                "dropping stale configure_warnings for generation {} (current {})",
                generation,
                ctx.configure_generation()
            );
            continue;
        }

        if let Some(sender) = ctx.progress_sender_handle() {
            sender(PushFrame::ConfigureWarnings(frame));
        }
    }
}

pub fn drain_inspect_events(ctx: &AppContext) {
    let drained = ctx.inspect_manager().drain_completions();
    // Watcher-driven Tier-2 scans complete via the reuse path, which bypasses
    // `result_rx`/`drain_completions`. Poll the manager's reuse counter so a
    // background scan still refreshes the bar (#3) — otherwise the counts and
    // `~` marker would only update on a manual `aft_inspect`.
    let reuse_completed = ctx.take_new_reuse_completions();
    // A completed background Tier-2 scan refreshes the agent status-bar counts
    // to the freshly-persisted aggregate, and clears the stale marker — so the
    // bar reflects the new numbers on the next tool result without waiting for
    // an explicit aft_inspect call.
    if drained > 0 || reuse_completed {
        if let Some(project_root) = ctx.config().project_root.clone() {
            let (dead_code, unused_exports, duplicates) = ctx
                .inspect_manager()
                .latest_tier2_counts(ctx.inspect_dir(), project_root);
            // Don't clear the `~` stale marker until the whole serial Tier-2
            // cycle has drained — while any category is still in flight the
            // already-persisted categories may predate the latest edit, so
            // claiming fresh would be premature (#20). `None` counts preserve
            // the last-known value rather than fabricating a `0` (#1).
            let stale = ctx.inspect_manager().tier2_any_in_flight();
            ctx.update_status_bar_tier2(dead_code, unused_exports, duplicates, None, stale);
            // Push the refreshed snapshot so the sidebar reflects the new Tier-2
            // counts immediately. `update_status_bar_tier2` only mutates the
            // in-memory counts (which the agent status bar reads live on each
            // tool result); the push-driven sidebar would otherwise keep showing
            // the pre-population snapshot — where `status_bar` was null and the
            // Code Health section stayed hidden — until some unrelated event
            // happened to emit a status frame.
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
    }
}

/// Drain all background build-completion receivers in standalone order.
///
/// Search installs first so watcher/pending updates apply to the freshest index,
/// followed by callgraph store and semantic index completion.
pub fn drain_build_completions(ctx: &AppContext) {
    drain_search_index_events(ctx);
    drain_callgraph_store_events(ctx);
    drain_semantic_index_events(ctx);
}

/// Return true when any background build-completion receiver is currently set.
///
/// Each receiver is checked under its own short lock; no lock is held while
/// checking the next subsystem.
pub fn any_build_in_flight(ctx: &AppContext) -> bool {
    {
        let rx = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rx.is_some() {
            return true;
        }
    }

    {
        let rx = ctx.callgraph_store_rx().lock();
        if rx.is_some() {
            return true;
        }
    }

    {
        let rx = ctx.semantic_index_rx().lock();
        rx.is_some()
    }
}

pub fn watcher_path_is_ignored_by_current_matcher(ctx: &AppContext, path: &Path) -> bool {
    if watcher_path_is_infra_skip(path) {
        return true;
    }

    if let Some(matcher) = ctx.gitignore() {
        if path.starts_with(matcher.path()) {
            let is_dir = path.is_dir();
            return matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore();
        }
    }

    false
}

fn replay_search_index_pending_updates(
    ctx: &AppContext,
    index: &mut crate::search_index::SearchIndex,
    pending_paths: Vec<std::path::PathBuf>,
) {
    for path in pending_paths {
        if path.exists() {
            if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                index.remove_file(&path);
            } else {
                index.update_file(&path);
            }
        } else {
            index.remove_file(&path);
        }
    }
}

pub fn watcher_path_is_semantic_source(path: &Path) -> bool {
    crate::semantic_index::is_semantic_indexed_extension(path)
}

pub fn mark_semantic_corpus_refresh_success(ctx: &AppContext) {
    ctx.clear_all_semantic_refresh_retry_attempts();
    ctx.reset_semantic_refresh_circuit_after_success();
}

pub fn drain_search_index_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx
            .search_index_rx()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(index) => latest = Some(index),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut status_changed = false;
    let mut installed_index = false;
    if let Some(mut index) = latest {
        let pending_paths = ctx.take_pending_search_index_paths();
        if !pending_paths.is_empty() {
            replay_search_index_pending_updates(ctx, &mut index, pending_paths);
        }
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        installed_index = true;
        status_changed = true;
    }

    if disconnected || installed_index {
        *ctx.search_index_rx()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if disconnected && !installed_index {
            let _ = ctx.take_pending_search_index_paths();
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

/// Install a background-built callgraph store once its cold build completes.
/// Mirrors `drain_search_index_events`: drains the receiver, installs the
/// freshest store, replays paths that changed during the build, and clears the
/// receiver. On build failure (channel disconnected with nothing installed) the
/// receiver is cleared so a later op can retry the cold build.
pub fn drain_callgraph_store_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.callgraph_store_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(store) => latest = Some(store),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut status_changed = false;
    let mut installed = false;
    if let Some(store) = latest {
        // Replay source files that changed while the cold build was running so
        // the freshly-installed store reflects mid-build edits.
        let pending = ctx
            .take_pending_callgraph_store_paths()
            .into_iter()
            .filter(|path| !watcher_path_is_generated_for_callgraph(ctx, path))
            .collect::<Vec<_>>();
        // Release the cold-build writer lease before waking the refresh worker.
        // The worker opens through the generation pointer when it processes the
        // batch, so a concurrent later swap still targets the current generation.
        drop(store);
        if !pending.is_empty() {
            ctx.enqueue_callgraph_store_refresh(pending);
        }
        if let Some(project_root) = ctx.callgraph_project_root() {
            match CallGraphStore::open_readonly(ctx.callgraph_store_dir(), project_root) {
                Ok(Some(store)) => {
                    *ctx.callgraph_store()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(store));
                    installed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    crate::slog_warn!("failed to install read-only callgraph store: {}", error);
                }
            }
        }
        status_changed = installed;
        let _ = ctx.request_tier2_refresh_pull();
    }

    if disconnected || installed {
        *ctx.callgraph_store_rx().lock() = None;
        if disconnected && !installed {
            // Build failed: discard pending paths (no store to apply them to);
            // a later op restarts the build and re-walks the project.
            let _ = ctx.take_pending_callgraph_store_paths();
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

pub fn drain_semantic_index_events(ctx: &AppContext) {
    let (events, disconnected) = {
        let rx_ref = ctx.semantic_index_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (events, disconnected)
    };

    if events.is_empty() && !disconnected {
        return;
    }

    let mut keep_receiver = true;
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    let mut replay_corpus_refresh = false;
    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
            } => {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Building {
                        stage,
                        files,
                        entries_done,
                        entries_total,
                    };
                // Push progress to the sidebar. Without this, a long rebuild
                // (e.g. a slow local embedding backend re-indexing after a prior
                // failure) leaves the sidebar showing the stale prior state —
                // "failed" with an old error — for the entire build, even though
                // it is actively embedding. Progress transitions are exactly
                // when the user needs to see "building".
                status_changed = true;
            }
            SemanticIndexEvent::ColdSeedGateCleared => {
                ctx.resume_deferred_work_after_semantic_cold_seed_gate_cleared();
            }
            SemanticIndexEvent::Ready(mut index) => {
                mark_semantic_corpus_refresh_success(ctx);
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if watcher_path_is_semantic_source(&path) {
                        index.invalidate_file(&path);
                        replay_refresh_paths.push(path);
                    }
                }
                replay_corpus_refresh = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::ready();
                keep_receiver = false;
                status_changed = true;
                ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
            }
            SemanticIndexEvent::Failed(error) => {
                let _ = ctx.take_pending_semantic_index_paths();
                let _ = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                ctx.clear_semantic_refresh_worker();
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Failed(error);
                keep_receiver = false;
                status_changed = true;
                ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
            }
        }
    }

    if disconnected && keep_receiver {
        let _ = ctx.take_pending_semantic_index_paths();
        let _ = ctx.take_pending_semantic_corpus_refresh();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        ctx.clear_semantic_refresh_worker();
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Failed(
            "semantic index build worker disconnected before reporting completion".to_string(),
        );
        keep_receiver = false;
        status_changed = true;
        ctx.clear_semantic_cold_seed_gate_and_resume_deferred_work();
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().lock() = None;
    }

    if replay_corpus_refresh {
        if ctx.canonical_cache_root_opt().is_some() {
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SemanticIndexStatus::Building {
                    stage: "refreshing_corpus".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
            let sent = ctx
                .semantic_refresh_sender()
                .is_some_and(|sender| sender.send(SemanticRefreshRequest::Corpus).is_ok());
            if !sent {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Failed(
                        "semantic corpus refresh worker unavailable".to_string(),
                    );
            }
            status_changed = true;
        }
    } else if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            crate::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

pub const MAX_RETRY_ATTEMPTS: usize = 6;
pub const BREAKER_TRIP_THRESHOLD: usize = 3;

/// Backoff for live semantic refresh retries after a transient embedding backend
/// failure. Mirrors the cold-build retry cadence (15s -> 30s -> 60s capped) so
/// a down backend cannot spin the watcher/refresh loop hot while still
/// self-healing once the backend returns.
fn semantic_refresh_retry_backoff(attempt: usize) -> Duration {
    // Test seam, intentionally matching the build-level retry override.
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

struct SemanticRefreshRetryPlan {
    retry_paths: Vec<std::path::PathBuf>,
    capped_paths: Vec<std::path::PathBuf>,
    delay: Option<Duration>,
}

fn next_semantic_refresh_retry_plan(
    ctx: &AppContext,
    paths: Vec<std::path::PathBuf>,
) -> SemanticRefreshRetryPlan {
    let mut retry_paths = Vec::new();
    let mut capped_paths = Vec::new();
    let mut max_attempt = 0usize;

    ctx.with_semantic_refresh_retry_attempts_mut(|attempts| {
        for path in paths {
            let attempt = attempts.get(&path).copied().unwrap_or(0);
            if attempt >= MAX_RETRY_ATTEMPTS {
                capped_paths.push(path);
                continue;
            }
            max_attempt = max_attempt.max(attempt);
            attempts.insert(path.clone(), attempt.saturating_add(1));
            retry_paths.push(path);
        }
    });

    let delay = if retry_paths.is_empty() {
        None
    } else {
        Some(semantic_refresh_retry_backoff(max_attempt))
    };

    SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    }
}

fn clear_semantic_refresh_retry_attempts(ctx: &AppContext, paths: &[std::path::PathBuf]) {
    ctx.clear_semantic_refresh_retry_attempts(paths);
}

fn clear_completed_pending_semantic_index_paths(
    ctx: &AppContext,
    completed_paths: &[std::path::PathBuf],
) {
    if completed_paths.is_empty() {
        return;
    }

    let completed = completed_paths.iter().cloned().collect::<HashSet<_>>();
    let remaining = ctx
        .take_pending_semantic_index_paths()
        .into_iter()
        .filter(|path| !completed.contains(path))
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        ctx.add_pending_semantic_index_paths(remaining);
    }
}

fn semantic_refresh_probe_delay() -> Duration {
    semantic_refresh_retry_backoff(usize::MAX)
}

pub fn semantic_refresh_circuit_is_open(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_circuit_is_open()
}

pub fn record_semantic_refresh_transient_failure(ctx: &AppContext) -> bool {
    ctx.record_semantic_refresh_transient_failure(BREAKER_TRIP_THRESHOLD)
}

fn reset_semantic_refresh_transient_failure_count(ctx: &AppContext) {
    ctx.reset_semantic_refresh_transient_failure_count();
}

fn reset_semantic_refresh_circuit_after_success(ctx: &AppContext) {
    ctx.reset_semantic_refresh_circuit_after_success();
}

fn mark_semantic_refresh_success(ctx: &AppContext, completed_paths: &[std::path::PathBuf]) {
    clear_semantic_refresh_retry_attempts(ctx, completed_paths);
    clear_completed_pending_semantic_index_paths(ctx, completed_paths);
    reset_semantic_refresh_circuit_after_success(ctx);
}

#[doc(hidden)]
pub fn semantic_refresh_transient_failure_count_for_test(ctx: &AppContext) -> usize {
    ctx.semantic_refresh_transient_failure_count()
}

#[doc(hidden)]
pub fn semantic_refresh_probe_is_scheduled_for_test(ctx: &AppContext) -> bool {
    ctx.semantic_refresh_probe_is_scheduled()
}

fn ensure_semantic_refresh_probe_scheduled(ctx: &AppContext) {
    ctx.ensure_semantic_refresh_probe_scheduled(semantic_refresh_probe_delay());
}

fn maybe_fire_semantic_refresh_probe(ctx: &AppContext) {
    if !ctx.take_semantic_refresh_probe_ready() {
        return;
    }
    if !semantic_refresh_circuit_is_open(ctx) {
        return;
    }

    let pending_paths = ctx.take_pending_semantic_index_paths();
    if pending_paths.is_empty() {
        return;
    }

    let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
        sender
            .send(SemanticRefreshRequest::Files {
                paths: pending_paths.clone(),
            })
            .is_ok()
    });
    if !sent {
        ctx.add_pending_semantic_index_paths(pending_paths);
    }
}

pub fn schedule_semantic_refresh_retry(
    ctx: &AppContext,
    paths: Vec<std::path::PathBuf>,
    error: &str,
) -> bool {
    if paths.is_empty() {
        return false;
    }
    let Some(sender) = ctx.semantic_refresh_sender() else {
        return false;
    };

    let SemanticRefreshRetryPlan {
        retry_paths,
        capped_paths,
        delay,
    } = next_semantic_refresh_retry_plan(ctx, paths);

    if !capped_paths.is_empty() {
        aft::slog_warn!(
            "semantic refresh retry limit reached for {} file(s); preserving for next watcher/configure refresh",
            capped_paths.len(),
        );
        ctx.add_pending_semantic_index_paths(capped_paths);
    }

    let Some(delay) = delay else {
        return true;
    };

    let clean = aft::semantic_index::strip_transient_embedding_marker(error);
    aft::slog_warn!(
        "semantic refresh hit a transient backend error ({}); retrying {} file(s) in {}ms",
        clean,
        retry_paths.len(),
        delay.as_millis(),
    );

    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            thread::sleep(delay);
            let _ = sender.send(SemanticRefreshRequest::Files { paths: retry_paths });
        });
    });
    true
}

pub fn drain_semantic_refresh_events(ctx: &AppContext) {
    let (events, disconnected) = {
        let rx_ref = ctx.semantic_refresh_event_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (events, disconnected)
    };

    if events.is_empty() && !disconnected {
        maybe_fire_semantic_refresh_probe(ctx);
        return;
    }

    let had_events = !events.is_empty();
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    for event in events {
        match event {
            SemanticRefreshEvent::Started { paths } => {
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in paths {
                        status.start_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusStarted { files } => {
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::Building {
                        stage: "refreshing_corpus".to_string(),
                        files: Some(files),
                        entries_done: None,
                        entries_total: None,
                    };
                status_changed = true;
            }
            SemanticRefreshEvent::Completed {
                added_entries,
                updated_metadata,
                completed_paths,
            } => {
                if let Some(index) = ctx
                    .semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    index.apply_refresh_update(added_entries, updated_metadata, &completed_paths);
                }
                mark_semantic_refresh_success(ctx, &completed_paths);
                let mut status = ctx
                    .semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &completed_paths {
                        status.complete_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusCompleted {
                mut index,
                changed,
                added,
                deleted,
                total_processed,
            } => {
                aft::runtime_drain::mark_semantic_corpus_refresh_success(ctx);
                if changed > 0 || added > 0 || deleted > 0 {
                    aft::slog_info!(
                        "semantic corpus refresh completed: {} changed, {} new, {} deleted, {} total processed",
                        changed,
                        added,
                        deleted,
                        total_processed
                    );
                }
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if !aft::runtime_drain::watcher_path_is_semantic_source(&path) {
                        continue;
                    }
                    index.invalidate_file(&path);
                    if !aft::runtime_drain::watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                        replay_refresh_paths.push(path);
                    }
                }
                *ctx.semantic_index()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
                *ctx.semantic_index_status()
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    SemanticIndexStatus::ready();
                status_changed = true;
            }
            SemanticRefreshEvent::Failed { paths, error } => {
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    if record_semantic_refresh_transient_failure(ctx) {
                        ctx.add_pending_semantic_index_paths(paths);
                        ensure_semantic_refresh_probe_scheduled(ctx);
                    } else if !schedule_semantic_refresh_retry(ctx, paths.clone(), &error) {
                        aft::slog_warn!(
                            "semantic refresh worker unavailable; preserving {} transiently failed file(s) for retry",
                            paths.len(),
                        );
                        ctx.add_pending_semantic_index_paths(paths);
                    }
                } else {
                    aft::slog_warn!("semantic refresh failed: {}", error);
                    reset_semantic_refresh_transient_failure_count(ctx);
                    clear_semantic_refresh_retry_attempts(ctx, &paths);
                    let mut status = ctx
                        .semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                        for path in &paths {
                            status.complete_refreshing_file(path);
                        }
                        status_changed = true;
                    }
                }
            }
            SemanticRefreshEvent::CorpusFailed { error } => {
                // A transient backend blip during a corpus refresh must NOT
                // destroy the working index — the prior index is still valid and
                // serving. Keep it Ready and let the next watcher/ignore change
                // re-trigger the refresh, rather than nuking everything to
                // `Failed` over a connection hiccup (the same park-forever trap
                // the initial build now rides out). Permanent errors (dimension
                // mismatch, too-many-files) still drop the index and surface the
                // real failure.
                if aft::semantic_index::embedding_failure_is_transient(&error) {
                    let clean = aft::semantic_index::strip_transient_embedding_marker(&error);
                    let has_index = ctx
                        .semantic_index()
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some();
                    if has_index {
                        aft::slog_warn!(
                            "semantic corpus refresh hit a transient backend error ({}); keeping the existing index",
                            clean,
                        );
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::ready();
                    } else {
                        // No index to fall back on — surface the clean message.
                        aft::slog_warn!("semantic corpus refresh failed: {}", clean);
                        *ctx.semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            SemanticIndexStatus::Failed(clean);
                    }
                    status_changed = true;
                } else {
                    aft::slog_warn!("semantic corpus refresh failed: {}", error);
                    let _ = ctx.take_pending_semantic_index_paths();
                    *ctx.semantic_index()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::Failed(error);
                    status_changed = true;
                }
            }
        }
    }

    if disconnected {
        ctx.clear_semantic_refresh_worker();
        let refreshing_paths = {
            let status = ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*status {
                SemanticIndexStatus::Ready { refreshing, .. } => refreshing.clone(),
                _ => Vec::new(),
            }
        };
        if !refreshing_paths.is_empty() {
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &refreshing_paths {
                status.cancel_refreshing_file(path);
            }
        }
        if !refreshing_paths.is_empty() || had_events {
            status_changed = true;
        }
    }

    if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed corpus file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    maybe_fire_semantic_refresh_probe(ctx);

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

/// Source file extensions that the call graph supports.
const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go",
];

pub const WATCHER_BATCH_INLINE_CAP: usize = 256;

/// A `tsconfig.json` / `jsconfig.json` (including variant names like
/// `tsconfig.base.json`). A change to any of these can shift TypeScript build
/// membership (which files `tsc` checks), so the status-bar membership cache
/// must be invalidated. Deliberately broad on the variant suffix and ignorant
/// of `extends` graphs: the cache is cleared wholesale on a match, and base
/// configs almost always follow the `tsconfig*.json` naming. Non-standard base
/// names are covered on the next `tsconfig.json` change or `configure`.
pub fn watcher_path_is_tsconfig(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n == "tsconfig.json"
                || n == "jsconfig.json"
                || ((n.starts_with("tsconfig.") || n.starts_with("jsconfig."))
                    && n.ends_with(".json"))
        })
        .unwrap_or(false)
}

pub fn watcher_path_is_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

/// A file the callgraph STORE would have indexed at cold-build time. The store
/// indexes every file `walk_project_files` yields (i.e. any detected language),
/// not just the trigram `SOURCE_EXTENSIONS` set. Gating the store's watcher
/// refresh on the narrower trigram set left edits to Java/C/C++/C#/Kotlin/Ruby/
/// PHP/… (all of which the store extracts calls for) serving stale results until
/// a full rebuild. Mirror cold-build exactly so refresh coverage == index
/// coverage.
pub fn watcher_path_is_callgraph_indexed(path: &std::path::Path) -> bool {
    aft::parser::detect_language(path).is_some()
}

pub fn semantic_corpus_refresh_in_progress(ctx: &AppContext) -> bool {
    let status = ctx
        .semantic_index_status()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    matches!(
        &*status,
        SemanticIndexStatus::Building { stage, .. } if stage == "refreshing_corpus"
    )
}

struct SearchRebuildPublishGate {
    reached_tx: crossbeam_channel::Sender<()>,
    release_rx: crossbeam_channel::Receiver<()>,
}

static SEARCH_REBUILD_PUBLISH_GATE: OnceLock<Mutex<Option<SearchRebuildPublishGate>>> =
    OnceLock::new();
static SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL: OnceLock<Mutex<Option<crossbeam_channel::Sender<()>>>> =
    OnceLock::new();

#[doc(hidden)]
pub fn install_search_rebuild_publish_gate_for_test() -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (shutdown_waiting_tx, shutdown_waiting_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    *SEARCH_REBUILD_PUBLISH_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild publish gate mutex poisoned") = Some(SearchRebuildPublishGate {
        reached_tx,
        release_rx,
    });
    *SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild shutdown wait signal mutex poisoned") = Some(shutdown_waiting_tx);
    (reached_rx, shutdown_waiting_rx, release_tx)
}

pub(crate) fn note_search_rebuild_shutdown_wait_for_test() {
    let signal = SEARCH_REBUILD_SHUTDOWN_WAIT_SIGNAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild shutdown wait signal mutex poisoned")
        .take();
    if let Some(signal) = signal {
        let _ = signal.send(());
    }
}

fn wait_on_search_rebuild_publish_gate_for_test() {
    let gate = SEARCH_REBUILD_PUBLISH_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("search rebuild publish gate mutex poisoned")
        .take();
    if let Some(gate) = gate {
        let _ = gate.reached_tx.send(());
        let _ = gate.release_rx.recv_timeout(Duration::from_secs(12));
    }
}

pub fn spawn_search_corpus_refresh(
    ctx: &AppContext,
    root: std::path::PathBuf,
    config: Arc<aft::config::Config>,
) {
    {
        let mut search_index = ctx
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = search_index.as_mut() {
            index.ready = false;
        }
    }

    let (tx, rx): (
        crossbeam_channel::Sender<aft::search_index::SearchIndex>,
        crossbeam_channel::Receiver<aft::search_index::SearchIndex>,
    ) = crossbeam_channel::unbounded();
    *ctx.search_index_rx()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(rx);
    ctx.reset_symbol_cache();

    let shared_artifacts_read_only = ctx.shared_artifacts_read_only();
    let project_key = ctx.memoized_artifact_cache_key(&root);
    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            let cache_dir = aft::search_index::resolve_cache_dir_with_key(
                &project_key,
                config.storage_dir.as_deref(),
            );
            let _cache_lock = if shared_artifacts_read_only {
                None
            } else {
                match aft::search_index::CacheLock::acquire(&cache_dir, &root) {
                    Ok(lock) => Some(lock),
                    Err(error) => {
                        aft::slog_warn!(
                            "failed to acquire search cache lock for ignore refresh: {}",
                            error
                        );
                        None
                    }
                }
            };
            let mut index = aft::search_index::SearchIndex::build_with_limit_to_cache_dir(
                &root,
                config.search_index_max_file_size,
                &cache_dir,
            );
            wait_on_search_rebuild_publish_gate_for_test();
            if !shared_artifacts_read_only {
                let head = index.stored_git_head().map(str::to_owned);
                index.write_to_disk(&cache_dir, head.as_deref());
            }
            let _ = tx.send(index);
        });
    });
}

pub fn refresh_project_corpus(
    ctx: &AppContext,
    reason: &str,
    _invalidate_ignore_paths: bool,
) -> bool {
    let Some(root) = ctx.canonical_cache_root_opt() else {
        return false;
    };
    if !ctx.heavy_root_work_allowed() {
        return false;
    }
    let config = ctx.config();
    let mut status_changed = false;

    if ctx.callgraph_writer() {
        // Do NOT cold-build the callgraph store synchronously here. This function
        // runs on the single-threaded dispatch loop from `drain_watcher_events`,
        // which fires before EVERY request (and on idle ticks). A full O(repo)
        // `refresh_corpus` (= `cold_build`: parse all files + resolve refs +
        // rewrite SQLite) blocks ALL queued requests — including `configure` and
        // `bash` — for its entire duration, which exceeds the 30s transport
        // timeout on a large repo. On a long-lived bridge (OpenCode Desktop) an
        // FSEvents overflow triggers this drain, so the user sees configure/bash
        // time out (regression: the watcher-overflow path that calls this is new
        // in 0.39.1; the ignore-rule path that also calls this had the same
        // latent inline block, just rarely triggered).
        //
        // Instead, drop the resident store and force a BACKGROUND rebuild: the
        // next `callgraph_store_for_ops()` spawns the cold build off-thread and
        // returns `Building` (callgraph ops + dead_code projection already handle
        // `Building`/unavailable gracefully). This mirrors the search/semantic
        // refreshes below, which are already async. A build already in flight
        // keeps running; the resident drop + force flag make the next op converge
        // to a fresh full rebuild.
        // Mirror the original "act only when the callgraph is actually loaded or
        // building" guard, but reschedule instead of inline-building.
        let callgraph_store_resident = {
            let guard = ctx
                .callgraph_store()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.is_some()
        };
        if callgraph_store_resident || ctx.callgraph_store_rx().lock().is_some() {
            *ctx.callgraph_store()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            ctx.mark_callgraph_store_force_rebuild();
            status_changed = true;
            aft::slog_info!(
                "callgraph store scheduled for background rebuild after {}",
                reason
            );
        }
    }

    if config.search_index && !ctx.shared_artifacts_read_only() {
        spawn_search_corpus_refresh(ctx, root.clone(), config.clone());
        status_changed = true;
        aft::slog_info!("started search index refresh after {}", reason);
    }

    if config.semantic_search && !ctx.shared_artifacts_read_only() {
        if let Some(sender) = ctx.semantic_refresh_sender() {
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                SemanticIndexStatus::Building {
                    stage: "refreshing_corpus".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                };
            match sender.send(SemanticRefreshRequest::Corpus) {
                Ok(()) => {
                    status_changed = true;
                }
                Err(error) => {
                    *ctx.semantic_index_status()
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        SemanticIndexStatus::Failed(format!(
                            "semantic corpus refresh worker unavailable: {error}"
                        ));
                    status_changed = true;
                }
            }
        } else if ctx.semantic_index_rx().lock().is_some() {
            ctx.mark_pending_semantic_corpus_refresh();
        }
    }

    status_changed
}

pub fn refresh_corpus_after_ignore_change(ctx: &AppContext) -> bool {
    refresh_project_corpus(ctx, "ignore-rule change", true)
}

pub fn refresh_project_after_watcher_rescan(ctx: &AppContext) -> bool {
    if ctx.canonical_cache_root_opt().is_none() {
        return false;
    }
    ctx.clear_pending_index_updates();
    ctx.reset_symbol_cache();
    let _ = ctx.mark_status_bar_tier2_stale();
    ctx.clear_tsconfig_membership_cache();
    let mut status_changed = true;

    status_changed |= refresh_project_corpus(ctx, "watcher overflow", false);
    status_changed
}

fn watcher_path_is_generated_for_callgraph(ctx: &AppContext, path: &Path) -> bool {
    ctx.callgraph_project_root()
        .is_some_and(|project_root| crate::inspect::is_generated_file(&project_root, path))
}

pub fn refresh_callgraph_store_for_watcher(
    ctx: &AppContext,
    changed: &HashSet<std::path::PathBuf>,
) {
    if !ctx.heavy_root_work_allowed() {
        return;
    }
    let source_paths = changed
        .iter()
        .filter(|path| {
            watcher_path_is_callgraph_indexed(path)
                && !watcher_path_is_generated_for_callgraph(ctx, path)
        })
        .cloned()
        .collect::<Vec<_>>();
    if source_paths.is_empty() {
        return;
    }
    // This is intentionally the only watcher call-site action. Opening and
    // mutating SQLite belongs to the process-wide store worker, outside every
    // executor lane and its epoch gate.
    ctx.enqueue_callgraph_store_refresh(source_paths);
}

/// Drain pre-filtered watcher events and apply cache invalidations on the
/// dispatch thread. The watcher filter thread owns notify receive/decode,
/// metadata filtering, ignore matching, root-deleted detection, and path
/// coalescing; this drain only reacts to compact control events and surviving
/// paths because the cache/index state below is not Send.
pub fn drain_watcher_events(ctx: &AppContext) {
    loop {
        let outcome = drain_watcher_events_bounded(ctx, WATCHER_PATH_DRAIN_BATCH_CAP);
        if !outcome.has_more {
            break;
        }
    }
}

fn watcher_drain_phase_name(stage: WatcherDrainApplyPhase) -> &'static str {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => "pending_tier2",
        WatcherDrainApplyPhase::PendingIndexes => "pending_indexes",
        WatcherDrainApplyPhase::SymbolCache => "symbol_cache",
        WatcherDrainApplyPhase::Callgraph => "callgraph",
        WatcherDrainApplyPhase::SearchIndex => "search_index",
        WatcherDrainApplyPhase::SemanticIndex => "semantic_index",
        WatcherDrainApplyPhase::LspDiagnostics => "lsp_diagnostics",
        WatcherDrainApplyPhase::Complete => "complete",
    }
}

fn apply_watcher_path_phase(
    stage: WatcherDrainApplyPhase,
    paths: &mut VecDeque<PathBuf>,
    remaining: &mut usize,
    started: Instant,
    budget: Duration,
    mut apply: impl FnMut(&Path),
) -> bool {
    while *remaining > 0 {
        let path = paths
            .pop_front()
            .expect("watcher apply phase tracks its remaining paths");
        {
            let _watchdog = WatcherDrainUnitGuard::start(stage, &path);
            delay_watcher_unit_for_test();
            apply(&path);
        }
        paths.push_back(path);
        *remaining -= 1;
        if started.elapsed() >= budget {
            return false;
        }
    }
    true
}

fn apply_callgraph_watcher_phase(
    ctx: &AppContext,
    paths: &mut VecDeque<PathBuf>,
    remaining: &mut usize,
    started: Instant,
    budget: Duration,
    enabled: bool,
    mut refresh: impl FnMut(&AppContext, &HashSet<PathBuf>),
) -> bool {
    let mut changed = HashSet::new();
    let mut generated_skipped = 0usize;
    let completed = apply_watcher_path_phase(
        WatcherDrainApplyPhase::Callgraph,
        paths,
        remaining,
        started,
        budget,
        |path| {
            if enabled && watcher_path_is_callgraph_indexed(path) {
                if watcher_path_is_generated_for_callgraph(ctx, path) {
                    generated_skipped += 1;
                } else {
                    changed.insert(path.to_path_buf());
                }
            }
        },
    );
    if generated_skipped > 0 {
        log::debug!(
            "callgraph refresh skipped {} generated file(s)",
            generated_skipped
        );
    }
    if !changed.is_empty() {
        let first = changed
            .iter()
            .min()
            .expect("non-empty callgraph watcher batch has a first path");
        let _watchdog = WatcherDrainUnitGuard::start_batch(
            WatcherDrainApplyPhase::Callgraph,
            first,
            changed.len(),
        );
        delay_watcher_unit_for_test();
        refresh(ctx, &changed);
    }
    completed
}

fn next_watcher_apply_phase(stage: WatcherDrainApplyPhase) -> WatcherDrainApplyPhase {
    match stage {
        WatcherDrainApplyPhase::PendingTier2 => WatcherDrainApplyPhase::PendingIndexes,
        WatcherDrainApplyPhase::PendingIndexes => WatcherDrainApplyPhase::SymbolCache,
        WatcherDrainApplyPhase::SymbolCache => WatcherDrainApplyPhase::Callgraph,
        WatcherDrainApplyPhase::Callgraph => WatcherDrainApplyPhase::SearchIndex,
        WatcherDrainApplyPhase::SearchIndex => WatcherDrainApplyPhase::SemanticIndex,
        WatcherDrainApplyPhase::SemanticIndex => WatcherDrainApplyPhase::LspDiagnostics,
        WatcherDrainApplyPhase::LspDiagnostics => WatcherDrainApplyPhase::Complete,
        WatcherDrainApplyPhase::Complete => WatcherDrainApplyPhase::Complete,
    }
}

fn apply_watcher_slice(ctx: &AppContext, state: &mut WatcherDrainSliceState, started: Instant) {
    let WatcherDrainPhase::Apply {
        mut stage,
        mut paths,
        mut remaining,
        oversized_inline_batch,
    } = std::mem::take(&mut state.phase)
    else {
        return;
    };
    if !paths.is_empty() || remaining > 0 {
        ctx.invalidate_warm_verify_memo();
    }
    let heavy_root_work_allowed = ctx.heavy_root_work_allowed();
    let shared_artifacts_read_only = ctx.shared_artifacts_read_only();
    let mut semantic_refresh_paths = std::mem::take(&mut state.semantic_refresh_paths);
    let mut status_changed = state.status_changed;

    loop {
        let completed = match stage {
            WatcherDrainApplyPhase::PendingTier2 => apply_watcher_path_phase(
                WatcherDrainApplyPhase::PendingTier2,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if heavy_root_work_allowed && ctx.inspect_writer() {
                        ctx.add_pending_tier2_paths([path.to_path_buf()]);
                    }
                },
            ),
            WatcherDrainApplyPhase::PendingIndexes => {
                let search_build_in_progress = ctx
                    .search_index_rx()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some();
                let semantic_build_in_progress = ctx.semantic_index_rx().lock().is_some();
                let semantic_corpus_refresh_in_progress = semantic_corpus_refresh_in_progress(ctx);
                apply_watcher_path_phase(
                    WatcherDrainApplyPhase::PendingIndexes,
                    &mut paths,
                    &mut remaining,
                    started,
                    WATCHER_DRAIN_SLICE_BUDGET,
                    |path| {
                        if heavy_root_work_allowed
                            && !shared_artifacts_read_only
                            && !oversized_inline_batch
                            && search_build_in_progress
                        {
                            ctx.add_pending_search_index_paths([path.to_path_buf()]);
                        }
                        if heavy_root_work_allowed
                            && !shared_artifacts_read_only
                            && !oversized_inline_batch
                            && (semantic_build_in_progress || semantic_corpus_refresh_in_progress)
                            && watcher_path_is_semantic_source(path)
                        {
                            ctx.add_pending_semantic_index_paths([path.to_path_buf()]);
                        }
                    },
                )
            }
            WatcherDrainApplyPhase::SymbolCache => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SymbolCache,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if !shared_artifacts_read_only {
                        if let Ok(mut symbol_cache) = ctx.symbol_cache().write() {
                            symbol_cache.invalidate(path);
                        }
                    }
                },
            ),
            WatcherDrainApplyPhase::Callgraph => apply_callgraph_watcher_phase(
                ctx,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                heavy_root_work_allowed && !oversized_inline_batch,
                refresh_callgraph_store_for_watcher,
            ),
            WatcherDrainApplyPhase::SearchIndex => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SearchIndex,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if heavy_root_work_allowed
                        && !shared_artifacts_read_only
                        && !oversized_inline_batch
                    {
                        let mut index_ref = ctx
                            .search_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(index) = index_ref.as_mut() {
                            if path.exists() {
                                index.update_file(path);
                            } else {
                                index.remove_file(path);
                            }
                        }
                    }
                },
            ),
            WatcherDrainApplyPhase::SemanticIndex => apply_watcher_path_phase(
                WatcherDrainApplyPhase::SemanticIndex,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if !heavy_root_work_allowed
                        || shared_artifacts_read_only
                        || oversized_inline_batch
                        || !watcher_path_is_semantic_source(path)
                    {
                        return;
                    }
                    let invalidated = {
                        let mut semantic_index_ref = ctx
                            .semantic_index()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        semantic_index_ref.as_mut().is_some_and(|index| {
                            index.invalidate_file(path);
                            true
                        })
                    };
                    if invalidated {
                        let mut status = ctx
                            .semantic_index_status()
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                            status.add_refreshing_file(path.to_path_buf());
                            semantic_refresh_paths.push(path.to_path_buf());
                            status_changed = true;
                        }
                    }
                },
            ),
            WatcherDrainApplyPhase::LspDiagnostics => apply_watcher_path_phase(
                WatcherDrainApplyPhase::LspDiagnostics,
                &mut paths,
                &mut remaining,
                started,
                WATCHER_DRAIN_SLICE_BUDGET,
                |path| {
                    if !path.exists() {
                        status_changed |= ctx.lsp_clear_diagnostics_for_file(path);
                        return;
                    }
                    let stale = ctx.lsp_mark_diagnostics_stale_for_file(path);
                    status_changed |= stale.changed;
                    if stale.had_entries {
                        ctx.lsp_resync_changed_file_for_diagnostics(path);
                    }
                },
            ),
            WatcherDrainApplyPhase::Complete => true,
        };

        if !completed {
            state.status_changed = status_changed;
            state.semantic_refresh_paths = semantic_refresh_paths;
            state.phase = WatcherDrainPhase::Apply {
                stage,
                paths,
                remaining,
                oversized_inline_batch,
            };
            return;
        }

        if stage == WatcherDrainApplyPhase::Complete {
            break;
        }
        stage = next_watcher_apply_phase(stage);
        remaining = paths.len();
        if started.elapsed() >= WATCHER_DRAIN_SLICE_BUDGET {
            state.status_changed = status_changed;
            state.semantic_refresh_paths = semantic_refresh_paths;
            state.phase = WatcherDrainPhase::Apply {
                stage,
                paths,
                remaining,
                oversized_inline_batch,
            };
            return;
        }
    }

    if !semantic_refresh_paths.is_empty() {
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: semantic_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} refreshing file(s)",
                semantic_refresh_paths.len()
            );
            let mut status = ctx
                .semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for path in &semantic_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    aft::slog_info!("invalidated {} files", paths.len());
    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    ctx.tick_tier2_refresh_scheduler(state.scheduler_changed_path_count);
    state.phase = WatcherDrainPhase::Collect;
    state.status_changed = false;
    state.scheduler_changed_path_count = 0;
    state.semantic_refresh_paths.clear();
}

pub fn drain_watcher_events_bounded(ctx: &AppContext, max_paths: usize) -> DrainBatchOutcome {
    let started = Instant::now();
    let configure_generation = ctx.configure_generation();
    let mut state = ctx
        .watcher_drain_slice()
        .lock()
        .take()
        .filter(|state| state.configure_generation == configure_generation)
        .unwrap_or_else(|| WatcherDrainSliceState::new(configure_generation));
    let mut outcome = DrainBatchOutcome::default();
    let mut dispatch_events_received = 0usize;
    let mut watcher_failed = None;
    let mut root_deleted = false;

    {
        let rx_ref = ctx.watcher_rx().lock();
        let Some(rx) = rx_ref.as_ref() else {
            ctx.tick_tier2_refresh_scheduler(0);
            return outcome;
        };

        loop {
            match rx.try_recv() {
                Ok(WatcherDispatchEvent::Paths(paths)) => {
                    dispatch_events_received += 1;
                    if !state.rescan_required {
                        state.pending_paths.extend(paths);
                    }
                }
                Ok(WatcherDispatchEvent::RescanRequired) => {
                    dispatch_events_received += 1;
                    state.rescan_required = true;
                    state.pending_paths.clear();
                    state.phase = WatcherDrainPhase::Collect;
                    state.semantic_refresh_paths.clear();
                    state.scheduler_changed_path_count = 0;
                }
                Ok(WatcherDispatchEvent::IgnoreRulesChanged { path }) => {
                    dispatch_events_received += 1;
                    state.ignore_changed = true;
                    log::debug!(
                        "watcher: ignore rules changed at {}, rebuilding matcher",
                        path.display()
                    );
                    if !state.rescan_required {
                        if ctx.heavy_root_work_allowed() {
                            ctx.rebuild_gitignore();
                        } else {
                            ctx.clear_gitignore();
                        }
                    }
                }
                Ok(WatcherDispatchEvent::RootDeleted) => {
                    dispatch_events_received += 1;
                    root_deleted = true;
                    break;
                }
                Ok(WatcherDispatchEvent::Error(error)) => {
                    dispatch_events_received += 1;
                    watcher_failed = Some(error);
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    watcher_failed = Some("watcher channel disconnected".to_string());
                    break;
                }
            }
            if started.elapsed() >= WATCHER_DRAIN_SLICE_BUDGET {
                break;
            }
        }
    }

    crate::logging::note_watcher_events(dispatch_events_received);
    let receiver_has_more_after_receive = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .is_some_and(|rx| !rx.is_empty());

    if root_deleted {
        ctx.stop_watcher_runtime_in_background();
        let _ = ctx.add_degraded_reason("project_root_deleted".to_string());
        aft::slog_warn!(
            "project root deleted; dropping watcher to avoid delete-storm: {:?}",
            ctx.canonical_cache_root_opt()
        );
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        return outcome;
    }
    if let Some(error) = watcher_failed {
        ctx.stop_watcher_runtime_in_background();
        let _ = ctx.add_degraded_reason("watcher_unavailable".to_string());
        aft::slog_warn!(
            "file watcher unavailable; continuing without live external-change invalidation: {}",
            error
        );
        ctx.status_emitter().signal(ctx.build_status_snapshot());
        return outcome;
    }

    if state.rescan_required && receiver_has_more_after_receive {
        outcome.has_more = true;
        *ctx.watcher_drain_slice().lock() = Some(state);
        return outcome;
    }

    if state.rescan_required {
        crate::logging::note_watcher_overflow();
        aft::slog_warn!("watcher overflow: forcing project rescan");
        if ctx.heavy_root_work_allowed() {
            ctx.rebuild_gitignore();
        } else {
            ctx.clear_gitignore();
        }
        state.status_changed |= refresh_project_after_watcher_rescan(ctx);
        state.scheduler_changed_path_count =
            aft::inspect::tier2_scheduler::TIER2_REFRESH_STORM_PATH_THRESHOLD + 1;
        if state.status_changed {
            ctx.status_emitter().signal(ctx.build_status_snapshot());
        }
        ctx.tick_tier2_refresh_scheduler(state.scheduler_changed_path_count);
        state.rescan_required = false;
        state.ignore_changed = false;
        state.status_changed = false;
        state.scheduler_changed_path_count = 0;
    } else if matches!(state.phase, WatcherDrainPhase::Collect) {
        let ignore_changed = state.ignore_changed;
        let mut project_corpus_refresh_requested = false;
        if ignore_changed {
            state.status_changed |= refresh_corpus_after_ignore_change(ctx);
            project_corpus_refresh_requested = true;
            state.ignore_changed = false;
        }

        if max_paths > 0 && !state.pending_paths.is_empty() {
            let mut unique = HashSet::new();
            let mut paths = VecDeque::new();
            while outcome.processed < max_paths {
                let Some(path) = state.pending_paths.pop_front() else {
                    break;
                };
                outcome.processed += 1;
                if unique.insert(path.clone()) {
                    paths.push_back(path);
                }
            }
            crate::logging::note_drain_paths(outcome.processed);

            if paths.is_empty() {
                if state.status_changed {
                    ctx.status_emitter().signal(ctx.build_status_snapshot());
                }
                ctx.tick_tier2_refresh_scheduler(usize::from(ignore_changed));
                state.status_changed = false;
            } else {
                state.path_slice_count += 1;
                state.scheduler_changed_path_count = if ignore_changed {
                    paths.len().max(1)
                } else {
                    paths.len()
                };
                if ctx.mark_status_bar_tier2_stale() {
                    state.status_changed = true;
                }
                if paths.iter().any(|path| watcher_path_is_tsconfig(path)) {
                    ctx.clear_tsconfig_membership_cache();
                    state.status_changed = true;
                }

                let oversized_inline_batch = paths.len() > WATCHER_BATCH_INLINE_CAP;
                if oversized_inline_batch {
                    aft::slog_warn!(
                        "watcher batch of {} paths exceeds inline cap {}; scheduling corpus refresh",
                        paths.len(),
                        WATCHER_BATCH_INLINE_CAP
                    );
                    if !project_corpus_refresh_requested {
                        state.status_changed |=
                            refresh_project_corpus(ctx, "oversized watcher batch", false);
                    }
                }
                let remaining = paths.len();
                state.phase = WatcherDrainPhase::Apply {
                    stage: WatcherDrainApplyPhase::PendingTier2,
                    paths,
                    remaining,
                    oversized_inline_batch,
                };
            }
        } else if ignore_changed {
            if state.status_changed {
                ctx.status_emitter().signal(ctx.build_status_snapshot());
            }
            ctx.tick_tier2_refresh_scheduler(1);
            state.status_changed = false;
        }
    }

    if matches!(state.phase, WatcherDrainPhase::Apply { .. })
        && started.elapsed() < WATCHER_DRAIN_SLICE_BUDGET
    {
        apply_watcher_slice(ctx, &mut state, started);
    }

    let receiver_has_more = ctx
        .watcher_rx()
        .lock()
        .as_ref()
        .is_some_and(|rx| !rx.is_empty());
    outcome.has_more = state.has_pending_work() || receiver_has_more;
    if state.configure_generation == ctx.configure_generation() {
        *ctx.watcher_drain_slice().lock() = Some(state);
    }
    outcome
}

pub fn drain_lsp_events(ctx: &AppContext) {
    let _ = drain_lsp_events_bounded(ctx, usize::MAX);
}

pub fn drain_lsp_events_bounded(ctx: &AppContext, max_events: usize) -> DrainBatchOutcome {
    let drained = {
        let mut lsp = ctx.lsp();
        lsp.drain_events_bounded(max_events)
    };
    let outcome = DrainBatchOutcome {
        processed: drained.events.len(),
        has_more: drained.has_more,
    };
    let mut status_changed = drained.diagnostics_changed;
    for event in drained.events {
        match event {
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] notification {:?} {} {} {}",
                    server_kind,
                    root.display(),
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerRequest {
                server_kind,
                root,
                id,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] request {:?} {} {:?} {} {}",
                    server_kind,
                    root.display(),
                    id,
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerExited { server_kind, root } => {
                aft::slog_info!("exited {:?} {}", server_kind, root.display());
                status_changed = true;
            }
        }
    }
    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    outcome
}

#[cfg(test)]
pub(crate) fn configure_search_order_context_for_test(
    root: &Path,
    storage: &Path,
) -> (AppContext, std::path::PathBuf) {
    std::fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(root.join("ignored.rs"), "fn ignored_marker() {}\n").unwrap();

    let ctx = AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        },
    );
    let canonical_root = std::fs::canonicalize(root).unwrap();
    let ignored_path = canonical_root.join("ignored.rs");
    ctx.set_canonical_cache_root(canonical_root.clone());
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx.enqueue_configure_maintenance(crate::context::ConfigureMaintenanceJob {
        generation: ctx.configure_generation(),
        root_path: root.to_path_buf(),
        canonical_cache_root: canonical_root,
        harness: crate::harness::Harness::Opencode,
        storage_root: storage.to_path_buf(),
        harness_dir: storage.join("opencode"),
        session_id: "order-test".to_string(),
        home_match: false,
        format_tool_cache_clear_needed: false,
        run_bash_replay: false,
        refresh_project_runtime: true,
        sync_bash_compress_flag: false,
        reset_filter_registry: false,
        clear_failed_spawns: false,
        warm_callgraph_store: false,
        artifact_load_starts: Vec::new(),
    });

    let (search_tx, search_rx) = crossbeam_channel::unbounded();
    search_tx
        .send(crate::search_index::SearchIndex::new())
        .unwrap();
    drop(search_tx);
    *ctx.search_index_rx()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(search_rx);
    ctx.add_pending_search_index_paths([ignored_path.clone()]);
    (ctx, ignored_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};

    fn watcher_context(
        root: &Path,
    ) -> (AppContext, crossbeam_channel::Sender<WatcherDispatchEvent>) {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.update_config(|config| {
            config.project_root = Some(root.to_path_buf());
        });
        ctx.set_canonical_cache_root(root.to_path_buf());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        (ctx, tx)
    }

    #[test]
    fn standalone_configure_tail_precedes_completed_search_install() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (ctx, ignored_path) =
            configure_search_order_context_for_test(root.path(), storage.path());
        assert!(!watcher_path_is_ignored_by_current_matcher(
            &ctx,
            &ignored_path
        ));

        drain_deferred_configure_maintenance(&ctx);
        drain_configure_warning_events(&ctx);
        drain_search_index_events(&ctx);

        assert!(watcher_path_is_ignored_by_current_matcher(
            &ctx,
            &ignored_path
        ));
        assert_eq!(
            ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .expect("completed search index installed")
                .file_count(),
            0,
            "configure must install the ignore matcher before pending paths replay"
        );
        ctx.stop_watcher_runtime();
    }

    #[test]
    fn post_ack_semantic_ready_transition_pushes_status_changed() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.project_root = Some(root.path().to_path_buf());
        config.semantic_search = true;
        let ctx = AppContext::new(default_language_provider_factory(), config);
        ctx.set_canonical_cache_root(root.path().to_path_buf());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        *ctx.semantic_index_rx().lock() = Some(event_rx);
        let (push_tx, push_rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = push_tx.send(frame);
        }))));

        event_tx
            .send(SemanticIndexEvent::Ready(
                crate::semantic_index::SemanticIndex::new(root.path().to_path_buf(), 3),
            ))
            .unwrap();
        drain_semantic_index_events(&ctx);

        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { .. }
        ));
        let pushed = push_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("ready transition should push status_changed");
        assert!(matches!(
            pushed,
            crate::protocol::PushFrame::StatusChanged(_)
        ));
    }

    #[test]
    fn watcher_drain_batch_cap_yields_with_events_remaining() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = watcher_context(temp.path());
        let cap = 3;
        for index in 0..(cap * 2 + 1) {
            tx.send(WatcherDispatchEvent::Paths(vec![temp
                .path()
                .join(format!("file-{index}.rs"))]))
                .unwrap();
        }

        let first = drain_watcher_events_bounded(&ctx, cap);

        assert_eq!(first.processed, cap);
        assert!(first.has_more);
        assert_eq!(ctx.pending_tier2_paths().len(), cap);
    }

    #[test]
    fn watcher_drain_requeues_until_all_events_are_applied() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = watcher_context(temp.path());
        let cap = 4;
        let total = cap * 2 + 3;
        for index in 0..total {
            tx.send(WatcherDispatchEvent::Paths(vec![temp
                .path()
                .join(format!("file-{index}.rs"))]))
                .unwrap();
        }

        let mut processed = 0;
        loop {
            let outcome = drain_watcher_events_bounded(&ctx, cap);
            assert!(outcome.processed <= cap);
            processed += outcome.processed;
            if !outcome.has_more {
                break;
            }
        }

        assert_eq!(processed, total);
        assert_eq!(ctx.pending_tier2_paths().len(), total);
    }
}

#[cfg(test)]
mod watcher_slice_tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};

    fn context_with_watcher(
        root: &Path,
    ) -> (AppContext, crossbeam_channel::Sender<WatcherDispatchEvent>) {
        let ctx = AppContext::new(default_language_provider_factory(), Config::default());
        ctx.update_config(|config| config.project_root = Some(root.to_path_buf()));
        ctx.set_canonical_cache_root(root.to_path_buf());
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        (ctx, tx)
    }

    fn set_watcher_unit_test_seam(delay: Duration, thresholds: Option<(Duration, Duration)>) {
        WATCHER_UNIT_TEST_DELAY.with(|value| value.set(delay));
        WATCHER_UNIT_TEST_THRESHOLDS.with(|value| value.set(thresholds));
        WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow_mut().clear());
    }

    fn clear_watcher_unit_test_seam() {
        set_watcher_unit_test_seam(Duration::ZERO, None);
    }

    #[test]
    fn callgraph_phase_batches_all_indexed_paths_into_one_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        let generated = temp.path().join("compiled.ts");
        std::fs::write(&generated, "// @generated\nexport const compiled = true;\n").unwrap();
        let mut paths = VecDeque::from([
            temp.path().join("a.rs"),
            temp.path().join("b.ts"),
            generated,
            temp.path().join("ignored.txt"),
        ]);
        let mut remaining = paths.len();
        let mut refreshed = Vec::new();

        let completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            WATCHER_DRAIN_SLICE_BUDGET,
            true,
            |_, changed| refreshed.push(changed.clone()),
        );

        assert!(completed);
        assert_eq!(remaining, 0);
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].len(), 2);
    }

    #[test]
    fn callgraph_phase_flushes_once_per_slice_before_requeue() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        let mut paths =
            VecDeque::from([temp.path().join("first.rs"), temp.path().join("second.rs")]);
        let mut remaining = paths.len();
        let mut refreshed = Vec::new();
        set_watcher_unit_test_seam(Duration::from_millis(2), None);

        let first_completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            Duration::from_millis(1),
            true,
            |_, changed| refreshed.push(changed.clone()),
        );
        assert!(!first_completed);
        assert_eq!(remaining, 1);
        assert_eq!(refreshed.len(), 1, "the yielded slice must flush its batch");

        let second_completed = apply_callgraph_watcher_phase(
            &ctx,
            &mut paths,
            &mut remaining,
            Instant::now(),
            Duration::from_millis(1),
            true,
            |_, changed| refreshed.push(changed.clone()),
        );
        clear_watcher_unit_test_seam();

        assert!(!second_completed);
        assert_eq!(remaining, 0);
        assert_eq!(refreshed.len(), 2);
        assert!(refreshed.iter().all(|batch| batch.len() == 1));
    }

    #[test]
    fn watcher_unit_watchdog_names_slow_phase_and_path() {
        let temp = tempfile::tempdir().unwrap();
        let slow_path = temp.path().join("slow.rs");
        let mut paths = VecDeque::from([slow_path.clone()]);
        let mut remaining = 1;
        set_watcher_unit_test_seam(
            Duration::from_millis(5),
            Some((Duration::from_millis(1), Duration::from_secs(1))),
        );

        let completed = apply_watcher_path_phase(
            WatcherDrainApplyPhase::SemanticIndex,
            &mut paths,
            &mut remaining,
            Instant::now(),
            WATCHER_DRAIN_SLICE_BUDGET,
            |_| {},
        );
        let logs = WATCHER_UNIT_TEST_LOGS.with(|logs| logs.borrow().clone());
        clear_watcher_unit_test_seam();

        assert!(completed);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("watcher drain unit exceeded 5s"));
        assert!(logs[0].contains("phase=semantic_index"));
        assert!(logs[0].contains(&format!("path={}", slow_path.display())));
    }

    #[test]
    fn watcher_callgraph_refresh_defers_when_ready_store_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.update_config(|config| config.callgraph_store = true);
        ctx.set_cache_role(false, None);
        let source = temp.path().join("pending.rs");
        let generated = temp.path().join("compiled.ts");
        std::fs::write(&generated, "// @generated\nexport const compiled = true;\n").unwrap();

        refresh_callgraph_store_for_watcher(&ctx, &HashSet::from([source.clone(), generated]));

        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let pending = ctx.take_pending_callgraph_store_paths();
            if !pending.is_empty() {
                assert_eq!(pending, vec![source]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "refresh worker did not defer the unavailable store batch"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn watcher_callgraph_refresh_keeps_worktree_paths_pending() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, _) = context_with_watcher(temp.path());
        ctx.update_config(|config| config.callgraph_store = true);
        ctx.set_cache_role(true, None);
        let source = temp.path().join("worktree.rs");

        refresh_callgraph_store_for_watcher(&ctx, &HashSet::from([source.clone()]));

        assert_eq!(ctx.take_pending_callgraph_store_paths(), vec![source]);
    }

    #[test]
    fn watcher_single_dispatch_event_is_sliced_by_path_count() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        let path_count = 1_024;
        let path_cap = 256;
        tx.send(WatcherDispatchEvent::Paths(
            (0..path_count)
                .map(|index| temp.path().join(format!("single-event-{index}.txt")))
                .collect(),
        ))
        .unwrap();

        let mut slices = 0;
        let mut processed = 0;
        loop {
            let outcome = drain_watcher_events_bounded(&ctx, path_cap);
            slices += 1;
            processed += outcome.processed;
            assert!(outcome.processed <= path_cap);
            if !outcome.has_more {
                break;
            }
            assert!(slices < 8, "single dispatch event did not converge");
        }

        assert_eq!(processed, path_count);
        // At least ceil(1024/256) slices from the path budget; the 250ms time
        // budget may end a slice early under parallel test load, so an exact
        // slice count would be load-sensitive.
        assert!(
            (4..=8).contains(&slices),
            "expected 4-8 path-budgeted slices, got {slices}"
        );
        assert_eq!(ctx.pending_tier2_paths().len(), path_count);
    }

    #[test]
    fn watcher_rescan_supersedes_pending_paths() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("before-rescan-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(first.processed, 2);
        assert!(first.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 3);

        tx.send(WatcherDispatchEvent::RescanRequired).unwrap();
        let second = drain_watcher_events_bounded(&ctx, 2);

        assert_eq!(second.processed, 0);
        assert!(!second.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
    }

    #[test]
    fn watcher_generation_change_discards_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let (ctx, tx) = context_with_watcher(temp.path());
        tx.send(WatcherDispatchEvent::Paths(
            (0..5)
                .map(|index| temp.path().join(format!("old-generation-{index}.txt")))
                .collect(),
        ))
        .unwrap();
        let first = drain_watcher_events_bounded(&ctx, 2);
        assert_eq!(first.processed, 2);
        assert!(first.has_more);

        ctx.advance_configure_generation();
        let second = drain_watcher_events_bounded(&ctx, 2);

        assert_eq!(second.processed, 0);
        assert!(!second.has_more);
        assert_eq!(ctx.watcher_drain_pending_path_count(), 0);
        assert_eq!(ctx.pending_tier2_paths().len(), 2);
    }
}
