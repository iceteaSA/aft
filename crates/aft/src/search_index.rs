use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use regex::bytes::Regex;
use regex_syntax::hir::{Hir, HirKind};
use serde::{Deserialize, Serialize};

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::fs_lock;
use crate::pattern_compile::{self, CompileOpts, CompileResult, CompiledPattern, LiteralSearch};

const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;
const CACHE_MAGIC: u32 = 0x3144_4958; // "XID1" little-endian
const INDEX_MAGIC: &[u8; 8] = b"AFTIDX01";
const LOOKUP_MAGIC: &[u8; 8] = b"AFTLKP01";
const SPILL_MAGIC: &[u8; 8] = b"AFTSPI01";
const FILE_TRIGRAM_COUNT_MAGIC: &[u8; 8] = b"AFTFTC01";
const INDEX_VERSION: u32 = 4;
const PREVIEW_BYTES: usize = 8 * 1024;
const SPIMI_SOFT_LIMIT_BYTES: usize = 128 * 1024 * 1024;
const SPIMI_HARD_LIMIT_BYTES: usize = 256 * 1024 * 1024;
const SPILL_RECORD_ESTIMATED_BYTES: usize = 16;
const DELTA_COMPACT_SOFT_FILES: usize = 1_000;
const DELTA_COMPACT_HARD_FILES: usize = 5_000;
const DELTA_COMPACT_SOFT_BYTES: usize = 32 * 1024 * 1024;
const DELTA_COMPACT_HARD_BYTES: usize = 128 * 1024 * 1024;
const EOF_SENTINEL: u8 = 0;
const MAX_ENTRIES: usize = 10_000_000;
const MIN_FILE_ENTRY_BYTES: usize = 57;
const LOOKUP_ENTRY_BYTES: usize = 16;
const POSTING_BYTES: usize = 6;
const ARTIFACT_CACHE_KEY_MEMO_FILE: &str = "cache-keys.json";
static CACHE_LOCK_ACQUIRE_MUTEX: Mutex<()> = Mutex::new(());
static ARTIFACT_CACHE_KEY_MEMO_STATE: OnceLock<Mutex<ArtifactCacheKeyMemoState>> = OnceLock::new();

#[cfg(test)]
type RootCommitProbeOverride =
    Arc<dyn Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static>;

#[cfg(test)]
static GIT_ROOT_COMMIT_PROBE_OVERRIDE: OnceLock<Mutex<Option<RootCommitProbeOverride>>> =
    OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArtifactCacheKeyMemoEntry {
    key: String,
    git_root_commit: String,
    recorded_at_ms: u64,
}

#[derive(Default)]
struct ArtifactCacheKeyMemoState {
    by_storage_root: BTreeMap<PathBuf, BTreeMap<String, ArtifactCacheKeyMemoEntry>>,
}

pub struct CacheLock {
    _guard: fs_lock::LockGuard,
}

impl CacheLock {
    pub fn acquire(cache_dir: &Path) -> std::io::Result<Self> {
        Self::acquire_with_timeout(cache_dir, Duration::from_secs(2))
    }

    pub fn try_acquire_for_shutdown(cache_dir: &Path) -> std::io::Result<Self> {
        // Graceful shutdown gets one short best-effort lock attempt so a
        // sibling writer cannot hold process exit open.
        Self::acquire_with_timeout(cache_dir, Duration::from_millis(25))
    }

    fn acquire_with_timeout(cache_dir: &Path, timeout: Duration) -> std::io::Result<Self> {
        fs::create_dir_all(cache_dir)?;
        let path = cache_dir.join("cache.lock");
        let _acquire_guard = CACHE_LOCK_ACQUIRE_MUTEX
            .lock()
            .map_err(|_| std::io::Error::other("search cache lock acquisition mutex poisoned"))?;
        fs_lock::try_acquire(&path, timeout)
            .map(|guard| Self { _guard: guard })
            .map_err(|error| match error {
                fs_lock::AcquireError::Timeout => {
                    std::io::Error::other("timed out acquiring search cache lock")
                }
                fs_lock::AcquireError::Io(error) => error,
            })
    }
}

#[derive(Clone, Debug)]
pub struct SearchIndex {
    base: Option<Arc<BasePostings>>,
    delta_postings: HashMap<u32, Vec<Posting>>,
    delta_file_trigrams: HashMap<u32, Vec<u32>>,
    pub files: Arc<Vec<FileEntry>>,
    pub path_to_id: Arc<HashMap<PathBuf, u32>>,
    pub ready: bool,
    project_root: PathBuf,
    git_head: Option<String>,
    max_file_size: u64,
    ignore_rules_fingerprint: String,
    pub file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
    superseded: HashSet<u32>,
    base_file_count: u32,
    delta_packed_bytes: usize,
    compaction_state: Arc<Mutex<CompactionState>>,
}

#[derive(Clone, Debug)]
struct BasePostings {
    file: Arc<File>,
    postings_blob_start: u64,
    postings_blob_len: u64,
    lookup: Arc<Vec<LookupEntry>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LookupEntry {
    trigram: u32,
    offset: u64,
    count: u32,
}

#[derive(Clone, Debug, Default)]
struct CompactionState {
    running: bool,
    requested_again: bool,
    buffered_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct SearchIndexSnapshot {
    base: Option<Arc<BasePostings>>,
    delta_postings: Arc<HashMap<u32, Vec<Posting>>>,
    files: Arc<Vec<FileEntry>>,
    path_to_id: Arc<HashMap<PathBuf, u32>>,
    ready: bool,
    project_root: PathBuf,
    file_trigram_count: Arc<Vec<u32>>,
    unindexed_files: Arc<HashSet<u32>>,
    superseded: Arc<HashSet<u32>>,
}

#[derive(Clone, Debug, Default)]
pub struct LexicalRankResult {
    pub files: Vec<(PathBuf, f32)>,
    pub engine_capped: bool,
}

impl SearchIndex {
    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Number of unique trigrams in the combined base index and delta postings.
    pub fn trigram_count(&self) -> usize {
        self.snapshot().trigram_count()
    }

    /// True when `write_to_disk` would persist changes beyond the current base.
    /// This covers pure deletions and unindexed file additions, which do not
    /// always populate `delta_file_trigrams`.
    pub(crate) fn has_pending_disk_changes(&self) -> bool {
        !self.delta_postings.is_empty()
            || !self.superseded.is_empty()
            || self.path_to_id.len() != self.base_file_count as usize
    }

    /// Returns an immutable snapshot for queries. Callers must obtain the
    /// snapshot while holding the RwLock that protects the SearchIndex, then
    /// drop the guard before running expensive operations such as grep, glob, or
    /// lexical ranking.
    pub fn snapshot(&self) -> SearchIndexSnapshot {
        SearchIndexSnapshot {
            base: self.base.clone(),
            delta_postings: Arc::new(self.delta_postings.clone()),
            files: Arc::clone(&self.files),
            path_to_id: Arc::clone(&self.path_to_id),
            ready: self.ready,
            project_root: self.project_root.clone(),
            file_trigram_count: Arc::clone(&self.file_trigram_count),
            unindexed_files: Arc::clone(&self.unindexed_files),
            superseded: Arc::new(self.superseded.clone()),
        }
    }

    /// Compute distinct query trigrams from literal tokens.
    pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
        query_trigrams_from_tokens(tokens)
    }

