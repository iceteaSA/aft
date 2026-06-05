use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{after, bounded, select, Receiver, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cache::{InspectCache, Tier2ContributionUpdates};
use super::dispatch::{default_worker, start_dispatch_loop, InspectWorker};
use super::freshness::ContributionFreshness;
use super::job::{
    normalize_path, CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, FileContribution,
    InspectCategory, InspectJob, InspectResult, InspectScanSuccess, InspectSnapshot, JobKey,
    JobOutcome, JobScope, DISPATCHED_CALLEE_SEPARATOR,
};
use super::scanners::DEFAULT_EXPORT_MARKER_KIND;
use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::callgraph::{is_bare_callee, resolve_symbol_query_in_data, CallGraph, EdgeResolution};
use crate::symbols::SymbolKind;

const DEFAULT_SOFT_DEADLINE: Duration = Duration::from_secs(1);

type WaiterTx = Sender<JobOutcome>;

#[derive(Clone)]
struct Waiter {
    tx: WaiterTx,
}

struct CachedContributionFreshness {
    file_path: PathBuf,
    freshness: FileFreshness,
}

#[derive(PartialEq, Eq)]
struct ContributionFingerprint {
    count: usize,
    set_hash: String,
    hash_complete: bool,
}

#[derive(Debug, Clone)]
pub struct Tier2RunSubmissionError {
    pub category: InspectCategory,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct Tier2RunSubmission {
    pub queued_categories: Vec<InspectCategory>,
    pub newly_queued_categories: Vec<InspectCategory>,
    pub errors: Vec<Tier2RunSubmissionError>,
}

impl Tier2RunSubmission {
    pub fn has_new_work(&self) -> bool {
        !self.newly_queued_categories.is_empty()
    }
}

pub struct InspectManager {
    request_tx: Sender<InspectJob>,
    result_rx: Receiver<InspectResult>,
    #[allow(dead_code)]
    pool: Arc<rayon::ThreadPool>,
    in_flight: Mutex<HashMap<JobKey, Vec<Waiter>>>,
    caches: Mutex<HashMap<PathBuf, Arc<InspectCache>>>,
    soft_deadline: Duration,
    next_job_id: AtomicU64,
    /// Monotonic count of Tier-2 completions delivered via the reuse path
    /// (watcher-driven scheduler runs). These bypass `result_rx`/
    /// `drain_completions`, so the `&AppContext`-side drain polls this counter
    /// to know when to refresh the agent status bar after a background scan.
    reuse_completions: AtomicU64,
}

impl InspectManager {
    pub fn new() -> Self {
        Self::with_worker(default_worker(), DEFAULT_SOFT_DEADLINE)
    }

    #[doc(hidden)]
    pub fn with_worker(worker: InspectWorker, soft_deadline: Duration) -> Self {
        let handles = start_dispatch_loop(worker);
        Self {
            request_tx: handles.request_tx,
            result_rx: handles.result_rx,
            pool: handles.pool,
            in_flight: Mutex::new(HashMap::new()),
            caches: Mutex::new(HashMap::new()),
            soft_deadline,
            next_job_id: AtomicU64::new(1),
            reuse_completions: AtomicU64::new(0),
        }
    }

    pub fn submit_category(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        self.submit_category_with_callgraph(snapshot, category, caller_scope, None)
    }

    pub fn submit_category_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        if !category.is_active() {
            return JobOutcome::Failed {
                message: format!("inspect category '{category}' is disabled in v0.33"),
            };
        }

        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let key = JobKey::for_category_scope(category, &caller_scope);
        let (waiter_tx, waiter_rx) = bounded(1);

        let wait_snapshot = snapshot.clone();
        match self.enqueue_with_waiter(
            snapshot,
            category,
            caller_scope.clone(),
            key.clone(),
            waiter_tx,
            callgraph_snapshot,
        ) {
            Ok(()) => self.wait_for_outcome(key, caller_scope, cache, waiter_rx, wait_snapshot),
            Err(message) => JobOutcome::Failed { message },
        }
    }

    pub fn submit_background(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> Result<JobKey, String> {
        self.submit_background_with_callgraph(snapshot, category, caller_scope, None)
    }

    pub fn submit_background_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<JobKey, String> {
        if !category.is_active() {
            return Err(format!(
                "inspect category '{category}' is disabled in v0.33"
            ));
        }
        let key = JobKey::for_category_scope(category, &caller_scope);
        self.enqueue_without_waiter(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        )?;
        Ok(key)
    }

    pub fn submit_tier2_run_with_reuse_background(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
    ) -> Result<JobKey, String> {
        if !category.is_active() {
            return Err(format!(
                "inspect category '{category}' is disabled in v0.33"
            ));
        }
        if !category.is_tier2() {
            return Err(format!(
                "inspect category '{category}' is not a Tier 2 category"
            ));
        }

        let job = self.tier2_reuse_job(snapshot, category, None);
        let key = job.key.clone();
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if in_flight.contains_key(&key) {
            return Ok(key);
        }
        in_flight.insert(key.clone(), Vec::new());
        drop(in_flight);

        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        pool.spawn(move || {
            let result = manager.tier2_run_with_reuse_job_result(job);
            manager.route_tier2_reuse_completion(result);
        });

        Ok(key)
    }

    pub fn submit_tier2_run_with_reuse_serial_background(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        categories: Vec<InspectCategory>,
    ) -> Tier2RunSubmission {
        let mut submission = Tier2RunSubmission::default();
        let mut requested = Vec::new();

        for category in categories {
            if !category.is_active() {
                submission.errors.push(Tier2RunSubmissionError {
                    category,
                    message: format!("inspect category '{category}' is disabled in v0.33"),
                });
                continue;
            }
            if !category.is_tier2() {
                submission.errors.push(Tier2RunSubmissionError {
                    category,
                    message: format!("inspect category '{category}' is not a Tier 2 category"),
                });
                continue;
            }
            requested.push(category);
        }

        if requested.is_empty() {
            return submission;
        }

        let mut in_flight = match self.in_flight.lock() {
            Ok(in_flight) => in_flight,
            Err(_) => {
                for category in requested {
                    submission.errors.push(Tier2RunSubmissionError {
                        category,
                        message: "inspect in-flight map lock poisoned".to_string(),
                    });
                }
                return submission;
            }
        };

        for category in requested {
            let key = JobKey::for_project_category(category);
            submission.queued_categories.push(category);
            if in_flight.contains_key(&key) {
                continue;
            }
            in_flight.insert(key, Vec::new());
            submission.newly_queued_categories.push(category);
        }
        drop(in_flight);

        if submission.newly_queued_categories.is_empty() {
            return submission;
        }

        let categories_for_worker = submission.newly_queued_categories.clone();
        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        pool.spawn(move || {
            for category in categories_for_worker {
                let result = manager.tier2_run_with_reuse_result(snapshot.clone(), category, None);
                manager.route_tier2_reuse_completion(result);
            }
        });

        submission
    }

    pub fn tier2_any_in_flight(&self) -> bool {
        self.in_flight
            .lock()
            .map(|in_flight| in_flight.keys().any(|key| key.category.is_tier2()))
            .unwrap_or(false)
    }

    pub fn drain_completions(&self) -> usize {
        let mut drained = 0usize;
        while let Ok(result) = self.result_rx.try_recv() {
            self.route_completion(result);
            drained += 1;
        }
        drained
    }

    pub fn cache_for_snapshot(
        &self,
        snapshot: &InspectSnapshot,
    ) -> Result<Arc<InspectCache>, String> {
        self.cache_for_paths(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
    }

    /// Latest persisted counts for the three Tier-2 categories, in
    /// `(dead_code, unused_exports, duplicates)` order. Reads the most recent
    /// aggregate regardless of contribution-hash freshness (last-known), so the
    /// agent status bar can refresh after a background scan completes without a
    /// freshness round-trip. A category with no readable aggregate reports
    /// `None` (never a fabricated `0`), so the status bar can preserve any
    /// last-known value and stay suppressed until every category is real (#1).
    pub fn latest_tier2_counts(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> (Option<usize>, Option<usize>, Option<usize>) {
        let Ok(cache) = self.cache_for_paths(inspect_dir, project_root) else {
            return (None, None, None);
        };
        let count_of = |category: InspectCategory| -> Option<usize> {
            cache
                .latest_aggregate_any_hash(category)
                .ok()
                .flatten()
                .and_then(|payload| {
                    payload
                        .get("count")
                        .and_then(serde_json::Value::as_u64)
                        .map(|count| count as usize)
                })
        };
        (
            count_of(InspectCategory::DeadCode),
            count_of(InspectCategory::UnusedExports),
            count_of(InspectCategory::Duplicates),
        )
    }

    pub fn cache_for_paths(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Arc<InspectCache>, String> {
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = inspect_dir.join(format!("{project_key}.sqlite"));
        let mut caches = self
            .caches
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?;
        if let Some(cache) = caches.get(&sqlite_path) {
            return Ok(Arc::clone(cache));
        }
        let cache = Arc::new(
            InspectCache::open(inspect_dir, project_root)
                .map_err(|error| format!("failed to open inspect cache: {error}"))?,
        );
        caches.insert(sqlite_path, Arc::clone(&cache));
        Ok(cache)
    }

    pub fn tier2_run_with_reuse(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        let result =
            self.tier2_run_with_reuse_result(snapshot.clone(), category, callgraph_snapshot);
        let outcome = match result.outcome {
            Ok(success) => JobOutcome::Fresh {
                payload: success.aggregate,
            },
            Err(message) => JobOutcome::Failed { message },
        };
        match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => filter_outcome_for_scope_with_contributions(
                outcome,
                &snapshot,
                category,
                cache.as_ref(),
                &caller_scope,
            ),
            Err(message) => JobOutcome::Failed { message },
        }
    }

    /// Read-only Tier 2 aggregate lookup for `aft_inspect`. Does NOT run any
    /// scanner — returns the latest cached aggregate if present and verifies
    /// its contribution freshness so warm cache hits are reported as fresh.
    /// This is the non-blocking variant intended for the synchronous `inspect`
    /// command path; Tier 2 scans run via the watcher-driven scheduler or the
    /// compatibility `aft_inspect_tier2_run` command.
    pub fn tier2_read_cached(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        self.tier2_read_cached_from_cache(&snapshot, category, &caller_scope, cache.as_ref())
    }

    pub fn tier2_read_cached_readonly(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        let key = JobKey::for_project_category(category);
        let in_flight = self
            .in_flight
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false);
        let cache = match InspectCache::open_readonly(
            snapshot.inspect_dir.clone(),
            snapshot.project_root.clone(),
        ) {
            Ok(Some(cache)) => cache,
            Ok(None) => return JobOutcome::Pending { in_flight },
            Err(error) => {
                return JobOutcome::Failed {
                    message: error.to_string(),
                }
            }
        };
        self.tier2_read_cached_from_cache(&snapshot, category, &caller_scope, &cache)
    }

    fn tier2_read_cached_from_cache(
        &self,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
        caller_scope: &JobScope,
        cache: &InspectCache,
    ) -> JobOutcome {
        let key = JobKey::for_project_category(category);
        let in_flight = self
            .in_flight
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false);
        match cache.get_aggregated(&key) {
            Ok(Some(payload)) => {
                match self.tier2_cached_aggregate_is_fresh(snapshot, category, cache) {
                    Ok(true) => filter_outcome_for_scope_with_contributions(
                        JobOutcome::Fresh { payload },
                        snapshot,
                        category,
                        cache,
                        caller_scope,
                    ),
                    Ok(false) => filter_outcome_for_scope_with_contributions(
                        JobOutcome::Stale {
                            cached: Some(payload),
                            in_flight,
                        },
                        snapshot,
                        category,
                        cache,
                        caller_scope,
                    ),
                    Err(message) => JobOutcome::Failed { message },
                }
            }
            Ok(None) => match cache.latest_aggregate_any_hash(category) {
                Ok(Some(payload)) => filter_outcome_for_scope_with_contributions(
                    JobOutcome::Stale {
                        cached: Some(payload),
                        in_flight,
                    },
                    snapshot,
                    category,
                    cache,
                    caller_scope,
                ),
                Ok(None) => JobOutcome::Pending { in_flight },
                Err(error) => JobOutcome::Failed {
                    message: error.to_string(),
                },
            },
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn tier2_cached_aggregate_is_fresh(
        &self,
        snapshot: &InspectSnapshot,
        category: InspectCategory,
        cache: &InspectCache,
    ) -> Result<bool, String> {
        let cached_records = load_contribution_freshness(cache, category)?;
        let project_scope = JobScope::for_project(snapshot.project_root.clone());
        let project_files = scope_files(&snapshot.project_root, &project_scope);
        let current_by_relative = current_project_files(&snapshot.project_root, &project_files);
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();

        for record in &cached_records {
            let relative = freshness_record_relative_key(record);
            if !current_by_relative.contains_key(&relative) {
                return Ok(false);
            }

            let absolute = if record.file_path.is_absolute() {
                record.file_path.clone()
            } else {
                snapshot.project_root.join(&record.file_path)
            };
            match verify_contribution_file_strict(&absolute, &record.freshness) {
                ContributionFreshness::Fresh { .. } => {}
                ContributionFreshness::Stale | ContributionFreshness::Deleted => return Ok(false),
            }
        }

        Ok(current_by_relative
            .keys()
            .all(|relative| cached_relative.contains(relative)))
    }

    #[doc(hidden)]
    pub fn tier2_run_with_reuse_result(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectResult {
        let job = self.tier2_reuse_job(snapshot, category, callgraph_snapshot);
        self.tier2_run_with_reuse_job_result(job)
    }

    fn tier2_run_with_reuse_job_result(&self, mut job: InspectJob) -> InspectResult {
        let started = Instant::now();
        if !job.category.is_active() {
            let result = InspectResult::failed(
                &job,
                format!("inspect category '{}' is disabled in v0.33", job.category),
                started.elapsed(),
            );
            log_tier2_benchmark_category_end(&result);
            return result;
        }
        if !job.category.is_tier2() {
            let result = InspectResult::failed(
                &job,
                format!(
                    "inspect category '{}' is not a Tier 2 category",
                    job.category
                ),
                started.elapsed(),
            );
            log_tier2_benchmark_category_end(&result);
            return result;
        }

        let project_scope = JobScope::for_project(job.project_root.clone());
        job.scope_files = scope_files(&job.project_root, &project_scope);
        log_tier2_benchmark_category_start(&job);
        let cache = match self.cache_for_paths(job.inspect_dir.clone(), job.project_root.clone()) {
            Ok(cache) => cache,
            Err(message) => {
                let result = InspectResult::failed(&job, message, started.elapsed());
                log_tier2_benchmark_category_end(&result);
                return result;
            }
        };
        if let Ok(Some(success)) = self.tier2_quick_reuse_success(&job, cache.as_ref()) {
            let result = InspectResult::success(&job, success, started.elapsed());
            log_tier2_benchmark_category_end(&result);
            return result;
        }

        let result = match self.tier2_run_with_reuse_job(&job, &cache) {
            Ok(success) => InspectResult::success(&job, success, started.elapsed()),
            Err(message) => InspectResult::failed(&job, message, started.elapsed()),
        };
        log_tier2_benchmark_category_end(&result);
        result
    }

    fn tier2_reuse_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectJob {
        InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key: JobKey::for_project_category(category),
            category,
            scope_files: Vec::new(),
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            callgraph_snapshot,
        }
    }

    fn tier2_quick_reuse_success(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
    ) -> Result<Option<InspectScanSuccess>, String> {
        let Some(aggregate) = cache
            .get_aggregated(&job.key)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        let cached = load_contribution_fingerprint(cache, job.category)?;
        let current = current_file_fingerprint(&job.project_root, &job.scope_files)?;
        if !cached.hash_complete || !current.hash_complete || cached != current {
            return Ok(None);
        }

        cache
            .touch_tier2_last_full_run(job.category)
            .map_err(|error| error.to_string())?;
        Ok(Some(InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate,
        }))
    }

    fn tier2_run_with_reuse_job(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
    ) -> Result<InspectScanSuccess, String> {
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();
        #[cfg(debug_assertions)]
        let cold_cache = cached_relative.is_empty();

        let mut updates = Tier2ContributionUpdates::default();
        let mut scan_by_relative = BTreeMap::<String, PathBuf>::new();
        let mut aggregate_job = job.clone();

        for record in cached_records {
            let relative = freshness_record_relative_key(&record);
            let relative_path = PathBuf::from(&relative);
            let Some(current_file) = current_by_relative.get(&relative) else {
                updates.deletes.push(relative_path);
                continue;
            };

            let absolute = job.project_root.join(&record.file_path);
            match verify_contribution_file_strict(&absolute, &record.freshness) {
                ContributionFreshness::Fresh {
                    metadata_changed,
                    freshness,
                } => {
                    if metadata_changed {
                        updates.metadata_updates.push((relative_path, freshness));
                    }
                }
                ContributionFreshness::Stale => {
                    updates.deletes.push(relative_path);
                    scan_by_relative.insert(relative, current_file.clone());
                }
                ContributionFreshness::Deleted => {
                    updates.deletes.push(relative_path);
                }
            }
        }

        for (relative, file) in &current_by_relative {
            if !cached_relative.contains(relative) {
                scan_by_relative.insert(relative.clone(), file.clone());
            }
        }

        let mut scan_files = scan_by_relative.into_values().collect::<Vec<_>>();
        if !scan_files.is_empty() {
            let mut scan_job = job.clone();
            scan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
            scan_job.scope_files = scan_files.clone();
            if scan_job.category == InspectCategory::DeadCode
                && scan_job.callgraph_snapshot.is_none()
            {
                scan_job.callgraph_snapshot = build_tier2_callgraph_snapshot(
                    &scan_job.project_root,
                    scan_job.config.max_callgraph_files,
                );
            }
            aggregate_job.callgraph_snapshot = scan_job.callgraph_snapshot.clone();
            #[cfg(debug_assertions)]
            if cold_cache {
                std::thread::sleep(Duration::from_millis(10));
            }
            let scan_result = run_tier2_scan(&scan_job);
            let scan_success = scan_result.outcome.map_err(|message| {
                format!("{} incremental scan failed: {message}", job.category)
            })?;
            updates.upserts.extend(scan_success.contributions);
        }

        let has_updates = !updates.upserts.is_empty()
            || !updates.deletes.is_empty()
            || !updates.metadata_updates.is_empty();
        if !has_updates {
            if let Some(aggregate) = cache
                .get_aggregated(&job.key)
                .map_err(|error| error.to_string())?
            {
                cache
                    .touch_tier2_last_full_run(job.category)
                    .map_err(|error| error.to_string())?;
                return Ok(InspectScanSuccess {
                    scanned_files: scan_files,
                    contributions: Vec::new(),
                    aggregate,
                });
            }
        }

        let mut contribution_set_hash = if has_updates {
            cache
                .apply_contribution_updates(job.category, updates)
                .map_err(|error| error.to_string())?
        } else {
            cache
                .contribution_set_hash(job.category)
                .map_err(|error| error.to_string())?
        };

        if let Some(aggregate) = cache
            .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
            .map_err(|error| error.to_string())?
        {
            cache
                .touch_tier2_last_full_run(job.category)
                .map_err(|error| error.to_string())?;
            let contributions = load_contributions(cache, job)?;
            return Ok(InspectScanSuccess {
                scanned_files: scan_files,
                contributions,
                aggregate,
            });
        }

        if category_contributions_depend_on_entry_points(job.category) {
            // Manifest edits can change entry/public roots without touching any
            // source file. Dead-code and unused-export file contributions embed
            // those roots, so an aggregate hash miss for these categories must
            // refresh every current contribution before rolling up again.
            let full_scan_files = current_by_relative.into_values().collect::<Vec<_>>();
            if !full_scan_files.is_empty() {
                let mut rescan_job = job.clone();
                rescan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
                rescan_job.scope_files = full_scan_files.clone();
                if rescan_job.category == InspectCategory::DeadCode
                    && rescan_job.callgraph_snapshot.is_none()
                {
                    rescan_job.callgraph_snapshot = build_tier2_callgraph_snapshot(
                        &rescan_job.project_root,
                        rescan_job.config.max_callgraph_files,
                    );
                }
                let scan_result = run_tier2_scan(&rescan_job);
                let scan_success = scan_result.outcome.map_err(|message| {
                    format!(
                        "{} full rescan after entry-point cache miss failed: {message}",
                        job.category
                    )
                })?;
                let rescan_updates = Tier2ContributionUpdates {
                    upserts: scan_success.contributions,
                    ..Tier2ContributionUpdates::default()
                };
                contribution_set_hash = cache
                    .apply_contribution_updates(job.category, rescan_updates)
                    .map_err(|error| error.to_string())?;
                aggregate_job.callgraph_snapshot = rescan_job.callgraph_snapshot.clone();
                scan_files = full_scan_files;

                if let Some(aggregate) = cache
                    .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
                    .map_err(|error| error.to_string())?
                {
                    cache
                        .touch_tier2_last_full_run(job.category)
                        .map_err(|error| error.to_string())?;
                    let contributions = load_contributions(cache, job)?;
                    return Ok(InspectScanSuccess {
                        scanned_files: scan_files,
                        contributions,
                        aggregate,
                    });
                }
            }
        }

        if aggregate_job.category == InspectCategory::DeadCode
            && aggregate_job.callgraph_snapshot.is_none()
        {
            aggregate_job.callgraph_snapshot = build_tier2_callgraph_snapshot(
                &aggregate_job.project_root,
                aggregate_job.config.max_callgraph_files,
            );
        }
        let contributions = load_contributions(cache, &aggregate_job)?;
        let aggregate = roll_up_tier2_contributions(&aggregate_job, &contributions);
        cache
            .store_tier2_aggregate(job.key.clone(), &contribution_set_hash, aggregate.clone())
            .map_err(|error| error.to_string())?;

        Ok(InspectScanSuccess {
            scanned_files: scan_files,
            contributions,
            aggregate,
        })
    }

    fn enqueue_with_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        waiter_tx: WaiterTx,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if let Some(waiters) = in_flight.get_mut(&key) {
            waiters.push(Waiter { tx: waiter_tx });
            return Ok(());
        }

        in_flight.insert(key.clone(), vec![Waiter { tx: waiter_tx }]);
        drop(in_flight);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&key);
            }
            return Err(message);
        }
        Ok(())
    }

    fn enqueue_without_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if in_flight.contains_key(&key) {
            return Ok(());
        }
        in_flight.insert(key.clone(), Vec::new());
        drop(in_flight);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&key);
            }
            return Err(message);
        }
        Ok(())
    }

    fn enqueue_new_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let scan_scope = if category.is_tier2() {
            JobScope::for_project(snapshot.project_root.clone())
        } else {
            caller_scope
        };
        let scope_files = scope_files(&snapshot.project_root, &scan_scope);
        let job = InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key,
            category,
            scope_files,
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            callgraph_snapshot,
        };
        self.request_tx
            .send(job)
            .map_err(|_| "inspect dispatch loop is unavailable".to_string())
    }

    fn wait_for_outcome(
        &self,
        key: JobKey,
        caller_scope: JobScope,
        cache: Arc<InspectCache>,
        waiter_rx: Receiver<JobOutcome>,
        snapshot: InspectSnapshot,
    ) -> JobOutcome {
        let timeout = after(self.soft_deadline);
        let result_rx = self.result_rx.clone();
        loop {
            select! {
                recv(waiter_rx) -> outcome => {
                    return match outcome {
                        Ok(outcome) => filter_outcome_for_scope_with_contributions(
                            outcome,
                            &snapshot,
                            key.category,
                            cache.as_ref(),
                            &caller_scope,
                        ),
                        Err(_) => self.timeout_outcome(&key, &caller_scope, &cache, &snapshot),
                    };
                }
                recv(result_rx) -> result => {
                    match result {
                        Ok(result) => self.route_completion(result),
                        Err(_) => return self.timeout_outcome(&key, &caller_scope, &cache, &snapshot),
                    }
                }
                recv(timeout) -> _ => {
                    return self.timeout_outcome(&key, &caller_scope, &cache, &snapshot);
                }
            }
        }
    }

    fn timeout_outcome(
        &self,
        key: &JobKey,
        caller_scope: &JobScope,
        cache: &InspectCache,
        snapshot: &InspectSnapshot,
    ) -> JobOutcome {
        match cache.get_aggregated(key) {
            Ok(Some(cached)) => filter_outcome_for_scope_with_contributions(
                JobOutcome::Stale {
                    cached: Some(cached),
                    in_flight: true,
                },
                snapshot,
                key.category,
                cache,
                caller_scope,
            ),
            Ok(None) => JobOutcome::Pending { in_flight: true },
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn route_completion(&self, result: InspectResult) {
        let outcome = self.completion_outcome(result.clone());
        let waiters = self
            .in_flight
            .lock()
            .ok()
            .and_then(|mut in_flight| in_flight.remove(&result.key))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.tx.send(outcome.clone());
        }
    }

    fn route_tier2_reuse_completion(&self, result: InspectResult) {
        let outcome = match result.outcome.clone() {
            Ok(success) => JobOutcome::Fresh {
                payload: success.aggregate,
            },
            Err(message) => JobOutcome::Failed { message },
        };
        let waiters = self
            .in_flight
            .lock()
            .ok()
            .and_then(|mut in_flight| in_flight.remove(&result.key))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.tx.send(outcome.clone());
        }
        // Signal the main-thread drain that a background (watcher-driven) Tier-2
        // scan finished so it can refresh the status bar. This path bypasses
        // `result_rx`/`drain_completions`, so without this counter the bar's
        // counts and `~` marker would only update on a manual `aft_inspect`.
        self.reuse_completions.fetch_add(1, Ordering::SeqCst);
    }

    /// Snapshot the cumulative count of reuse-path (watcher-driven) Tier-2
    /// completions. The main-thread drain compares this against its last-seen
    /// value to detect background scans that finished since the previous tick.
    pub fn reuse_completion_count(&self) -> u64 {
        self.reuse_completions.load(Ordering::SeqCst)
    }

    fn completion_outcome(&self, result: InspectResult) -> JobOutcome {
        let cache =
            match self.cache_for_paths(result.inspect_dir.clone(), result.project_root.clone()) {
                Ok(cache) => cache,
                Err(message) => return JobOutcome::Failed { message },
            };

        match result.outcome {
            Ok(success) => {
                let store_result = if result.category.is_tier2() {
                    cache.store_tier2_result(
                        result.key.clone(),
                        &success.scanned_files,
                        &success.contributions,
                        success.aggregate.clone(),
                    )
                } else {
                    cache.store_aggregated(result.key, success.aggregate.clone())
                };

                match store_result {
                    Ok(()) => JobOutcome::Fresh {
                        payload: success.aggregate,
                    },
                    Err(error) => JobOutcome::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Err(message) => JobOutcome::Failed { message },
        }
    }
}

impl Default for InspectManager {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_tier2_read_category(category: InspectCategory) -> Result<(), JobOutcome> {
    if !category.is_active() {
        return Err(JobOutcome::Failed {
            message: format!("inspect category '{category}' is disabled in v0.33"),
        });
    }
    if !category.is_tier2() {
        return Err(JobOutcome::Failed {
            message: format!("inspect category '{category}' is not a Tier 2 category"),
        });
    }
    Ok(())
}

fn scope_files(project_root: &Path, scope: &JobScope) -> Vec<PathBuf> {
    let mut files = crate::callgraph::walk_project_files(project_root)
        .filter(|path| scope.contains(path))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn current_project_files(project_root: &Path, files: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    files
        .iter()
        .map(|file| (relative_cache_key(project_root, file), file.clone()))
        .collect()
}

fn tier2_benchmark_logging_enabled() -> bool {
    std::env::var_os("AFT_SETTLE_BENCH_LOG").is_some()
}

fn log_tier2_benchmark_category_start(job: &InspectJob) {
    if !tier2_benchmark_logging_enabled() {
        return;
    }
    crate::slog_info!(
        "settle bench: tier2_category_start category={} job_id={} files={}",
        job.category.as_str(),
        job.job_id,
        job.scope_files.len()
    );
}

fn log_tier2_benchmark_category_end(result: &InspectResult) {
    if !tier2_benchmark_logging_enabled() {
        return;
    }
    match &result.outcome {
        Ok(success) => {
            let count = success
                .aggregate
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            crate::slog_info!(
                "settle bench: tier2_category_end category={} job_id={} status=success total_ms={} scanned_files={} contributions={} count={}",
                result.category.as_str(),
                result.job_id,
                result.duration.as_millis(),
                success.scanned_files.len(),
                success.contributions.len(),
                count
            );
        }
        Err(message) => {
            crate::slog_info!(
                "settle bench: tier2_category_end category={} job_id={} status=failed total_ms={} error={}",
                result.category.as_str(),
                result.job_id,
                result.duration.as_millis(),
                message.replace('\n', " ")
            );
        }
    }
}

fn build_tier2_callgraph_snapshot(
    project_root: &Path,
    max_callgraph_files: usize,
) -> Option<Arc<CallgraphSnapshot>> {
    let started = Instant::now();
    let mut graph = CallGraph::new(project_root.to_path_buf());
    let graph_files = graph.project_files().to_vec();
    // Interim #86 guard: the per-file parse + cross-file resolve loop below is the
    // unbounded multi-core CPU sink on large repos (the interactive call graph already
    // self-caps at the same threshold). Skipping above the cap returns None, which
    // dead_code surfaces honestly as `callgraph_available: false` rather than a partial
    // (silently-corrupt) graph. The file-discovery walk above is cheap; the parse loop is
    // the cost we avoid. `--force`/UNCAPPED (1e9) keeps full builds for benchmarks.
    if graph_files.len() > max_callgraph_files {
        crate::slog_info!(
            "tier2 dead_code: skipping callgraph snapshot — {} files exceeds max_callgraph_files={}; reporting dead_code as callgraph_unavailable (raise lsp.max_callgraph_files or scope the repo)",
            graph_files.len(),
            max_callgraph_files
        );
        return None;
    }
    if tier2_benchmark_logging_enabled() {
        crate::slog_info!(
            "settle bench: tier2_callgraph_snapshot_start files={}",
            graph_files.len()
        );
    }
    let files = graph_files
        .iter()
        .map(canonicalize_for_snapshot)
        .collect::<Vec<_>>();
    let resolved_entry_points = super::entry_points::resolve_entry_points(project_root);

    let mut exported_symbols = Vec::new();
    let mut outbound_calls = Vec::new();
    let mut entry_points = BTreeSet::new();
    let mut built_files = 0usize;

    for file in &graph_files {
        let snapshot_file = canonicalize_for_snapshot(file);
        if is_entry_point_file(&resolved_entry_points, &snapshot_file) {
            entry_points.insert(snapshot_file.clone());
        }

        let file_data = match graph.build_file(file) {
            Ok(file_data) => file_data.clone(),
            Err(_) => continue,
        };
        built_files += 1;

        for symbol in &file_data.exported_symbols {
            let metadata = file_data.symbol_metadata_for(symbol);
            exported_symbols.push(CallgraphExport {
                file: snapshot_file.clone(),
                symbol: symbol.clone(),
                kind: metadata
                    .map(|metadata| symbol_kind_name(&metadata.kind))
                    .unwrap_or("unknown")
                    .to_string(),
                line: metadata.map(|metadata| metadata.line).unwrap_or(1),
            });
        }

        if let Some(default_symbol) = &file_data.default_export_symbol {
            let metadata = file_data.symbol_metadata_for(default_symbol);
            exported_symbols.push(CallgraphExport {
                file: snapshot_file.clone(),
                symbol: default_symbol.clone(),
                kind: DEFAULT_EXPORT_MARKER_KIND.to_string(),
                line: metadata.map(|metadata| metadata.line).unwrap_or(1),
            });
        }

        for (caller_symbol, calls) in &file_data.calls_by_symbol {
            for call in calls {
                let target = match graph.resolve_cross_file_edge(
                    &call.full_callee,
                    &call.callee_name,
                    file,
                    &file_data.import_block,
                ) {
                    EdgeResolution::Resolved { file, symbol } => {
                        let file = canonicalize_for_snapshot(&file);
                        format!("{}::{symbol}", file.display())
                    }
                    // Unresolved cross-file edge. Before falling back to a bare
                    // callee name, try to resolve it to a symbol DEFINED IN THE
                    // SAME FILE (private functions included) — mirroring
                    // build_reverse_index. This is what makes a local call like
                    // `main()` -> `dispatch()` resolve to `main.rs::dispatch`
                    // (the private command router) instead of leaking a bare
                    // `dispatch` that dead_code then misresolves to an unrelated
                    // exported `dispatch` in another file. Without this, liveness
                    // breaks at every private same-file intermediary.
                    EdgeResolution::Unresolved { callee_name } => {
                        if is_bare_callee(&call.full_callee, &callee_name) {
                            match resolve_symbol_query_in_data(&file_data, file, &callee_name) {
                                Ok(symbol) => {
                                    format!("{}::{symbol}", snapshot_file.display())
                                }
                                Err(_) => callee_name,
                            }
                        } else {
                            callee_name
                        }
                    }
                };
                let target = if is_method_dispatch_callee(&call.full_callee, &call.callee_name) {
                    format!("{target}{DISPATCHED_CALLEE_SEPARATOR}{}", call.full_callee)
                } else {
                    target
                };
                outbound_calls.push(CallgraphOutboundCall {
                    caller_file: snapshot_file.clone(),
                    caller_symbol: caller_symbol.clone(),
                    target,
                    line: call.line,
                });
            }
        }
    }

    if tier2_benchmark_logging_enabled() {
        crate::slog_info!(
            "settle bench: tier2_callgraph_snapshot_end files={} built_files={} exported_symbols={} outbound_calls={} entry_points={} ms={}",
            graph_files.len(),
            built_files,
            exported_symbols.len(),
            outbound_calls.len(),
            entry_points.len(),
            started.elapsed().as_millis()
        );
    }

    Some(Arc::new(CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        files,
        exported_symbols,
        outbound_calls,
        entry_points,
    }))
}

fn canonicalize_for_snapshot(path: &PathBuf) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn is_entry_point_file(entry_points: &super::entry_points::EntryPointSet, file: &Path) -> bool {
    entry_points.is_entry_point(file)
}

fn is_method_dispatch_callee(full_callee: &str, callee_name: &str) -> bool {
    let full_callee = full_callee.trim();
    if !full_callee.contains('.') || full_callee == callee_name.trim() {
        return false;
    }

    full_callee
        .rsplit('.')
        .next()
        .map(|segment| segment.trim().trim_start_matches('?') == callee_name.trim())
        .unwrap_or(false)
}

fn symbol_kind_name(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn load_contribution_fingerprint(
    cache: &InspectCache,
    category: InspectCategory,
) -> Result<ContributionFingerprint, String> {
    let (count, set_hash, hash_complete) = cache
        .contribution_fingerprint(category)
        .map_err(|error| error.to_string())?;
    Ok(ContributionFingerprint {
        count,
        set_hash,
        hash_complete,
    })
}

fn current_file_fingerprint(
    project_root: &Path,
    files: &[PathBuf],
) -> Result<ContributionFingerprint, String> {
    let mut entries = Vec::with_capacity(files.len());
    let mut hash_complete = true;
    for file in files {
        let freshness = cache_freshness::collect(file)
            .map_err(|error| format!("failed to fingerprint {}: {error}", file.display()))?;
        let relative_path = relative_cache_key(project_root, file);
        let mtime_ns = system_time_to_ns_i64(freshness.mtime);
        if freshness.content_hash == cache_freshness::zero_hash() {
            hash_complete = false;
        }
        entries.push((
            relative_path,
            mtime_ns,
            freshness.size,
            freshness.content_hash.to_hex().to_string(),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = blake3::Hasher::new();
    for (relative_path, mtime_ns, file_size, file_hash) in &entries {
        update_contribution_fingerprint_hash(
            &mut hasher,
            relative_path,
            *mtime_ns,
            *file_size,
            file_hash,
        );
    }

    Ok(ContributionFingerprint {
        count: entries.len(),
        set_hash: hasher.finalize().to_hex().to_string(),
        hash_complete,
    })
}

fn update_contribution_fingerprint_hash(
    hasher: &mut blake3::Hasher,
    relative_path: &str,
    mtime_ns: i64,
    file_size: u64,
    file_hash: &str,
) {
    hasher.update(relative_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(&mtime_ns.to_le_bytes());
    hasher.update(&file_size.to_le_bytes());
    hasher.update(&[0]);
    hasher.update(file_hash.as_bytes());
}

fn verify_contribution_file_strict(path: &Path, cached: &FileFreshness) -> ContributionFreshness {
    match cache_freshness::verify_file_strict(path, cached) {
        FreshnessVerdict::HotFresh => ContributionFreshness::Fresh {
            metadata_changed: false,
            freshness: *cached,
        },
        FreshnessVerdict::ContentFresh {
            new_mtime,
            new_size,
        } => ContributionFreshness::Fresh {
            metadata_changed: true,
            freshness: FileFreshness {
                mtime: new_mtime,
                size: new_size,
                content_hash: cached.content_hash,
            },
        },
        FreshnessVerdict::Stale => ContributionFreshness::Stale,
        FreshnessVerdict::Deleted => ContributionFreshness::Deleted,
    }
}

fn load_contribution_freshness(
    cache: &InspectCache,
    category: InspectCategory,
) -> Result<Vec<CachedContributionFreshness>, String> {
    cache
        .contribution_freshness(category)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|(file_path, freshness)| CachedContributionFreshness {
                    file_path,
                    freshness,
                })
                .collect()
        })
}

fn freshness_record_relative_key(record: &CachedContributionFreshness) -> String {
    record.file_path.to_string_lossy().to_string()
}

fn system_time_to_ns_i64(time: SystemTime) -> i64 {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    nanos.min(i64::MAX as u128) as i64
}

fn relative_cache_key(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn load_contributions(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<Vec<FileContribution>, String> {
    cache
        .load_tier2_contributions(job.category)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|record| contribution_from_record(&job.project_root, record))
                .collect()
        })
}

fn contribution_from_record(
    project_root: &Path,
    record: super::cache::ContributionRecord,
) -> FileContribution {
    FileContribution::new(
        record.category,
        project_root.join(record.file_path),
        record.freshness,
        record.contribution,
    )
    .with_type_ref_names(record.type_ref_names)
}

fn run_tier2_scan(job: &InspectJob) -> InspectResult {
    use super::scanners;

    match job.category {
        InspectCategory::DeadCode => scanners::dead_code::run_dead_code_scan(job),
        InspectCategory::UnusedExports => scanners::unused_exports::run_unused_exports_scan(job),
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(job),
        other => InspectResult::failed(
            job,
            format!("inspect category '{other}' is not an active Tier 2 scanner"),
            Duration::from_secs(0),
        ),
    }
}

fn roll_up_tier2_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    roll_up_tier2_contributions_with_limit(job, contributions, Some(MAX_DRILL_DOWN_ITEMS))
}

fn roll_up_tier2_contributions_with_limit(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    match job.category {
        InspectCategory::DeadCode => {
            roll_up_dead_code_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::UnusedExports => {
            roll_up_unused_exports_contributions(job, contributions, drill_down_limit)
        }
        InspectCategory::Duplicates => {
            roll_up_duplicate_contributions(job, contributions, drill_down_limit)
        }
        _ => json!({
            "count": 0,
            "items": [],
            "scanned_files": contributions.len(),
        }),
    }
}

fn scoped_tier2_payload_from_contributions(
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &InspectCache,
    project_payload: Value,
    scope: &JobScope,
) -> Result<Value, String> {
    if scope.is_project_wide() {
        return Ok(project_payload);
    }

    let project_scope = JobScope::for_project(snapshot.project_root.clone());
    let rollup_job = scoped_tier2_rollup_job(snapshot, category, &project_scope);
    let contributions = load_contributions(cache, &rollup_job)?;
    let full_payload = roll_up_tier2_contributions_with_limit(&rollup_job, &contributions, None);
    let scoped_payload = filter_payload_for_scope(full_payload, scope);
    Ok(cap_payload_drill_down(scoped_payload, MAX_DRILL_DOWN_ITEMS))
}

fn scoped_tier2_rollup_job(
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    scope: &JobScope,
) -> InspectJob {
    InspectJob {
        job_id: 0,
        key: JobKey::for_project_category(category),
        category,
        scope_files: scope_files(&snapshot.project_root, scope),
        project_root: snapshot.project_root.clone(),
        inspect_dir: snapshot.inspect_dir.clone(),
        config: Arc::clone(&snapshot.config),
        symbol_cache: Arc::clone(&snapshot.symbol_cache),
        callgraph_snapshot: (category == InspectCategory::DeadCode)
            .then(|| Arc::new(CallgraphSnapshot::default())),
    }
}

fn roll_up_dead_code_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    if job.callgraph_snapshot.is_none() {
        return super::scanners::dead_code::callgraph_unavailable_aggregate(job.scope_files.len());
    }

    let public_api_files = super::scanners::dead_code::collect_public_api_files(&job.project_root);
    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    super::scanners::dead_code::aggregate_dead_code_contributions_with_limit(
        contributions,
        &public_api_files,
        &roles,
        drill_down_limit,
    )
}

fn roll_up_unused_exports_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<UnusedExportsContribution>(contribution.contribution.clone())
                .ok()
        })
        .collect::<Vec<_>>();

    let (public_api_entries, package_warnings) = unused_public_api_entries(&job.project_root);
    let mut imported_by: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut uncertain_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for scan in &parsed {
        for import in &scan.imports {
            let Some(resolved_file) = &import.resolved_file else {
                continue;
            };
            for name in &import.named {
                if name == "*" {
                    uncertain_by
                        .entry(resolved_file.clone())
                        .or_default()
                        .insert(scan.file.clone());
                } else {
                    imported_by
                        .entry((resolved_file.clone(), name.clone()))
                        .or_default()
                        .insert(scan.file.clone());
                }
            }
        }
    }

    let mut count = 0usize;
    let mut items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    for scan in &parsed {
        if public_api_entries.contains(&scan.file) {
            continue;
        }
        // Mirror the fresh-scan path: fixtures/corpora/mock data are consumed
        // by path, never imported, so their exports always look unused.
        if super::job::is_test_support_file(&scan.file) {
            continue;
        }

        for export in &scan.exports {
            let imported = imported_by
                .get(&(scan.file.clone(), export.symbol.clone()))
                .map(|files| !files.is_empty())
                .unwrap_or(false);
            let uncertain = uncertain_by
                .get(&scan.file)
                .map(|files| !files.is_empty())
                .unwrap_or(false);

            if imported {
                continue;
            }
            if uncertain {
                uncertain_count += 1;
                if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                    uncertain_items.push(json!({
                        "file": scan.file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "reason": "wildcard_import",
                    }));
                }
                continue;
            }

            count += 1;
            // Collect uncapped; rank by signal tier and truncate below.
            items.push(json!({
                "file": scan.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            }));
        }
    }

    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    let items = super::entry_points::rank_and_truncate_items(items, &roles, drill_down_limit);
    let top = super::entry_points::top_preview_symbols(&items);

    let mut aggregate = json!({
        "count": count,
        "items": items,
        "top": top,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count > limit),
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
    });
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn roll_up_duplicate_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    super::scanners::duplicates::aggregate_duplicate_contributions_with_limit(
        contributions,
        skipped_languages(&job.scope_files, LanguageSkipMode::Duplicates),
        drill_down_limit,
    )
}

