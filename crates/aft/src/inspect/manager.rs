use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{after, bounded, select, Receiver, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cache::{InspectCache, Tier2ContributionUpdates};
use super::dispatch::{default_worker, start_dispatch_loop, InspectWorker};
use super::freshness::{verify_contribution_file, ContributionFreshness};
#[cfg(test)]
use super::job::normalize_path;
use super::job::{
    is_test_file, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob, InspectResult,
    InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope,
};
use super::oxc_engine::LivenessVerdict;
use super::oxc_engine::{
    analyze_file_facts, analyze_files_with_cache, normalize_input_path, AnalyzeOptions,
    DynamicImportFact, ExportFact, FileFacts, FileId, ImportFact, OxcEngineResult, OxcFactsCache,
    ReExportFact, FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::cache_freshness::{self, FileFreshness};
use crate::callgraph_store::{project_dead_code_snapshot, CallGraphStore, CallGraphStoreError};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InspectCacheIdentity {
    sqlite_path: PathBuf,
    project_root: PathBuf,
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

#[derive(Debug, Clone)]
pub struct DirectTier2RunOutcome {
    pub outcome: JobOutcome,
    pub force_paths_completed: bool,
}

#[derive(Debug, Clone)]
struct Tier2ReuseOptions {
    force_rescan_paths: BTreeSet<PathBuf>,
    allow_callgraph_cold_build: bool,
}

impl Tier2ReuseOptions {
    fn direct(paths: Vec<PathBuf>) -> Self {
        Self {
            force_rescan_paths: paths.into_iter().collect(),
            allow_callgraph_cold_build: false,
        }
    }

    fn has_force_paths(&self) -> bool {
        !self.force_rescan_paths.is_empty()
    }
}

impl Default for Tier2ReuseOptions {
    fn default() -> Self {
        Self {
            force_rescan_paths: BTreeSet::new(),
            allow_callgraph_cold_build: true,
        }
    }
}

pub struct InspectManager {
    request_tx: Sender<InspectJob>,
    result_rx: Receiver<InspectResult>,
    #[allow(dead_code)]
    pool: Arc<rayon::ThreadPool>,
    in_flight: Mutex<HashMap<JobKey, Vec<Waiter>>>,
    caches: Mutex<HashMap<InspectCacheIdentity, Arc<InspectCache>>>,
    oxc_facts_cache: Mutex<OxcFactsCache>,
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
            oxc_facts_cache: Mutex::new(OxcFactsCache::new()),
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
                    if category == InspectCategory::DeadCode
                        && payload
                            .get("callgraph_available")
                            .and_then(serde_json::Value::as_bool)
                            == Some(false)
                    {
                        return None;
                    }
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
        let project_key = crate::search_index::artifact_cache_key(&project_root);
        let sqlite_path = inspect_dir.join(format!("{project_key}.sqlite"));
        let identity = InspectCacheIdentity {
            sqlite_path,
            project_root: project_root.clone(),
        };
        let mut caches = self
            .caches
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?;
        if let Some(cache) = caches.get(&identity) {
            return Ok(Arc::clone(cache));
        }
        let cache = Arc::new(
            InspectCache::open(inspect_dir, project_root)
                .map_err(|error| format!("failed to open inspect cache: {error}"))?,
        );
        caches.insert(identity, Arc::clone(&cache));
        Ok(cache)
    }

    fn oxc_result_for_scan(
        &self,
        job: &InspectJob,
        files: &[PathBuf],
        force_reparse_files: &[PathBuf],
    ) -> Result<Option<OxcEngineResult>, String> {
        if !category_uses_oxc(job.category) {
            return Ok(None);
        }
        if job.category == InspectCategory::DeadCode && job.callgraph_snapshot.is_none() {
            return Ok(None);
        }

        let public_api_entries = super::entry_points::resolve_entry_points(&job.project_root);
        let entry_points = if job.category == InspectCategory::DeadCode {
            job.callgraph_snapshot
                .as_ref()
                .map(|snapshot| snapshot.entry_points.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let options = AnalyzeOptions {
            entry_points,
            public_api_files: public_api_entries.public_api_files(),
            executable_root_exports: public_api_entries.executable_root_exports(),
            force_reparse_files: force_reparse_files.to_vec(),
            entry_reachability: job.category == InspectCategory::DeadCode,
        };

        let mut cache = self
            .oxc_facts_cache
            .lock()
            .map_err(|_| "inspect oxc facts cache lock poisoned".to_string())?;
        analyze_files_with_cache(&job.project_root, files, options, &mut cache)
            .map(Some)
            .map_err(|message| format!("oxc analyze failed: {message}"))
    }

    pub fn tier2_run_with_reuse(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        if let Err(outcome) = validate_tier2_read_category(category) {
            return outcome;
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let job = self.tier2_reuse_job(snapshot.clone(), category, callgraph_snapshot);
        let key = job.key.clone();
        let (waiter_tx, waiter_rx) = bounded(1);
        let claimed = match self.register_tier2_reuse_waiter(&key, waiter_tx) {
            Ok(claimed) => claimed,
            Err(message) => return JobOutcome::Failed { message },
        };

        if claimed {
            let result = self
                .tier2_run_with_reuse_job_result_with_options(job, Tier2ReuseOptions::default());
            self.route_tier2_reuse_completion(result);
        }

        match waiter_rx.recv() {
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
                outcome,
                &snapshot,
                category,
                cache.as_ref(),
                &caller_scope,
            ),
            Err(_) => JobOutcome::Pending { in_flight: true },
        }
    }

    pub fn tier2_run_with_reuse_direct(
        self: &Arc<Self>,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        deadline: Instant,
        force_rescan_paths: Vec<PathBuf>,
    ) -> DirectTier2RunOutcome {
        if let Err(outcome) = validate_tier2_read_category(category) {
            return DirectTier2RunOutcome {
                outcome,
                force_paths_completed: false,
            };
        }
        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => {
                return DirectTier2RunOutcome {
                    outcome: JobOutcome::Failed { message },
                    force_paths_completed: false,
                }
            }
        };

        let must_run_forced_followup = !force_rescan_paths.is_empty();
        loop {
            let options = if must_run_forced_followup {
                Tier2ReuseOptions::direct(force_rescan_paths.clone())
            } else {
                Tier2ReuseOptions::direct(Vec::new())
            };
            let job = self.tier2_reuse_job(snapshot.clone(), category, None);
            let key = job.key.clone();
            let (waiter_tx, waiter_rx) = bounded(1);
            let claimed = match self.register_tier2_reuse_waiter(&key, waiter_tx) {
                Ok(claimed) => claimed,
                Err(message) => {
                    return DirectTier2RunOutcome {
                        outcome: JobOutcome::Failed { message },
                        force_paths_completed: false,
                    }
                }
            };
            if claimed {
                self.spawn_tier2_reuse_job(job, options);
            }

            let completed_force_run = claimed && must_run_forced_followup;
            let outcome = self.wait_for_tier2_reuse_until(
                &key,
                &caller_scope,
                cache.as_ref(),
                waiter_rx,
                &snapshot,
                deadline,
            );

            delay_direct_force_followup_deadline_check_for_debug(&snapshot.project_root);
            if must_run_forced_followup
                && !claimed
                && !matches!(outcome, JobOutcome::Pending { .. })
            {
                // The category was already in flight before this direct inspect
                // could supply its forced paths. Wait for that scan to finish,
                // then claim a follow-up reuse pass so the direct answer is based
                // on the paths invalidated by the edit/watcher stream rather than
                // on a possibly stat-fresh pre-existing scan. If the original scan
                // used the whole deadline, the forced paths were not incorporated,
                // so the honest direct result is still incomplete.
                if Instant::now() < deadline {
                    continue;
                }
                return DirectTier2RunOutcome {
                    outcome: JobOutcome::Pending { in_flight: true },
                    force_paths_completed: false,
                };
            }

            return DirectTier2RunOutcome {
                outcome,
                force_paths_completed: completed_force_run,
            };
        }
    }

    fn register_tier2_reuse_waiter(
        &self,
        key: &JobKey,
        waiter_tx: WaiterTx,
    ) -> Result<bool, String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if let Some(waiters) = in_flight.get_mut(key) {
            waiters.push(Waiter { tx: waiter_tx });
            return Ok(false);
        }

        in_flight.insert(key.clone(), vec![Waiter { tx: waiter_tx }]);
        Ok(true)
    }

    fn spawn_tier2_reuse_job(self: &Arc<Self>, job: InspectJob, options: Tier2ReuseOptions) {
        let manager = Arc::clone(self);
        let pool = Arc::clone(&self.pool);
        pool.spawn(move || {
            let result = manager.tier2_run_with_reuse_job_result_catching(job, options);
            manager.route_tier2_reuse_completion(result);
        });
    }

    fn wait_for_tier2_reuse_until(
        &self,
        key: &JobKey,
        caller_scope: &JobScope,
        cache: &InspectCache,
        waiter_rx: Receiver<JobOutcome>,
        snapshot: &InspectSnapshot,
        deadline: Instant,
    ) -> JobOutcome {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return JobOutcome::Pending { in_flight: true };
        };
        if remaining.is_zero() {
            return JobOutcome::Pending { in_flight: true };
        }

        match waiter_rx.recv_timeout(remaining) {
            Ok(outcome) => filter_outcome_for_scope_with_contributions(
                outcome,
                snapshot,
                key.category,
                cache,
                caller_scope,
            ),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                JobOutcome::Pending { in_flight: true }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                JobOutcome::Pending { in_flight: true }
            }
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
        match cache.get_aggregated_for_config(&key, snapshot.config.as_ref()) {
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
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();

        for record in &cached_records {
            let absolute = if record.file_path.is_absolute() {
                record.file_path.clone()
            } else {
                snapshot.project_root.join(&record.file_path)
            };
            match verify_contribution_file(&absolute, &record.freshness) {
                ContributionFreshness::Fresh { .. } => {}
                ContributionFreshness::Stale | ContributionFreshness::Deleted => return Ok(false),
            }
        }

        // Detect files added since the cached aggregate was generated (and files
        // that still exist but are no longer in the gitignore-aware project
        // scope). This walk remains on the read path because the current API does
        // not provide a watcher-maintained project file set, and additions cannot
        // be detected from cached contribution records alone. Existing cached
        // files are checked above first so ordinary edits/deletes can return stale
        // without walking the project.
        let project_scope = JobScope::for_project(snapshot.project_root.clone());
        let project_files = scope_files(&snapshot.project_root, &project_scope);
        let current_by_relative = current_project_files(&snapshot.project_root, &project_files);

        Ok(current_by_relative.len() == cached_relative.len()
            && current_by_relative
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

    fn tier2_run_with_reuse_job_result(&self, job: InspectJob) -> InspectResult {
        self.tier2_run_with_reuse_job_result_with_options(job, Tier2ReuseOptions::default())
    }

    fn tier2_run_with_reuse_job_result_catching(
        &self,
        job: InspectJob,
        options: Tier2ReuseOptions,
    ) -> InspectResult {
        let started = Instant::now();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tier2_run_with_reuse_job_result_with_options(job.clone(), options)
        })) {
            Ok(result) => result,
            Err(_) => InspectResult::failed(
                &job,
                "tier2 reuse worker panicked before completion",
                started.elapsed(),
            ),
        }
    }

    fn tier2_run_with_reuse_job_result_with_options(
        &self,
        mut job: InspectJob,
        options: Tier2ReuseOptions,
    ) -> InspectResult {
        let started = Instant::now();
        panic_tier2_reuse_for_debug(&job);
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
        delay_tier2_reuse_for_debug(&job.project_root);
        if !options.has_force_paths() {
            if let Ok(Some(success)) = self.tier2_quick_reuse_success(&job, cache.as_ref()) {
                let result = InspectResult::success(&job, success, started.elapsed());
                crate::slog_debug!(
                    "perf tier2 category={} reuse=hit ms={}",
                    job.category,
                    started.elapsed().as_millis()
                );
                log_tier2_benchmark_category_end(&result);
                return result;
            }
        }

        let result = match self.tier2_run_with_reuse_job(&job, &cache, &options) {
            Ok(success) => InspectResult::success(&job, success, started.elapsed()),
            Err(message) => InspectResult::failed(&job, message, started.elapsed()),
        };
        // Always-on perf line: a full (reuse=miss) scan is the expensive path —
        // for dead_code it includes store snapshot projection plus the scanner.
        // ms here lets us attribute background CPU bursts to a specific category from the log.
        crate::slog_info!(
            "perf tier2 category={} reuse=miss ms={}",
            job.category,
            started.elapsed().as_millis()
        );
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
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        if cached_records.len() != current_by_relative.len() {
            return Ok(None);
        }
        for record in &cached_records {
            let relative = freshness_record_relative_key(record);
            let Some(current_file) = current_by_relative.get(&relative) else {
                return Ok(None);
            };
            match cache_freshness::metadata_matches(current_file, &record.freshness) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "failed to stat {} for tier2 quick reuse: {error}",
                        current_file.display()
                    ));
                }
            }
        }

        let contribution_set_hash = cache
            .contribution_set_hash_for_config(job.category, job.config.as_ref())
            .map_err(|error| error.to_string())?;
        let Some(aggregate) = cache
            .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };

        cache
            .touch_tier2_last_full_run(job.category)
            .map_err(|error| error.to_string())?;
        Ok(Some(InspectScanSuccess {
            scanned_files: Vec::new(),
            contributions: Vec::new(),
            aggregate,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn tier2_run_with_reuse_job(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
        options: &Tier2ReuseOptions,
    ) -> Result<InspectScanSuccess, String> {
        let mut phases = Tier2PhaseTimings::default();
        let phase_started = Instant::now();
        let cached_records = load_contribution_freshness(cache, job.category)?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        let cached_relative = cached_records
            .iter()
            .map(freshness_record_relative_key)
            .collect::<BTreeSet<_>>();
        let force_relative = forced_relative_paths(job, &options.force_rescan_paths);
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

            if force_relative.contains(&relative) {
                updates.deletes.push(relative_path);
                scan_by_relative.insert(relative, current_file.clone());
                continue;
            }

            let absolute = job.project_root.join(&record.file_path);
            match verify_contribution_file(&absolute, &record.freshness) {
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
        phases.freshness = phase_started.elapsed();

        let mut scan_files = scan_by_relative.into_values().collect::<Vec<_>>();
        let force_reparse_files = scan_files.clone();
        if !scan_files.is_empty() {
            let mut scan_job = job.clone();
            scan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
            scan_job.scope_files = scan_files.clone();
            if scan_job.category == InspectCategory::DeadCode
                && scan_job.callgraph_snapshot.is_none()
            {
                let snapshot_started = Instant::now();
                scan_job.callgraph_snapshot =
                    build_tier2_callgraph_snapshot(&scan_job, options.allow_callgraph_cold_build);
                phases.snapshot += snapshot_started.elapsed();
            }
            aggregate_job.callgraph_snapshot = scan_job.callgraph_snapshot.clone();
            #[cfg(debug_assertions)]
            if cold_cache {
                std::thread::sleep(Duration::from_millis(10));
            }
            let scan_started = Instant::now();
            let oxc_result =
                self.oxc_result_for_scan(&scan_job, &scan_job.scope_files, &force_reparse_files)?;
            let scan_result = run_tier2_scan(&scan_job, oxc_result.as_ref());
            phases.scan += scan_started.elapsed();
            phases.scanned_files += scan_files.len();
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
                .get_aggregated_for_config(&job.key, job.config.as_ref())
                .map_err(|error| error.to_string())?
            {
                cache
                    .touch_tier2_last_full_run(job.category)
                    .map_err(|error| error.to_string())?;
                phases.log(job.category);
                return Ok(InspectScanSuccess {
                    scanned_files: scan_files,
                    contributions: Vec::new(),
                    aggregate,
                });
            }
        }

        let db_started = Instant::now();
        let mut contribution_set_hash = if has_updates {
            cache
                .apply_contribution_updates_for_config(job.category, updates, job.config.as_ref())
                .map_err(|error| error.to_string())?
        } else {
            cache
                .contribution_set_hash_for_config(job.category, job.config.as_ref())
                .map_err(|error| error.to_string())?
        };
        phases.db = db_started.elapsed();

        if let Some(aggregate) = cache
            .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
            .map_err(|error| error.to_string())?
        {
            cache
                .touch_tier2_last_full_run(job.category)
                .map_err(|error| error.to_string())?;
            let contributions = load_contributions(cache, job)?;
            phases.log(job.category);
            return Ok(InspectScanSuccess {
                scanned_files: scan_files,
                contributions,
                aggregate,
            });
        }

        let refresh_dead_code_facts = if job.category == InspectCategory::DeadCode {
            dead_code_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        let refresh_unused_exports_facts = if job.category == InspectCategory::UnusedExports {
            unused_exports_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        let refresh_duplicates_facts = if job.category == InspectCategory::Duplicates {
            duplicates_contributions_need_fact_refresh(cache, job)?
        } else {
            false
        };
        if refresh_dead_code_facts || refresh_unused_exports_facts || refresh_duplicates_facts {
            // Raw-facts contributions can be rolled up after manifest/resolver
            // edits without re-reading source. Only legacy verdict-bearing or
            // facts-version-mismatched caches need a one-time full refresh before
            // verdicts/roots can be recomputed globally.
            let full_scan_files = current_by_relative.into_values().collect::<Vec<_>>();
            if !full_scan_files.is_empty() {
                let mut rescan_job = job.clone();
                rescan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
                rescan_job.scope_files = full_scan_files.clone();
                if rescan_job.category == InspectCategory::DeadCode
                    && rescan_job.callgraph_snapshot.is_none()
                {
                    let snapshot_started = Instant::now();
                    rescan_job.callgraph_snapshot = build_tier2_callgraph_snapshot(
                        &rescan_job,
                        options.allow_callgraph_cold_build,
                    );
                    phases.snapshot += snapshot_started.elapsed();
                }
                let scan_started = Instant::now();
                let oxc_result = self.oxc_result_for_scan(
                    &rescan_job,
                    &rescan_job.scope_files,
                    &force_reparse_files,
                )?;
                let scan_result = run_tier2_scan(&rescan_job, oxc_result.as_ref());
                phases.scan += scan_started.elapsed();
                phases.scanned_files += full_scan_files.len();
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
                let db_started = Instant::now();
                contribution_set_hash = cache
                    .apply_contribution_updates_for_config(
                        job.category,
                        rescan_updates,
                        job.config.as_ref(),
                    )
                    .map_err(|error| error.to_string())?;
                phases.db += db_started.elapsed();
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
                    phases.log(job.category);
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
            let snapshot_started = Instant::now();
            aggregate_job.callgraph_snapshot =
                build_tier2_callgraph_snapshot(&aggregate_job, options.allow_callgraph_cold_build);
            phases.snapshot += snapshot_started.elapsed();
        }
        let rollup_started = Instant::now();
        let contributions = load_contributions(cache, &aggregate_job)?;
        let aggregate = roll_up_tier2_contributions(&aggregate_job, &contributions);
        cache
            .store_tier2_aggregate(job.key.clone(), &contribution_set_hash, aggregate.clone())
            .map_err(|error| error.to_string())?;
        phases.rollup = rollup_started.elapsed();
        phases.log(job.category);

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
        match cache.get_aggregated_for_config(key, snapshot.config.as_ref()) {
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
                    cache.store_tier2_result_for_config(
                        result.key.clone(),
                        &success.scanned_files,
                        &success.contributions,
                        success.aggregate.clone(),
                        result.config.as_ref(),
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

/// Phase-level wall-time attribution for one Tier-2 reuse=miss pass.
///
/// Exists to self-attribute pathological scans (e.g. a normally-100ms
/// unused_exports pass once took 677s under heavy machine load) without
/// needing a lucky live `sample`. Logged as ONE info line per pass, only when
/// real work happened (freshness/scan/snapshot/rollup/db), so quiet reuse passes stay silent.
#[derive(Default)]
struct Tier2PhaseTimings {
    /// Freshness verification of cached contributions (file stat + hash reads).
    freshness: Duration,
    /// Callgraph store snapshot projection (dead_code only).
    snapshot: Duration,
    /// Scanner compute over files needing (re)scan.
    scan: Duration,
    /// SQLite contribution upserts/deletes (busy-wait contention shows here).
    db: Duration,
    /// Aggregate roll-up + store.
    rollup: Duration,
    scanned_files: usize,
}

impl Tier2PhaseTimings {
    fn log(&self, category: InspectCategory) {
        let worked = self.freshness + self.scan + self.snapshot + self.rollup + self.db;
        if worked < Duration::from_millis(50) {
            return;
        }
        crate::slog_info!(
            "perf tier2 phases category={} freshness={}ms snapshot={}ms scan={}ms({} files) db={}ms rollup={}ms",
            category,
            self.freshness.as_millis(),
            self.snapshot.as_millis(),
            self.scan.as_millis(),
            self.scanned_files,
            self.db.as_millis(),
            self.rollup.as_millis()
        );
    }
}

fn scope_files(project_root: &Path, scope: &JobScope) -> Vec<PathBuf> {
    let mut files = crate::callgraph::walk_project_files(project_root)
        .filter(|path| scope.contains(path))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn forced_relative_paths(job: &InspectJob, paths: &BTreeSet<PathBuf>) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            job.project_root.join(path)
        };
        keys.insert(relative_cache_key(&job.project_root, &absolute));
        if let Ok(canonical) = std::fs::canonicalize(&absolute) {
            keys.insert(relative_cache_key(&job.project_root, &canonical));
        }
    }
    keys
}

fn panic_tier2_reuse_for_debug(job: &InspectJob) {
    #[cfg(not(debug_assertions))]
    let _ = job;
    #[cfg(debug_assertions)]
    {
        if !env_project_root_matches("AFT_TEST_TIER2_REUSE_PANIC_ROOT", &job.project_root) {
            return;
        }
        let should_panic = std::env::var("AFT_TEST_TIER2_REUSE_PANIC_CATEGORY")
            .ok()
            .is_some_and(|category| category == job.category.as_str());
        if should_panic {
            panic!("forced tier2 reuse panic for {}", job.category);
        }
    }
}

fn delay_direct_force_followup_deadline_check_for_debug(project_root: &Path) {
    #[cfg(not(debug_assertions))]
    let _ = project_root;
    #[cfg(debug_assertions)]
    {
        if !env_project_root_matches("AFT_TEST_DIRECT_FORCE_FOLLOWUP_DELAY_ROOT", project_root) {
            return;
        }
        if let Some(delay_ms) = std::env::var("AFT_TEST_DIRECT_FORCE_FOLLOWUP_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
}

fn delay_tier2_reuse_for_debug(project_root: &Path) {
    #[cfg(not(debug_assertions))]
    let _ = project_root;
    #[cfg(debug_assertions)]
    {
        if !env_project_root_matches("AFT_TEST_TIER2_REUSE_DELAY_ROOT", project_root) {
            return;
        }
        if let Some(delay_ms) = std::env::var("AFT_TEST_TIER2_REUSE_DELAY_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
    }
}

#[cfg(debug_assertions)]
fn env_project_root_matches(var: &str, project_root: &Path) -> bool {
    let Some(raw) = std::env::var_os(var) else {
        return true;
    };
    let expected = PathBuf::from(raw);
    let expected = std::fs::canonicalize(&expected).unwrap_or(expected);
    let actual = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    expected == actual
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
    job: &InspectJob,
    allow_cold_build: bool,
) -> Option<Arc<CallgraphSnapshot>> {
    let started = Instant::now();
    if !job.config.callgraph_store {
        crate::slog_info!(
            "tier2 dead_code: callgraph store disabled; reporting callgraph_unavailable"
        );
        return None;
    }

    let Some(callgraph_dir) = callgraph_store_dir_from_inspect_dir(&job.inspect_dir) else {
        crate::slog_info!(
            "tier2 dead_code: inspect_dir has no harness parent ({}); reporting callgraph_unavailable",
            job.inspect_dir.display()
        );
        return None;
    };

    // Background Tier-2 refresh may repair moved-root metadata by publishing a
    // one-time cold rebuild. Direct inspect cannot spend its request budget on
    // that repair path, so it uses the open-only variant and reports
    // callgraph_unavailable when a rebuild would be required.
    let store = match if allow_cold_build {
        CallGraphStore::open_ready_repairing(callgraph_dir.clone(), job.project_root.clone())
    } else {
        CallGraphStore::open_ready_no_rebuild(callgraph_dir.clone(), job.project_root.clone())
    } {
        Ok(Some(store)) => store,
        Ok(None) => {
            crate::slog_info!(
                "tier2 dead_code: callgraph store unavailable at {} (cold/building/not ready); reporting callgraph_unavailable",
                callgraph_dir.display()
            );
            return None;
        }
        Err(error) => {
            crate::slog_warn!(
                "tier2 dead_code: failed to open callgraph store at {}: {}; reporting callgraph_unavailable",
                callgraph_dir.display(),
                error
            );
            return None;
        }
    };

    let snapshot = match project_dead_code_snapshot(store.sqlite_path()) {
        Ok(snapshot) => snapshot,
        Err(CallGraphStoreError::Unavailable(message)) => {
            crate::slog_info!(
                "tier2 dead_code: callgraph store projection unavailable ({}); reporting callgraph_unavailable",
                message
            );
            return None;
        }
        Err(error) => {
            crate::slog_warn!(
                "tier2 dead_code: callgraph store projection failed: {}; reporting callgraph_unavailable",
                error
            );
            return None;
        }
    };

    crate::slog_info!(
        "perf tier2_callgraph_snapshot: source=callgraph_store files={} exports={} edges={} entry_points={} ms={}",
        snapshot.files.len(),
        snapshot.exported_symbols.len(),
        snapshot.outbound_calls.len(),
        snapshot.entry_points.len(),
        started.elapsed().as_millis()
    );

    Some(Arc::new(snapshot))
}

fn callgraph_store_dir_from_inspect_dir(inspect_dir: &Path) -> Option<PathBuf> {
    inspect_dir
        .parent()
        .map(|harness_dir| harness_dir.join("callgraph"))
}

#[cfg(test)]
fn canonicalize_for_snapshot(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
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

fn dead_code_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(dead_code_contribution_needs_fact_refresh))
}

fn dead_code_contribution_needs_fact_refresh(contribution: &FileContribution) -> bool {
    let Ok(parsed) =
        serde_json::from_value::<DeadCodeRefreshContribution>(contribution.contribution.clone())
    else {
        return true;
    };

    if parsed.facts_format_version
        != Some(super::scanners::dead_code::DEAD_CODE_FACTS_FORMAT_VERSION)
    {
        return true;
    }

    matches!(
        parsed.oxc_facts,
        Some(facts) if facts.format_version != FACTS_FORMAT_VERSION
    )
}

fn unused_exports_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(unused_exports_contribution_needs_fact_refresh))
}

/// Duplicates contributions written before v0.44 lack the `line_count` field
/// (serde defaults it to 0), so a cached roll-up computes total_analyzed_lines
/// as 0 and the summary renders "0.0% of 0 analyzed lines". One full rescan
/// repopulates the counts; fresh contributions always carry line_count.
fn duplicates_contributions_need_fact_refresh(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<bool, String> {
    let contributions = load_contributions(cache, job)?;
    Ok(contributions
        .iter()
        .any(|contribution| contribution.contribution.get("line_count").is_none()))
}

fn unused_exports_contribution_needs_fact_refresh(contribution: &FileContribution) -> bool {
    let top_level_oxc = contribution
        .contribution
        .get("provenance")
        .and_then(Value::as_str)
        == Some(OXC_PROVENANCE);
    let Ok(parsed) =
        serde_json::from_value::<UnusedExportsContribution>(contribution.contribution.clone())
    else {
        return false;
    };
    let uses_oxc =
        top_level_oxc || parsed.oxc_facts.is_some() || parsed.exports.iter().any(export_uses_oxc);
    if !uses_oxc {
        return false;
    }

    !matches!(
        parsed.oxc_facts,
        Some(facts) if facts.format_version == FACTS_FORMAT_VERSION
    )
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

fn run_tier2_scan(job: &InspectJob, oxc_result: Option<&OxcEngineResult>) -> InspectResult {
    use super::scanners;

    match job.category {
        InspectCategory::DeadCode => {
            scanners::dead_code::run_dead_code_scan_with_oxc(job, oxc_result)
        }
        InspectCategory::UnusedExports => {
            scanners::unused_exports::run_unused_exports_scan_with_oxc(job, oxc_result)
        }
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(job),
        InspectCategory::Cycles => scanners::cycles::run_cycles_scan_with_oxc(job, oxc_result),
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
        InspectCategory::Cycles => {
            roll_up_cycle_contributions(job, contributions, drill_down_limit)
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
    let mut job = InspectJob {
        job_id: 0,
        key: JobKey::for_project_category(category),
        category,
        scope_files: scope_files(&snapshot.project_root, scope),
        project_root: snapshot.project_root.clone(),
        inspect_dir: snapshot.inspect_dir.clone(),
        config: Arc::clone(&snapshot.config),
        symbol_cache: Arc::clone(&snapshot.symbol_cache),
        callgraph_snapshot: None,
    };

    if category == InspectCategory::DeadCode {
        // Scoped read-path rollups recompute dead-code liveness from cached
        // contributions. Use a real ready store snapshot when one exists; if no
        // snapshot is available, leave it absent so the rollup reports degraded
        // callgraph_unavailable instead of treating an empty graph as truth.
        job.callgraph_snapshot = build_tier2_callgraph_snapshot(&job, false);
    }

    job
}

fn roll_up_dead_code_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        return super::scanners::dead_code::callgraph_unavailable_aggregate(job.scope_files.len());
    };

    let public_api_files = super::scanners::dead_code::collect_public_api_files(&job.project_root);
    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    super::scanners::dead_code::aggregate_dead_code_contributions_with_snapshot(
        &job.project_root,
        snapshot,
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

    if parsed.iter().any(|scan| scan.oxc_facts.is_some()) {
        return roll_up_unused_exports_oxc_contributions(job, &parsed, drill_down_limit);
    }

    let (public_api_files, package_warnings) = unused_public_api_entries(&job.project_root);
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
    let test_only_count = 0usize;
    let test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    for scan in &parsed {
        if public_api_files.contains(&scan.file) {
            continue;
        }
        // Mirror the fresh-scan path: fixtures/corpora/mock data are consumed
        // by path, never imported, so their exports always look unused.
        if super::job::is_test_support_file(&scan.file) {
            continue;
        }

        for export in &scan.exports {
            if export_uses_oxc(export) {
                match export.verdict.unwrap_or(LivenessVerdict::Unused) {
                    LivenessVerdict::Used => continue,
                    LivenessVerdict::Uncertain => {
                        uncertain_count += 1;
                        if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                            uncertain_items.push(json!({
                                "file": scan.file,
                                "symbol": export.symbol,
                                "kind": export.kind,
                                "line": export.line,
                                "reason": export.reason.as_deref().unwrap_or("oxc_uncertain"),
                                "provenance": export.provenance.as_deref().unwrap_or(OXC_PROVENANCE),
                            }));
                        }
                        continue;
                    }
                    LivenessVerdict::Unused => {}
                }
            } else {
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
            }

            count += 1;
            // Collect uncapped; rank by signal tier and truncate below.
            let mut item = json!({
                "file": scan.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            });
            if let Some(provenance) = &export.provenance {
                item["provenance"] = json!(provenance);
            }
            items.push(item);
        }
    }

    let roles = super::entry_points::resolve_project_roles(&job.project_root);
    let items = super::entry_points::rank_and_truncate_items(items, &roles, drill_down_limit);
    let top = super::entry_points::top_preview_symbols(&items);
    let test_only_items =
        super::entry_points::rank_and_truncate_items(test_only_items, &roles, drill_down_limit);
    let test_only_top = test_only_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();

    let (parse_errors, skipped_files) = unused_exports_honesty_fields(&parsed);
    let mut aggregate = json!({
        "count": count,
        "items": items,
        "top": top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn roll_up_unused_exports_oxc_contributions(
    job: &InspectJob,
    parsed: &[UnusedExportsContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    let (public_api_files, package_warnings) = unused_public_api_entries(&job.project_root);
    let facts = parsed
        .iter()
        .filter_map(|scan| {
            let oxc_facts = scan.oxc_facts.as_ref()?;
            let path = job.project_root.join(&scan.file);
            Some(FileFacts {
                file_id: FileId(0),
                path: normalize_input_path(&job.project_root, &path),
                content_hash: oxc_facts.content_hash.clone(),
                exports: oxc_facts.exports.clone(),
                imports: oxc_facts.imports.clone(),
                re_exports: oxc_facts.re_exports.clone(),
                dynamic_imports: oxc_facts.dynamic_imports.clone(),
                same_file_value_references: oxc_facts.same_file_value_references.clone(),
                used_import_bindings: oxc_facts.used_import_bindings.clone(),
                type_referenced_import_bindings: oxc_facts.type_referenced_import_bindings.clone(),
                value_referenced_import_bindings: oxc_facts
                    .value_referenced_import_bindings
                    .clone(),
                parse_error: oxc_facts.parse_error.clone(),
            })
        })
        .collect::<Vec<_>>();
    let entry_point_set = super::entry_points::resolve_entry_points(&job.project_root);
    let oxc_result = analyze_file_facts(
        &job.project_root,
        facts,
        AnalyzeOptions {
            entry_points: Vec::new(),
            public_api_files: entry_point_set.public_api_files(),
            executable_root_exports: entry_point_set.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: false,
        },
        Vec::new(),
    );
    let roles = super::entry_points::resolve_project_roles(&job.project_root);

    let mut count = 0usize;
    let mut items = Vec::new();
    let mut test_only_count = 0usize;
    let mut test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    for file in &oxc_result.files {
        if public_api_files.contains(&file.relative_file)
            || super::job::is_test_support_file(&file.relative_file)
        {
            continue;
        }

        for export in &file.exports {
            match export.verdict {
                LivenessVerdict::Used => {
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
                        test_only_count += 1;
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
                            "used_by": export.test_only_reference_files,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        test_only_items.push(item);
                    }
                }
                LivenessVerdict::Uncertain => {
                    uncertain_count += 1;
                    if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "reason": export.reason,
                            "provenance": export.provenance,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        uncertain_items.push(item);
                    }
                }
                LivenessVerdict::Unused => {
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
                        test_only_count += 1;
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
                            "used_by": export.test_only_reference_files,
                        });
                        add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                        test_only_items.push(item);
                        continue;
                    }
                    if export.has_references {
                        continue;
                    }
                    count += 1;
                    let mut item = json!({
                        "file": file.relative_file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "provenance": export.provenance,
                    });
                    add_oxc_reexport_contexts(&mut item, &export.also_reexported);
                    items.push(item);
                }
            }
        }
    }

    let items = super::entry_points::rank_and_truncate_items(items, &roles, drill_down_limit);
    let top = super::entry_points::top_preview_symbols(&items);
    let test_only_items =
        super::entry_points::rank_and_truncate_items(test_only_items, &roles, drill_down_limit);
    let test_only_top = test_only_items
        .iter()
        .take(super::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let (mut parse_errors, skipped_files) = unused_exports_honesty_fields(parsed);
    for scan in parsed {
        if let Some(oxc_facts) = &scan.oxc_facts {
            if oxc_facts.format_version != FACTS_FORMAT_VERSION {
                parse_errors.push(json!({
                    "file": scan.file,
                    "message": format!(
                        "unsupported oxc facts format {}; expected {}",
                        oxc_facts.format_version, FACTS_FORMAT_VERSION
                    ),
                }));
            }
        }
    }

    let mut aggregate = json!({
        "count": count,
        "items": items,
        "top": top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn add_oxc_reexport_contexts(
    item: &mut Value,
    contexts: &[crate::inspect::oxc_engine::OxcReExportContext],
) {
    if !contexts.is_empty() {
        item["also_reexported"] = json!(contexts);
    }
}

fn unused_exports_honesty_fields(parsed: &[UnusedExportsContribution]) -> (Vec<Value>, Vec<Value>) {
    let mut parse_error_keys = BTreeSet::new();
    let mut parse_errors = Vec::new();
    let mut skipped_file_keys = BTreeSet::new();
    let mut skipped_files = Vec::new();
    for contribution in parsed {
        for value in &contribution.parse_errors {
            let key = value.to_string();
            if parse_error_keys.insert(key) {
                parse_errors.push(value.clone());
            }
        }
        for value in &contribution.skipped_files {
            let key = value.to_string();
            if skipped_file_keys.insert(key) {
                skipped_files.push(value.clone());
            }
        }
    }
    (parse_errors, skipped_files)
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
        &job.config.inspect.duplicates.expected_mirrors,
    )
}

fn roll_up_cycle_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
    drill_down_limit: Option<usize>,
) -> Value {
    super::scanners::cycles::aggregate_cycle_contributions_with_limit(
        &job.project_root,
        contributions,
        skipped_languages(&job.scope_files, LanguageSkipMode::Cycles),
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
    #[serde(default)]
    verdict: Option<LivenessVerdict>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

fn export_uses_oxc(export: &ExportContribution) -> bool {
    export.verdict.is_some() || export.provenance.as_deref() == Some(OXC_PROVENANCE)
}

#[derive(Debug, Clone, Deserialize)]
struct DeadCodeRefreshContribution {
    #[serde(default)]
    facts_format_version: Option<u32>,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct UnusedExportsContribution {
    file: String,
    exports: Vec<ExportContribution>,
    #[serde(default)]
    imports: Vec<ImportContribution>,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
    #[serde(default)]
    parse_errors: Vec<Value>,
    #[serde(default)]
    skipped_files: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImportContribution {
    resolved_file: Option<String>,
    named: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OxcFactsContribution {
    format_version: u32,
    content_hash: String,
    exports: Vec<ExportFact>,
    imports: Vec<ImportFact>,
    re_exports: Vec<ReExportFact>,
    dynamic_imports: Vec<DynamicImportFact>,
    same_file_value_references: BTreeSet<String>,
    used_import_bindings: BTreeSet<String>,
    type_referenced_import_bindings: BTreeSet<String>,
    value_referenced_import_bindings: BTreeSet<String>,
    #[serde(default)]
    parse_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum LanguageSkipMode {
    Duplicates,
    Cycles,
    UnusedExports,
}

fn category_uses_oxc(category: InspectCategory) -> bool {
    matches!(
        category,
        InspectCategory::DeadCode | InspectCategory::UnusedExports | InspectCategory::Cycles
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
            LanguageSkipMode::Cycles => Some("unknown".to_string()),
            LanguageSkipMode::UnusedExports => None,
        };
    };

    let skipped = match mode {
        LanguageSkipMode::Duplicates => !duplicates_supports_language(language),
        LanguageSkipMode::Cycles => !is_js_ts_language(language),
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
            | crate::parser::LangId::Pascal
            | crate::parser::LangId::R
            | crate::parser::LangId::ObjC
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
        crate::parser::LangId::Pascal => "pascal",
        crate::parser::LangId::R => "r",
        crate::parser::LangId::ObjC => "objc",
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
        let largest_cycle = items
            .iter()
            .filter_map(|item| item.get("files").and_then(Value::as_array).map(Vec::len))
            .max();
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            if object.contains_key("largest") {
                object.insert(
                    "largest".to_string(),
                    serde_json::json!(largest_cycle.unwrap_or(0)),
                );
            }
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

    // `by_language` is a project-wide breakdown computed before scope filtering.
    // Leaving it in a scoped payload contradicts the recomputed in-scope `count`
    // (e.g. count: 3 alongside `(rust 214, ts 143)`). The filtered items don't
    // carry per-item language, so we can't faithfully recompute it — drop it so
    // the scoped summary doesn't render a misleading project-wide breakdown.
    if let Some(object) = payload.as_object_mut() {
        if object.contains_key("top") {
            if let Some(top) = recompute_scoped_top_preview(object) {
                object.insert("top".to_string(), top);
            } else if let Some(top) = object.get_mut("top").and_then(Value::as_array_mut) {
                filter_values_for_scope(top, scope);
            }
        }
        if object.contains_key("duplicated_lines") {
            recompute_duplicate_payload_stats(object);
        }
        object.remove("by_language");
    }

    payload
}

fn recompute_duplicate_payload_stats(object: &mut serde_json::Map<String, Value>) {
    let values = object
        .get("items")
        .or_else(|| object.get("groups"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (duplicated_lines, duplicated_file_count) = duplicate_line_stats_from_values(&values);
    let total_analyzed_lines = object
        .get("total_analyzed_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duplicated_percent = if total_analyzed_lines == 0 {
        0.0
    } else {
        (duplicated_lines as f64 * 100.0) / total_analyzed_lines as f64
    };
    object.insert("duplicated_lines".to_string(), json!(duplicated_lines));
    object.insert(
        "duplicated_file_count".to_string(),
        json!(duplicated_file_count),
    );
    object.insert("duplicated_percent".to_string(), json!(duplicated_percent));
}

fn duplicate_line_stats_from_values(values: &[Value]) -> (u64, usize) {
    let mut by_file = BTreeMap::<String, Vec<(u64, u64)>>::new();
    for value in values {
        let Some(files) = value.get("files").and_then(Value::as_array) else {
            continue;
        };
        for occurrence in files.iter().filter_map(Value::as_str) {
            let Some((file, start, end)) = parse_duplicate_occurrence(occurrence) else {
                continue;
            };
            by_file
                .entry(file.to_string())
                .or_default()
                .push((start, end));
        }
    }
    let file_count = by_file.len();
    let duplicated_lines = by_file
        .values_mut()
        .map(|intervals| merged_duplicate_interval_lines(intervals))
        .sum();
    (duplicated_lines, file_count)
}

fn merged_duplicate_interval_lines(intervals: &mut [(u64, u64)]) -> u64 {
    if intervals.is_empty() {
        return 0;
    }
    intervals.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    let (mut current_start, mut current_end) = intervals[0];
    let mut total = 0;
    for &(start, end) in &intervals[1..] {
        if start <= current_end.saturating_add(1) {
            current_end = current_end.max(end);
        } else {
            total += current_end.saturating_sub(current_start).saturating_add(1);
            current_start = start;
            current_end = end;
        }
    }
    total + current_end.saturating_sub(current_start).saturating_add(1)
}

fn recompute_scoped_top_preview(
    object: &serde_json::Map<String, Value>,
) -> Option<serde_json::Value> {
    let values = object
        .get("items")
        .or_else(|| object.get("groups"))
        .and_then(Value::as_array)?;
    Some(Value::Array(
        values
            .iter()
            .take(super::entry_points::TOP_PREVIEW_ITEMS)
            .map(top_preview_value)
            .collect(),
    ))
}

fn top_preview_value(value: &Value) -> Value {
    if let Some(files) = value.get("files").and_then(Value::as_array) {
        let mut object = serde_json::Map::new();
        object.insert("files".to_string(), Value::Array(files.clone()));
        if let Some(cost) = value.get("cost").cloned() {
            object.insert("cost".to_string(), cost);
        }
        return Value::Object(object);
    }

    json!({
        "file": value.get("file").and_then(Value::as_str).unwrap_or(""),
        "symbol": value.get("symbol").and_then(Value::as_str).unwrap_or(""),
    })
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
    fn scoped_filter_recomputes_top_preview_from_scoped_items() {
        let project_root = PathBuf::from("/project");
        let scope = JobScope::from_roots(project_root.clone(), vec![project_root.join("src/in")]);
        let payload = json!({
            "count": 4,
            "items": [
                { "file": "src/out/a.ts", "symbol": "outside" },
                { "file": "src/in/b.ts", "symbol": "inside_b" },
                { "file": "src/in/c.ts", "symbol": "inside_c" }
            ],
            "top": [
                { "file": "src/out/a.ts", "symbol": "outside" },
                { "file": "src/out/z.ts", "symbol": "outside_z" }
            ],
            "by_language": { "typescript": 4 }
        });

        let filtered = filter_payload_for_scope(payload, &scope);

        assert_eq!(filtered["count"], json!(2));
        assert_eq!(
            filtered["top"],
            json!([
                { "file": "src/in/b.ts", "symbol": "inside_b" },
                { "file": "src/in/c.ts", "symbol": "inside_c" }
            ])
        );
        assert!(filtered["top"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["file"]
                .as_str()
                .is_some_and(|file| file.starts_with("src/in/"))));
    }

    #[test]
    fn cache_for_paths_rebinds_same_project_key_to_current_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).expect("create source repo");
        std::fs::write(
            source.join("package.json"),
            r#"{"name":"inspect-cache-fixture","version":"1.0.0"}"#,
        )
        .expect("write source manifest");
        std::fs::write(source.join("index.ts"), "export const source = 1;\n")
            .expect("write source file");
        assert!(std::process::Command::new("git")
            .current_dir(&source)
            .arg("init")
            .status()
            .expect("git init source repo")
            .success());
        assert!(std::process::Command::new("git")
            .current_dir(&source)
            .args(["add", "."])
            .status()
            .expect("git add source repo")
            .success());
        assert!(std::process::Command::new("git")
            .current_dir(&source)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial",
            ])
            .status()
            .expect("git commit source repo")
            .success());

        let clone = dir.path().join("clone");
        assert!(std::process::Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone source repo")
            .success());
        std::fs::write(
            clone.join("package.json"),
            r#"{"name":"inspect-cache-fixture","version":"2.0.0"}"#,
        )
        .expect("write clone manifest edit");
        assert_eq!(
            crate::search_index::artifact_cache_key(&source),
            crate::search_index::artifact_cache_key(&clone),
            "clones with the same root commit should share the sqlite project key"
        );

        let source = std::fs::canonicalize(source).expect("canonical source root");
        let clone = std::fs::canonicalize(clone).expect("canonical clone root");
        let manager = InspectManager::new();
        let inspect_dir = dir.path().join("inspect");
        let key = JobKey::for_project_category(InspectCategory::DeadCode);
        let source_cache = manager
            .cache_for_paths(inspect_dir.clone(), source.clone())
            .expect("open source cache");
        let source_hash = source_cache
            .contribution_set_hash(InspectCategory::DeadCode)
            .expect("source contribution hash");
        source_cache
            .store_tier2_aggregate(
                key.clone(),
                &source_hash,
                serde_json::json!({ "count": 7, "items": [] }),
            )
            .expect("store source aggregate");
        assert_eq!(
            source_cache
                .get_aggregated(&key)
                .expect("read source aggregate")
                .and_then(|payload| payload.get("count").and_then(Value::as_u64)),
            Some(7)
        );

        let clone_cache = manager
            .cache_for_paths(inspect_dir, clone.clone())
            .expect("open clone cache");
        assert_eq!(clone_cache.project_root(), clone.as_path());
        assert!(
            clone_cache
                .get_aggregated(&key)
                .expect("read clone aggregate")
                .is_none(),
            "same-key clone with a different manifest must not reuse the source root's cached count"
        );
    }

    fn snapshot_job(root: &Path, inspect_dir: &Path, callgraph_store: bool) -> InspectJob {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files: Vec::new(),
            project_root: root.to_path_buf(),
            inspect_dir: inspect_dir.to_path_buf(),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                callgraph_store,
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            callgraph_snapshot: None,
        }
    }

    #[test]
    fn callgraph_snapshot_reports_unavailable_when_store_disabled() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, false), false);

        assert!(
            snapshot.is_none(),
            "dead_code must not rebuild the legacy graph when the store is disabled"
        );
    }

    #[test]
    fn callgraph_snapshot_reports_unavailable_when_store_not_ready() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir).expect("store dir");
        let _store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open empty store");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false);

        assert!(
            snapshot.is_none(),
            "a cold/mid-build store must surface callgraph_unavailable instead of rebuilding inline"
        );
    }

    #[test]
    fn direct_callgraph_snapshot_does_not_cold_rebuild_when_store_needs_rebuild() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir).expect("store dir");
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");
        let sqlite_path = store.sqlite_path().to_path_buf();
        drop(store);

        let still_existing_previous_root = root.with_file_name("previous-root-still-exists");
        std::fs::create_dir_all(&still_existing_previous_root).expect("create previous root");
        let conn = rusqlite::Connection::open(sqlite_path).expect("open store sqlite");
        conn.execute(
            "UPDATE backend_file_state SET workspace_root = ?1",
            rusqlite::params![still_existing_previous_root.display().to_string()],
        )
        .expect("force root repair rebuild state");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false);

        assert!(
            snapshot.is_none(),
            "direct inspect must report callgraph_unavailable instead of cold-rebuilding a root-repair store"
        );
    }

    #[test]
    fn callgraph_snapshot_reads_ready_callgraph_store() {
        let dir = write_ts_project(3);
        let root = std::fs::canonicalize(dir.path()).expect("canonical root");
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir).expect("store dir");
        let store = CallGraphStore::open(callgraph_dir, root.clone()).expect("open store");
        let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
        store.cold_build(&files).expect("cold build store");

        let snapshot =
            build_tier2_callgraph_snapshot(&snapshot_job(&root, &inspect_dir, true), false)
                .expect("ready store snapshot");

        assert_eq!(snapshot.files.len(), 3);
        assert_eq!(snapshot.exported_symbols.len(), 3);
    }

    // A scoped payload must not carry the project-wide `by_language` breakdown
    // alongside the recomputed in-scope count — that contradiction renders as
    // e.g. "Dead code: 1 (rust 214, ts 143)".
    #[test]
    fn scoped_filter_drops_project_wide_by_language() {
        let scope = JobScope::from_roots("/proj", vec![PathBuf::from("/proj/src/a")]);
        assert!(
            !scope.is_project_wide(),
            "scope must be non-project for test"
        );
        let payload = serde_json::json!({
            "count": 99,
            "by_language": { "rust": 214, "typescript": 143 },
            "items": [
                { "file": "/proj/src/a/x.rs", "symbol": "live" },
                { "file": "/proj/src/other/y.rs", "symbol": "out" },
            ],
        });
        let filtered = filter_payload_for_scope(payload, &scope);
        assert!(
            filtered.get("by_language").is_none(),
            "scoped payload must drop project-wide by_language: {filtered}"
        );
        // Count is recomputed to the in-scope items (only x.rs under src/a).
        assert_eq!(filtered.get("count").and_then(|v| v.as_u64()), Some(1));
    }
    #[cfg(debug_assertions)]
    #[test]
    fn tier2_read_cached_freshness_does_not_hash_unchanged_contributions() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();

        crate::cache_freshness::reset_hash_file_if_small_count_for_debug();
        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        assert_fresh(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_for_debug(),
            0,
            "dispatch-thread inspect freshness must not use strict verification"
        );
        assert_eq!(
            crate::cache_freshness::hash_file_if_small_count_for_debug(),
            0,
            "unchanged contribution files must stay on the stat-only fast path"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn tier2_read_cached_freshness_returns_byte_identical_cold_scan_aggregate() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_uncached_fixture();
        let cold_payload = fresh_payload(manager.tier2_run_with_reuse(
            snapshot.clone(),
            InspectCategory::Duplicates,
            scope.clone(),
            None,
        ));

        crate::cache_freshness::reset_hash_file_if_small_count_for_debug();
        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        let warm_payload =
            fresh_payload(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let cold_bytes = serde_json::to_vec(&cold_payload).expect("serialize cold aggregate");
        let warm_bytes = serde_json::to_vec(&warm_payload).expect("serialize warm aggregate");
        assert_eq!(
            warm_bytes, cold_bytes,
            "warm unchanged read must return the byte-identical aggregate as the cold scan"
        );
        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_for_debug(),
            0,
            "dispatch-thread warm read must not use strict verification"
        );
        assert_eq!(
            crate::cache_freshness::hash_file_if_small_count_for_debug(),
            0,
            "warm unchanged read must not content-hash cached contribution files"
        );
    }

    #[test]
    fn tier2_read_cached_freshness_detects_changed_added_and_deleted_files() {
        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();
        write_fixture_file(
            &snapshot.project_root,
            "src/foo.ts",
            "export const foo = 101;\nexport const changed = true;\n",
            3_000_000_001,
        );
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let (_dir, manager, snapshot, scope, _files) = duplicate_cache_fixture();
        write_fixture_file(
            &snapshot.project_root,
            "src/added.ts",
            "export const added = 3;\n",
            3_000_000_002,
        );
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));

        let (_dir, manager, snapshot, scope, files) = duplicate_cache_fixture();
        std::fs::remove_file(&files[0]).expect("delete cached contribution file");
        assert_stale(manager.tier2_read_cached(snapshot, InspectCategory::Duplicates, scope));
    }

    fn duplicate_cache_fixture() -> (
        tempfile::TempDir,
        InspectManager,
        InspectSnapshot,
        JobScope,
        Vec<PathBuf>,
    ) {
        let (dir, manager, snapshot, scope, files) = duplicate_uncached_fixture();
        store_duplicate_cache(&manager, &snapshot, &files);
        (dir, manager, snapshot, scope, files)
    }

    fn duplicate_uncached_fixture() -> (
        tempfile::TempDir,
        InspectManager,
        InspectSnapshot,
        JobScope,
        Vec<PathBuf>,
    ) {
        use crate::config::Config;
        use crate::parser::SymbolCache;
        use std::sync::RwLock;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical fixture root");
        let files = vec![
            write_fixture_file(
                &root,
                "src/foo.ts",
                "export const fixture = () => 1;
export const shared = 1;
",
                3_000_000_000,
            ),
            write_fixture_file(
                &root,
                "src/bar.ts",
                "export const fixture = () => 1;
export const shared = 1;
",
                3_000_000_000,
            ),
        ];
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let snapshot = InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            Arc::new(Config {
                project_root: Some(root.clone()),
                ..Config::default()
            }),
            Arc::new(RwLock::new(SymbolCache::new())),
        );
        let scope = JobScope::for_project(root);
        let manager = InspectManager::new();
        (dir, manager, snapshot, scope, files)
    }

    fn write_fixture_file(root: &Path, relative: &str, content: &str, mtime_secs: i64) -> PathBuf {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture parent");
        }
        std::fs::write(&path, content).expect("write fixture file");
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(mtime_secs, 0))
            .expect("set fixture mtime");
        path
    }

    fn store_duplicate_cache(
        manager: &InspectManager,
        snapshot: &InspectSnapshot,
        files: &[PathBuf],
    ) {
        let cache = manager
            .cache_for_snapshot(snapshot)
            .expect("open inspect cache");
        let contributions = files
            .iter()
            .map(|file| {
                let freshness = crate::cache_freshness::collect(file).expect("collect freshness");
                FileContribution::new(
                    InspectCategory::Duplicates,
                    file.clone(),
                    freshness,
                    serde_json::json!({
                        "file": relative_cache_key(&snapshot.project_root, file),
                        "fragments": [],
                    }),
                )
            })
            .collect::<Vec<_>>();
        cache
            .store_tier2_result(
                JobKey::for_project_category(InspectCategory::Duplicates),
                files,
                &contributions,
                serde_json::json!({
                    "count": 0,
                    "groups": [],
                    "scanned_files": files.len(),
                    "total_groups": 0,
                }),
            )
            .expect("store tier2 cache fixture");
    }

    fn assert_fresh(outcome: JobOutcome) {
        let _ = fresh_payload(outcome);
    }

    fn fresh_payload(outcome: JobOutcome) -> Value {
        match outcome {
            JobOutcome::Fresh { payload } => payload,
            other => panic!("expected fresh cached Tier-2 outcome, got {other:?}"),
        }
    }

    fn assert_stale(outcome: JobOutcome) {
        match outcome {
            JobOutcome::Stale { .. } => {}
            other => panic!("expected stale cached Tier-2 outcome, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod dead_code_projection_tests {
    use super::*;
    use crate::callgraph::walk_project_files;
    use crate::callgraph_store::{project_dead_code_snapshot, CallGraphStore};
    use crate::config::Config;
    use crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR;
    use crate::inspect::scanners::DEFAULT_EXPORT_MARKER_KIND;
    use crate::parser::SymbolCache;
    use filetime::FileTime;
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
    use std::sync::RwLock;

    static NEXT_MTIME: AtomicI64 = AtomicI64::new(1_900_000_000);

    #[test]
    fn scoped_dead_code_rollup_uses_ready_callgraph_and_degrades_without_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let inspect_dir = root.join(".aft-cache").join("inspect");
        let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir).expect("store dir");
        let store = CallGraphStore::open(callgraph_dir.clone(), root.clone()).expect("open store");
        let files = project_files(&root);
        store.cold_build(&files).expect("cold build store");
        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        drop(store);

        let config = Arc::new(Config {
            project_root: Some(root.clone()),
            callgraph_store: true,
            ..Config::default()
        });
        let symbol_cache = Arc::new(RwLock::new(SymbolCache::new()));
        let scan_job = InspectJob {
            job_id: 87,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files: files.clone(),
            project_root: root.clone(),
            inspect_dir: inspect_dir.clone(),
            config: Arc::clone(&config),
            symbol_cache: Arc::clone(&symbol_cache),
            callgraph_snapshot: Some(Arc::new(projected)),
        };
        let success = crate::inspect::scanners::dead_code::run_dead_code_scan(&scan_job)
            .outcome
            .expect("dead_code scan succeeds");
        let cache = InspectCache::open(inspect_dir.clone(), root.clone()).expect("open cache");
        cache
            .store_tier2_result(
                scan_job.key.clone(),
                &success.scanned_files,
                &success.contributions,
                success.aggregate.clone(),
            )
            .expect("store tier2 result");

        let snapshot = InspectSnapshot::new(root.clone(), inspect_dir, config, symbol_cache);
        let scope = JobScope::from_roots(root.clone(), vec![root.join("src/live.ts")]);
        assert!(
            !scope.is_project_wide(),
            "live.ts file scope must be scoped"
        );

        let ready_payload = scoped_tier2_payload_from_contributions(
            &snapshot,
            InspectCategory::DeadCode,
            &cache,
            success.aggregate.clone(),
            &scope,
        )
        .expect("ready scoped payload");
        assert_eq!(
            ready_payload
                .get("callgraph_available")
                .and_then(Value::as_bool),
            Some(true),
            "ready store should produce a callgraph-backed scoped rollup: {ready_payload:#}"
        );
        assert_live_item(&ready_payload, "src/live.ts", "knownLive");

        std::fs::remove_dir_all(&callgraph_dir).expect("remove ready callgraph store");
        let unavailable_payload = scoped_tier2_payload_from_contributions(
            &snapshot,
            InspectCategory::DeadCode,
            &cache,
            success.aggregate,
            &scope,
        )
        .expect("unavailable scoped payload");
        assert_eq!(
            unavailable_payload
                .get("callgraph_available")
                .and_then(Value::as_bool),
            Some(false),
            "missing store must report callgraph_unavailable instead of fabricating an empty graph: {unavailable_payload:#}"
        );
        assert_live_item(&unavailable_payload, "src/live.ts", "knownLive");
    }
    #[derive(Debug, PartialEq, Eq)]
    struct ComparableSnapshot {
        files: BTreeSet<PathBuf>,
        exported_symbols: BTreeSet<(PathBuf, String, String, u32)>,
        outbound_calls: BTreeSet<(PathBuf, String, String, u32)>,
        entry_points: BTreeSet<PathBuf>,
        entry_point_symbols: BTreeMap<PathBuf, BTreeSet<String>>,
    }

    #[test]
    fn dead_code_projection_contains_expected_fixture_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let projected = store_projected_snapshot(&root, ".store-dead-code-surface");

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
    fn dead_code_projection_dead_code_scan_reports_expected_verdicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_projection_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let projected = store_projected_snapshot(&root, ".store-dead-code-e2e");

        let projected_aggregate = dead_code_aggregate(&root, files, projected);
        assert_dead_item(&projected_aggregate, "src/dead.ts", "knownDead");
        assert_live_item(&projected_aggregate, "src/live.ts", "knownLive");
        assert_live_item(&projected_aggregate, "src/render.ts", "render");
        assert_live_item(&projected_aggregate, "src/other_render.ts", "render");
    }

    #[test]
    fn dead_code_projection_rust_attribute_entry_points_are_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files = project_files(&root);
        let store = CallGraphStore::open(root.join(".store-tauri-commands"), root.clone())
            .expect("open store");
        store.cold_build(&files).expect("cold build store");
        let command = store
            .node_for(Path::new("src/commands.rs"), "get_primers")
            .expect("command node");
        assert!(
            command.is_entry_point,
            "attribute-rooted commands must be labeled as callgraph entry points"
        );
        let private_command = store
            .node_for(Path::new("src/commands.rs"), "private_command")
            .expect("private command node");
        assert!(
            private_command.is_entry_point,
            "private attribute-rooted commands must also be callgraph entry points"
        );

        let projected = project_dead_code_snapshot(store.sqlite_path()).expect("project snapshot");
        let aggregate = dead_code_aggregate(&root, files, projected);
        assert_live_item(&aggregate, "src/commands.rs", "get_primers");
        assert_live_item(&aggregate, "src/db.rs", "helper");
        assert_live_item(&aggregate, "src/db.rs", "private_helper");
        assert_live_item(&aggregate, "src/imported.rs", "imported_command");
        assert_live_item(&aggregate, "src/db.rs", "imported_helper");
        assert_dead_item(&aggregate, "src/commands.rs", "planted_dead");
        assert_dead_item(&aggregate, "src/unimported.rs", "false_command");
        assert_dead_item(&aggregate, "src/db.rs", "false_helper");
    }

    #[test]
    fn dead_code_projection_rust_attribute_roots_are_cold_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let first = store_projected_snapshot(&root, ".store-tauri-cold-a");
        let second = store_projected_snapshot(&root, ".store-tauri-cold-b");

        assert_snapshot_parts_eq("rust attribute roots cold", &first, &second);
    }

    #[test]
    fn dead_code_projection_rust_attribute_roots_survive_unrelated_incremental_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_rust_attribute_entry_fixture(dir.path());
        let root = canonical_root(dir.path());
        let files_before = project_files(&root);
        let incremental_store =
            CallGraphStore::open(root.join(".store-tauri-incremental"), root.clone())
                .expect("open incremental store");
        incremental_store
            .cold_build(&files_before)
            .expect("initial cold build");

        write_file(
            &root.join("src/unrelated.rs"),
            r#"// unrelated edit should not refresh command attribute facts
pub fn unrelated() -> u32 { 2 }
"#,
        );
        let stats = incremental_store
            .refresh_files(&[root.join("src/unrelated.rs")])
            .expect("refresh unrelated file");
        assert_eq!(stats.refreshed_own_files, 1);
        assert_eq!(stats.changed_files, vec!["src/unrelated.rs".to_string()]);
        assert!(
            !stats
                .surface_changed
                .iter()
                .any(|file| file == "src/commands.rs"),
            "unrelated edit must not refresh the command module: {stats:#?}"
        );
        let incremental = project_dead_code_snapshot(incremental_store.sqlite_path())
            .expect("project incremental snapshot");

        let cold_store = CallGraphStore::open(root.join(".store-tauri-cold"), root.clone())
            .expect("open cold store");
        cold_store
            .cold_build(&project_files(&root))
            .expect("cold rebuild");
        let cold = project_dead_code_snapshot(cold_store.sqlite_path()).expect("project cold");
        assert_snapshot_parts_eq("rust attribute roots unrelated edit", &cold, &incremental);

        let aggregate = dead_code_aggregate(&root, project_files(&root), incremental);
        assert_live_item(&aggregate, "src/commands.rs", "get_primers");
        assert_live_item(&aggregate, "src/db.rs", "helper");
        assert_live_item(&aggregate, "src/db.rs", "private_helper");
        assert_dead_item(&aggregate, "src/commands.rs", "planted_dead");
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

    /// Store-backed dead_code benchmark. Measures, on a real checkout, the
    /// persisted-store cold build, the warm SQLite projection cost, and the
    /// remaining `run_dead_code_scan` cost (per-file reexport/type-ref reparse +
    /// BFS roll-up). Production Tier-2 reads a warm store; cold_build is included
    /// here only to make end-to-end store cost visible.
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
            "\n=== Store-backed dead_code benchmark ===\nrepo: {}\nsource files (walk_project_files): {}\nstarted store cold_build...",
            root.display(),
            files.len()
        );

        // Store cold_build + projection. Production warm runs skip cold_build and
        // pay only the projection below.
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
            "store cold_build: {} ms ({:?}) + projection: {} ms = {} ms  (exports={}, outbound={})\nstarted scan...",
            store_build_ms, cold_stats, proj_ms, store_build_ms + proj_ms,
            projected.exported_symbols.len(), projected.outbound_calls.len()
        );

        // Remaining scanner cost: run_dead_code_scan given a ready snapshot.
        let t = Instant::now();
        let _result = dead_code_aggregate(&root, files.clone(), projected.clone());
        let scan_ms = t.elapsed().as_millis();
        mark!("run_dead_code_scan (cold contributions): {} ms", scan_ms);

        mark!(
            "\nSUMMARY  files={}  store_cold_plus_projection={}ms  projection={}ms  scan_cold={}ms  total={}ms",
            files.len(),
            store_build_ms + proj_ms,
            proj_ms,
            scan_ms,
            store_build_ms + proj_ms + scan_ms
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
            "{label} store-projected snapshot must match cold store snapshot"
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
            entry_point_symbols: snapshot.entry_point_symbols.clone(),
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

    fn write_rust_attribute_entry_fixture(root: &Path) {
        write_file(
            &root.join("src/main.rs"),
            r#"mod commands;
mod db;
mod imported;
mod unimported;
mod unrelated;

fn main() {
    tauri::generate_handler![commands::get_primers, imported::imported_command];
}
"#,
        );
        write_file(
            &root.join("src/commands.rs"),
            r#"use crate::db;

#[tauri::command]
pub fn get_primers() -> String {
    db::helper()
}

pub fn planted_dead() -> String {
    "dead".to_string()
}

#[tauri::command]
fn private_command() -> String {
    db::private_helper()
}
"#,
        );
        write_file(
            &root.join("src/imported.rs"),
            r#"use crate::db;
use tauri::command;

#[command]
pub fn imported_command() -> String {
    db::imported_helper()
}
"#,
        );
        write_file(
            &root.join("src/unimported.rs"),
            r#"use crate::db;

#[command]
pub fn false_command() -> String {
    db::false_helper()
}
"#,
        );
        write_file(
            &root.join("src/db.rs"),
            r#"pub fn helper() -> String { "live".to_string() }
pub fn imported_helper() -> String { "live".to_string() }
pub fn private_helper() -> String { "live".to_string() }
pub fn false_helper() -> String { "dead".to_string() }
"#,
        );
        write_file(
            &root.join("src/unrelated.rs"),
            r#"pub fn unrelated() -> u32 { 1 }
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