    /// Score-rank file candidates by lexical relevance to query trigrams.
    pub fn lexical_rank(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> Vec<(PathBuf, f32)> {
        self.snapshot()
            .lexical_rank_with_stats(query_trigrams, candidate_filter, max_files)
            .files
    }

    /// Score-rank file candidates and report whether the pre-filter step that
    /// collects candidates reached its internal size limit before ranking.
    pub fn lexical_rank_with_stats(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> LexicalRankResult {
        self.snapshot()
            .lexical_rank_with_stats(query_trigrams, candidate_filter, max_files)
    }
}

impl SearchIndexSnapshot {
    /// Number of unique trigrams in the combined base index and delta postings.
    pub fn trigram_count(&self) -> usize {
        let base_count = self.base.as_ref().map_or(0, |base| base.lookup.len());
        let Some(base) = &self.base else {
            return self.delta_postings.len();
        };
        base_count
            + self
                .delta_postings
                .keys()
                .filter(|trigram| base.lookup_entry(**trigram).is_none())
                .count()
    }

    /// Score-rank file candidates and report whether the pre-filter step that
    /// collects candidates reached its internal size limit before ranking.
    pub fn lexical_rank_with_stats(
        &self,
        query_trigrams: &[u32],
        candidate_filter: Option<&dyn Fn(&Path) -> bool>,
        max_files: usize,
    ) -> LexicalRankResult {
        if query_trigrams.is_empty() || max_files == 0 {
            return LexicalRankResult::default();
        }

        let mut non_zero: Vec<(u32, usize)> = query_trigrams
            .iter()
            .filter_map(|trigram| {
                let posting_count = self.posting_count(*trigram);
                (posting_count > 0).then_some((*trigram, posting_count))
            })
            .collect();
        if non_zero.is_empty() {
            return LexicalRankResult::default();
        }

        non_zero.sort_unstable_by_key(|(_, posting_count)| *posting_count);
        let selected_count = non_zero.len().min(3);
        let candidate_cap = if selected_count == 3 { 200 } else { 500 };

        let mut candidate_ids = BTreeSet::new();
        for (trigram, _) in non_zero.iter().take(selected_count) {
            for file_id in self.postings_for_trigram(*trigram, None) {
                candidate_ids.insert(file_id);
            }
        }
        let pre_filter_candidate_count = candidate_ids.len();
        let engine_capped = pre_filter_candidate_count > candidate_cap;
        let filtered_candidates = candidate_ids
            .into_iter()
            .filter_map(|file_id| {
                self.files
                    .get(file_id as usize)
                    .map(|entry| (file_id, entry))
            })
            .filter(|(_, entry)| {
                if let Some(filter) = candidate_filter {
                    filter(&entry.path)
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();

        let mut ranked = Vec::new();
        for (file_id, entry) in filtered_candidates.into_iter().take(candidate_cap) {
            let score = lexical_score_snapshot(self, query_trigrams, file_id);
            if score > 0.0 {
                ranked.push((entry.path.clone(), score));
            }
        }

        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(max_files);
        LexicalRankResult {
            files: ranked,
            engine_capped,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file_id: u32,
    pub next_mask: u8,
    pub loc_mask: u8,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    pub content_hash: blake3::Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
    pub match_text: String,
}

#[derive(Clone, Debug)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub files_with_matches: usize,
    pub index_status: IndexStatus,
    pub truncated: bool,
    pub fully_degraded: bool,
    pub engine_capped: bool,
    /// True when a fallback directory walk stopped early due to file-count or time budget.
    pub walk_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexStatus {
    Ready,
    Building,
    Fallback,
    Disabled,
}

impl IndexStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            IndexStatus::Ready => "Ready",
            IndexStatus::Building => "Building",
            IndexStatus::Fallback => "Fallback",
            IndexStatus::Disabled => "Disabled",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RegexQuery {
    pub and_trigrams: Vec<u32>,
    pub or_groups: Vec<Vec<u32>>,
    pub(crate) and_filters: HashMap<u32, PostingFilter>,
    pub(crate) or_filters: Vec<HashMap<u32, PostingFilter>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PostingFilter {
    next_mask: u8,
    loc_mask: u8,
}

#[derive(Clone, Copy)]
struct SearchFileMetadata {
    size: u64,
    modified: SystemTime,
}

struct PreparedIndexedFile {
    metadata: SearchFileMetadata,
    content_hash: blake3::Hash,
    trigram_map: BTreeMap<u32, PostingFilter>,
}

enum PreparedSearchPath {
    Indexed(PreparedIndexedFile),
    Unindexed(SearchFileMetadata),
    Skipped,
}

#[derive(Clone, Debug, Default)]
struct QueryBuild {
    and_runs: Vec<Vec<u8>>,
    or_groups: Vec<Vec<Vec<u8>>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PathFilters {
    includes: Option<GlobSet>,
    excludes: Option<GlobSet>,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchScope {
    pub root: PathBuf,
    pub use_index: bool,
}

#[derive(Clone, Debug)]
struct SharedGrepMatch {
    file: Arc<PathBuf>,
    line: u32,
    column: u32,
    line_text: String,
    match_text: String,
}

#[derive(Clone, Debug)]
enum SearchMatcher {
    Literal(LiteralSearch),
    Regex(Regex),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum IgnoreRulesLoadPolicy {
    Strict,
    BorrowTolerant,
}

impl SearchIndex {
    pub fn new() -> Self {
        SearchIndex {
            base: None,
            delta_postings: HashMap::new(),
            delta_file_trigrams: HashMap::new(),
            files: Arc::new(Vec::new()),
            path_to_id: Arc::new(HashMap::new()),
            ready: false,
            project_root: PathBuf::new(),
            git_head: None,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            ignore_rules_fingerprint: String::new(),
            file_trigram_count: Arc::new(Vec::new()),
            unindexed_files: Arc::new(HashSet::new()),
            superseded: HashSet::new(),
            base_file_count: 0,
            delta_packed_bytes: 0,
            compaction_state: Arc::new(Mutex::new(CompactionState::default())),
        }
    }

    pub fn build(root: &Path) -> Self {
        Self::build_with_limit(root, DEFAULT_MAX_FILE_SIZE)
    }

    pub fn build_with_limit(root: &Path, max_file_size: u64) -> Self {
        let cache_dir = transient_search_cache_dir(root);
        Self::build_with_limit_to_cache_dir(root, max_file_size, &cache_dir)
    }

    pub fn build_with_limit_to_cache_dir(
        root: &Path,
        max_file_size: u64,
        cache_dir: &Path,
    ) -> Self {
        let started = std::time::Instant::now();
        match build_streaming_index(root, max_file_size, cache_dir) {
            Ok((mut index, indexed)) => {
                index.ready = true;
                crate::slog_info!(
                    "search index cold streaming build: {} files, {} trigrams, {} ms (pool={})",
                    indexed,
                    index.trigram_count(),
                    started.elapsed().as_millis(),
                    search_index_build_pool_size()
                );
                index
            }
            Err(error) => {
                log::warn!(
                    "search index: streaming build failed ({}); falling back to bounded in-memory delta",
                    error
                );
                let mut index = SearchIndex {
                    project_root: fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
                    max_file_size,
                    ignore_rules_fingerprint: ignore_rules_fingerprint(
                        &fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
                    ),
                    ..SearchIndex::new()
                };
                let filters = PathFilters::default();
                let paths: Vec<PathBuf> = walk_project_files(&index.project_root, &filters);
                let indexed = index.ingest_paths_parallel(&paths);
                index.git_head = current_git_head(&index.project_root);
                index.ready = true;
                crate::slog_info!(
                    "search index fallback build: {} files, {} trigrams, {} ms (pool={})",
                    indexed,
                    index.trigram_count(),
                    started.elapsed().as_millis(),
                    search_index_build_pool_size()
                );
                index
            }
        }
    }

    /// Serial cold build for tests and parity checks against [`build_with_limit`].
    #[cfg(test)]
    pub fn build_with_limit_serial(root: &Path, max_file_size: u64) -> Self {
        let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut index = SearchIndex {
            project_root: project_root.clone(),
            max_file_size,
            ignore_rules_fingerprint: ignore_rules_fingerprint(&project_root),
            ..SearchIndex::new()
        };
        let filters = PathFilters::default();
        for path in walk_project_files(&project_root, &filters) {
            index.update_file(&path);
        }
        index.git_head = current_git_head(&project_root);
        index.ready = true;
        index
    }

    fn ingest_paths_parallel(&mut self, paths: &[PathBuf]) -> usize {
        let max_file_size = self.max_file_size;
        let pool_size = search_index_build_pool_size();
        let chunk_size = pool_size.saturating_mul(4).clamp(1, 32);
        let pool = match rayon::ThreadPoolBuilder::new()
            .num_threads(pool_size)
            .thread_name(|index| format!("aft-search-build-{index}"))
            .stack_size(8 * 1024 * 1024)
            .build()
        {
            Ok(pool) => Some(pool),
            Err(error) => {
                log::warn!(
                    "search index: bounded build pool unavailable ({error}); using global pool"
                );
                None
            }
        };

        let mut indexed = 0usize;
        for chunk in paths.chunks(chunk_size) {
            let prepare_chunk = || -> Vec<PreparedSearchPath> {
                chunk
                    .par_iter()
                    .map(|path| prepare_search_path(path, max_file_size))
                    .collect()
            };
            let prepared = match &pool {
                Some(pool) => pool.install(prepare_chunk),
                None => prepare_chunk(),
            };

            for (path, prepared) in chunk.iter().zip(prepared) {
                let inserted = match prepared {
                    PreparedSearchPath::Indexed(file) => self.index_prepared_new_file(path, file),
                    PreparedSearchPath::Unindexed(metadata) => {
                        self.track_unindexed_file_with_metadata(path, metadata)
                    }
                    PreparedSearchPath::Skipped => false,
                };
                if inserted {
                    indexed += 1;
                }
            }
        }

        indexed
    }

    pub fn index_file(&mut self, path: &Path, content: &[u8]) {
        self.remove_file(path);
        let metadata = metadata_for_indexed_content(path, content.len() as u64);
        self.index_file_with_metadata(path, content, metadata);
    }

    fn index_file_with_metadata(
        &mut self,
        path: &Path,
        content: &[u8],
        metadata: SearchFileMetadata,
    ) -> bool {
        self.index_prepared_new_file(
            path,
            PreparedIndexedFile {
                metadata,
                content_hash: cache_freshness::hash_bytes(content),
                trigram_map: trigram_filter_map(content, true),
            },
        )
    }

    fn index_prepared_new_file(&mut self, path: &Path, file: PreparedIndexedFile) -> bool {
        let file_id = match self.allocate_file_id_with_metadata(path, file.metadata) {
            Some(file_id) => file_id,
            None => return false,
        };
        if let Some(entry) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            entry.content_hash = file.content_hash;
        }

        let mut file_trigrams = Vec::with_capacity(file.trigram_map.len());
        for (trigram, filter) in file.trigram_map {
            let postings = self.delta_postings.entry(trigram).or_default();
            postings.push(Posting {
                file_id,
                next_mask: filter.next_mask,
                loc_mask: filter.loc_mask,
            });
            if postings.len() > 1
                && postings[postings.len() - 2].file_id > postings[postings.len() - 1].file_id
            {
                postings.sort_unstable_by_key(|p| p.file_id);
            }
            file_trigrams.push(trigram);
        }

        let trigram_count = file_trigrams.len() as u32;
        self.delta_packed_bytes = self
            .delta_packed_bytes
            .saturating_add(file_trigrams.len().saturating_mul(POSTING_BYTES));
        self.delta_file_trigrams.insert(file_id, file_trigrams);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = trigram_count;
        }
        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        self.update_compaction_flags(Some(path));
        true
    }

    pub fn remove_file(&mut self, path: &Path) {
        let canonical_path = canonicalize_existing_or_deleted_path(path);
        let file_id = {
            let path_to_id = Arc::make_mut(&mut self.path_to_id);
            if let Some(file_id) = path_to_id.remove(path) {
                file_id
            } else if canonical_path.as_path() != path {
                let Some(file_id) = path_to_id.remove(&canonical_path) else {
                    return;
                };
                file_id
            } else {
                return;
            }
        };

        if file_id < self.base_file_count {
            self.superseded.insert(file_id);
        }

        if let Some(trigrams) = self.delta_file_trigrams.remove(&file_id) {
            self.delta_packed_bytes = self
                .delta_packed_bytes
                .saturating_sub(trigrams.len().saturating_mul(POSTING_BYTES));
            for trigram in trigrams {
                let should_remove = if let Some(postings) = self.delta_postings.get_mut(&trigram) {
                    postings.retain(|posting| posting.file_id != file_id);
                    postings.is_empty()
                } else {
                    false
                };

                if should_remove {
                    self.delta_postings.remove(&trigram);
                }
            }
        }

        Arc::make_mut(&mut self.unindexed_files).remove(&file_id);
        if let Some(file) = Arc::make_mut(&mut self.files).get_mut(file_id as usize) {
            file.path = PathBuf::new();
            file.size = 0;
            file.modified = UNIX_EPOCH;
            file.content_hash = cache_freshness::zero_hash();
        }
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
        self.update_compaction_flags(Some(path));
    }

    pub fn update_file(&mut self, path: &Path) {
        self.remove_file(path);

        let metadata = match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => return,
        };

        let metadata = search_file_metadata(&metadata);

        if is_binary_path(path, metadata.size) {
            self.track_unindexed_file_with_metadata(path, metadata);
            return;
        }

        if metadata.size > self.max_file_size {
            self.track_unindexed_file_with_metadata(path, metadata);
            return;
        }

        let content = match fs::read(path) {
            Ok(content) => content,
            Err(_) => return,
        };

        if is_binary_bytes(&content) {
            self.track_unindexed_file_with_metadata(path, metadata);
            return;
        }

        self.index_file_with_metadata(path, &content, metadata);
    }

    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.snapshot().grep(
            pattern,
            case_sensitive,
            include,
            exclude,
            search_root,
            max_results,
        )
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        self.snapshot()
            .search_grep(pattern, include, exclude, search_root, max_results)
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        self.snapshot().glob(pattern, search_root)
    }

    pub fn candidates(&self, query: &RegexQuery) -> Vec<u32> {
        self.snapshot().candidates(query)
    }

    pub fn write_to_disk(&mut self, cache_dir: &Path, git_head: Option<&str>) {
        let Some(plan) = CacheWritePlan::from_index(self, git_head) else {
            return;
        };

        let write_result = {
            let mut sources = self.compaction_record_sources(Arc::clone(&plan.id_map));
            write_cache_file_from_sources(cache_dir, &plan, &mut sources)
        };

        match write_result {
            Ok(base) => {
                self.base = Some(Arc::new(base));
                self.delta_postings.clear();
                self.delta_file_trigrams.clear();
                self.superseded.clear();
                self.delta_packed_bytes = 0;
                self.base_file_count = u32::try_from(plan.files.len()).unwrap_or(u32::MAX);
                self.files = Arc::new(plan.files);
                self.path_to_id = Arc::new(plan.path_to_id);
                self.unindexed_files = Arc::new(plan.unindexed_files);
                self.file_trigram_count = Arc::new(plan.file_trigram_count);
                self.git_head = plan.git_head.filter(|head| !head.is_empty());
                self.ignore_rules_fingerprint = plan.ignore_fingerprint;
            }
            Err(error) => {
                log::warn!("search index: failed to write disk cache: {}", error);
            }
        }
    }

    pub fn read_from_disk(cache_dir: &Path, current_canonical_root: &Path) -> Option<Self> {
        Self::read_from_disk_with_options(cache_dir, current_canonical_root, true)
    }

    pub(crate) fn read_from_disk_borrow_tolerant(
        cache_dir: &Path,
        current_canonical_root: &Path,
    ) -> Option<(Self, bool)> {
        Self::read_from_disk_with_policy(
            cache_dir,
            current_canonical_root,
            false,
            IgnoreRulesLoadPolicy::BorrowTolerant,
        )
    }

    fn read_from_disk_with_options(
        cache_dir: &Path,
        current_canonical_root: &Path,
        allow_legacy_repair: bool,
    ) -> Option<Self> {
        Self::read_from_disk_with_policy(
            cache_dir,
            current_canonical_root,
            allow_legacy_repair,
            IgnoreRulesLoadPolicy::Strict,
        )
        .map(|(index, _)| index)
    }

    fn read_from_disk_with_policy(
        cache_dir: &Path,
        current_canonical_root: &Path,
        allow_legacy_repair: bool,
        ignore_rules_load_policy: IgnoreRulesLoadPolicy,
    ) -> Option<(Self, bool)> {
        debug_assert!(current_canonical_root.is_absolute());
        let cache_path = cache_dir.join("cache.bin");
        let cache_file = open_cache_file_read(&cache_path).ok()?;
        let file_len = cache_file.metadata().ok()?.len();
        if file_len < 16 {
            return None;
        }

        let mut reader = BufReader::new(cache_file.try_clone().ok()?);
        if read_u32(&mut reader).ok()? != CACHE_MAGIC {
            return None;
        }
        if read_u32(&mut reader).ok()? != INDEX_VERSION {
            return None;
        }
        let postings_len_total = read_u64(&mut reader).ok()?;
        let postings_section_start = reader.stream_position().ok()?;
        let postings_section_end = postings_section_start.checked_add(postings_len_total)?;
        if postings_len_total < 4 || postings_section_end > file_len {
            return None;
        }
        let postings_body_end = postings_section_end.checked_sub(4)?;

        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic).ok()?;
        if &magic != INDEX_MAGIC {
            return None;
        }
        if read_u32(&mut reader).ok()? != INDEX_VERSION {
            return None;
        }

        let head_len = read_u32(&mut reader).ok()? as usize;
        let root_len = read_u32(&mut reader).ok()? as usize;
        let ignore_fingerprint_len = read_u32(&mut reader).ok()? as usize;
        let max_file_size = read_u64(&mut reader).ok()?;
        let file_count = read_u32(&mut reader).ok()? as usize;
        if file_count > MAX_ENTRIES {
            return None;
        }

        if !reader_has_remaining(&mut reader, postings_body_end, head_len).ok()? {
            return None;
        }
        let mut head_bytes = vec![0u8; head_len];
        reader.read_exact(&mut head_bytes).ok()?;
        let git_head = String::from_utf8(head_bytes)
            .ok()
            .filter(|head| !head.is_empty());

        if !reader_has_remaining(&mut reader, postings_body_end, root_len).ok()? {
            return None;
        }
        let mut root_bytes = vec![0u8; root_len];
        reader.read_exact(&mut root_bytes).ok()?;
        let _stored_project_root = PathBuf::from(String::from_utf8(root_bytes).ok()?);
        let project_root = current_canonical_root.to_path_buf();

        if !reader_has_remaining(&mut reader, postings_body_end, ignore_fingerprint_len).ok()? {
            return None;
        }
        let mut ignore_fingerprint_bytes = vec![0u8; ignore_fingerprint_len];
        reader.read_exact(&mut ignore_fingerprint_bytes).ok()?;
        let stored_ignore_rules_fingerprint = String::from_utf8(ignore_fingerprint_bytes).ok()?;
        let current_ignore_rules_fingerprint = ignore_rules_fingerprint(&project_root);
        let ignore_rules_differ =
            stored_ignore_rules_fingerprint != current_ignore_rules_fingerprint;
        if ignore_rules_differ && ignore_rules_load_policy == IgnoreRulesLoadPolicy::Strict {
            return None;
        }

        let mut files = Vec::with_capacity(file_count);
        let mut path_to_id = HashMap::new();
        let mut unindexed_files = HashSet::new();

        for file_id in 0..file_count {
            if !reader_has_remaining(&mut reader, postings_body_end, MIN_FILE_ENTRY_BYTES).ok()? {
                return None;
            }
            let mut unindexed = [0u8; 1];
            reader.read_exact(&mut unindexed).ok()?;
            let path_len = read_u32(&mut reader).ok()? as usize;
            let size = read_u64(&mut reader).ok()?;
            let secs = read_u64(&mut reader).ok()?;
            let nanos = read_u32(&mut reader).ok()?;
            let mut hash_bytes = [0u8; 32];
            reader.read_exact(&mut hash_bytes).ok()?;
            let content_hash = blake3::Hash::from_bytes(hash_bytes);
            if nanos >= 1_000_000_000 {
                return None;
            }
            if !reader_has_remaining(&mut reader, postings_body_end, path_len).ok()? {
                return None;
            }
            let mut path_bytes = vec![0u8; path_len];
            reader.read_exact(&mut path_bytes).ok()?;
            let relative_path = PathBuf::from(String::from_utf8(path_bytes).ok()?);
            let full_path = cached_path_under_root(&project_root, &relative_path)?;
            let file_id_u32 = u32::try_from(file_id).ok()?;

            files.push(FileEntry {
                path: full_path.clone(),
                size,
                modified: UNIX_EPOCH + Duration::new(secs, nanos),
                content_hash,
            });
            path_to_id.insert(full_path, file_id_u32);
            if unindexed[0] == 1 {
                unindexed_files.insert(file_id_u32);
            }
        }

        if !reader_has_remaining(&mut reader, postings_body_end, 8).ok()? {
            return None;
        }
        let postings_blob_len = read_u64(&mut reader).ok()?;
        let postings_blob_start = reader.stream_position().ok()?;
        let postings_blob_end = postings_blob_start.checked_add(postings_blob_len)?;
        if postings_blob_end > postings_body_end || postings_blob_len % POSTING_BYTES as u64 != 0 {
            return None;
        }

        let lookup_section_start = postings_section_end;
        if lookup_section_start >= file_len {
            return None;
        }
        let mut lookup_file = cache_file.try_clone().ok()?;
        lookup_file
            .seek(SeekFrom::Start(lookup_section_start))
            .ok()?;
        let mut lookup_bytes = Vec::new();
        lookup_file.read_to_end(&mut lookup_bytes).ok()?;
        if lookup_bytes.len() < 4 {
            return None;
        }
        verify_crc32_bytes_slice(&lookup_bytes).ok()?;
        let lookup_body_len = lookup_bytes.len().checked_sub(4)?;
        let mut lookup_reader = BufReader::new(Cursor::new(&lookup_bytes));
        let mut lookup_magic = [0u8; 8];
        lookup_reader.read_exact(&mut lookup_magic).ok()?;
        if &lookup_magic != LOOKUP_MAGIC {
            return None;
        }
        if read_u32(&mut lookup_reader).ok()? != INDEX_VERSION {
            return None;
        }
        let entry_count = read_u32(&mut lookup_reader).ok()? as usize;
        if entry_count > MAX_ENTRIES {
            return None;
        }
        let remaining_lookup = remaining_bytes(&mut lookup_reader, lookup_body_len)?;
        let minimum_lookup_bytes = entry_count.checked_mul(LOOKUP_ENTRY_BYTES)?;
        if minimum_lookup_bytes > remaining_lookup {
            return None;
        }

        let mut lookup = Vec::with_capacity(entry_count);
        let mut previous_trigram = None;
        for _ in 0..entry_count {
            let trigram = read_u32(&mut lookup_reader).ok()?;
            let offset = read_u64(&mut lookup_reader).ok()?;
            let count = read_u32(&mut lookup_reader).ok()?;
            if count as usize > MAX_ENTRIES {
                return None;
            }
            if previous_trigram.is_some_and(|previous| previous >= trigram) {
                return None;
            }
            previous_trigram = Some(trigram);
            let bytes_len = (count as u64).checked_mul(POSTING_BYTES as u64)?;
            let end = offset.checked_add(bytes_len)?;
            if end > postings_blob_len {
                return None;
            }
            lookup.push(LookupEntry {
                trigram,
                offset,
                count,
            });
        }

        let base = BasePostings {
            file: Arc::new(cache_file),
            postings_blob_start,
            postings_blob_len,
            lookup: Arc::new(lookup),
        };

        let (file_trigram_count, migrated_counts) = match read_file_trigram_count_extension(
            &base,
            postings_blob_end,
            postings_body_end,
            file_count,
        ) {
            Ok(Some(counts)) => (counts, false),
            Ok(None) => (
                compute_file_trigram_counts_from_base(&base, file_count).ok()?,
                true,
            ),
            Err(_) => return None,
        };

        let mut index = SearchIndex {
            base: Some(Arc::new(base)),
            delta_postings: HashMap::new(),
            delta_file_trigrams: HashMap::new(),
            files: Arc::new(files),
            path_to_id: Arc::new(path_to_id),
            ready: false,
            project_root,
            git_head,
            max_file_size,
            ignore_rules_fingerprint: current_ignore_rules_fingerprint,
            file_trigram_count: Arc::new(file_trigram_count),
            unindexed_files: Arc::new(unindexed_files),
            superseded: HashSet::new(),
            base_file_count: u32::try_from(file_count).ok()?,
            delta_packed_bytes: 0,
            compaction_state: Arc::new(Mutex::new(CompactionState::default())),
        };

        if migrated_counts && allow_legacy_repair {
            if let Ok(_lock) = CacheLock::acquire(cache_dir) {
                let head = index.git_head.clone();
                index.write_to_disk(cache_dir, head.as_deref());
            }
        }

        Some((index, ignore_rules_differ))
    }

    pub fn stored_git_head(&self) -> Option<&str> {
        self.git_head.as_deref()
    }

    pub(crate) fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    pub(crate) fn verify_against_disk(&mut self, current_head: Option<String>) {
        self.git_head = current_head;
        verify_file_mtimes(self);
        self.ready = true;
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn verify_against_disk_for_debug(&mut self, current_head: Option<String>) {
        self.verify_against_disk(current_head);
    }

    pub(crate) fn rebuild_or_refresh(
        root: &Path,
        max_file_size: u64,
        current_head: Option<String>,
        baseline: Option<SearchIndex>,
        cache_dir: Option<&Path>,
    ) -> Self {
        if let Some(mut baseline) = baseline {
            baseline.project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            baseline.max_file_size = max_file_size;
            let current_ignore_rules_fingerprint = ignore_rules_fingerprint(&baseline.project_root);
            if baseline.ignore_rules_fingerprint != current_ignore_rules_fingerprint {
                return match cache_dir {
                    Some(cache_dir) => {
                        SearchIndex::build_with_limit_to_cache_dir(root, max_file_size, cache_dir)
                    }
                    None => SearchIndex::build_with_limit(root, max_file_size),
                };
            }
            baseline.ignore_rules_fingerprint = current_ignore_rules_fingerprint;

            if baseline.git_head == current_head || current_head.is_none() {
                // HEAD matches, but files may have changed on disk since the index was
                // last written (e.g., uncommitted edits, stash pop, manual file changes
                // while OpenCode was closed). Verify mtimes and re-index stale files.
                // Non-git projects also use this per-file (path, mtime, size)
                // fingerprint so unchanged trees reuse the disk cache instead of
                // rebuilding every configure.
                baseline.git_head = current_head;
                verify_file_mtimes(&mut baseline);
                baseline.ready = true;
                return baseline;
            }

            if let (Some(previous), Some(current)) =
                (baseline.git_head.clone(), current_head.clone())
            {
                let project_root = baseline.project_root.clone();
                if apply_git_diff_updates(&mut baseline, &project_root, &previous, &current) {
                    baseline.git_head = Some(current);
                    verify_file_mtimes(&mut baseline);
                    baseline.ready = true;
                    return baseline;
                }
            }
        }

        match cache_dir {
            Some(cache_dir) => {
                SearchIndex::build_with_limit_to_cache_dir(root, max_file_size, cache_dir)
            }
            None => SearchIndex::build_with_limit(root, max_file_size),
        }
    }

    fn allocate_file_id_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> Option<u32> {
        let file_id = u32::try_from(self.files.len()).ok()?;
        Arc::make_mut(&mut self.files).push(FileEntry {
            path: path.to_path_buf(),
            size: metadata.size,
            modified: metadata.modified,
            content_hash: cache_freshness::zero_hash(),
        });
        Arc::make_mut(&mut self.path_to_id).insert(path.to_path_buf(), file_id);
        ensure_count_slot(Arc::make_mut(&mut self.file_trigram_count), file_id);
        Some(file_id)
    }

    fn track_unindexed_file_with_metadata(
        &mut self,
        path: &Path,
        metadata: SearchFileMetadata,
    ) -> bool {
        let Some(file_id) = self.allocate_file_id_with_metadata(path, metadata) else {
            return false;
        };
        Arc::make_mut(&mut self.unindexed_files).insert(file_id);
        if let Some(count) = Arc::make_mut(&mut self.file_trigram_count).get_mut(file_id as usize) {
            *count = 0;
        }
        true
    }

    fn active_file_ids(&self) -> Vec<u32> {
        self.snapshot().active_file_ids()
    }

    #[cfg(test)]
    fn postings_for_trigram(&self, trigram: u32, filter: Option<PostingFilter>) -> Vec<u32> {
        self.snapshot().postings_for_trigram(trigram, filter)
    }

    fn update_compaction_flags(&mut self, changed_path: Option<&Path>) {
        let delta_files = self.delta_file_trigrams.len();
        let hard = delta_files >= DELTA_COMPACT_HARD_FILES
            || self.delta_packed_bytes >= DELTA_COMPACT_HARD_BYTES;
        let soft = delta_files >= DELTA_COMPACT_SOFT_FILES
            || self.delta_packed_bytes >= DELTA_COMPACT_SOFT_BYTES;
        if let Ok(mut state) = self.compaction_state.lock() {
            if state.running {
                if let Some(path) = changed_path {
                    state.buffered_paths.push(path.to_path_buf());
                }
                if soft || hard {
                    state.requested_again = true;
                }
            } else if hard || (soft && !state.requested_again) {
                state.requested_again = true;
            }
        }
    }

    fn compaction_record_sources(
        &self,
        id_map: Arc<HashMap<u32, u32>>,
    ) -> Vec<Box<dyn PostingRecordSource>> {
        let mut sources: Vec<Box<dyn PostingRecordSource>> = Vec::new();
        if let Some(base) = self.base.clone() {
            sources.push(Box::new(BaseRecordSource::new(
                base,
                Arc::clone(&id_map),
                Arc::new(self.superseded.clone()),
            )));
        }

        let mut delta_records = Vec::new();
        for (&trigram, postings) in &self.delta_postings {
            for posting in postings {
                let Some(mapped_file_id) = id_map.get(&posting.file_id).copied() else {
                    continue;
                };
                delta_records.push(SpillRecord {
                    trigram,
                    file_id: mapped_file_id,
                    next_mask: posting.next_mask,
                    loc_mask: posting.loc_mask,
                });
            }
        }
        if !delta_records.is_empty() {
            delta_records.sort_unstable_by_key(|record| (record.trigram, record.file_id));
            sources.push(Box::new(VecRecordSource::new(delta_records)));
        }
        sources
    }
}

impl BasePostings {
    fn lookup_entry(&self, trigram: u32) -> Option<LookupEntry> {
        self.lookup
            .binary_search_by_key(&trigram, |entry| entry.trigram)
            .ok()
            .and_then(|index| self.lookup.get(index).copied())
    }

    fn read_postings(&self, entry: LookupEntry) -> std::io::Result<Vec<Posting>> {
        let bytes_len = (entry.count as usize)
            .checked_mul(POSTING_BYTES)
            .ok_or_else(|| std::io::Error::other("posting list too large"))?;
        let offset = self
            .postings_blob_start
            .checked_add(entry.offset)
            .ok_or_else(|| std::io::Error::other("posting offset overflow"))?;
        let end = entry
            .offset
            .checked_add(bytes_len as u64)
            .ok_or_else(|| std::io::Error::other("posting offset overflow"))?;
        if end > self.postings_blob_len {
            return Err(std::io::Error::other("posting list exceeds blob"));
        }
        let mut bytes = vec![0u8; bytes_len];
        pread_exact(&self.file, offset, &mut bytes)?;
        let mut postings = Vec::with_capacity(entry.count as usize);
        for chunk in bytes.chunks_exact(POSTING_BYTES) {
            postings.push(Posting {
                file_id: u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                next_mask: chunk[4],
                loc_mask: chunk[5],
            });
        }
        Ok(postings)
    }
}

impl SearchIndexSnapshot {
    pub fn grep(
        &self,
        pattern: &str,
        case_sensitive: bool,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        match pattern_compile::compile(
            pattern,
            CompileOpts {
                case_insensitive: !case_sensitive,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => {
                self.search_grep(&compiled, include, exclude, search_root, max_results)
            }
            CompileResult::InvalidPattern { .. } | CompileResult::UnsupportedSyntax { .. } => {
                self.empty_grep_result()
            }
        }
    }

    pub fn search_grep(
        &self,
        pattern: &CompiledPattern,
        include: &[String],
        exclude: &[String],
        search_root: &Path,
        max_results: usize,
    ) -> GrepResult {
        let matcher = match pattern {
            CompiledPattern::Literal(literal) => SearchMatcher::Literal(literal.clone()),
            CompiledPattern::Regex { compiled, .. } => SearchMatcher::Regex(compiled.clone()),
        };

        let filters = match build_path_filters(include, exclude) {
            Ok(filters) => filters,
            Err(_) => PathFilters::default(),
        };
        let search_root = canonicalize_or_normalize(search_root);

        let raw_pattern = pattern.raw_pattern_for_trigrams();
        let query = if pattern.case_insensitive() && !raw_pattern.is_ascii() {
            RegexQuery::default()
        } else {
            decompose_regex(&raw_pattern)
        };
        let fully_degraded = query.and_trigrams.is_empty() && query.or_groups.is_empty();
        let candidate_ids = self.candidates(&query);

        let candidate_files: Vec<&FileEntry> = candidate_ids
            .into_iter()
            .filter_map(|file_id| self.files.get(file_id as usize))
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .collect();

        let total_matches = AtomicUsize::new(0);
        let files_searched = AtomicUsize::new(0);
        let files_with_matches = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let engine_capped = AtomicBool::new(false);
        let stop_after = max_results.saturating_mul(2);
        let stop_scan = Arc::new(AtomicBool::new(false));

        let mut matches = if candidate_files.len() > 10 {
            candidate_files
                .par_iter()
                .map(|file| {
                    if grep_scan_should_stop(
                        Some(&stop_scan),
                        &truncated,
                        &total_matches,
                        stop_after,
                    ) {
                        engine_capped.store(true, Ordering::Relaxed);
                        return Vec::new();
                    }
                    search_candidate_file(
                        file,
                        &matcher,
                        max_results,
                        stop_after,
                        &total_matches,
                        &files_searched,
                        &files_with_matches,
                        &truncated,
                        &engine_capped,
                        Some(&stop_scan),
                    )
                })
                .reduce(Vec::new, |mut left, mut right| {
                    // When concatenating partial match lists from parallel file
                    // searches, simply append the chunks. The stop checks in
                    // each worker decide whether the result cap was reached.
                    left.append(&mut right);
                    left
                })
        } else {
            let mut matches = Vec::new();
            for file in candidate_files {
                matches.extend(search_candidate_file(
                    file,
                    &matcher,
                    max_results,
                    stop_after,
                    &total_matches,
                    &files_searched,
                    &files_with_matches,
                    &truncated,
                    &engine_capped,
                    None,
                ));

                if should_stop_search(&truncated, &total_matches, stop_after) {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }
            }
            matches
        };

        sort_shared_grep_matches_by_cached_mtime_desc(&mut matches, &self.project_root, |path| {
            self.path_to_id
                .get(path)
                .and_then(|file_id| self.files.get(*file_id as usize))
                .map(|file| file.modified)
        });

        let matches = matches
            .into_iter()
            .map(|matched| GrepMatch {
                file: matched.file.as_ref().clone(),
                line: matched.line,
                column: matched.column,
                line_text: matched.line_text,
                match_text: matched.match_text,
            })
            .collect();

        GrepResult {
            total_matches: total_matches.load(Ordering::Relaxed),
            matches,
            files_searched: files_searched.load(Ordering::Relaxed),
            files_with_matches: files_with_matches.load(Ordering::Relaxed),
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: truncated.load(Ordering::Relaxed),
            fully_degraded,
            engine_capped: engine_capped.load(Ordering::Relaxed),
            walk_truncated: false,
        }
    }

    fn empty_grep_result(&self) -> GrepResult {
        GrepResult {
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: if self.ready {
                IndexStatus::Ready
            } else {
                IndexStatus::Building
            },
            truncated: false,
            fully_degraded: false,
            engine_capped: false,
            walk_truncated: false,
        }
    }

    pub fn glob(&self, pattern: &str, search_root: &Path) -> Vec<PathBuf> {
        let filters = match build_path_filters(&[pattern.to_string()], &[]) {
            Ok(filters) => filters,
            Err(_) => return Vec::new(),
        };
        let search_root = canonicalize_or_normalize(search_root);
        let mut entries = self
            .files
            .iter()
            .filter(|file| !file.path.as_os_str().is_empty())
            .filter(|file| is_within_search_root(&search_root, &file.path))
            .filter(|file| filters.matches(&self.project_root, &file.path))
            .map(|file| (file.path.clone(), file.modified))
            .collect::<Vec<_>>();

        entries.sort_by(|(left_path, left_mtime), (right_path, right_mtime)| {
            right_mtime
                .cmp(left_mtime)
                .then_with(|| left_path.cmp(right_path))
        });

        entries.into_iter().map(|(path, _)| path).collect()
    }

    pub fn candidates(&self, query: &RegexQuery) -> Vec<u32> {
        if query.and_trigrams.is_empty() && query.or_groups.is_empty() {
            return self.active_file_ids();
        }

        let mut and_trigrams = query.and_trigrams.clone();
        and_trigrams.sort_unstable_by_key(|trigram| self.posting_count(*trigram));

        let mut current: Option<Vec<u32>> = None;

        for trigram in and_trigrams {
            let filter = query.and_filters.get(&trigram).copied();
            let matches = self.postings_for_trigram(trigram, filter);
            current = Some(match current.take() {
                Some(existing) => intersect_sorted_ids(&existing, &matches),
                None => matches,
            });

            if current.as_ref().is_some_and(|ids| ids.is_empty()) {
                break;
            }
        }

        let mut current = current.unwrap_or_else(|| self.active_file_ids());

        for (index, group) in query.or_groups.iter().enumerate() {
            let mut group_matches = Vec::new();
            let filters = query.or_filters.get(index);

            for trigram in group {
                let filter = filters.and_then(|filters| filters.get(trigram).copied());
                let matches = self.postings_for_trigram(*trigram, filter);
                if group_matches.is_empty() {
                    group_matches = matches;
                } else {
                    group_matches = union_sorted_ids(&group_matches, &matches);
                }
            }

            current = intersect_sorted_ids(&current, &group_matches);
            if current.is_empty() {
                break;
            }
        }

        let mut unindexed = self
            .unindexed_files
            .iter()
            .copied()
            .filter(|file_id| self.is_active_file(*file_id))
            .collect::<Vec<_>>();
        if !unindexed.is_empty() {
            unindexed.sort_unstable();
            current = union_sorted_ids(&current, &unindexed);
        }

        current
    }

    fn posting_count(&self, trigram: u32) -> usize {
        let base_count = self
            .base
            .as_ref()
            .and_then(|base| base.lookup_entry(trigram))
            .map_or(0usize, |entry| entry.count as usize);
        base_count.saturating_add(self.delta_postings.get(&trigram).map_or(0usize, Vec::len))
    }

    fn active_file_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.path_to_id.values().copied().collect();
        ids.retain(|file_id| self.is_active_file(*file_id));
        ids.sort_unstable();
        ids
    }

    fn is_active_file(&self, file_id: u32) -> bool {
        if self.superseded.contains(&file_id) {
            return false;
        }
        self.files
            .get(file_id as usize)
            .map(|file| !file.path.as_os_str().is_empty())
            .unwrap_or(false)
    }

    fn postings_for_trigram(&self, trigram: u32, filter: Option<PostingFilter>) -> Vec<u32> {
        let mut matches = Vec::new();

        if let Some(base_entry) = self
            .base
            .as_ref()
            .and_then(|base| base.lookup_entry(trigram))
        {
            if let Some(base) = &self.base {
                if let Ok(postings) = base.read_postings(base_entry) {
                    matches.reserve(postings.len());
                    for posting in postings {
                        if self.superseded.contains(&posting.file_id) {
                            continue;
                        }
                        if !posting_matches_filter(&posting, filter) {
                            continue;
                        }
                        if self.is_active_file(posting.file_id) {
                            matches.push(posting.file_id);
                        }
                    }
                }
            }
        }

        if let Some(postings) = self.delta_postings.get(&trigram) {
            matches.reserve(postings.len());
            for posting in postings {
                if !posting_matches_filter(posting, filter) {
                    continue;
                }
                if self.is_active_file(posting.file_id) {
                    matches.push(posting.file_id);
                }
            }
        }

        if matches.len() > 1 {
            matches.sort_unstable();
            matches.dedup();
        }
        matches
    }
}

fn posting_matches_filter(posting: &Posting, filter: Option<PostingFilter>) -> bool {
    if let Some(filter) = filter {
        // next_mask is a bloom filter: the character following this trigram in
        // the query must also appear after this trigram somewhere in the file.
        if filter.next_mask != 0 && posting.next_mask & filter.next_mask == 0 {
            return false;
        }
        // loc_mask is persisted for future adjacency checks. It is intentionally
        // not used as a single-trigram filter because query positions do not
        // correspond to file positions.
    }
    true
}

fn search_candidate_file(
    file: &FileEntry,
    matcher: &SearchMatcher,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    stop_scan: Option<&Arc<AtomicBool>>,
) -> Vec<SharedGrepMatch> {
    if grep_scan_should_stop(stop_scan, truncated, total_matches, stop_after) {
        engine_capped.store(true, Ordering::Relaxed);
        return Vec::new();
    }

    let content = match read_indexed_file_bytes(&file.path) {
        Some(content) => content,
        None => return Vec::new(),
    };
    // Defense in depth: even though indexing tries to filter binaries via
    // `is_binary_path` + full-content `is_binary_bytes`, we double-check at
    // query time. content_inspector is fast (~bytes-per-cycle on a small
    // preview) and this guarantees we never surface matches inside binary
    // files even if the indexer somehow let one through (e.g. file changed
    // between indexing and query).
    if is_binary_bytes(&content) {
        return Vec::new();
    }
    files_searched.fetch_add(1, Ordering::Relaxed);

    let shared_path = Arc::new(file.path.clone());
    let mut matches = Vec::new();
    let mut line_starts = None;
    let mut seen_lines = HashSet::new();
    let mut matched_this_file = false;

    match matcher {
        SearchMatcher::Literal(literal) if !literal.case_insensitive_ascii => {
            let needle = &literal.needle;
            let finder = memchr::memmem::Finder::new(needle);
            let mut start = 0;

            while let Some(position) = finder.find(&content[start..]) {
                if grep_scan_should_stop(stop_scan, truncated, total_matches, stop_after) {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }

                let offset = start + position;
                start = offset + 1;

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) = line_details_bytes(&content, line_starts, offset);
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    signal_grep_scan_cap(stop_scan, total_matches, stop_after);
                    break;
                }

                let end = offset + needle.len();
                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
                });
            }
        }
        SearchMatcher::Literal(literal) => {
            let needle = &literal.needle;
            let search_content = content.to_ascii_lowercase();
            let finder = memchr::memmem::Finder::new(needle);
            let mut start = 0;

            while let Some(position) = finder.find(&search_content[start..]) {
                if grep_scan_should_stop(stop_scan, truncated, total_matches, stop_after) {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }

                let offset = start + position;
                start = offset + 1;

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) = line_details_bytes(&content, line_starts, offset);
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    signal_grep_scan_cap(stop_scan, total_matches, stop_after);
                    break;
                }

                let end = offset + needle.len();
                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(&content[offset..end]).into_owned(),
                });
            }
        }
        SearchMatcher::Regex(regex) => {
            for matched in regex.find_iter(&content) {
                if grep_scan_should_stop(stop_scan, truncated, total_matches, stop_after) {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }

                let line_starts = line_starts.get_or_insert_with(|| line_starts_bytes(&content));
                let (line, column, line_text) =
                    line_details_bytes(&content, line_starts, matched.start());
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    signal_grep_scan_cap(stop_scan, total_matches, stop_after);
                    break;
                }

                matches.push(SharedGrepMatch {
                    file: shared_path.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(matched.as_bytes()).into_owned(),
                });
            }
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

fn should_stop_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    truncated.load(Ordering::Relaxed) && total_matches.load(Ordering::Relaxed) >= stop_after
}

fn grep_scan_should_stop(
    stop_scan: Option<&Arc<AtomicBool>>,
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    stop_scan.is_some_and(|flag| flag.load(Ordering::Relaxed))
        || should_stop_search(truncated, total_matches, stop_after)
}

fn signal_grep_scan_cap(
    stop_scan: Option<&Arc<AtomicBool>>,
    total_matches: &AtomicUsize,
    stop_after: usize,
) {
    if let Some(flag) = stop_scan {
        if total_matches.load(Ordering::Relaxed) >= stop_after {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn search_file_metadata(metadata: &fs::Metadata) -> SearchFileMetadata {
    SearchFileMetadata {
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(UNIX_EPOCH),
    }
}

fn metadata_for_indexed_content(path: &Path, size_hint: u64) -> SearchFileMetadata {
    fs::metadata(path)
        .ok()
        .map(|metadata| search_file_metadata(&metadata))
        .unwrap_or(SearchFileMetadata {
            size: size_hint,
            modified: UNIX_EPOCH,
        })
}

fn prepare_search_path(path: &Path, max_file_size: u64) -> PreparedSearchPath {
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => search_file_metadata(&metadata),
        _ => return PreparedSearchPath::Skipped,
    };

    if is_binary_path(path, metadata.size) || metadata.size > max_file_size {
        return PreparedSearchPath::Unindexed(metadata);
    }

    let content = match fs::read(path) {
        Ok(content) => content,
        Err(_) => return PreparedSearchPath::Skipped,
    };

    if is_binary_bytes(&content) {
        return PreparedSearchPath::Unindexed(metadata);
    }

    PreparedSearchPath::Indexed(PreparedIndexedFile {
        metadata,
        content_hash: cache_freshness::hash_bytes(&content),
        trigram_map: trigram_filter_map(&content, true),
    })
}

/// Returns the worker pool size for cold search-index builds: half of available
/// cores, capped at 8 to keep the same limit used by the callgraph store.
fn search_index_build_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpillRecord {
    trigram: u32,
    file_id: u32,
    next_mask: u8,
    loc_mask: u8,
}

struct CacheWritePlan {
    project_root: PathBuf,
    git_head: Option<String>,
    ignore_fingerprint: String,
    max_file_size: u64,
    files: Vec<FileEntry>,
    path_to_id: HashMap<PathBuf, u32>,
    unindexed_files: HashSet<u32>,
    file_trigram_count: Vec<u32>,
    id_map: Arc<HashMap<u32, u32>>,
}

impl CacheWritePlan {
    fn from_index(index: &SearchIndex, git_head: Option<&str>) -> Option<Self> {
        let active_ids = index.active_file_ids();
        let mut id_map = HashMap::with_capacity(active_ids.len());
        for (new_id, old_id) in active_ids.iter().enumerate() {
            let new_id = u32::try_from(new_id).ok()?;
            id_map.insert(*old_id, new_id);
        }

        let mut files = Vec::with_capacity(active_ids.len());
        let mut path_to_id = HashMap::with_capacity(active_ids.len());
        let mut unindexed_files = HashSet::new();
        let mut file_trigram_count = Vec::with_capacity(active_ids.len());
        for old_id in active_ids {
            let new_id = *id_map.get(&old_id)?;
            let file = index.files.get(old_id as usize)?.clone();
            if file.path.as_os_str().is_empty() {
                continue;
            }
            path_to_id.insert(file.path.clone(), new_id);
            if index.unindexed_files.contains(&old_id) {
                unindexed_files.insert(new_id);
            }
            file_trigram_count.push(
                index
                    .file_trigram_count
                    .get(old_id as usize)
                    .copied()
                    .unwrap_or(0),
            );
            files.push(file);
        }

        Some(Self {
            project_root: index.project_root.clone(),
            git_head: git_head.map(ToOwned::to_owned),
            ignore_fingerprint: if index.ignore_rules_fingerprint.is_empty() {
                ignore_rules_fingerprint(&index.project_root)
            } else {
                index.ignore_rules_fingerprint.clone()
            },
            max_file_size: index.max_file_size,
            files,
            path_to_id,
            unindexed_files,
            file_trigram_count,
            id_map: Arc::new(id_map),
        })
    }
}

trait PostingRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>>;
}

struct VecRecordSource {
    records: Vec<SpillRecord>,
    index: usize,
}

impl VecRecordSource {
    fn new(records: Vec<SpillRecord>) -> Self {
        Self { records, index: 0 }
    }
}

impl PostingRecordSource for VecRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        let record = self.records.get(self.index).copied();
        if record.is_some() {
            self.index += 1;
        }
        Ok(record)
    }
}