fn cap_payload_drill_down(mut payload: Value, limit: usize) -> Value {
    let mut capped = false;
    if let Some(items) = payload.get_mut("items").and_then(Value::as_array_mut) {
        capped |= items.len() > limit;
        items.truncate(limit);
    }
    if let Some(groups) = payload.get_mut("groups").and_then(Value::as_array_mut) {
        capped |= groups.len() > limit;
        groups.truncate(limit);
    }
    if let Some(object) = payload.as_object_mut() {
        object.insert("drill_down_capped".to_string(), json!(capped));
    }
    payload
}

const MAX_DRILL_DOWN_ITEMS: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct UnusedExportsContribution {
    file: String,
    exports: Vec<ExportContribution>,
    imports: Vec<ImportContribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImportContribution {
    resolved_file: Option<String>,
    named: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum LanguageSkipMode {
    Duplicates,
    UnusedExports,
}

fn category_contributions_depend_on_entry_points(category: InspectCategory) -> bool {
    matches!(
        category,
        InspectCategory::DeadCode | InspectCategory::UnusedExports
    )
}

fn skipped_languages(files: &[PathBuf], mode: LanguageSkipMode) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| skipped_language(file, mode))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn skipped_language(file: &Path, mode: LanguageSkipMode) -> Option<String> {
    let Some(language) = crate::parser::detect_language(file) else {
        return match mode {
            LanguageSkipMode::Duplicates => Some("unknown".to_string()),
            LanguageSkipMode::UnusedExports => None,
        };
    };

    let skipped = match mode {
        LanguageSkipMode::Duplicates => !duplicates_supports_language(language),
        LanguageSkipMode::UnusedExports => !is_js_ts_language(language),
    };
    skipped.then(|| language_name(language).to_string())
}

fn duplicates_supports_language(language: crate::parser::LangId) -> bool {
    !matches!(
        language,
        crate::parser::LangId::Bash
            | crate::parser::LangId::Html
            | crate::parser::LangId::Json
            | crate::parser::LangId::Scala
            | crate::parser::LangId::Solidity
            | crate::parser::LangId::Scss
            | crate::parser::LangId::Vue
            | crate::parser::LangId::Markdown
            | crate::parser::LangId::Java
            | crate::parser::LangId::Ruby
            | crate::parser::LangId::Kotlin
            | crate::parser::LangId::Swift
            | crate::parser::LangId::Php
            | crate::parser::LangId::Lua
            | crate::parser::LangId::Perl
    )
}

fn is_js_ts_language(language: crate::parser::LangId) -> bool {
    matches!(
        language,
        crate::parser::LangId::TypeScript
            | crate::parser::LangId::Tsx
            | crate::parser::LangId::JavaScript
    )
}

fn language_name(language: crate::parser::LangId) -> &'static str {
    match language {
        crate::parser::LangId::TypeScript => "typescript",
        crate::parser::LangId::Tsx => "tsx",
        crate::parser::LangId::JavaScript => "javascript",
        crate::parser::LangId::Python => "python",
        crate::parser::LangId::Rust => "rust",
        crate::parser::LangId::Go => "go",
        crate::parser::LangId::C => "c",
        crate::parser::LangId::Cpp => "cpp",
        crate::parser::LangId::Zig => "zig",
        crate::parser::LangId::CSharp => "csharp",
        crate::parser::LangId::Bash => "bash",
        crate::parser::LangId::Html => "html",
        crate::parser::LangId::Markdown => "markdown",
        crate::parser::LangId::Yaml => "yaml",
        crate::parser::LangId::Solidity => "solidity",
        crate::parser::LangId::Scss => "scss",
        crate::parser::LangId::Vue => "vue",
        crate::parser::LangId::Json => "json",
        crate::parser::LangId::Scala => "scala",
        crate::parser::LangId::Java => "java",
        crate::parser::LangId::Ruby => "ruby",
        crate::parser::LangId::Kotlin => "kotlin",
        crate::parser::LangId::Swift => "swift",
        crate::parser::LangId::Php => "php",
        crate::parser::LangId::Lua => "lua",
        crate::parser::LangId::Perl => "perl",
    }
}

fn unused_public_api_entries(project_root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let entry_points = super::entry_points::resolve_entry_points(project_root);
    (
        entry_points.public_api_files_relative(project_root),
        entry_points.warnings().to_vec(),
    )
}

fn filter_outcome_for_scope_with_contributions(
    outcome: JobOutcome,
    snapshot: &InspectSnapshot,
    category: InspectCategory,
    cache: &InspectCache,
    scope: &JobScope,
) -> JobOutcome {
    if !category.is_tier2() || scope.is_project_wide() {
        return filter_outcome_for_scope(outcome, scope);
    }

    match outcome {
        JobOutcome::Fresh { payload } => {
            match scoped_tier2_payload_from_contributions(snapshot, category, cache, payload, scope)
            {
                Ok(payload) => JobOutcome::Fresh { payload },
                Err(message) => JobOutcome::Failed { message },
            }
        }
        JobOutcome::Stale { cached, in_flight } => match cached {
            Some(payload) => {
                match scoped_tier2_payload_from_contributions(
                    snapshot, category, cache, payload, scope,
                ) {
                    Ok(payload) => JobOutcome::Stale {
                        cached: Some(payload),
                        in_flight,
                    },
                    Err(message) => JobOutcome::Failed { message },
                }
            }
            None => JobOutcome::Stale {
                cached: None,
                in_flight,
            },
        },
        JobOutcome::Pending { in_flight } => JobOutcome::Pending { in_flight },
        JobOutcome::Failed { message } => JobOutcome::Failed { message },
    }
}