struct SpillSegmentSource {
    reader: BufReader<File>,
    remaining_records: u64,
    current_trigram: u32,
    remaining_in_group: u32,
}

impl SpillSegmentSource {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != SPILL_MAGIC {
            return Err(std::io::Error::other("invalid search spill magic"));
        }
        if read_u32(&mut reader)? != INDEX_VERSION {
            return Err(std::io::Error::other("invalid search spill version"));
        }
        let remaining_records = read_u64(&mut reader)?;
        Ok(Self {
            reader,
            remaining_records,
            current_trigram: 0,
            remaining_in_group: 0,
        })
    }
}

impl PostingRecordSource for SpillSegmentSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        if self.remaining_records == 0 {
            return Ok(None);
        }
        if self.remaining_in_group == 0 {
            self.current_trigram = read_u32(&mut self.reader)?;
            self.remaining_in_group = read_u32(&mut self.reader)?;
            if self.remaining_in_group == 0 {
                return Err(std::io::Error::other("empty search spill group"));
            }
        }
        let mut file_id = [0u8; 4];
        self.reader.read_exact(&mut file_id)?;
        let mut masks = [0u8; 2];
        self.reader.read_exact(&mut masks)?;
        self.remaining_in_group -= 1;
        self.remaining_records -= 1;
        Ok(Some(SpillRecord {
            trigram: self.current_trigram,
            file_id: u32::from_le_bytes(file_id),
            next_mask: masks[0],
            loc_mask: masks[1],
        }))
    }
}