fn filter_outcome_for_scope(outcome: JobOutcome, scope: &JobScope) -> JobOutcome {
    match outcome {
        JobOutcome::Fresh { payload } => JobOutcome::Fresh {
            payload: filter_payload_for_scope(payload, scope),
        },
        JobOutcome::Stale { cached, in_flight } => JobOutcome::Stale {
            cached: cached.map(|payload| filter_payload_for_scope(payload, scope)),
            in_flight,
        },
        JobOutcome::Pending { in_flight } => JobOutcome::Pending { in_flight },
        JobOutcome::Failed { message } => JobOutcome::Failed { message },
    }
}

fn filter_payload_for_scope(mut payload: serde_json::Value, scope: &JobScope) -> serde_json::Value {
    if scope.is_project_wide() {
        return payload;
    }

    // Scoped Tier 2 callers pass an uncapped rollup into this filter and cap
    // drill-down only afterwards, so the recomputed count below remains the
    // true in-scope total rather than the size of a capped sample.
    if let Some(items) = payload
        .get_mut("items")
        .and_then(|value| value.as_array_mut())
    {
        let count = filter_values_for_scope(items, scope);
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            if object.contains_key("total_groups") {
                object.insert("total_groups".to_string(), serde_json::json!(count));
            }
            if object.contains_key("groups_count") {
                object.insert("groups_count".to_string(), serde_json::json!(count));
            }
        }
    }

    if let Some(groups) = payload
        .get_mut("groups")
        .and_then(|value| value.as_array_mut())
    {
        let count = filter_values_for_scope(groups, scope);
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            object.insert("total_groups".to_string(), serde_json::json!(count));
            if object.contains_key("groups_count") {
                object.insert("groups_count".to_string(), serde_json::json!(count));
            }
        }
    }

    payload
}

fn filter_values_for_scope(values: &mut Vec<serde_json::Value>, scope: &JobScope) -> usize {
    values.retain_mut(|value| prune_value_for_scope(value, scope));
    values.len()
}

fn prune_value_for_scope(value: &mut serde_json::Value, scope: &JobScope) -> bool {
    if let Some(file) = value.get("file").and_then(|file| file.as_str()) {
        return scope.contains_display_path(file);
    }

    let first_scoped_occurrence = if let Some(files) = value
        .get_mut("files")
        .and_then(|files| files.as_array_mut())
    {
        files.retain(|file| {
            file.as_str()
                .is_some_and(|file| scope.contains_display_path(display_file_from_occurrence(file)))
        });
        if files.len() < 2 {
            return false;
        }
        files.first().and_then(Value::as_str).map(str::to_string)
    } else {
        None
    };

    if let Some(occurrence) = first_scoped_occurrence {
        update_duplicate_group_sample(value, &occurrence);
    }

    true
}

fn update_duplicate_group_sample(value: &mut serde_json::Value, occurrence: &str) {
    let Some((file, start_line, end_line)) = parse_duplicate_occurrence(occurrence) else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };

    if object.contains_key("sample_file") {
        object.insert("sample_file".to_string(), json!(file));
    }
    if object.contains_key("sample_start_line") {
        object.insert("sample_start_line".to_string(), json!(start_line));
    }
    if object.contains_key("sample_end_line") {
        object.insert("sample_end_line".to_string(), json!(end_line));
    }
}