struct BaseRecordSource {
    base: Arc<BasePostings>,
    id_map: Arc<HashMap<u32, u32>>,
    superseded: Arc<HashSet<u32>>,
    lookup_index: usize,
    current: Vec<SpillRecord>,
    current_index: usize,
}

impl BaseRecordSource {
    fn new(
        base: Arc<BasePostings>,
        id_map: Arc<HashMap<u32, u32>>,
        superseded: Arc<HashSet<u32>>,
    ) -> Self {
        Self {
            base,
            id_map,
            superseded,
            lookup_index: 0,
            current: Vec::new(),
            current_index: 0,
        }
    }

    fn load_next_group(&mut self) -> std::io::Result<bool> {
        while let Some(entry) = self.base.lookup.get(self.lookup_index).copied() {
            self.lookup_index += 1;
            let postings = self.base.read_postings(entry)?;
            self.current.clear();
            self.current_index = 0;
            for posting in postings {
                if self.superseded.contains(&posting.file_id) {
                    continue;
                }
                let Some(mapped_file_id) = self.id_map.get(&posting.file_id).copied() else {
                    continue;
                };
                self.current.push(SpillRecord {
                    trigram: entry.trigram,
                    file_id: mapped_file_id,
                    next_mask: posting.next_mask,
                    loc_mask: posting.loc_mask,
                });
            }
            if !self.current.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl PostingRecordSource for BaseRecordSource {
    fn next_record(&mut self) -> std::io::Result<Option<SpillRecord>> {
        if self.current_index >= self.current.len() && !self.load_next_group()? {
            return Ok(None);
        }
        let record = self.current[self.current_index];
        self.current_index += 1;
        Ok(Some(record))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeapItem {
    record: SpillRecord,
    source_index: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .record
            .trigram
            .cmp(&self.record.trigram)
            .then_with(|| other.record.file_id.cmp(&self.record.file_id))
            .then_with(|| other.source_index.cmp(&self.source_index))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn build_streaming_index(
    root: &Path,
    max_file_size: u64,
    cache_dir: &Path,
) -> std::io::Result<(SearchIndex, usize)> {
    fs::create_dir_all(cache_dir)?;
    sweep_stale_search_build_dirs(cache_dir);
    let project_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let ignore_fingerprint = ignore_rules_fingerprint(&project_root);
    let filters = PathFilters::default();
    let paths: Vec<PathBuf> = walk_project_files(&project_root, &filters);
    let pool_size = search_index_build_pool_size();
    let chunk_size = pool_size.saturating_mul(4).clamp(1, 32);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(pool_size)
        .thread_name(|index| format!("aft-search-build-{index}"))
        .stack_size(8 * 1024 * 1024)
        .build()
        .ok();

    let spill_dir = create_spill_dir(cache_dir)?;
    let mut spill_paths = Vec::new();
    let mut spill_seq = 0usize;
    let mut block: Vec<SpillRecord> = Vec::new();
    let mut files = Vec::new();
    let mut path_to_id = HashMap::new();
    let mut unindexed_files = HashSet::new();
    let mut file_trigram_count = Vec::new();
    let mut indexed = 0usize;

    let build_result = (|| -> std::io::Result<BasePostings> {
        for chunk in paths.chunks(chunk_size) {
            let prepare_chunk = || -> Vec<PreparedSearchPath> {
                chunk
                    .par_iter()
                    .map(|path| prepare_search_path(path, max_file_size))
                    .collect()
            };
            let prepared = match &pool {
                Some(pool) => pool.install(prepare_chunk),
                None => prepare_chunk(),
            };

            for (path, prepared) in chunk.iter().zip(prepared) {
                match prepared {
                    PreparedSearchPath::Indexed(file) => {
                        let file_id = u32::try_from(files.len())
                            .map_err(|_| std::io::Error::other("too many files to index"))?;
                        files.push(FileEntry {
                            path: path.clone(),
                            size: file.metadata.size,
                            modified: file.metadata.modified,
                            content_hash: file.content_hash,
                        });
                        path_to_id.insert(path.clone(), file_id);
                        file_trigram_count.push(file.trigram_map.len() as u32);
                        for (trigram, filter) in file.trigram_map {
                            block.push(SpillRecord {
                                trigram,
                                file_id,
                                next_mask: filter.next_mask,
                                loc_mask: filter.loc_mask,
                            });
                        }
                        indexed += 1;
                    }
                    PreparedSearchPath::Unindexed(metadata) => {
                        let file_id = u32::try_from(files.len())
                            .map_err(|_| std::io::Error::other("too many files to index"))?;
                        files.push(FileEntry {
                            path: path.clone(),
                            size: metadata.size,
                            modified: metadata.modified,
                            content_hash: cache_freshness::zero_hash(),
                        });
                        path_to_id.insert(path.clone(), file_id);
                        unindexed_files.insert(file_id);
                        file_trigram_count.push(0);
                        indexed += 1;
                    }
                    PreparedSearchPath::Skipped => {}
                }

                let block_bytes = block.len().saturating_mul(SPILL_RECORD_ESTIMATED_BYTES);
                if block_bytes >= SPIMI_SOFT_LIMIT_BYTES || block_bytes >= SPIMI_HARD_LIMIT_BYTES {
                    let path = flush_spill_segment(&spill_dir, spill_seq, &mut block)?;
                    spill_paths.push(path);
                    spill_seq += 1;
                }
            }
        }

        block.sort_unstable_by_key(|record| (record.trigram, record.file_id));
        let mut sources: Vec<Box<dyn PostingRecordSource>> = Vec::new();
        for path in &spill_paths {
            sources.push(Box::new(SpillSegmentSource::open(path)?));
        }
        if !block.is_empty() {
            sources.push(Box::new(VecRecordSource::new(std::mem::take(&mut block))));
        }

        let plan = CacheWritePlan {
            project_root: project_root.clone(),
            git_head: current_git_head(&project_root),
            ignore_fingerprint: ignore_fingerprint.clone(),
            max_file_size,
            files: files.clone(),
            path_to_id: path_to_id.clone(),
            unindexed_files: unindexed_files.clone(),
            file_trigram_count: file_trigram_count.clone(),
            id_map: Arc::new(
                (0..files.len())
                    .filter_map(|id| {
                        let id = u32::try_from(id).ok()?;
                        Some((id, id))
                    })
                    .collect(),
            ),
        };
        write_cache_file_from_sources(cache_dir, &plan, &mut sources)
    })();

    let _ = fs::remove_dir_all(&spill_dir);
    let base = build_result?;
    let base_file_count =
        u32::try_from(files.len()).map_err(|_| std::io::Error::other("too many files to index"))?;
    let git_head = current_git_head(&project_root);
    let index = SearchIndex {
        base: Some(Arc::new(base)),
        delta_postings: HashMap::new(),
        delta_file_trigrams: HashMap::new(),
        files: Arc::new(files),
        path_to_id: Arc::new(path_to_id),
        ready: false,
        project_root,
        git_head,
        max_file_size,
        ignore_rules_fingerprint: ignore_fingerprint,
        file_trigram_count: Arc::new(file_trigram_count),
        unindexed_files: Arc::new(unindexed_files),
        superseded: HashSet::new(),
        base_file_count,
        delta_packed_bytes: 0,
        compaction_state: Arc::new(Mutex::new(CompactionState::default())),
    };
    Ok((index, indexed))
}

fn write_cache_file_from_sources(
    cache_dir: &Path,
    plan: &CacheWritePlan,
    sources: &mut [Box<dyn PostingRecordSource>],
) -> std::io::Result<BasePostings> {
    fs::create_dir_all(cache_dir)?;
    sweep_stale_search_build_dirs(cache_dir);
    let cache_path = cache_dir.join("cache.bin");
    let tmp_cache = cache_dir.join(format!(
        "cache.bin.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));

    let write_result = (|| -> std::io::Result<BasePostings> {
        let raw = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_cache)?;
        let mut writer = BufWriter::new(raw);
        write_u32(&mut writer, CACHE_MAGIC)?;
        write_u32(&mut writer, INDEX_VERSION)?;
        let postings_len_patch = writer.stream_position()?;
        write_u64(&mut writer, 0)?;

        let postings_section_start = writer.stream_position()?;
        let postings_header = build_postings_header_bytes(plan)?;
        writer.write_all(&postings_header)?;
        let postings_blob_len_patch = writer.stream_position()?;
        write_u64(&mut writer, 0)?;
        let postings_blob_start = writer.stream_position()?;

        let (lookup_entries, postings_blob_len) = merge_sources_to_writer(sources, &mut writer)?;
        let extension = build_file_trigram_count_extension(&plan.file_trigram_count)?;
        writer.write_all(&extension)?;
        let postings_crc_end = writer.stream_position()?;

        writer.flush()?;
        writer.seek(SeekFrom::Start(postings_blob_len_patch))?;
        write_u64(&mut writer, postings_blob_len)?;
        writer.flush()?;

        let checksum = crc32_file_range(
            &tmp_cache,
            postings_section_start,
            postings_crc_end.saturating_sub(postings_section_start),
        )?;
        writer.seek(SeekFrom::Start(postings_crc_end))?;
        writer.write_all(&checksum.to_le_bytes())?;
        let postings_section_end = writer.stream_position()?;
        let postings_len_total = postings_section_end.saturating_sub(postings_section_start);
        writer.seek(SeekFrom::Start(postings_len_patch))?;
        write_u64(&mut writer, postings_len_total)?;
        writer.seek(SeekFrom::Start(postings_section_end))?;

        let lookup_blob = build_lookup_section_bytes(&lookup_entries)?;
        writer.write_all(&lookup_blob)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);

        fs::rename(&tmp_cache, &cache_path)?;
        sync_parent_dir(&cache_path);
        let file = open_cache_file_read(&cache_path)?;
        Ok(BasePostings {
            file: Arc::new(file),
            postings_blob_start,
            postings_blob_len,
            lookup: Arc::new(lookup_entries),
        })
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_cache);
    }
    write_result
}

fn merge_sources_to_writer(
    sources: &mut [Box<dyn PostingRecordSource>],
    writer: &mut BufWriter<File>,
) -> std::io::Result<(Vec<LookupEntry>, u64)> {
    let mut heap = BinaryHeap::new();
    for (source_index, source) in sources.iter_mut().enumerate() {
        if let Some(record) = source.next_record()? {
            heap.push(HeapItem {
                record,
                source_index,
            });
        }
    }

    let mut lookup_entries = Vec::new();
    let mut postings_blob_len = 0u64;
    let mut current_trigram: Option<u32> = None;
    let mut current_offset = 0u64;
    let mut current_count = 0u32;

    while let Some(item) = heap.pop() {
        let record = item.record;
        if current_trigram != Some(record.trigram) {
            if let Some(trigram) = current_trigram {
                lookup_entries.push(LookupEntry {
                    trigram,
                    offset: current_offset,
                    count: current_count,
                });
            }
            current_trigram = Some(record.trigram);
            current_offset = postings_blob_len;
            current_count = 0;
        }

        writer.write_all(&record.file_id.to_le_bytes())?;
        writer.write_all(&[record.next_mask, record.loc_mask])?;
        postings_blob_len = postings_blob_len
            .checked_add(POSTING_BYTES as u64)
            .ok_or_else(|| std::io::Error::other("postings blob too large"))?;
        current_count = current_count
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("posting list too large"))?;

        if let Some(next) = sources[item.source_index].next_record()? {
            heap.push(HeapItem {
                record: next,
                source_index: item.source_index,
            });
        }
    }

    if let Some(trigram) = current_trigram {
        lookup_entries.push(LookupEntry {
            trigram,
            offset: current_offset,
            count: current_count,
        });
    }

    Ok((lookup_entries, postings_blob_len))
}

fn build_postings_header_bytes(plan: &CacheWritePlan) -> std::io::Result<Vec<u8>> {
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    writer.write_all(INDEX_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;

    let head = plan.git_head.as_deref().unwrap_or_default();
    let root = plan.project_root.to_string_lossy();
    let head_len = u32::try_from(head.len())
        .map_err(|_| std::io::Error::other("git head too large to cache"))?;
    let root_len = u32::try_from(root.len())
        .map_err(|_| std::io::Error::other("project root too large to cache"))?;
    let ignore_fingerprint_len = u32::try_from(plan.ignore_fingerprint.len())
        .map_err(|_| std::io::Error::other("ignore fingerprint too large to cache"))?;
    let file_count = u32::try_from(plan.files.len())
        .map_err(|_| std::io::Error::other("too many files to cache"))?;

    write_u32(&mut writer, head_len)?;
    write_u32(&mut writer, root_len)?;
    write_u32(&mut writer, ignore_fingerprint_len)?;
    write_u64(&mut writer, plan.max_file_size)?;
    write_u32(&mut writer, file_count)?;
    writer.write_all(head.as_bytes())?;
    writer.write_all(root.as_bytes())?;
    writer.write_all(plan.ignore_fingerprint.as_bytes())?;

    for (file_id, file) in plan.files.iter().enumerate() {
        let file_id =
            u32::try_from(file_id).map_err(|_| std::io::Error::other("too many files to cache"))?;
        let path = cache_relative_path(&plan.project_root, &file.path)
            .or_else(|| {
                fs::canonicalize(&file.path)
                    .ok()
                    .and_then(|canonical| cache_relative_path(&plan.project_root, &canonical))
            })
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "refusing to cache path outside project root: {}",
                    file.path.display()
                ))
            })?;
        let path = path.to_string_lossy();
        let path_len = u32::try_from(path.len())
            .map_err(|_| std::io::Error::other("cached path too large"))?;
        let modified = file
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let unindexed = if plan.unindexed_files.contains(&file_id) {
            1u8
        } else {
            0u8
        };

        writer.write_all(&[unindexed])?;
        write_u32(&mut writer, path_len)?;
        write_u64(&mut writer, file.size)?;
        write_u64(&mut writer, modified.as_secs())?;
        write_u32(&mut writer, modified.subsec_nanos())?;
        writer.write_all(file.content_hash.as_bytes())?;
        writer.write_all(path.as_bytes())?;
    }

    writer.flush()?;
    Ok(writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner())
}

fn build_lookup_section_bytes(lookup_entries: &[LookupEntry]) -> std::io::Result<Vec<u8>> {
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    let entry_count = u32::try_from(lookup_entries.len())
        .map_err(|_| std::io::Error::other("too many lookup entries to cache"))?;
    writer.write_all(LOOKUP_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u32(&mut writer, entry_count)?;
    for entry in lookup_entries {
        write_u32(&mut writer, entry.trigram)?;
        write_u64(&mut writer, entry.offset)?;
        write_u32(&mut writer, entry.count)?;
    }
    writer.flush()?;
    let mut lookup_blob = writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner();
    let checksum = crc32fast::hash(&lookup_blob);
    lookup_blob.extend_from_slice(&checksum.to_le_bytes());
    Ok(lookup_blob)
}

fn build_file_trigram_count_extension(counts: &[u32]) -> std::io::Result<Vec<u8>> {
    let mut writer = BufWriter::new(Cursor::new(Vec::new()));
    writer.write_all(FILE_TRIGRAM_COUNT_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u32(
        &mut writer,
        u32::try_from(counts.len())
            .map_err(|_| std::io::Error::other("too many file trigram counts"))?,
    )?;
    for count in counts {
        write_u32(&mut writer, *count)?;
    }
    writer.flush()?;
    Ok(writer
        .into_inner()
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_inner())
}

fn flush_spill_segment(
    spill_dir: &Path,
    seq: usize,
    block: &mut Vec<SpillRecord>,
) -> std::io::Result<PathBuf> {
    if block.is_empty() {
        return Err(std::io::Error::other(
            "refusing to write empty search spill",
        ));
    }
    block.sort_unstable_by_key(|record| (record.trigram, record.file_id));
    let path = spill_dir.join(format!("segment.{seq:06}.bin"));
    let mut writer = BufWriter::new(File::create(&path)?);
    writer.write_all(SPILL_MAGIC)?;
    write_u32(&mut writer, INDEX_VERSION)?;
    write_u64(
        &mut writer,
        u64::try_from(block.len()).map_err(|_| std::io::Error::other("search spill too large"))?,
    )?;

    let mut index = 0usize;
    while index < block.len() {
        let trigram = block[index].trigram;
        let group_start = index;
        while index < block.len() && block[index].trigram == trigram {
            index += 1;
        }
        write_u32(&mut writer, trigram)?;
        write_u32(
            &mut writer,
            u32::try_from(index - group_start)
                .map_err(|_| std::io::Error::other("search spill group too large"))?,
        )?;
        for record in &block[group_start..index] {
            writer.write_all(&record.file_id.to_le_bytes())?;
            writer.write_all(&[record.next_mask, record.loc_mask])?;
        }
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    block.clear();
    Ok(path)
}

fn create_spill_dir(cache_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = cache_dir.join(format!(
        "search-build.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn sweep_stale_search_build_dirs(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        if file_name.to_string_lossy().starts_with("search-build.tmp.") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn transient_search_cache_dir(root: &Path) -> PathBuf {
    std::env::temp_dir().join(format!(
        "aft-search-cache.{}.{}.{}",
        artifact_cache_key(root),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ))
}

fn read_file_trigram_count_extension(
    base: &BasePostings,
    extension_start: u64,
    postings_body_end: u64,
    file_count: usize,
) -> std::io::Result<Option<Vec<u32>>> {
    if extension_start >= postings_body_end {
        return Ok(None);
    }
    let extension_len = postings_body_end - extension_start;
    if extension_len < 16 {
        return Ok(None);
    }
    let mut header = [0u8; 16];
    pread_exact(&base.file, extension_start, &mut header)?;
    if &header[..8] != FILE_TRIGRAM_COUNT_MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != INDEX_VERSION {
        return Err(std::io::Error::other("invalid file trigram count version"));
    }
    let count = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
    if count != file_count {
        return Err(std::io::Error::other("file trigram count length mismatch"));
    }
    let counts_len = count
        .checked_mul(4)
        .ok_or_else(|| std::io::Error::other("file trigram count extension too large"))?;
    if 16u64 + counts_len as u64 > extension_len {
        return Err(std::io::Error::other(
            "truncated file trigram count extension",
        ));
    }
    let mut bytes = vec![0u8; counts_len];
    pread_exact(&base.file, extension_start + 16, &mut bytes)?;
    let mut counts = Vec::with_capacity(count);
    for chunk in bytes.chunks_exact(4) {
        counts.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(counts))
}

fn compute_file_trigram_counts_from_base(
    base: &BasePostings,
    file_count: usize,
) -> std::io::Result<Vec<u32>> {
    let mut counts = vec![0u32; file_count];
    for entry in base.lookup.iter().copied() {
        for posting in base.read_postings(entry)? {
            let Some(count) = counts.get_mut(posting.file_id as usize) else {
                return Err(std::io::Error::other("posting references missing file"));
            };
            *count = count.saturating_add(1);
        }
    }
    Ok(counts)
}

fn ensure_count_slot(counts: &mut Vec<u32>, file_id: u32) {
    let len = file_id as usize + 1;
    if counts.len() < len {
        counts.resize(len, 0);
    }
}

fn reader_has_remaining<R: Seek>(
    reader: &mut R,
    absolute_end: u64,
    len: usize,
) -> std::io::Result<bool> {
    let position = reader.stream_position()?;
    Ok(position <= absolute_end && (len as u64) <= absolute_end - position)
}

fn crc32_file_range(path: &Path, start: u64, len: u64) -> std::io::Result<u32> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let read_len = buffer.len().min(remaining as usize);
        let bytes_read = file.read(&mut buffer[..read_len])?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated cache while checksumming",
            ));
        }
        hasher.update(&buffer[..bytes_read]);
        remaining -= bytes_read as u64;
    }
    Ok(hasher.finalize())
}

fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

fn open_cache_file_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    options.open(path)
}

#[cfg(unix)]
fn pread_exact(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buffer.is_empty() {
        let bytes_read = file.read_at(buffer, offset)?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short pread from search cache",
            ));
        }
        offset += bytes_read as u64;
        let (_, rest) = buffer.split_at_mut(bytes_read);
        buffer = rest;
    }
    Ok(())
}