fn parse_duplicate_occurrence(value: &str) -> Option<(&str, u64, u64)> {
    let (file, range) = value.rsplit_once(':')?;
    let (start, end) = range.split_once('-')?;
    if !start.chars().all(|char| char.is_ascii_digit())
        || !end.chars().all(|char| char.is_ascii_digit())
    {
        return None;
    }

    Some((file, start.parse().ok()?, end.parse().ok()?))
}

fn display_file_from_occurrence(value: &str) -> &str {
    let Some((file, range)) = value.rsplit_once(':') else {
        return value;
    };
    let Some((start, end)) = range.split_once('-') else {
        return value;
    };
    if start.chars().all(|char| char.is_ascii_digit())
        && end.chars().all(|char| char.is_ascii_digit())
    {
        file
    } else {
        value
    }
}

#[allow(dead_code)]
fn normalize_scope_root(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn write_ts_project(file_count: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for i in 0..file_count {
            std::fs::write(
                root.join(format!("mod{i}.ts")),
                format!("export function f{i}() {{ return {i}; }}\n"),
            )
            .expect("write fixture");
        }
        dir
    }

    #[test]
    fn callgraph_snapshot_skips_above_max_callgraph_files() {
        // Interim #86 guard: above the cap, the expensive parse loop is skipped and
        // None is returned (dead_code then reports callgraph_available: false).
        let dir = write_ts_project(3);
        let snapshot = build_tier2_callgraph_snapshot(dir.path(), 1);
        assert!(
            snapshot.is_none(),
            "3 files with max=1 must skip the snapshot build"
        );
    }

    #[test]
    fn callgraph_snapshot_builds_at_or_below_max_callgraph_files() {
        // Below the cap the snapshot builds normally (real edges, not skipped).
        let dir = write_ts_project(3);
        let snapshot = build_tier2_callgraph_snapshot(dir.path(), 5000);
        assert!(
            snapshot.is_some(),
            "3 files with max=5000 must build the snapshot"
        );
    }
}