#[cfg(windows)]
fn pread_exact(file: &File, mut offset: u64, mut buffer: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buffer.is_empty() {
        let bytes_read = file.seek_read(buffer, offset)?;
        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short pread from search cache",
            ));
        }
        offset += bytes_read as u64;
        let (_, rest) = buffer.split_at_mut(bytes_read);
        buffer = rest;
    }
    Ok(())
}

fn intersect_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged
}

fn union_sorted_ids(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                merged.push(left[left_index]);
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(right[right_index]);
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    merged.extend_from_slice(&left[left_index..]);
    merged.extend_from_slice(&right[right_index..]);
    merged
}

pub fn decompose_regex(pattern: &str) -> RegexQuery {
    let hir = match regex_syntax::parse(pattern) {
        Ok(hir) => hir,
        Err(_) => return RegexQuery::default(),
    };

    let build = build_query(&hir);
    build.into_query()
}

pub fn pack_trigram(a: u8, b: u8, c: u8) -> u32 {
    ((a as u32) << 16) | ((b as u32) << 8) | c as u32
}

pub fn normalize_char(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

fn scan_trigrams(content: &[u8], mut visit: impl FnMut(u32, u8, usize)) {
    if content.len() < 3 {
        return;
    }

    for start in 0..=content.len() - 3 {
        let trigram = pack_trigram(
            normalize_char(content[start]),
            normalize_char(content[start + 1]),
            normalize_char(content[start + 2]),
        );
        let next_char = content.get(start + 3).copied().unwrap_or(EOF_SENTINEL);
        visit(trigram, next_char, start);
    }
}

pub fn extract_trigrams(content: &[u8]) -> Vec<(u32, u8, usize)> {
    let mut trigrams = Vec::with_capacity(content.len().saturating_sub(2));
    scan_trigrams(content, |trigram, next_char, position| {
        trigrams.push((trigram, next_char, position));
    });
    trigrams
}

fn trigram_filter_map(content: &[u8], include_eof_next_char: bool) -> BTreeMap<u32, PostingFilter> {
    let mut filters: BTreeMap<u32, PostingFilter> = BTreeMap::new();
    scan_trigrams(content, |trigram, next_char, position| {
        let entry = filters.entry(trigram).or_default();
        if include_eof_next_char || next_char != EOF_SENTINEL {
            entry.next_mask |= mask_for_next_char(next_char);
        }
        entry.loc_mask |= mask_for_position(position);
    });
    filters
}

pub fn query_trigrams_from_tokens(tokens: &[&str]) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for token in tokens {
        scan_trigrams(token.as_bytes(), |trigram, _, _| {
            if seen.insert(trigram) {
                out.push(trigram);
            }
        });
    }
    out
}

pub fn lexical_score(index: &SearchIndex, query_trigrams: &[u32], file_id: u32) -> f32 {
    lexical_score_snapshot(&index.snapshot(), query_trigrams, file_id)
}

fn lexical_score_snapshot(
    index: &SearchIndexSnapshot,
    query_trigrams: &[u32],
    file_id: u32,
) -> f32 {
    if query_trigrams.is_empty() {
        return 0.0;
    }

    let mut hits = 0u32;
    for &trigram in query_trigrams {
        let postings = index.postings_for_trigram(trigram, None);
        if postings.binary_search(&file_id).is_ok() {
            hits += 1;
        }
    }

    if hits == 0 {
        return 0.0;
    }

    let file_trigram_count = index
        .file_trigram_count
        .get(file_id as usize)
        .copied()
        .unwrap_or(1)
        .max(1) as f32;
    (hits as f32) / (1.0 + file_trigram_count.ln())
}

pub fn resolve_cache_dir(project_root: &Path, storage_dir: Option<&Path>) -> PathBuf {
    resolve_cache_dir_with_key(&artifact_cache_key(project_root), storage_dir)
}

pub(crate) fn build_path_filters(
    include: &[String],
    exclude: &[String],
) -> Result<PathFilters, String> {
    Ok(PathFilters {
        includes: build_globset(include)?,
        excludes: build_globset(exclude)?,
    })
}

pub(crate) fn walk_project_files(root: &Path, filters: &PathFilters) -> Vec<PathBuf> {
    walk_project_files_from(root, root, filters)
}

pub fn walk_project_files_bounded_default(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner(root, root, &PathFilters::default(), Some(max_files), true)
}

pub(crate) fn walk_project_files_bounded_matching<F>(
    root: &Path,
    filters: &PathFilters,
    max_files: usize,
    matches_file: F,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(root, root, filters, Some(max_files), matches_file, true)
}

pub fn walk_project_files_bounded_default_matching<F>(
    root: &Path,
    max_files: usize,
    matches_file: F,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    walk_project_files_from_inner_matching(
        root,
        root,
        &PathFilters::default(),
        Some(max_files),
        matches_file,
        true,
    )
}

pub(crate) fn walk_project_files_from(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> Vec<PathBuf> {
    walk_project_files_from_inner(filter_root, search_root, filters, None, true)
        .expect("unbounded project walk cannot exceed a file limit")
}

pub(crate) fn has_any_project_file_from(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
) -> bool {
    walk_project_files_from_inner(filter_root, search_root, filters, Some(0), true).is_err()
}

fn walk_project_files_from_inner(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize> {
    walk_project_files_from_inner_matching(
        filter_root,
        search_root,
        filters,
        max_files,
        |_| true,
        sort_by_mtime,
    )
}

fn project_walk_builder(search_root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(search_root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                return !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | ".git"
                        | "__pycache__"
                        | ".tox"
                        | "dist"
                        | "build"
                );
            }
            true
        });
    builder
}

fn walk_project_files_from_inner_matching<F>(
    filter_root: &Path,
    search_root: &Path,
    filters: &PathFilters,
    max_files: Option<usize>,
    matches_file: F,
    sort_by_mtime: bool,
) -> Result<Vec<PathBuf>, usize>
where
    F: Fn(&Path) -> bool,
{
    let builder = project_walk_builder(search_root);

    let mut files = Vec::new();
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if filters.matches(filter_root, &path) && matches_file(&path) {
            files.push(path);
            if max_files.is_some_and(|limit| files.len() > limit) {
                return Err(files.len());
            }
        }
    }

    if sort_by_mtime {
        sort_paths_by_mtime_desc(&mut files);
    }
    Ok(files)
}

pub(crate) fn read_searchable_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if is_binary_bytes(&bytes) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn read_indexed_file_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

pub(crate) fn relative_to_root(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn cache_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let normalized_root = normalize_path(root);
    let normalized_path = normalize_path(path);
    let relative = normalized_path.strip_prefix(&normalized_root).ok()?;
    validate_cached_relative_path(relative)
}

pub(crate) fn cached_path_under_root(root: &Path, relative_path: &Path) -> Option<PathBuf> {
    let relative = validate_cached_relative_path(relative_path)?;
    let normalized_root = normalize_path(root);
    let full_path = normalize_path(&normalized_root.join(relative));

    match fs::canonicalize(&full_path) {
        Ok(canonical_path) => {
            if canonical_path.starts_with(&normalized_root) {
                return Some(full_path);
            }

            let canonical_root = fs::canonicalize(&normalized_root).ok()?;
            canonical_path
                .starts_with(&canonical_root)
                .then_some(full_path)
        }
        Err(_) => full_path.starts_with(&normalized_root).then_some(full_path),
    }
}

pub(crate) fn validate_cached_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// Sort paths newest-first by mtime, falling back to normalized display-path order.
///
/// Pre-v0.15.2 this called `path_modified_time(...)` directly inside the
/// `sort_by()` closure. That made the comparator non-deterministic — a
/// `stat()` syscall for the same path can return different values across
/// invocations (file edited mid-sort, file deleted, OS clock adjustments,
/// concurrent file-watcher activity), and Rust's slice::sort panics at
/// runtime when it detects a non-total-order comparator. CI hit this on
/// a Pi e2e test where the bridge invalidated files in parallel with grep.
///
/// Fix: snapshot mtimes ONCE into a HashMap before sorting, then look up
/// from the map inside the closure. Pure function ⇒ guaranteed total order.
pub(crate) fn sort_paths_by_mtime_desc(paths: &mut [PathBuf]) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(paths.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(paths.len());
    for path in paths.iter() {
        mtimes
            .entry(path.clone())
            .or_insert_with(|| path_modified_time(path));
        display_paths
            .entry(path.clone())
            .or_insert_with(|| normalized_display_sort_key(None, path));
    }
    paths.sort_by(|left, right| {
        let left_mtime = mtimes.get(left).and_then(|v| *v);
        let right_mtime = mtimes.get(right).and_then(|v| *v);
        let left_display = display_paths
            .get(left)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right)
            .map(String::as_bytes)
            .unwrap_or_default();
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. Same fix, applied to grep matches that share files.
pub(crate) fn sort_grep_matches_by_mtime_desc(matches: &mut [GrepMatch], project_root: &Path) {
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::new();
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        mtimes.entry(m.file.clone()).or_insert_with(|| {
            let resolved = resolve_match_path(project_root, &m.file);
            path_modified_time(&resolved)
        });
        display_paths
            .entry(m.file.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &m.file));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(&left.file).and_then(|v| *v);
        let right_mtime = mtimes.get(&right.file).and_then(|v| *v);
        let left_display = display_paths
            .get(&left.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(&right.file)
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

/// See `sort_paths_by_mtime_desc` for why mtimes are snapshotted ahead of
/// the sort. The cached lookup function `modified_for_path` is fast (in-memory
/// table from the search index), but it can still return different values if
/// the file is modified mid-sort. Snapshot once.
fn sort_shared_grep_matches_by_cached_mtime_desc<F>(
    matches: &mut [SharedGrepMatch],
    project_root: &Path,
    modified_for_path: F,
) where
    F: Fn(&Path) -> Option<SystemTime>,
{
    use std::collections::HashMap;
    let mut mtimes: HashMap<PathBuf, Option<SystemTime>> = HashMap::with_capacity(matches.len());
    let mut display_paths: HashMap<PathBuf, String> = HashMap::with_capacity(matches.len());
    for m in matches.iter() {
        let path = m.file.as_path().to_path_buf();
        mtimes
            .entry(path.clone())
            .or_insert_with(|| modified_for_path(&path));
        display_paths
            .entry(path.clone())
            .or_insert_with(|| normalized_display_sort_key(Some(project_root), &path));
    }
    matches.sort_by(|left, right| {
        let left_mtime = mtimes.get(left.file.as_path()).and_then(|v| *v);
        let right_mtime = mtimes.get(right.file.as_path()).and_then(|v| *v);
        let left_display = display_paths
            .get(left.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        let right_display = display_paths
            .get(right.file.as_path())
            .map(String::as_bytes)
            .unwrap_or_default();
        // The display-path tiebreak makes complete result sets deterministic.
        // If a parallel grep stops early after hitting a cap, the capped subset
        // can still depend on which worker reaches the cap first.
        right_mtime
            .cmp(&left_mtime)
            .then_with(|| left_display.cmp(right_display))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.column.cmp(&right.column))
    });
}

pub(crate) fn resolve_search_scope(project_root: &Path, path: Option<&str>) -> SearchScope {
    let resolved_project_root = canonicalize_or_normalize(project_root);
    let root = match path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                canonicalize_or_normalize(&path)
            } else {
                normalize_path(&resolved_project_root.join(path))
            }
        }
        None => resolved_project_root.clone(),
    };

    let use_index = is_within_search_root(&resolved_project_root, &root);
    SearchScope { root, use_index }
}

pub(crate) fn is_binary_bytes(content: &[u8]) -> bool {
    content_inspector::inspect(content).is_binary()
}

pub(crate) fn current_git_head(root: &Path) -> Option<String> {
    run_git(root, &["rev-parse", "HEAD"])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactCacheKeyProbeError {
    root: PathBuf,
    detail: String,
}

impl ArtifactCacheKeyProbeError {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for ArtifactCacheKeyProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "artifact cache key probe failed for {}: {}",
            self.root.display(),
            self.detail
        )
    }
}

impl std::error::Error for ArtifactCacheKeyProbeError {}

pub fn artifact_cache_key(project_root: &Path) -> String {
    match repo_root_commit_with_retry(project_root) {
        RootCommitResolution::Commit(root_commit) => artifact_key_from_git_identity(&root_commit),
        RootCommitResolution::NotARepo => artifact_key_from_path_identity(project_root),
        RootCommitResolution::Failed(detail) => {
            crate::slog_warn!(
                "artifact cache key: git root-commit probe failed after retries ({}); \
                 falling back to path identity for {}",
                detail,
                project_root.display()
            );
            artifact_key_from_path_identity(project_root)
        }
    }
}

pub fn artifact_cache_key_with_memo(
    probe_root: &Path,
    memo_root: &Path,
    storage_root: &Path,
    git_common_dir: Option<&Path>,
) -> Result<String, ArtifactCacheKeyProbeError> {
    let memo_root_key = artifact_cache_key_memo_root_key(memo_root);
    let git_marker_state = root_git_marker_state(probe_root, git_common_dir);
    if git_marker_state == GitMarkerState::Absent {
        return Ok(artifact_key_from_path_identity(probe_root));
    }

    match repo_root_commit_with_retry(probe_root) {
        RootCommitResolution::Commit(root_commit) => {
            let key = artifact_key_from_git_identity(&root_commit);
            if let Err(error) =
                record_artifact_cache_key_memo(storage_root, &memo_root_key, &key, &root_commit)
            {
                crate::slog_warn!(
                    "artifact cache key: failed to persist memo for {} in {}: {}",
                    memo_root.display(),
                    storage_root.display(),
                    error
                );
            }
            Ok(key)
        }
        RootCommitResolution::NotARepo => Ok(artifact_key_from_path_identity(probe_root)),
        RootCommitResolution::Failed(detail) => {
            if let Some(entry) = lookup_artifact_cache_key_memo(storage_root, &memo_root_key) {
                crate::slog_warn!(
                    "artifact cache key: probe failed, using memoized key {} for {}",
                    entry.key,
                    memo_root.display()
                );
                return Ok(entry.key);
            }

            match git_marker_state {
                GitMarkerState::Absent => Ok(artifact_key_from_path_identity(probe_root)),
                GitMarkerState::Present => Err(ArtifactCacheKeyProbeError {
                    root: memo_root.to_path_buf(),
                    detail,
                }),
                GitMarkerState::Unknown(marker_detail) => Err(ArtifactCacheKeyProbeError {
                    root: memo_root.to_path_buf(),
                    detail: format!("{detail}; {marker_detail}"),
                }),
            }
        }
    }
}

pub fn resolve_cache_dir_with_key(project_key: &str, storage_dir: Option<&Path>) -> PathBuf {
    if let Some(override_dir) = std::env::var_os("AFT_CACHE_DIR") {
        return PathBuf::from(override_dir).join("index").join(project_key);
    }
    if let Some(dir) = storage_dir {
        return dir.join("index").join(project_key);
    }
    crate::bash_background::storage_dir(None)
        .join("index")
        .join(project_key)
}

fn artifact_key_from_git_identity(root_commit: &str) -> String {
    artifact_hash16(root_commit.as_bytes())
}

fn artifact_key_from_path_identity(project_root: &Path) -> String {
    let canonical_root = canonicalize_or_normalize(project_root);
    artifact_hash16(canonical_root.to_string_lossy().as_bytes())
}

#[cfg(test)]
pub(crate) fn artifact_path_identity_key_for_test(project_root: &Path) -> String {
    artifact_key_from_path_identity(project_root)
}

fn artifact_hash16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

fn artifact_cache_key_memo_root_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

fn artifact_cache_key_memo_path(storage_root: &Path) -> PathBuf {
    storage_root.join(ARTIFACT_CACHE_KEY_MEMO_FILE)
}

fn artifact_cache_key_memo_state() -> &'static Mutex<ArtifactCacheKeyMemoState> {
    ARTIFACT_CACHE_KEY_MEMO_STATE.get_or_init(|| Mutex::new(ArtifactCacheKeyMemoState::default()))
}

impl ArtifactCacheKeyMemoState {
    fn entries_for_storage_root(
        &mut self,
        storage_root: &Path,
    ) -> &mut BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        if !self.by_storage_root.contains_key(storage_root) {
            let entries = read_artifact_cache_key_memo_file(storage_root);
            self.by_storage_root
                .insert(storage_root.to_path_buf(), entries);
        }
        self.by_storage_root
            .get_mut(storage_root)
            .expect("memo storage root inserted")
    }
}

fn lookup_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
) -> Option<ArtifactCacheKeyMemoEntry> {
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .entries_for_storage_root(storage_root)
        .get(memo_root_key)
        .cloned()
}

fn record_artifact_cache_key_memo(
    storage_root: &Path,
    memo_root_key: &str,
    key: &str,
    git_root_commit: &str,
) -> std::io::Result<()> {
    let mut state = artifact_cache_key_memo_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entries = state.entries_for_storage_root(storage_root);
    if entries
        .get(memo_root_key)
        .is_some_and(|entry| entry.key == key && entry.git_root_commit == git_root_commit)
    {
        return Ok(());
    }
    entries.insert(
        memo_root_key.to_string(),
        ArtifactCacheKeyMemoEntry {
            key: key.to_string(),
            git_root_commit: git_root_commit.to_string(),
            recorded_at_ms: current_time_millis(),
        },
    );
    write_artifact_cache_key_memo_file(storage_root, entries)
}

fn read_artifact_cache_key_memo_file(
    storage_root: &Path,
) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
    let path = artifact_cache_key_memo_path(storage_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return BTreeMap::new(),
    };
    let entries =
        match serde_json::from_slice::<BTreeMap<String, ArtifactCacheKeyMemoEntry>>(&bytes) {
            Ok(entries) => entries,
            Err(error) => {
                crate::slog_warn!(
                    "artifact cache key: ignoring corrupt memo file {}: {}",
                    path.display(),
                    error
                );
                return BTreeMap::new();
            }
        };
    entries
        .into_iter()
        .filter(|(root, entry)| {
            !root.is_empty()
                && artifact_key_looks_valid(&entry.key)
                && !entry.git_root_commit.trim().is_empty()
        })
        .collect()
}

fn write_artifact_cache_key_memo_file(
    storage_root: &Path,
    entries: &BTreeMap<String, ArtifactCacheKeyMemoEntry>,
) -> std::io::Result<()> {
    fs::create_dir_all(storage_root)?;
    let path = artifact_cache_key_memo_path(storage_root);
    let temp_path = storage_root.join(format!(
        ".{ARTIFACT_CACHE_KEY_MEMO_FILE}.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    let bytes = serde_json::to_vec_pretty(entries).map_err(std::io::Error::other)?;
    {
        let mut file = File::create(&temp_path)?;
        file.write_all(&bytes)?;
    }
    if let Err(error) = fs::rename(&temp_path, &path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn artifact_key_looks_valid(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, PartialEq, Eq)]
enum GitMarkerState {
    Present,
    Absent,
    Unknown(String),
}

fn root_git_marker_state(project_root: &Path, git_common_dir: Option<&Path>) -> GitMarkerState {
    if git_common_dir.is_some() {
        return GitMarkerState::Present;
    }
    let git_marker = project_root.join(".git");
    match fs::symlink_metadata(&git_marker) {
        Ok(_) => GitMarkerState::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GitMarkerState::Absent,
        Err(error) => GitMarkerState::Unknown(format!(
            "failed to inspect git marker {}: {}",
            git_marker.display(),
            error
        )),
    }
}

/// Resolve the repository root commit, retrying transient git failures.
///
/// The distinction matters because the fallback is not benign: two clones of
/// one repo that key differently (one by commit, one by path) each claim
/// artifact ownership and write the shared cache concurrently. A git
/// invocation that fails under load (spawn failure, resource exhaustion) must
/// therefore be retried, and callers that need stable identity can refuse path
/// fallback when the result is still ambiguous after retry.
fn repo_root_commit_with_retry(project_root: &Path) -> RootCommitResolution {
    for attempt in 0..3u32 {
        match git_root_commit_once(project_root) {
            RootCommitProbe::Commit(commit) => return RootCommitResolution::Commit(commit),
            RootCommitProbe::NotARepo => return RootCommitResolution::NotARepo,
            RootCommitProbe::NoCommit => return RootCommitResolution::NotARepo,
            RootCommitProbe::Transient(detail) => {
                if attempt == 2 {
                    return RootCommitResolution::Failed(detail);
                }
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt as u64 + 1)));
            }
        }
    }
    RootCommitResolution::Failed("git root-commit probe retry loop exhausted".to_string())
}

enum RootCommitResolution {
    Commit(String),
    NotARepo,
    Failed(String),
}

enum RootCommitProbe {
    Commit(String),
    /// Deterministic: not a git work tree.
    NotARepo,
    /// Deterministic but still git-like: a repository exists but has no commit identity yet.
    NoCommit,
    /// Ambiguous failure (spawn error, killed, unexpected git error): retry.
    Transient(String),
}

fn git_root_commit_once(project_root: &Path) -> RootCommitProbe {
    #[cfg(test)]
    if let Some(override_probe) = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        if let Some(result) = override_probe(project_root) {
            return result;
        }
    }

    git_root_commit_once_real(project_root)
}

fn git_root_commit_once_real(project_root: &Path) -> RootCommitProbe {
    let output = match Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
    {
        Ok(output) => output,
        Err(error) => return RootCommitProbe::Transient(format!("spawn failed: {error}")),
    };

    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            return RootCommitProbe::NoCommit;
        }
        return RootCommitProbe::Commit(value);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not a git repository") {
        return RootCommitProbe::NotARepo;
    }
    if stderr.contains("unknown revision")
        || stderr.contains("bad revision")
        || stderr.contains("ambiguous argument 'HEAD'")
    {
        return RootCommitProbe::NoCommit;
    }
    RootCommitProbe::Transient(format!(
        "exit {:?}: {}",
        output.status.code(),
        stderr.trim().chars().take(200).collect::<String>()
    ))
}

#[cfg(test)]
pub(crate) struct GitRootCommitProbeOverrideGuard {
    previous: Option<RootCommitProbeOverride>,
}

#[cfg(test)]
impl Drop for GitRootCommitProbeOverrideGuard {
    fn drop(&mut self) {
        set_git_root_commit_probe_override_for_test(self.previous.take());
    }
}

#[cfg(test)]
pub(crate) fn git_root_commit_probe_override_lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_transient_for_paths_for_test(
    roots: Vec<PathBuf>,
    detail: impl Into<String>,
) -> GitRootCommitProbeOverrideGuard {
    let detail = Arc::new(detail.into());
    install_git_root_commit_probe_override_for_test(move |project_root| {
        roots
            .iter()
            .any(|root| root == project_root)
            .then(|| RootCommitProbe::Transient((*detail).clone()))
    })
}

#[cfg(test)]
pub(crate) fn force_git_root_commit_probe_commits_for_test(
    commits_by_root: BTreeMap<PathBuf, String>,
) -> GitRootCommitProbeOverrideGuard {
    install_git_root_commit_probe_override_for_test(move |project_root| {
        commits_by_root
            .get(project_root)
            .cloned()
            .map(RootCommitProbe::Commit)
    })
}

#[cfg(test)]
fn install_git_root_commit_probe_override_for_test(
    override_probe: impl Fn(&Path) -> Option<RootCommitProbe> + Send + Sync + 'static,
) -> GitRootCommitProbeOverrideGuard {
    let previous = set_git_root_commit_probe_override_for_test(Some(Arc::new(override_probe)));
    GitRootCommitProbeOverrideGuard { previous }
}

#[cfg(test)]
fn set_git_root_commit_probe_override_for_test(
    override_probe: Option<RootCommitProbeOverride>,
) -> Option<RootCommitProbeOverride> {
    let mut slot = GIT_ROOT_COMMIT_PROBE_OVERRIDE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::replace(&mut *slot, override_probe)
}

/// Fingerprint corpus-shaping ignore rules that are not represented by git HEAD.
///
/// The search cache stores this value next to the file mtimes. If `.gitignore`,
/// `.aftignore`, or `.git/info/exclude` changes while AFT is not running, a
/// matching HEAD + matching file mtimes is not enough to safely reuse the old
/// cache: files that are now ignored may still be indexed. Hashing the ignore
/// files themselves makes cold-start cache reuse agree with the current walker.
pub fn ignore_rules_fingerprint(project_root: &Path) -> String {
    use sha2::{Digest, Sha256};

    let root = canonicalize_or_normalize(project_root);
    let mut files = Vec::new();
    collect_ignore_rule_files(&root, &mut files);
    if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
        if global_ignore.is_file() {
            files.push(global_ignore);
        }
    }
    let info_exclude = git_info_exclude_path(&root);
    if info_exclude.is_file() {
        files.push(info_exclude);
    }
    files.sort();
    files.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"aft-ignore-rules-v1\0");
    for path in files {
        if let Some(relative) = cache_relative_path(&root, &path) {
            hasher.update(relative.to_string_lossy().as_bytes());
        } else {
            hasher.update(path.to_string_lossy().as_bytes());
        }
        hasher.update(b"\0");
        match fs::read(&path) {
            Ok(bytes) => hasher.update(&bytes),
            Err(error) => hasher.update(format!("read-error:{error}").as_bytes()),
        }
        hasher.update(b"\0");
    }

    format!("{:x}", hasher.finalize())
}

fn git_info_exclude_path(root: &Path) -> PathBuf {
    run_git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from)
    .unwrap_or_else(|| root.join(".git"))
    .join("info")
    .join("exclude")
}

fn collect_ignore_rule_files(root: &Path, files: &mut Vec<PathBuf>) {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                return !matches!(
                    name.as_ref(),
                    ".git"
                        | "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | "__pycache__"
                        | ".tox"
                        | "dist"
                        | "build"
                );
            }
            true
        });

    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if !entry
            .file_type()
            .map_or(false, |file_type| file_type.is_file())
        {
            continue;
        }
        let file_name = entry.file_name();
        if file_name == ".gitignore" || file_name == ".aftignore" {
            files.push(entry.into_path());
        }
    }
}

/// Count directories visited when discovering ignore rule files (for perf regression tests).
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs(root: &Path) -> usize {
    let mut dirs = 0usize;
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore");
    for entry in builder.build().filter_map(|entry| entry.ok()) {
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            dirs += 1;
        }
    }
    dirs
}

/// Legacy stack-based discovery (pre ignore-walker fix); used only in perf tests.
#[cfg(test)]
pub(crate) fn count_ignore_rule_discovery_dirs_legacy_stack(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut dirs = 0usize;
    while let Some(dir) = stack.pop() {
        dirs += 1;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            if file_name == ".gitignore" || file_name == ".aftignore" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            if matches!(
                file_name.to_str().unwrap_or(""),
                ".git"
                    | "node_modules"
                    | "target"
                    | "venv"
                    | ".venv"
                    | "__pycache__"
                    | ".tox"
                    | "dist"
                    | "build"
            ) {
                continue;
            }
            stack.push(path);
        }
    }
    dirs
}

impl PathFilters {
    pub(crate) fn matches(&self, root: &Path, path: &Path) -> bool {
        let relative = to_glob_path(&relative_to_root(root, path));
        if self
            .includes
            .as_ref()
            .is_some_and(|includes| !includes.is_match(&relative))
        {
            return false;
        }
        if self
            .excludes
            .as_ref()
            .is_some_and(|excludes| excludes.is_match(&relative))
        {
            return false;
        }
        true
    }
}

fn canonicalize_or_normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

fn resolve_match_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn path_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn normalized_display_sort_key(project_root: Option<&Path>, path: &Path) -> String {
    let display_path = project_root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    to_glob_path(display_path)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    result.push(component);
                }
            }
            Component::CurDir => {}
            _ => result.push(component),
        }
    }
    result
}

fn canonicalize_existing_or_deleted_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }

    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };

    fs::canonicalize(parent)
        .map(|canonical_parent| canonical_parent.join(file_name))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Verify stored file mtimes against disk. Re-index any files whose mtime changed
/// since the index was last written. Also detect new files and deleted files.
fn verify_file_mtimes(index: &mut SearchIndex) {
    let filters = PathFilters::default();
    let current_files = walk_project_files(&index.project_root, &filters);
    let current_file_set: HashSet<PathBuf> = current_files.iter().cloned().collect();
    let mut stale_paths = Vec::new();
    let mut removed_paths = Vec::new();

    for entry in Arc::make_mut(&mut index.files).iter_mut() {
        if entry.path.as_os_str().is_empty() {
            continue; // tombstoned entry
        }
        if !current_file_set.contains(&entry.path) {
            removed_paths.push(entry.path.clone());
            continue;
        }
        let cached = FileFreshness {
            mtime: entry.modified,
            size: entry.size,
            content_hash: entry.content_hash,
        };
        match cache_freshness::verify_file_strict(&entry.path, &cached) {
            FreshnessVerdict::HotFresh => {}
            FreshnessVerdict::ContentFresh {
                new_mtime,
                new_size,
            } => {
                entry.modified = new_mtime;
                entry.size = new_size;
            }
            FreshnessVerdict::Stale | FreshnessVerdict::Deleted => {
                stale_paths.push(entry.path.clone())
            }
        }
    }

    for path in &removed_paths {
        index.remove_file(path);
    }

    // Re-index stale files that are still in the current walk set. If an ignore
    // rule changed while AFT was down but the fingerprint missed it, this keeps
    // warm-cache verification from resurrecting now-ignored cached entries.
    for path in &stale_paths {
        if current_file_set.contains(path) {
            index.update_file(path);
        } else {
            index.remove_file(path);
        }
    }

    // Detect new files not in the index
    for path in current_files {
        if !index.path_to_id.contains_key(&path) {
            index.update_file(&path);
        }
    }

    if !stale_paths.is_empty() {
        crate::slog_info!(
            "search index: refreshed {} stale file(s) from disk cache",
            stale_paths.len()
        );
    }
}

fn is_within_search_root(search_root: &Path, path: &Path) -> bool {
    normalize_path(path).starts_with(normalize_path(search_root))
}

impl QueryBuild {
    fn into_query(self) -> RegexQuery {
        let mut query = RegexQuery::default();

        for run in self.and_runs {
            add_run_to_and_query(&mut query, &run);
        }

        for group in self.or_groups {
            let mut trigrams = BTreeSet::new();
            let mut filters = HashMap::new();
            for run in group {
                for (trigram, filter) in trigram_filters(&run) {
                    trigrams.insert(trigram);
                    merge_filter(filters.entry(trigram).or_default(), filter);
                }
            }
            if !trigrams.is_empty() {
                query.or_groups.push(trigrams.into_iter().collect());
                query.or_filters.push(filters);
            }
        }

        query
    }
}