#[cfg(test)]
mod dead_code_projection_tests {
    use super::*;
    use crate::callgraph::walk_project_files;
    use crate::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
    use crate::config::Config;
    use crate::parser::SymbolCache;
    use filetime::FileTime;
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
    use std::sync::RwLock;

    static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_900_000_000);

    #[derive(Debug, PartialEq, Eq)]
    struct ComparableSnapshot {
        files: BTreeSet<PathBuf>,
        exported_symbols: BTreeSet<(PathBuf, String, String, u32)>,
        outbound_calls: BTreeSet<(PathBuf, String, String, u32)>,
        entry_points: BTreeSet<PathBuf>,
    }

    #[test]
    fn dead_code_projection_matches_builder_byte_parity() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let old = legacy_snapshot(&root);
        let projected = store_projected_snapshot(&root, ".store-dead-code-parity");

        assert_snapshot_parts_eq("fixture byte parity", &old, &projected);
        assert_projection_fixture_coverage(&root, &projected);
    }

    #[test]
    fn dead_code_projection_incremental_scenario_matrix_matches_cold_rebuild() {
        run_projection_scenario("rename", setup_projection_rename, edit_projection_rename);
        run_projection_scenario("delete", setup_projection_delete, edit_projection_delete);
        run_projection_scenario(
            "barrel delete",
            setup_projection_barrel,
            edit_projection_barrel_delete,
        );
        run_projection_scenario(
            "dispatch edit",
            setup_projection_dispatch,
            edit_projection_dispatch,
        );
        run_projection_scenario(
            "body-only edit",
            setup_projection_body_only,
            edit_projection_body_only,
        );
    }

    #[test]
    fn dead_code_projection_dead_code_scan_matches_builder_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let old = legacy_snapshot(&root);
        let projected = store_projected_snapshot(&root, ".store-dead-code-e2e");
        assert_snapshot_parts_eq("e2e fixture byte parity", &old, &projected);

        let old_aggregate = dead_code_aggregate(&root, files.clone(), old);
        let projected_aggregate = dead_code_aggregate(&root, files, projected);
        assert_eq!(
            serde_json::to_string(&projected_aggregate).expect("projected aggregate json"),
            serde_json::to_string(&old_aggregate).expect("legacy aggregate json"),
            "store-projected dead_code verdicts must match builder verdicts\nlegacy: {old_aggregate:#}\nprojected: {projected_aggregate:#}"
        );
        assert_dead_item(&projected_aggregate, "src/dead.ts", "knownDead");
        assert_live_item(&projected_aggregate, "src/live.ts", "knownLive");
        assert_live_item(&projected_aggregate, "src/render.ts", "render");
        assert_live_item(&projected_aggregate, "src/other_render.ts", "render");
    }

    fn assert_projection_fixture_coverage(root: &Path, snapshot: &CallgraphSnapshot) {
        let comparable = comparable_snapshot(snapshot);
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("ts")),
            "fixture must include TypeScript files: {:#?}",
            comparable.files
        );
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("js")),
            "fixture must include JavaScript files: {:#?}",
            comparable.files
        );
        assert!(
            comparable
                .files
                .iter()
                .any(|file| file.extension().and_then(|ext| ext.to_str()) == Some("rs")),
            "fixture must include Rust files: {:#?}",
            comparable.files
        );

        let main_file = canonicalize_for_snapshot(&root.join("src/main.ts"));
        let private_dispatch_target = format!("{}::dispatch", main_file.display());
        assert!(
            comparable
                .outbound_calls
                .iter()
                .any(
                    |(caller_file, caller_symbol, target, _)| caller_file == &main_file
                        && caller_symbol == "main"
                        && target == &private_dispatch_target
                ),
            "fixture must cover same-file private fallback target {private_dispatch_target}: {:#?}",
            comparable.outbound_calls
        );
        assert!(
            comparable
                .outbound_calls
                .iter()
                .any(|(_, _, target, _)| target.contains(DISPATCHED_CALLEE_SEPARATOR)),
            "fixture must cover method-dispatch suffixes: {:#?}",
            comparable.outbound_calls
        );
        assert!(
            comparable
                .exported_symbols
                .iter()
                .any(|(_, symbol, kind, _)| symbol == "runDefault"
                    && kind == DEFAULT_EXPORT_MARKER_KIND),
            "fixture must cover default-export marker rows: {:#?}",
            comparable.exported_symbols
        );
    }

    fn run_projection_scenario(name: &str, setup: fn(&Path), edit: fn(&Path) -> Vec<PathBuf>) {
        let dir = tempfile::tempdir().expect("tempdir");
        setup(dir.path());
        let root = canonical_root(dir.path());
        let files_before = project_files(&root);
        let incremental_store = CallGraphStore::open(
            root.join(format!(".store-dead-code-projection-{name}-incremental")),
            root.clone(),
        )
        .expect("open incremental store");
        incremental_store
            .cold_build(&files_before)
            .expect("initial cold build");

        let changed = edit(&root);
        incremental_store
            .refresh_files(&changed)
            .expect("refresh changed files");
        let incremental = project_dead_code_snapshot(incremental_store.sqlite_path())
            .expect("project incremental snapshot");

        let cold_store = CallGraphStore::open(
            root.join(format!(".store-dead-code-projection-{name}-cold")),
            root.clone(),
        )
        .expect("open cold store");
        cold_store
            .cold_build(&project_files(&root))
            .expect("cold rebuild");
        let cold =
            project_dead_code_snapshot(cold_store.sqlite_path()).expect("project cold snapshot");

        assert_snapshot_parts_eq(name, &cold, &incremental);
    }

    fn legacy_snapshot(root: &Path) -> CallgraphSnapshot {
        build_tier2_callgraph_snapshot(root, usize::MAX)
            .expect("legacy snapshot")
            .as_ref()
            .clone()
    }

    /// Phase 3a Decision-B benchmark (cap removal). Measures, on a real large
    /// repo, the three numbers that decide whether the `max_callgraph_files`
    /// cap is still needed once the callgraph reparse is replaced by the store:
    ///   (1) OLD snapshot build  = `build_tier2_callgraph_snapshot` full reparse
    ///   (2) NEW snapshot build  = store cold_build + `project_dead_code_snapshot`
    ///   (3) REMAINING cold cost = `run_dead_code_scan` given a ready snapshot
    ///       (the par_iter scan + per-file reexport/type-ref reparse + BFS roll-up
    ///        that the cap currently suppresses entirely above 5000 files)
    /// (1) vs (2) is the #86 headline; (3) is what determines the cap decision.
    /// Ignored by default; run with:
    ///   AFT_BENCH_REPO=/path/to/large/repo cargo test -p agent-file-tools --lib \
    ///     -- --ignored --nocapture --test-threads=1 dead_code_decision_b_benchmark
    #[test]
    #[ignore = "manual benchmark; needs AFT_BENCH_REPO pointing at a large checkout"]
    fn dead_code_decision_b_benchmark() {
        let Ok(repo) = std::env::var("AFT_BENCH_REPO") else {
            eprintln!("AFT_BENCH_REPO unset; skipping");
            return;
        };
        // Each phase flushes immediately so a file-redirected run shows live progress.
        macro_rules! mark {
            ($($a:tt)*) => {{ eprintln!($($a)*); let _ = std::io::Write::flush(&mut std::io::stderr()); }};
        }
        let root = canonical_root(Path::new(&repo));
        let files = project_files(&root);
        mark!(
            "\n=== Phase 3a Decision-B benchmark ===\nrepo: {}\nsource files (walk_project_files): {}\nstarted phase (2)...",
            root.display(),
            files.len()
        );

        // (2) NEW snapshot path: store cold_build + projection. Run FIRST — this is
        // the new per-cold-build cost that replaces the reparse.
        let store_dir = root.join(".aft-bench-store");
        let _ = std::fs::remove_dir_all(&store_dir);
        let store = CallGraphStore::open(store_dir.clone(), root.clone()).expect("open store");
        let t = Instant::now();
        let cold_stats = store.cold_build(&files).expect("store cold build");
        let store_build_ms = t.elapsed().as_millis();
        let t = Instant::now();
        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("projection");
        let proj_ms = t.elapsed().as_millis();
        mark!(
            "(2) NEW store cold_build: {} ms ({:?}) + projection: {} ms = {} ms  (exports={}, outbound={})\nstarted phase (3)...",
            store_build_ms, cold_stats, proj_ms, store_build_ms + proj_ms,
            projected.exported_symbols.len(), projected.outbound_calls.len()
        );

        // (3) REMAINING cold cost: run_dead_code_scan given a ready snapshot — the
        // par_iter scan + per-file reexport/type-ref reparse + BFS roll-up the cap
        // currently suppresses. THIS is the Decision-B number.
        let t = Instant::now();
        let _result = dead_code_aggregate(&root, files.clone(), projected.clone());
        let scan_ms = t.elapsed().as_millis();
        mark!(
            "(3) REMAINING run_dead_code_scan (cold, uncapped): {} ms",
            scan_ms
        );

        // (1) OLD full-reparse build — the #86 sink, headline only. OPT-IN
        // (AFT_BENCH_OLD=1) because it can take many minutes on large repos.
        let old_desc = if std::env::var("AFT_BENCH_OLD").is_ok() {
            mark!("started phase (1) OLD reparse (the slow #86 sink)...");
            let t = Instant::now();
            let old = build_tier2_callgraph_snapshot(&root, usize::MAX).expect("old snapshot");
            let ms = t.elapsed().as_millis();
            mark!(
                "(1) OLD build_tier2 reparse: {} ms  (exports={}, outbound={})",
                ms,
                old.exported_symbols.len(),
                old.outbound_calls.len()
            );
            format!("{}ms", ms)
        } else {
            mark!("(1) OLD build_tier2 reparse: SKIPPED (set AFT_BENCH_OLD=1)");
            "skipped".to_string()
        };

        mark!(
            "\nSUMMARY  files={}  new_build={}ms  scan_cold={}ms  NEW_total={}ms  old_reparse={}",
            files.len(),
            store_build_ms + proj_ms,
            scan_ms,
            store_build_ms + proj_ms + scan_ms,
            old_desc
        );
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    fn store_projected_snapshot(root: &Path, store_name: &str) -> CallgraphSnapshot {
        let store =
            CallGraphStore::open(root.join(store_name), root.to_path_buf()).expect("open store");
        store
            .cold_build(&project_files(root))
            .expect("store cold build");
        project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot")
    }

    fn dead_code_aggregate(
        root: &Path,
        scope_files: Vec<PathBuf>,
        snapshot: CallgraphSnapshot,
    ) -> Value {
        let job = InspectJob {
            job_id: 86,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join(".aft-cache").join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            callgraph_snapshot: Some(Arc::new(snapshot)),
        };
        crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
            .outcome
            .expect("dead_code scan succeeds")
            .aggregate
    }

    fn assert_snapshot_parts_eq(
        label: &str,
        expected: &CallgraphSnapshot,
        actual: &CallgraphSnapshot,
    ) {
        let expected = comparable_snapshot(expected);
        let actual = comparable_snapshot(actual);
        assert_eq!(
            actual, expected,
            "{label} store-projected snapshot must match legacy builder snapshot"
        );
    }

    fn comparable_snapshot(snapshot: &CallgraphSnapshot) -> ComparableSnapshot {
        ComparableSnapshot {
            files: snapshot.files.iter().cloned().collect(),
            exported_symbols: snapshot
                .exported_symbols
                .iter()
                .map(|export| {
                    (
                        export.file.clone(),
                        export.symbol.clone(),
                        export.kind.clone(),
                        export.line,
                    )
                })
                .collect(),
            outbound_calls: snapshot
                .outbound_calls
                .iter()
                .map(|call| {
                    (
                        call.caller_file.clone(),
                        call.caller_symbol.clone(),
                        call.target.clone(),
                        call.line,
                    )
                })
                .collect(),
            entry_points: snapshot.entry_points.clone(),
        }
    }

    fn assert_dead_item(aggregate: &Value, file: &str, symbol: &str) {
        assert!(
            aggregate_has_item(aggregate, file, symbol),
            "expected {file}::{symbol} to be reported dead: {aggregate:#}"
        );
    }

    fn assert_live_item(aggregate: &Value, file: &str, symbol: &str) {
        assert!(
            !aggregate_has_item(aggregate, file, symbol),
            "expected {file}::{symbol} to be live/not reported dead: {aggregate:#}"
        );
    }

    fn aggregate_has_item(aggregate: &Value, file: &str, symbol: &str) -> bool {
        let Some(items) = aggregate.get("items").and_then(Value::as_array) else {
            return false;
        };
        items.iter().any(|item| {
            item.get("file").and_then(Value::as_str) == Some(file)
                && item.get("symbol").and_then(Value::as_str) == Some(symbol)
        })
    }

    fn project_files(root: &Path) -> Vec<PathBuf> {
        walk_project_files(root).collect()
    }

    fn canonical_root(root: &Path) -> PathBuf {
        std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
        bump_mtime(path);
    }

    fn bump_mtime(path: &Path) {
        let secs = NEXT_MTIME.fetch_add(1, AtomicOrdering::SeqCst);
        filetime::set_file_mtime(path, FileTime::from_unix_time(secs, 0)).expect("bump mtime");
    }

    fn remove_file(path: &Path) {
        std::fs::remove_file(path).expect("remove fixture");
    }

    fn write_projection_fixture(root: &Path) {
        write_file(
            &root.join("package.json"),
            r#"{"name":"dead-code-projection-fixture","type":"module","main":"src/main.ts"}"#,
        );
        write_file(
            &root.join("Cargo.toml"),
            r#"[package]
name = "dead_code_projection_fixture"
version = "0.1.0"
edition = "2021"
"#,
        );
        write_file(
            &root.join("src/main.ts"),
            r#"import runDefault from "./default";
import { knownLive } from "./live";
import { jsEntry } from "./app.js";

export function main() {
  dispatch();
  runDefault();
  jsEntry();
}

function dispatch() {
  knownLive();
  const service = { render() {} };
  service.render();
}
"#,
        );
        write_file(
            &root.join("src/default.ts"),
            r#"export default function runDefault() {}
"#,
        );
        write_file(
            &root.join("src/live.ts"),
            r#"export function knownLive() {}
"#,
        );
        write_file(
            &root.join("src/dead.ts"),
            r#"export function knownDead() {}
"#,
        );
        write_file(
            &root.join("src/render.ts"),
            r#"export function render() {}
"#,
        );
        write_file(
            &root.join("src/other_render.ts"),
            r#"export function render() {}
"#,
        );
        write_file(
            &root.join("src/app.js"),
            r#"import { jsHelper } from "./js_helper.js";

export function jsEntry() {
  jsHelper();
}
"#,
        );
        write_file(
            &root.join("src/js_helper.js"),
            r#"export function jsHelper() {}
"#,
        );
        write_file(
            &root.join("src/lib.rs"),
            r#"mod util;
use crate::util::rust_helper;

pub fn rust_entry() {
    rust_helper();
}
"#,
        );
        write_file(
            &root.join("src/util.rs"),
            r#"pub fn rust_helper() {}
"#,
        );
    }

    fn setup_projection_rename(root: &Path) {
        write_file(
            &root.join("a.ts"),
            r#"export function outer() {
  inner();
}

export function inner() {}
"#,
        );
    }

    fn edit_projection_rename(root: &Path) -> Vec<PathBuf> {
        let path = root.join("a.ts");
        write_file(
            &path,
            r#"export function outer() {
  renamed();
}

export function renamed() {}
"#,
        );
        vec![path]
    }

    fn setup_projection_delete(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
        );
        write_file(&root.join("foo.ts"), "export function foo() {}\n");
    }

    fn edit_projection_delete(root: &Path) -> Vec<PathBuf> {
        let path = root.join("foo.ts");
        remove_file(&path);
        vec![path]
    }

    fn setup_projection_barrel(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./barrel";
export function main() { foo(); }
"#,
        );
        write_file(&root.join("barrel.ts"), "export { foo } from \"./foo\";\n");
        write_file(&root.join("foo.ts"), "export function foo() {}\n");
    }

    fn edit_projection_barrel_delete(root: &Path) -> Vec<PathBuf> {
        let path = root.join("barrel.ts");
        remove_file(&path);
        vec![path]
    }

    fn setup_projection_dispatch(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"export function main() {
  const service = { render() {}, paint() {} };
  service.render();
}
"#,
        );
        write_file(&root.join("render.ts"), "export function render() {}\n");
        write_file(&root.join("paint.ts"), "export function paint() {}\n");
    }

    fn edit_projection_dispatch(root: &Path) -> Vec<PathBuf> {
        let path = root.join("main.ts");
        write_file(
            &path,
            r#"export function main() {
  const service = { render() {}, paint() {} };
  service.paint();
}
"#,
        );
        vec![path]
    }

    fn setup_projection_body_only(root: &Path) {
        write_file(
            &root.join("main.ts"),
            r#"import { foo } from "./foo";
export function main() { foo(); }
"#,
        );
        write_file(
            &root.join("foo.ts"),
            r#"export function foo() {
  return 1;
}
"#,
        );
    }

    fn edit_projection_body_only(root: &Path) -> Vec<PathBuf> {
        let path = root.join("foo.ts");
        write_file(
            &path,
            r#"export function foo() {
  return 2;
}
"#,
        );
        vec![path]
    }
}