fn build_query(hir: &Hir) -> QueryBuild {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                QueryBuild {
                    and_runs: vec![literal.0.to_vec()],
                    or_groups: Vec::new(),
                }
            } else {
                QueryBuild::default()
            }
        }
        HirKind::Capture(capture) => build_query(&capture.sub),
        HirKind::Concat(parts) => {
            let mut build = QueryBuild::default();
            for part in parts {
                let part_build = build_query(part);
                build.and_runs.extend(part_build.and_runs);
                build.or_groups.extend(part_build.or_groups);
            }
            build
        }
        HirKind::Alternation(parts) => {
            let mut group = Vec::new();
            for part in parts {
                let Some(mut choices) = guaranteed_run_choices(part) else {
                    return QueryBuild::default();
                };
                group.append(&mut choices);
            }
            if group.is_empty() {
                QueryBuild::default()
            } else {
                QueryBuild {
                    and_runs: Vec::new(),
                    or_groups: vec![group],
                }
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                QueryBuild::default()
            } else {
                build_query(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => QueryBuild::default(),
    }
}

fn guaranteed_run_choices(hir: &Hir) -> Option<Vec<Vec<u8>>> {
    match hir.kind() {
        HirKind::Literal(literal) => {
            if literal.0.len() >= 3 {
                Some(vec![literal.0.to_vec()])
            } else {
                None
            }
        }
        HirKind::Capture(capture) => guaranteed_run_choices(&capture.sub),
        HirKind::Concat(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                if let Some(mut part_runs) = guaranteed_run_choices(part) {
                    runs.append(&mut part_runs);
                }
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Alternation(parts) => {
            let mut runs = Vec::new();
            for part in parts {
                let Some(mut part_runs) = guaranteed_run_choices(part) else {
                    return None;
                };
                runs.append(&mut part_runs);
            }
            if runs.is_empty() {
                None
            } else {
                Some(runs)
            }
        }
        HirKind::Repetition(repetition) => {
            if repetition.min == 0 {
                None
            } else {
                guaranteed_run_choices(&repetition.sub)
            }
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => None,
    }
}

fn add_run_to_and_query(query: &mut RegexQuery, run: &[u8]) {
    for (trigram, filter) in trigram_filters(run) {
        if !query.and_trigrams.contains(&trigram) {
            query.and_trigrams.push(trigram);
        }
        merge_filter(query.and_filters.entry(trigram).or_default(), filter);
    }
}

fn trigram_filters(run: &[u8]) -> Vec<(u32, PostingFilter)> {
    trigram_filter_map(run, false).into_iter().collect()
}

fn merge_filter(target: &mut PostingFilter, filter: PostingFilter) {
    target.next_mask |= filter.next_mask;
    target.loc_mask |= filter.loc_mask;
}

fn mask_for_next_char(next_char: u8) -> u8 {
    let bit = (normalize_char(next_char).wrapping_mul(31) & 7) as u32;
    1u8 << bit
}

fn mask_for_position(position: usize) -> u8 {
    1u8 << (position % 8)
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| error.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buffer = [0u8; 4];
    reader.read_exact(&mut buffer)?;
    Ok(u32::from_le_bytes(buffer))
}

fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut buffer = [0u8; 8];
    reader.read_exact(&mut buffer)?;
    Ok(u64::from_le_bytes(buffer))
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn verify_crc32_bytes_slice(bytes: &[u8]) -> std::io::Result<()> {
    let Some((body, stored)) = bytes.split_last_chunk::<4>() else {
        return Err(std::io::Error::other("search index checksum missing"));
    };
    let expected = u32::from_le_bytes(*stored);
    let actual = crc32fast::hash(body);
    if actual != expected {
        return Err(std::io::Error::other("search index checksum mismatch"));
    }
    Ok(())
}

fn remaining_bytes<R: Seek>(reader: &mut R, total_len: usize) -> Option<usize> {
    let pos = usize::try_from(reader.stream_position().ok()?).ok()?;
    total_len.checked_sub(pos)
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn apply_git_diff_updates(index: &mut SearchIndex, root: &Path, from: &str, to: &str) -> bool {
    let diff_range = format!("{}..{}", from, to);
    let output = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-status", "-M", &diff_range])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };

    if !output.status.success() {
        return false;
    }

    let Ok(diff) = String::from_utf8(output.stdout) else {
        return false;
    };

    for line in diff.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut fields = line.split('\t');
        let Some(status) = fields.next() else {
            continue;
        };

        if status.starts_with('R') {
            let Some(old_path) = fields
                .next()
                .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
            else {
                continue;
            };
            let Some(new_path) = fields
                .next()
                .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
            else {
                continue;
            };
            index.remove_file(&old_path);
            index.update_file(&new_path);
            continue;
        }

        let Some(path) = fields
            .next()
            .and_then(|path| cached_path_under_root(root, &PathBuf::from(path)))
        else {
            continue;
        };
        if status.starts_with('D') || !path.exists() {
            index.remove_file(&path);
        } else {
            index.update_file(&path);
        }
    }

    true
}

fn is_binary_path(path: &Path, size: u64) -> bool {
    if size == 0 {
        return false;
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return true,
    };

    let mut preview = vec![0u8; PREVIEW_BYTES.min(size as usize)];
    match file.read(&mut preview) {
        Ok(read) => is_binary_bytes(&preview[..read]),
        Err(_) => true,
    }
}

fn line_starts_bytes(content: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in content.iter().copied().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_details_bytes(content: &[u8], line_starts: &[usize], offset: usize) -> (u32, u32, String) {
    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    let line_end = content[line_start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|length| line_start + length)
        .unwrap_or(content.len());
    let mut line_slice = &content[line_start..line_end];
    if line_slice.ends_with(b"\r") {
        line_slice = &line_slice[..line_slice.len() - 1];
    }
    let line_text = String::from_utf8_lossy(line_slice).into_owned();
    let column = String::from_utf8_lossy(&content[line_start..offset])
        .chars()
        .count() as u32
        + 1;
    (line_index as u32 + 1, column, line_text)
}

fn to_glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn cached_path_under_root_allows_missing_lexical_child() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        let path = cached_path_under_root(&root, Path::new("future/file.rs"))
            .expect("missing child should fall back to lexical validation");

        assert_eq!(path, root.join("future/file.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn cached_path_under_root_rejects_symlink_escape() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        fs::write(outside.join("secret.txt"), "secret").expect("write outside file");
        std::os::unix::fs::symlink(&outside, project.join("link")).expect("create symlink");
        let root = fs::canonicalize(&project).expect("canonicalize project");

        assert!(cached_path_under_root(&root, Path::new("link/secret.txt")).is_none());
    }

    #[test]
    fn extract_trigrams_tracks_next_char_and_position() {
        let trigrams = extract_trigrams(b"Rust");
        assert_eq!(trigrams.len(), 2);
        assert_eq!(trigrams[0], (pack_trigram(b'r', b'u', b's'), b't', 0));
        assert_eq!(
            trigrams[1],
            (pack_trigram(b'u', b's', b't'), EOF_SENTINEL, 1)
        );
    }

    #[test]
    fn index_file_trigram_filters_match_legacy_extraction() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("sample.txt");
        let content = b"Rust rust RUST\nxy";
        fs::write(&path, content).expect("write sample");

        let mut expected = BTreeMap::new();
        for (trigram, next_char, position) in extract_trigrams(content) {
            let entry: &mut PostingFilter = expected.entry(trigram).or_default();
            entry.next_mask |= mask_for_next_char(next_char);
            entry.loc_mask |= mask_for_position(position);
        }

        let mut index = SearchIndex::new();
        index.project_root = dir.path().to_path_buf();
        index.index_file(&path, content);

        let file_id = *index.path_to_id.get(&path).expect("file indexed");
        let file_trigrams = index
            .delta_file_trigrams
            .get(&file_id)
            .expect("delta file trigrams");
        assert_eq!(file_trigrams, &expected.keys().copied().collect::<Vec<_>>());
        for (trigram, filter) in expected {
            let postings = index
                .delta_postings
                .get(&trigram)
                .expect("delta posting list");
            assert_eq!(postings.len(), 1);
            assert_eq!(postings[0].file_id, file_id);
            assert_eq!(postings[0].next_mask, filter.next_mask);
            assert_eq!(postings[0].loc_mask, filter.loc_mask);
        }
    }

    #[test]
    fn decompose_regex_extracts_literals_and_alternations() {
        let query = decompose_regex("abc(def|ghi)xyz");
        assert!(query.and_trigrams.contains(&pack_trigram(b'a', b'b', b'c')));
        assert!(query.and_trigrams.contains(&pack_trigram(b'x', b'y', b'z')));
        assert_eq!(query.or_groups.len(), 1);
        assert!(query.or_groups[0].contains(&pack_trigram(b'd', b'e', b'f')));
        assert!(query.or_groups[0].contains(&pack_trigram(b'g', b'h', b'i')));
    }

    #[test]
    fn candidates_intersect_posting_lists() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let alpha = dir.path().join("alpha.txt");
        let beta = dir.path().join("beta.txt");
        fs::write(&alpha, "abcdef").expect("write alpha");
        fs::write(&beta, "abcxyz").expect("write beta");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&alpha, b"abcdef");
        index.index_file(&beta, b"abcxyz");

        let query = RegexQuery {
            and_trigrams: vec![
                pack_trigram(b'a', b'b', b'c'),
                pack_trigram(b'd', b'e', b'f'),
            ],
            ..RegexQuery::default()
        };

        let candidates = index.candidates(&query);
        assert_eq!(candidates.len(), 1);
        assert_eq!(index.files[candidates[0] as usize].path, alpha);
    }

    #[test]
    fn candidates_apply_bloom_filters() {
        let mut index = SearchIndex::new();
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("sample.txt");
        fs::write(&file, "abcd efgh").expect("write sample");
        index.project_root = dir.path().to_path_buf();
        index.index_file(&file, b"abcd efgh");

        let trigram = pack_trigram(b'a', b'b', b'c');
        let matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'd'),
            loc_mask: mask_for_position(0),
        };
        let non_matching_filter = PostingFilter {
            next_mask: mask_for_next_char(b'z'),
            loc_mask: mask_for_position(0),
        };

        assert_eq!(
            index
                .postings_for_trigram(trigram, Some(matching_filter))
                .len(),
            1
        );
        assert!(index
            .postings_for_trigram(trigram, Some(non_matching_filter))
            .is_empty());
    }

    #[test]
    fn base_delta_readd_masks_base_and_keeps_postings_sorted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let a = project.join("a.txt");
        let b = project.join("b.txt");
        fs::write(&a, "abc old").expect("write a");
        fs::write(&b, "abc base").expect("write b");

        let mut built = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        built.write_to_disk(&cache_dir, None);
        let mut index = SearchIndex::read_from_disk(&cache_dir, &project).expect("load base");
        assert_eq!(index.base_file_count, 2);

        let old_a_id = *index.path_to_id.get(&a).expect("original a id");
        let b_id = *index.path_to_id.get(&b).expect("original b id");
        index.remove_file(&a);
        index.index_file(&a, b"abc new");
        let new_id = *index.path_to_id.get(&a).expect("re-added file id");
        assert!(new_id >= index.base_file_count);
        let abc = pack_trigram(b'a', b'b', b'c');
        let ids = index.postings_for_trigram(abc, None);
        assert_eq!(ids, {
            let mut expected = vec![b_id, new_id];
            expected.sort_unstable();
            expected
        });
        assert!(!ids.contains(&old_a_id));
    }

    #[test]
    fn write_to_disk_compacts_base_and_delta() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");
        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);
        fs::write(&file, "abcxyz").expect("edit source");
        index.update_file(&file);
        assert!(!index.delta_postings.is_empty());
        index.write_to_disk(&cache_dir, None);
        assert!(index.delta_postings.is_empty());
        assert!(index.superseded.is_empty());
        assert_eq!(
            index.postings_for_trigram(pack_trigram(b'a', b'b', b'c'), None),
            vec![0]
        );
        assert!(index
            .postings_for_trigram(pack_trigram(b'd', b'e', b'f'), None)
            .is_empty());
    }

    #[test]
    fn legacy_cache_without_file_trigram_count_migrates_streaming_counts() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");
        let cache_dir = dir.path().join("cache");
        let mut index = SearchIndex::build(&project);
        index.write_to_disk(&cache_dir, None);
        let cache_path = cache_dir.join("cache.bin");
        strip_file_trigram_count_extension(&cache_path);
        assert!(!cache_has_file_trigram_count_extension(&cache_path));

        let loaded = SearchIndex::read_from_disk(&cache_dir, &project).expect("load legacy cache");
        assert_eq!(loaded.file_trigram_count.as_ref(), &[4]);
        assert!(loaded.delta_postings.is_empty());
        assert!(cache_has_file_trigram_count_extension(&cache_path));
    }

    #[test]
    fn compaction_flags_buffer_paths_while_running() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");
        let mut index = SearchIndex::new();
        index.project_root = project.clone();
        {
            let mut state = index.compaction_state.lock().expect("compaction state");
            state.running = true;
        }
        index.update_file(&file);
        let state = index.compaction_state.lock().expect("compaction state");
        assert!(state.requested_again || !index.delta_postings.is_empty());
        assert!(state.buffered_paths.contains(&file));
    }

    fn cache_has_file_trigram_count_extension(cache_path: &Path) -> bool {
        file_trigram_count_extension_range(cache_path).is_some()
    }

    fn strip_file_trigram_count_extension(cache_path: &Path) {
        let mut bytes = fs::read(cache_path).expect("read cache");
        let (start, end) = file_trigram_count_extension_range_from_bytes(&bytes)
            .expect("file trigram count extension");
        bytes.drain(start..end);
        let postings_len_total = u64::from_le_bytes(bytes[8..16].try_into().unwrap())
            - u64::try_from(end - start).unwrap();
        bytes[8..16].copy_from_slice(&postings_len_total.to_le_bytes());
        let checksum_pos = 16 + usize::try_from(postings_len_total).unwrap() - 4;
        let checksum = crc32fast::hash(&bytes[16..checksum_pos]);
        bytes[checksum_pos..checksum_pos + 4].copy_from_slice(&checksum.to_le_bytes());
        fs::write(cache_path, bytes).expect("write legacy cache");
    }

    fn file_trigram_count_extension_range(cache_path: &Path) -> Option<(usize, usize)> {
        let bytes = fs::read(cache_path).ok()?;
        file_trigram_count_extension_range_from_bytes(&bytes)
    }

    fn file_trigram_count_extension_range_from_bytes(bytes: &[u8]) -> Option<(usize, usize)> {
        let postings_len_total = u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?) as usize;
        let postings_start = 16usize;
        let postings_end = postings_start.checked_add(postings_len_total)?;
        let postings_body_end = postings_end.checked_sub(4)?;
        let mut reader = Cursor::new(&bytes[postings_start..postings_body_end]);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic).ok()?;
        if &magic != INDEX_MAGIC {
            return None;
        }
        read_u32(&mut reader).ok()?;
        let head_len = read_u32(&mut reader).ok()? as u64;
        let root_len = read_u32(&mut reader).ok()? as u64;
        let ignore_len = read_u32(&mut reader).ok()? as u64;
        read_u64(&mut reader).ok()?;
        let file_count = read_u32(&mut reader).ok()? as usize;
        let skip = head_len.checked_add(root_len)?.checked_add(ignore_len)?;
        reader.seek(SeekFrom::Current(skip as i64)).ok()?;
        for _ in 0..file_count {
            let mut unindexed = [0u8; 1];
            reader.read_exact(&mut unindexed).ok()?;
            let path_len = read_u32(&mut reader).ok()? as u64;
            read_u64(&mut reader).ok()?;
            read_u64(&mut reader).ok()?;
            read_u32(&mut reader).ok()?;
            let mut hash = [0u8; 32];
            reader.read_exact(&mut hash).ok()?;
            reader.seek(SeekFrom::Current(path_len as i64)).ok()?;
        }
        let postings_blob_len = read_u64(&mut reader).ok()? as usize;
        let extension_start = postings_start
            .checked_add(reader.position() as usize)?
            .checked_add(postings_blob_len)?;
        if extension_start + 16 > postings_body_end {
            return None;
        }
        if bytes.get(extension_start..extension_start + 8)? != FILE_TRIGRAM_COUNT_MAGIC {
            return None;
        }
        let count = u32::from_le_bytes(
            bytes[extension_start + 12..extension_start + 16]
                .try_into()
                .ok()?,
        ) as usize;
        let extension_end = extension_start
            .checked_add(16)?
            .checked_add(count.checked_mul(4)?)?;
        (extension_end <= postings_body_end).then_some((extension_start, extension_end))
    }

    #[test]
    fn disk_round_trip_preserves_postings_and_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("src.txt");
        fs::write(&file, "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        index.git_head = Some("deadbeef".to_string());
        let cache_dir = dir.path().join("cache");
        let head = index.git_head.clone();
        index.write_to_disk(&cache_dir, head.as_deref());

        let loaded =
            SearchIndex::read_from_disk(&cache_dir, &project).expect("load index from disk");
        assert_eq!(loaded.stored_git_head(), Some("deadbeef"));
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            relative_to_root(&loaded.project_root, &loaded.files[0].path),
            PathBuf::from("src.txt")
        );
        assert_eq!(loaded.trigram_count(), index.trigram_count());
        assert_eq!(
            loaded.postings_for_trigram(pack_trigram(b'a', b'b', b'c'), None),
            vec![0]
        );
        assert_eq!(
            loaded.file_trigram_count.as_ref(),
            index.file_trigram_count.as_ref()
        );
    }

    #[test]
    fn cache_path_helpers_reject_absolute_and_parent_paths() {
        let root = PathBuf::from("/tmp/aft-project");

        assert_eq!(
            cache_relative_path(&root, &root.join("src/lib.rs")),
            Some(PathBuf::from("src/lib.rs"))
        );
        assert!(cache_relative_path(&root, Path::new("/tmp/outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("../outside.rs")).is_none());
        assert!(cached_path_under_root(&root, Path::new("/tmp/outside.rs")).is_none());
        assert_eq!(
            cached_path_under_root(&root, Path::new("src/./lib.rs")),
            Some(root.join("src/lib.rs"))
        );
    }

    fn git_command_for_test(root: &Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.arg("-C").arg(root));
        command
    }

    #[test]
    fn refresh_after_head_change_removes_renames_and_detects_local_files() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let canonical_project = fs::canonicalize(&project).expect("canonical project");
        fs::write(project.join("old.txt"), "old token\n").expect("write old");
        fs::write(project.join("unchanged.txt"), "before\n").expect("write unchanged");

        let mut init = Command::new("git");
        crate::test_env::apply_hermetic_git_env(init.arg("init").arg(&project))
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            git_command_for_test(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .expect("git add initial");
        git_command_for_test(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit initial");
        let previous = run_git(&project, &["rev-parse", "HEAD"]).expect("previous head");
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = Some(previous.clone());

        fs::rename(project.join("old.txt"), project.join("new.txt")).expect("rename file");
        git_command_for_test(&project)
            .args(["add", "-A"])
            .status()
            .expect("git add rename");
        git_command_for_test(&project)
            .args(["commit", "-m", "rename"])
            .status()
            .expect("git commit rename");
        let current = run_git(&project, &["rev-parse", "HEAD"]).expect("current head");

        fs::write(project.join("unchanged.txt"), "after local edit\n").expect("local edit");
        fs::write(project.join("untracked.txt"), "untracked token\n").expect("untracked");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            Some(current),
            Some(baseline),
            None,
        );

        assert!(!refreshed
            .path_to_id
            .contains_key(&canonical_project.join("old.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("new.txt")));
        assert!(refreshed
            .path_to_id
            .contains_key(&canonical_project.join("untracked.txt")));
        let matches = refreshed.grep("after local edit", true, &[], &[], &canonical_project, 10);
        assert_eq!(matches.matches.len(), 1);
    }

    #[test]
    fn read_from_disk_rejects_corrupt_lookup_checksum() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        let cache_path = cache_dir.join("cache.bin");
        let mut bytes = fs::read(&cache_path).expect("read cache");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&cache_path, bytes).expect("write corrupted cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_none());
    }

    #[test]
    fn write_to_disk_uses_temp_files_and_cleans_them_up() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef").expect("write source");

        let mut index = SearchIndex::build(&project);
        let cache_dir = dir.path().join("cache");
        index.write_to_disk(&cache_dir, None);

        assert!(cache_dir.join("cache.bin").is_file());
        assert!(fs::read_dir(&cache_dir)
            .expect("read cache dir")
            .all(|entry| !entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")));
    }

    #[test]
    fn concurrent_search_index_writes_do_not_corrupt() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("src.txt"), "abcdef\n").expect("write source");
        let cache_dir = dir.path().join("cache");

        let a_project = project.clone();
        let a_cache = cache_dir.clone();
        let a = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&a_cache).expect("acquire cache lock a");
            let mut index = SearchIndex::build(&a_project);
            index.write_to_disk(&a_cache, None);
        });
        let b_project = project.clone();
        let b_cache = cache_dir.clone();
        let b = std::thread::spawn(move || {
            let _lock = CacheLock::acquire(&b_cache).expect("acquire cache lock b");
            let mut index = SearchIndex::build(&b_project);
            index.write_to_disk(&b_cache, None);
        });
        a.join().expect("writer a");
        b.join().expect("writer b");

        assert!(SearchIndex::read_from_disk(&cache_dir, &project).is_some());
    }

    #[test]
    fn search_index_atomic_rename_survives_partial_write() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");
        fs::write(cache_dir.join("cache.bin.tmp.1.1"), b"partial").expect("write partial tmp");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    #[test]
    fn artifact_cache_key_shared_across_clones_of_same_repo() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).expect("create source repo dir");
        fs::write(source.join("tracked.txt"), "content\n").expect("write tracked file");

        let mut init = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&source))
                .args(["init"])
                .status()
                .expect("init git repo")
                .success()
        );
        assert!(git_command_for_test(&source)
            .args(["add", "."])
            .status()
            .expect("git add")
            .success());
        assert!(git_command_for_test(&source)
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
            .expect("git commit")
            .success());

        let clone = dir.path().join("clone");
        let mut clone_command = Command::new("git");
        assert!(crate::test_env::apply_hermetic_git_env(&mut clone_command)
            .args(["clone", "--quiet"])
            .arg(&source)
            .arg(&clone)
            .status()
            .expect("git clone")
            .success());

        let source_key = artifact_cache_key(&source);
        let clone_key = artifact_cache_key(&clone);

        assert_eq!(source_key.len(), 16);
        assert_eq!(clone_key.len(), 16);
        // Same repo (same root commit) → same cache key regardless of clone path
        assert_eq!(source_key, clone_key);
    }

    fn read_cache_key_memo(storage_root: &Path) -> BTreeMap<String, ArtifactCacheKeyMemoEntry> {
        let bytes = fs::read(artifact_cache_key_memo_path(storage_root)).expect("read memo file");
        serde_json::from_slice(&bytes).expect("parse memo file")
    }

    fn git_like_root(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(root.join(".git")).expect("create git marker");
        root
    }

    #[test]
    fn artifact_cache_key_success_writes_memo() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root.clone(), commit.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("cache key from successful probe");

        assert_eq!(key, artifact_key_from_git_identity(&commit));
        let memo = read_cache_key_memo(&storage);
        let entry = memo
            .get(root.to_string_lossy().as_ref())
            .expect("memo entry for root");
        assert_eq!(entry.key, key);
        assert_eq!(entry.git_root_commit, commit);
        assert!(entry.recorded_at_ms > 0);
    }

    #[test]
    fn artifact_cache_key_probe_failure_uses_memoized_key() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let expected_key = artifact_key_from_git_identity(&commit);
        {
            let mut commits = BTreeMap::new();
            commits.insert(root.clone(), commit);
            let _override = force_git_root_commit_probe_commits_for_test(commits);
            assert_eq!(
                artifact_cache_key_with_memo(&root, &root, &storage, None).expect("initial key"),
                expected_key
            );
        }
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let rescued = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("memo should rescue transient probe failure");

        assert_eq!(rescued, expected_key);
        assert_ne!(rescued, artifact_key_from_path_identity(&root));
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_memo_rejects_git_like_root() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("git-like root without memo must not use path identity");

        assert_eq!(error.root(), root.as_path());
        assert!(error.detail().contains("Too many open files"));
    }

    #[test]
    fn artifact_cache_key_probe_failure_without_git_marker_uses_path_identity() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root = dir.path().join("plain");
        fs::create_dir_all(&root).expect("create non-git root");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let key = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect("non-git root keeps legacy path identity fallback");

        assert_eq!(key, artifact_key_from_path_identity(&root));
    }

    #[test]
    fn artifact_cache_key_corrupt_memo_is_absent_not_a_path_identity_escape() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        fs::create_dir_all(&storage).expect("create storage root");
        fs::write(artifact_cache_key_memo_path(&storage), b"not json").expect("write corrupt memo");
        let root = git_like_root(&dir, "repo");
        let _override = force_git_root_commit_probe_transient_for_paths_for_test(
            vec![root.clone()],
            "spawn failed: Too many open files (os error 24)",
        );

        let error = artifact_cache_key_with_memo(&root, &root, &storage, None)
            .expect_err("corrupt memo is treated as absent");

        assert!(error.detail().contains("Too many open files"));
    }

    #[test]
    fn artifact_cache_key_concurrent_memo_writes_keep_valid_json() {
        let _probe_lock = git_root_commit_probe_override_lock_for_test();
        let dir = tempfile::tempdir().expect("create temp dir");
        let storage = dir.path().join("storage");
        let root_a = git_like_root(&dir, "repo-a");
        let root_b = git_like_root(&dir, "repo-b");
        let commit_a = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let commit_b = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let mut commits = BTreeMap::new();
        commits.insert(root_a.clone(), commit_a.clone());
        commits.insert(root_b.clone(), commit_b.clone());
        let _override = force_git_root_commit_probe_commits_for_test(commits);

        let storage_a = storage.clone();
        let thread_a = std::thread::spawn({
            let root_a = root_a.clone();
            move || artifact_cache_key_with_memo(&root_a, &root_a, &storage_a, None)
        });
        let storage_b = storage.clone();
        let thread_b = std::thread::spawn({
            let root_b = root_b.clone();
            move || artifact_cache_key_with_memo(&root_b, &root_b, &storage_b, None)
        });

        let key_a = thread_a.join().expect("join writer a").expect("key a");
        let key_b = thread_b.join().expect("join writer b").expect("key b");
        let memo = read_cache_key_memo(&storage);

        assert_eq!(
            memo.get(root_a.to_string_lossy().as_ref())
                .expect("root a memo")
                .key,
            key_a
        );
        assert_eq!(
            memo.get(root_b.to_string_lossy().as_ref())
                .expect("root b memo")
                .key,
            key_b
        );
        assert_eq!(key_a, artifact_key_from_git_identity(&commit_a));
        assert_eq!(key_b, artifact_key_from_git_identity(&commit_b));
    }

    #[test]
    fn git_head_unchanged_picks_up_local_edits() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("repo");
        fs::create_dir_all(&project).expect("create repo dir");
        let file = project.join("tracked.txt");
        fs::write(&file, "oldtoken\n").expect("write file");
        let mut init = Command::new("git");
        assert!(
            crate::test_env::apply_hermetic_git_env(init.current_dir(&project))
                .arg("init")
                .status()
                .unwrap()
                .success()
        );
        assert!(git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(git_command_for_test(&project)
            .args([
                "-c",
                "user.name=AFT Tests",
                "-c",
                "user.email=aft-tests@example.com",
                "commit",
                "-m",
                "initial"
            ])
            .status()
            .unwrap()
            .success());
        let head = current_git_head(&project);
        let mut baseline = SearchIndex::build(&project);
        baseline.git_head = head.clone();
        fs::write(&file, "newtoken\n").expect("edit tracked file");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            head,
            Some(baseline),
            None,
        );
        let result = refreshed.grep("newtoken", true, &[], &[], &project, 10);

        assert_eq!(result.total_matches, 1);
    }

    #[test]
    fn non_git_project_reuses_cache_when_files_unchanged() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        fs::write(project.join("file.txt"), "unchangedtoken\n").expect("write file");
        let baseline = SearchIndex::build(&project);
        let baseline_file_count = baseline.file_count();

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            None,
            Some(baseline),
            None,
        );

        assert_eq!(refreshed.file_count(), baseline_file_count);
        assert_eq!(
            refreshed
                .grep("unchangedtoken", true, &[], &[], &project, 10)
                .total_matches,
            1
        );
    }

    #[test]
    fn resolve_search_scope_disables_index_for_external_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&project).expect("create project dir");
        fs::create_dir_all(&outside).expect("create outside dir");

        let scope = resolve_search_scope(&project, outside.to_str());

        assert_eq!(
            scope.root,
            fs::canonicalize(&outside).expect("canonicalize outside")
        );
        assert!(!scope.use_index);
    }

    #[test]
    fn grep_filters_matches_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let docs = project.join("docs");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&docs).expect("create docs dir");
        fs::write(src.join("main.rs"), "pub struct SearchIndex;\n").expect("write src file");
        fs::write(docs.join("guide.md"), "SearchIndex guide\n").expect("write docs file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.files_searched, 1);
        assert_eq!(result.files_with_matches, 1);
        assert_eq!(result.matches.len(), 1);
        // Index stores canonicalized paths; on macOS /var → /private/var
        let expected = fs::canonicalize(src.join("main.rs")).expect("canonicalize");
        assert_eq!(result.matches[0].file, expected);
    }

    #[test]
    fn grep_deduplicates_multiple_matches_on_same_line() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex SearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 10);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn grep_case_insensitive_unicode_literal_matches_indexed_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("unicode.txt");
        fs::write(&file, "äbc\n").expect("write unicode file");

        let index = SearchIndex::build(&project);
        let result = index.grep("Äbc", false, &[], &[], &project, 10);

        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            result.matches[0].file,
            fs::canonicalize(file).expect("canonicalize unicode file")
        );
    }

    #[test]
    fn refresh_reindexes_same_size_edit_with_preserved_mtime() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let file = project.join("tokens.txt");
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        fs::write(&file, "alpha").expect("write original file");
        filetime::set_file_mtime(&file, original_mtime).expect("set original mtime");

        let baseline = SearchIndex::build(&project);
        fs::write(&file, "bravo").expect("write same-size edit");
        filetime::set_file_mtime(&file, original_mtime).expect("restore original mtime");

        let refreshed = SearchIndex::rebuild_or_refresh(
            &project,
            DEFAULT_MAX_FILE_SIZE,
            None,
            Some(baseline),
            None,
        );
        let result = refreshed.grep("bravo", true, &[], &[], &project, 10);
        let canonical_file = fs::canonicalize(&file).expect("canonicalize edited file");
        let refreshed_id = *refreshed
            .path_to_id
            .get(&canonical_file)
            .expect("file remains indexed");

        assert_eq!(result.total_matches, 1);
        assert!(refreshed
            .postings_for_trigram(pack_trigram(b'b', b'r', b'a'), None)
            .contains(&refreshed_id));
        assert!(!refreshed
            .postings_for_trigram(pack_trigram(b'a', b'l', b'p'), None)
            .contains(&refreshed_id));
    }

    #[test]
    fn grep_reports_total_matches_before_truncation() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("create src dir");
        fs::write(src.join("main.rs"), "SearchIndex\nSearchIndex\n").expect("write src file");

        let index = SearchIndex::build(&project);
        let result = index.grep("SearchIndex", true, &[], &[], &src, 1);

        assert_eq!(result.total_matches, 2);
        assert_eq!(result.matches.len(), 1);
        assert!(result.truncated);
    }

    #[test]
    fn glob_filters_results_to_search_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let src = project.join("src");
        let scripts = project.join("scripts");
        fs::create_dir_all(&src).expect("create src dir");
        fs::create_dir_all(&scripts).expect("create scripts dir");
        fs::write(src.join("main.rs"), "pub fn main() {}\n").expect("write src file");
        fs::write(scripts.join("tool.rs"), "pub fn tool() {}\n").expect("write scripts file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.rs", &src);

        assert_eq!(
            files,
            vec![fs::canonicalize(src.join("main.rs")).expect("canonicalize src file")]
        );
    }

    #[test]
    fn glob_includes_hidden_and_binary_files() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        let hidden_dir = project.join(".hidden");
        fs::create_dir_all(&hidden_dir).expect("create hidden dir");
        let hidden_file = hidden_dir.join("data.bin");
        fs::write(&hidden_file, [0u8, 159, 146, 150]).expect("write binary file");

        let index = SearchIndex::build(&project);
        let files = index.glob("**/*.bin", &project);

        assert_eq!(
            files,
            vec![fs::canonicalize(hidden_file).expect("canonicalize binary file")]
        );
    }

    #[test]
    fn read_from_disk_rejects_invalid_nanos() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).expect("create cache dir");

        let mut postings = Vec::new();
        postings.extend_from_slice(INDEX_MAGIC);
        postings.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        postings.extend_from_slice(&0u32.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&DEFAULT_MAX_FILE_SIZE.to_le_bytes());
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(b"/");
        postings.push(0u8);
        postings.extend_from_slice(&1u32.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&0u64.to_le_bytes());
        postings.extend_from_slice(&1_000_000_000u32.to_le_bytes());
        postings.extend_from_slice(b"a");
        postings.extend_from_slice(&0u64.to_le_bytes());

        let mut lookup = Vec::new();
        lookup.extend_from_slice(LOOKUP_MAGIC);
        lookup.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        lookup.extend_from_slice(&0u32.to_le_bytes());

        let postings_checksum = crc32fast::hash(&postings);
        postings.extend_from_slice(&postings_checksum.to_le_bytes());
        let lookup_checksum = crc32fast::hash(&lookup);
        lookup.extend_from_slice(&lookup_checksum.to_le_bytes());
        let mut cache = Vec::new();
        cache.extend_from_slice(&CACHE_MAGIC.to_le_bytes());
        cache.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        cache.extend_from_slice(&(postings.len() as u64).to_le_bytes());
        cache.extend_from_slice(&postings);
        cache.extend_from_slice(&lookup);
        fs::write(cache_dir.join("cache.bin"), cache).expect("write cache");

        assert!(SearchIndex::read_from_disk(&cache_dir, dir.path()).is_none());
    }

    #[test]
    fn parallel_cold_build_matches_serial_index() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        for index in 0..80 {
            let sub = project.join(format!("pkg_{index:03}"));
            fs::create_dir_all(&sub).expect("create subdir");
            fs::write(
                sub.join("lib.rs"),
                format!(
                    "pub fn unique_marker_{index}() {{ println!(\"aft_perf_marker_{index}\"); }}\n"
                ),
            )
            .expect("write lib");
        }

        let serial = SearchIndex::build_with_limit_serial(&project, DEFAULT_MAX_FILE_SIZE);
        let parallel = SearchIndex::build_with_limit(&project, DEFAULT_MAX_FILE_SIZE);

        assert_eq!(serial.file_count(), parallel.file_count());
        assert_eq!(serial.trigram_count(), parallel.trigram_count());
        assert_eq!(serial.path_to_id.len(), parallel.path_to_id.len());
        assert_eq!(
            serial.file_trigram_count.as_ref(),
            parallel.file_trigram_count.as_ref()
        );
        for (path, id) in serial.path_to_id.iter() {
            assert_eq!(parallel.path_to_id.get(path), Some(id));
        }
        for (serial_file, parallel_file) in serial.files.iter().zip(parallel.files.iter()) {
            assert_eq!(serial_file.path, parallel_file.path);
            assert_eq!(serial_file.size, parallel_file.size);
            assert_eq!(serial_file.modified, parallel_file.modified);
            assert_eq!(serial_file.content_hash, parallel_file.content_hash);
        }

        let serial_grep = serial.grep("aft_perf_marker_17", true, &[], &[], &project, 10);
        let parallel_grep = parallel.grep("aft_perf_marker_17", true, &[], &[], &project, 10);
        assert_eq!(serial_grep.matches, parallel_grep.matches);
        assert_eq!(serial_grep.total_matches, parallel_grep.total_matches);
        assert_eq!(serial_grep.files_searched, parallel_grep.files_searched);
        assert_eq!(
            serial_grep.files_with_matches,
            parallel_grep.files_with_matches
        );
    }

    #[test]
    fn ignore_rule_discovery_respects_gitignore() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let dir = tempfile::tempdir().expect("create temp dir");
        let project = dir.path().join("project");
        fs::create_dir_all(project.join("src")).expect("mkdir src");
        fs::write(project.join("src/.gitignore"), "data/\n").expect("write gitignore");
        let data = project.join("src/data");
        fs::create_dir_all(&data).expect("mkdir data");
        for index in 0..200 {
            fs::create_dir_all(data.join(format!("d{index}"))).expect("mkdir nested");
            fs::write(data.join(format!("d{index}/f.rs")), "fn ignored() {}\n")
                .expect("write ignored file");
        }

        let mut init = Command::new("git");
        crate::test_env::apply_hermetic_git_env(init.arg("init").arg(&project))
            .status()
            .expect("git init");
        for args in [
            ["config", "user.email", "aft@example.invalid"],
            ["config", "user.name", "AFT Test"],
        ] {
            git_command_for_test(&project)
                .args(args)
                .status()
                .expect("git config");
        }
        git_command_for_test(&project)
            .args(["add", "."])
            .status()
            .expect("git add");
        git_command_for_test(&project)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit");

        let legacy_dirs = count_ignore_rule_discovery_dirs_legacy_stack(&project);
        let walker_dirs = count_ignore_rule_discovery_dirs(&project);
        assert!(
            legacy_dirs > walker_dirs,
            "legacy stack should descend into gitignored data/ (legacy={legacy_dirs}, walker={walker_dirs})"
        );
        assert!(
            walker_dirs < 50,
            "ignore walker should not descend deeply into ignored tree (dirs={walker_dirs})"
        );
    }

    /// Regression: v0.15.2 — sort_paths_by_mtime_desc panicked when files
    /// changed between cmp() calls.
    ///
    /// Pre-fix, the sort closure called `path_modified_time(path)` directly,
    /// which does a `stat()` syscall. If the file was deleted, modified, or
    /// touched mid-sort, the comparator returned different values for the
    /// same input pair on different invocations. Rust's slice::sort detects
    /// this and panics with "user-provided comparison function does not
    /// correctly implement a total order".
    ///
    /// CI hit this on a Pi e2e test (workflow run 24887807972) where the
    /// bridge invalidated files in parallel with grep's sort path. This
    /// test simulates the worst case: most paths don't exist (Err from
    /// fs::metadata) and sort still completes successfully.
    #[test]
    fn sort_paths_by_mtime_desc_does_not_panic_on_missing_files() {
        // Mix of existing and non-existing paths in deliberately
        // non-monotonic order — pre-fix, the sort would call stat() at
        // least N log N times and any flakiness would trigger the panic.
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..30 {
            // Half exist, half don't.
            let path = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            paths.push(path);
        }

        // Run the sort many times to maximise the chance of catching any
        // residual non-determinism. Pre-fix: panic. Post-fix: stable.
        for _ in 0..50 {
            let mut copy = paths.clone();
            sort_paths_by_mtime_desc(&mut copy);
            assert_eq!(copy.len(), paths.len());
        }
    }

    /// Regression: the indexed parallel search's reduce() combine closure must
    /// NOT set engine_capped. reduce runs on every partial-result merge in a
    /// multi-chunk parallel search (>10 candidate files), capped or not — an
    /// unconditional store there falsely reported every such grep as capped,
    /// lying to the agent that results were truncated.
    #[test]
    fn uncapped_indexed_grep_over_many_files_is_not_engine_capped() {
        let dir = tempfile::tempdir().expect("create tempdir");
        // >10 files so the parallel (reduce) branch is taken, each with exactly
        // one match, and a generous cap so the search is NOT actually capped.
        for i in 0..40 {
            fs::write(
                dir.path().join(format!("file-{i}.rs")),
                format!("fn unique_marker_{i}() {{ let _ = \"needle_token\"; }}\n"),
            )
            .expect("write");
        }
        let index = SearchIndex::build_with_limit(dir.path(), DEFAULT_MAX_FILE_SIZE);
        let result = index.grep("needle_token", false, &[], &[], dir.path(), 1000);
        assert!(
            result.matches.len() >= 40,
            "expected a match per file, got {}",
            result.matches.len()
        );
        assert!(
            !result.engine_capped,
            "an uncapped grep over >10 files must not report engine_capped"
        );
        assert!(!result.truncated, "uncapped grep must not be truncated");
    }

    /// Regression: v0.15.2 — sort_grep_matches_by_mtime_desc panicked under
    /// the same conditions as sort_paths_by_mtime_desc. See the
    /// sort_paths_... test above for the full rationale.
    #[test]
    fn sort_grep_matches_by_mtime_desc_does_not_panic_on_missing_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut matches: Vec<GrepMatch> = Vec::new();
        for i in 0..30 {
            let file = if i % 2 == 0 {
                let p = dir.path().join(format!("real-{i}.rs"));
                fs::write(&p, format!("// {i}\n")).expect("write");
                p
            } else {
                dir.path().join(format!("missing-{i}.rs"))
            };
            matches.push(GrepMatch {
                file,
                line: u32::try_from(i).unwrap_or(0),
                column: 0,
                line_text: format!("match {i}"),
                match_text: format!("match {i}"),
            });
        }

        for _ in 0..50 {
            let mut copy = matches.clone();
            sort_grep_matches_by_mtime_desc(&mut copy, dir.path());
            assert_eq!(copy.len(), matches.len());
        }
    }
}
