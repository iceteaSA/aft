//! Persistent call/reference graph sidecar.
//!
//! Phase 1 intentionally keeps this substrate self-contained: callers can build
//! and query the sidecar directly, but no runtime command reads from it yet.

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::callgraph::{self, EdgeResolution, FileCallData};
use crate::error::AftError;
use crate::imports::{ImportForm, ImportKind, ImportStatement};
use crate::parser::LangId;
use crate::symbols::{Range, SymbolKind};
use rayon::prelude::*;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Transaction};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 1;
const BACKEND_TREESITTER: &str = "treesitter";
const PROVENANCE_TREESITTER: &str = "treesitter+resolver";
const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

type ColdBuildSwapObserver = dyn Fn(&Path, &Path) + Send + Sync + 'static;
static COLD_BUILD_SWAP_OBSERVER: OnceLock<Mutex<Option<Arc<ColdBuildSwapObserver>>>> =
    OnceLock::new();

mod dead_code_projection;
pub use dead_code_projection::project_dead_code_snapshot;

#[doc(hidden)]
pub fn set_cold_build_swap_observer(observer: Option<Arc<ColdBuildSwapObserver>>) {
    let slot = COLD_BUILD_SWAP_OBSERVER.get_or_init(|| Mutex::new(None));
    *slot
        .lock()
        .expect("callgraph cold-build observer mutex poisoned") = observer;
}

fn notify_cold_build_swap_observer(temp_path: &Path, target_path: &Path) {
    let Some(slot) = COLD_BUILD_SWAP_OBSERVER.get() else {
        return;
    };
    let observer = slot
        .lock()
        .expect("callgraph cold-build observer mutex poisoned")
        .clone();
    if let Some(observer) = observer {
        observer(temp_path, target_path);
    }
}

#[derive(Debug)]
pub enum CallGraphStoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Aft(AftError),
    Lock(crate::fs_lock::AcquireError),
    MissingCallerData { file: String },
    Unavailable(String),
    StaleFiles(Vec<String>),
}

impl fmt::Display for CallGraphStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sqlite(error) => write!(formatter, "sqlite error: {error}"),
            Self::Json(error) => write!(formatter, "json error: {error}"),
            Self::Aft(error) => write!(formatter, "callgraph extraction error: {error}"),
            Self::Lock(error) => write!(formatter, "callgraph build lock error: {error}"),
            Self::MissingCallerData { file } => {
                write!(formatter, "missing extracted caller data for {file}")
            }
            Self::Unavailable(message) => {
                write!(formatter, "callgraph store unavailable: {message}")
            }
            Self::StaleFiles(files) => {
                write!(
                    formatter,
                    "callgraph store has stale files: {}",
                    files.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for CallGraphStoreError {}

impl From<std::io::Error> for CallGraphStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CallGraphStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<serde_json::Error> for CallGraphStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<AftError> for CallGraphStoreError {
    fn from(error: AftError) -> Self {
        Self::Aft(error)
    }
}

impl From<crate::fs_lock::AcquireError> for CallGraphStoreError {
    fn from(error: crate::fs_lock::AcquireError) -> Self {
        Self::Lock(error)
    }
}

pub type Result<T> = std::result::Result<T, CallGraphStoreError>;

/// Runtime gate name for Phase-1 callers. The substrate is compiled and tested,
/// but production commands should only open it through `open_if_enabled` until
/// Phase 2 migrates consumers.
pub const CALLGRAPH_STORE_FLAG: &str = "callgraph_store";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallGraphStoreOptions {
    pub enabled: bool,
}

#[derive(Debug)]
pub struct CallGraphStore {
    project_root: PathBuf,
    project_key: String,
    sqlite_path: PathBuf,
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct ColdBuildStats {
    pub files: usize,
    pub nodes: usize,
    pub refs: usize,
    pub edges: usize,
    pub failed_files: Vec<String>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
pub struct IncrementalStats {
    pub changed_files: Vec<String>,
    pub surface_changed: Vec<String>,
    pub deleted_files: Vec<String>,
    pub dependency_selected_refs: usize,
    pub refreshed_own_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredEdge {
    pub source_file: String,
    pub source_symbol: String,
    pub target_file: String,
    pub target_symbol: String,
    pub kind: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreNode {
    node_id: String,
    pub file: String,
    pub symbol: String,
    pub name: String,
    pub kind: String,
    pub line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub exported: bool,
    pub is_entry_point: bool,
    pub lang: LangId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallSite {
    pub caller: StoreNode,
    pub target_file: String,
    pub target_symbol: String,
    pub target: Option<StoreNode>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreUnresolvedCall {
    pub caller: StoreNode,
    pub symbol: String,
    pub full_ref: Option<String>,
    pub line: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallersResult {
    pub target: StoreNode,
    pub callers: Vec<StoreCallSite>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactCaller {
    pub site: StoreCallSite,
    pub signature: Option<String>,
    pub is_entry_point: bool,
    pub call_expression: Option<String>,
    pub parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreImpactResult {
    pub target: StoreNode,
    pub parameters: Vec<String>,
    pub callers: Vec<StoreImpactCaller>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone)]
struct ExtractFailure {
    rel_path: String,
    freshness: Option<FileFreshness>,
}

#[derive(Debug, Clone)]
struct BuildExtractsResult {
    extracts: Vec<FileExtract>,
    failures: Vec<ExtractFailure>,
}

#[derive(Debug, Clone)]
enum StoreForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

impl StoreForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }
}

#[derive(Debug, Clone)]
struct FileExtract {
    abs_path: PathBuf,
    rel_path: String,
    freshness: FileFreshness,
    lang: LangId,
    data: FileCallData,
    nodes: Vec<NodeRecord>,
    raw_refs: Vec<RawRef>,
    dispatch_hints: Vec<DispatchHint>,
    surface_fingerprint: String,
}

#[derive(Debug, Clone)]
struct NodeRecord {
    id: String,
    file_path: String,
    name: String,
    scoped_name: String,
    kind: String,
    range: Range,
    range_ordinal: u32,
    signature: Option<String>,
    exported: bool,
    is_default_export: bool,
    is_type_like: bool,
    is_callgraph_entry_point: bool,
}

#[derive(Debug, Clone)]
struct RawRef {
    ref_id: String,
    caller_node: Option<String>,
    caller_symbol: Option<String>,
    caller_file: String,
    kind: String,
    short_name: Option<String>,
    full_ref: Option<String>,
    module_path: Option<String>,
    import_kind: Option<String>,
    local_name: Option<String>,
    requested_name: Option<String>,
    namespace_alias: Option<String>,
    wildcard: bool,
    line: u32,
    byte_start: usize,
    byte_end: usize,
    raw_payload: String,
    dependencies: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct ResolvedRef {
    raw: RawRef,
    status: String,
    target_node: Option<String>,
    target_file: Option<String>,
    target_symbol: Option<String>,
    dependencies: BTreeSet<String>,
    edge: Option<EdgeRecord>,
}

#[derive(Debug, Clone)]
struct EdgeRecord {
    edge_id: String,
    source_node: String,
    target_node: Option<String>,
    target_file: String,
    target_symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct DispatchHint {
    id: String,
    method_name: String,
    caller_node: String,
    file: String,
    line: u32,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Debug, Clone)]
struct FileRow {
    surface_fingerprint: String,
    freshness: FileFreshness,
}

#[derive(Debug, Clone)]
struct DbFileIndex {
    lang: Option<LangId>,
    exports: HashSet<String>,
    default_export: Option<String>,
    export_aliases: HashMap<String, String>,
    node_by_scoped: HashMap<String, String>,
    node_by_bare: HashMap<String, String>,
    module_targets: HashMap<String, Option<String>>,
    reexports: Vec<ReexportIndex>,
}

#[derive(Debug, Clone)]
struct ReexportIndex {
    target_file: Option<String>,
    named: HashMap<String, String>,
    wildcard: bool,
}

#[derive(Debug, Clone)]
struct ProjectIndex<'a> {
    project_root: PathBuf,
    files: HashMap<String, DbFileIndex>,
    caller_data: HashMap<String, &'a FileCallData>,
    /// Lazily-built `crate_name -> src prefix` map for Rust workspace resolution.
    /// Built once (whole-tree walk) on first qualified-ref resolution and reused,
    /// instead of re-walking the project per ref. Skipped entirely when no Rust
    /// workspace ref is resolved (e.g. warm query path with no Rust changes).
    workspace_crate_prefixes: std::sync::OnceLock<HashMap<String, String>>,
}

impl ProjectIndex<'_> {
    /// Resolve a crate name to its `src` prefix, building the workspace map on
    /// first use. The map walks the project tree exactly once per index.
    fn crate_src_prefix(&self, crate_name: &str) -> Option<String> {
        self.workspace_crate_prefixes
            .get_or_init(|| build_workspace_crate_prefixes(&self.project_root))
            .get(crate_name)
            .cloned()
    }
}

impl CallGraphStore {
    pub fn open_if_enabled(
        options: CallGraphStoreOptions,
        callgraph_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Option<Self>> {
        if !options.enabled {
            return Ok(None);
        }
        Self::open(callgraph_dir, project_root).map(Some)
    }

    pub fn open(callgraph_dir: PathBuf, project_root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&callgraph_dir)?;
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        Self::open_at_path(project_root, project_key, sqlite_path, true)
    }

    pub fn open_readonly(callgraph_dir: PathBuf, project_root: PathBuf) -> Result<Option<Self>> {
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        if !sqlite_path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        if !database_ready(&conn).unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(Self::from_connection(
            project_root,
            project_key,
            sqlite_path,
            conn,
        )))
    }

    pub fn cold_build_with_lease(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
    ) -> Result<(Self, ColdBuildStats)> {
        std::fs::create_dir_all(&callgraph_dir)?;
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        let lock_path = callgraph_dir.join(format!("{project_key}.build.lock"));
        let _guard = crate::fs_lock::try_acquire(&lock_path, Duration::from_secs(30))?;
        let stats = Self::cold_build_swap_locked(
            &callgraph_dir,
            &project_root,
            &project_key,
            &sqlite_path,
            files,
        )?;
        let store = Self::open(callgraph_dir, project_root)?;
        Ok((store, stats))
    }

    pub fn ensure_built_with_lease(
        callgraph_dir: PathBuf,
        project_root: PathBuf,
        files: &[PathBuf],
    ) -> Result<(Self, Option<ColdBuildStats>)> {
        std::fs::create_dir_all(&callgraph_dir)?;
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        let lock_path = callgraph_dir.join(format!("{project_key}.build.lock"));
        let _guard = crate::fs_lock::try_acquire(&lock_path, Duration::from_secs(30))?;
        if sqlite_path.is_file() {
            let conn = Connection::open_with_flags(&sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            conn.busy_timeout(Duration::from_millis(5_000))?;
            if database_ready(&conn).unwrap_or(false) {
                return Ok((Self::open(callgraph_dir, project_root)?, None));
            }
        }
        let stats = Self::cold_build_swap_locked(
            &callgraph_dir,
            &project_root,
            &project_key,
            &sqlite_path,
            files,
        )?;
        Ok((Self::open(callgraph_dir, project_root)?, Some(stats)))
    }

    fn cold_build_swap_locked(
        callgraph_dir: &Path,
        project_root: &Path,
        project_key: &str,
        sqlite_path: &Path,
        files: &[PathBuf],
    ) -> Result<ColdBuildStats> {
        let temp_path = callgraph_dir.join(format!(
            "{project_key}.sqlite.tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos()
        ));
        remove_sqlite_file_set(&temp_path);

        let stats = {
            let temp_store = Self::open_at_path(
                project_root.to_path_buf(),
                project_key.to_string(),
                temp_path.clone(),
                false,
            )?;
            let stats = temp_store.cold_build(files)?;
            temp_store.prepare_for_atomic_swap()?;
            stats
        };

        notify_cold_build_swap_observer(&temp_path, sqlite_path);
        remove_sqlite_sidecars(sqlite_path);
        if let Err(error) = std::fs::rename(&temp_path, sqlite_path) {
            if sqlite_path.exists() {
                std::fs::remove_file(sqlite_path)?;
                std::fs::rename(&temp_path, sqlite_path)?;
            } else {
                return Err(error.into());
            }
        }
        remove_sqlite_sidecars(sqlite_path);
        Ok(stats)
    }

    pub fn needs_cold_build(callgraph_dir: &Path, project_root: &Path) -> Result<bool> {
        let project_key = crate::search_index::project_cache_key(project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        if !sqlite_path.is_file() {
            return Ok(true);
        }
        let conn = Connection::open_with_flags(&sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        Ok(!database_ready(&conn).unwrap_or(false))
    }

    fn open_at_path(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        use_wal: bool,
    ) -> Result<Self> {
        if let Some(parent) = sqlite_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&sqlite_path)?;
        if use_wal {
            configure_connection(&conn)?;
        } else {
            configure_build_connection(&conn)?;
        }
        initialize_schema(&conn)?;
        Ok(Self::from_connection(
            project_root,
            project_key,
            sqlite_path,
            conn,
        ))
    }

    fn prepare_for_atomic_swap(&self) -> Result<()> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
        Ok(())
    }

    fn from_connection(
        project_root: PathBuf,
        project_key: String,
        sqlite_path: PathBuf,
        conn: Connection,
    ) -> Self {
        Self {
            project_root,
            project_key,
            sqlite_path,
            conn: Mutex::new(conn),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    pub fn sqlite_path(&self) -> &Path {
        &self.sqlite_path
    }

    pub fn cold_build(&self, files: &[PathBuf]) -> Result<ColdBuildStats> {
        let started = Instant::now();
        let bench = std::env::var("AFT_BENCH_COLD").is_ok();
        macro_rules! phase {
            ($label:expr, $t:expr) => {
                if bench {
                    eprintln!("  cold_build[{}]: {} ms", $label, $t.elapsed().as_millis());
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                }
            };
        }
        let files = normalize_file_list(&self.project_root, files)?;
        let t = Instant::now();
        let build = build_extracts_parallel(&self.project_root, &files);
        phase!("extract_parallel", t);
        let extracts = build.extracts;
        let failures = build.failures;
        let node_count = extracts.iter().map(|extract| extract.nodes.len()).sum();

        let t = Instant::now();
        let index = ProjectIndex::from_extracts(&self.project_root, &extracts);
        phase!("build_index", t);
        let t = Instant::now();
        let mut resolved_refs = Vec::new();
        for extract in &extracts {
            for raw_ref in &extract.raw_refs {
                resolved_refs.push(resolve_ref(raw_ref.clone(), &index)?);
            }
        }
        phase!("resolve_refs", t);
        let ref_count = resolved_refs.len();
        let edge_count = resolved_refs
            .iter()
            .filter(|item| item.edge.is_some())
            .count();

        let t = Instant::now();
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let tx = conn.transaction()?;
        clear_tables(&tx)?;
        insert_meta(&tx)?;
        for extract in &extracts {
            insert_file_extract(&tx, &self.project_root, extract)?;
        }
        for failure in &failures {
            mark_backend_state(
                &tx,
                &self.project_root,
                &failure.rel_path,
                failure
                    .freshness
                    .as_ref()
                    .map(|freshness| &freshness.content_hash),
                "stale",
            )?;
        }
        for resolved in &resolved_refs {
            insert_resolved_ref(&tx, resolved)?;
        }
        set_meta_ready(&tx, true)?;
        tx.commit()?;
        phase!("sqlite_insert", t);

        Ok(ColdBuildStats {
            files: extracts.len(),
            nodes: node_count,
            refs: ref_count,
            edges: edge_count,
            failed_files: failures
                .into_iter()
                .map(|failure| failure.rel_path)
                .collect(),
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub fn refresh_files(&self, changed_files: &[PathBuf]) -> Result<IncrementalStats> {
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let tx = conn.transaction()?;
        ensure_database_ready(&tx)?;
        let mut changed = Vec::new();
        let mut surface_changed = BTreeSet::new();
        let mut deleted = BTreeSet::new();
        let mut own_refresh = BTreeSet::new();
        let mut selected_ref_ids = BTreeSet::new();
        let mut changed_extracts: HashMap<String, FileExtract> = HashMap::new();

        for input in changed_files {
            let abs_path = normalize_file_path(&self.project_root, input)?;
            let rel_path = relative_path(&self.project_root, &abs_path);
            changed.push(rel_path.clone());
            let old_row = load_file_row(&tx, &rel_path)?;
            if !abs_path.exists() {
                if old_row.is_some() {
                    surface_changed.insert(rel_path.clone());
                    deleted.insert(rel_path.clone());
                    selected_ref_ids.extend(ref_ids_depending_on(&tx, &rel_path)?);
                    delete_file_rows(&tx, &rel_path)?;
                    mark_backend_state(&tx, &self.project_root, &rel_path, None, "stale")?;
                }
                continue;
            }

            if let Some(row) = &old_row {
                match cache_freshness::verify_file(&abs_path, &row.freshness) {
                    FreshnessVerdict::HotFresh => continue,
                    FreshnessVerdict::ContentFresh {
                        new_mtime,
                        new_size,
                    } => {
                        update_file_fresh_metadata(
                            &tx,
                            &rel_path,
                            &row.freshness.content_hash,
                            new_mtime,
                            new_size,
                        )?;
                        continue;
                    }
                    FreshnessVerdict::Deleted => {
                        surface_changed.insert(rel_path.clone());
                        deleted.insert(rel_path.clone());
                        selected_ref_ids.extend(ref_ids_depending_on(&tx, &rel_path)?);
                        delete_file_rows(&tx, &rel_path)?;
                        mark_backend_state(&tx, &self.project_root, &rel_path, None, "stale")?;
                        continue;
                    }
                    FreshnessVerdict::Stale => {}
                }
            }

            let extract = build_file_extract(&self.project_root, &abs_path)?;
            let surface_is_changed = old_row
                .as_ref()
                .map(|row| row.surface_fingerprint != extract.surface_fingerprint)
                .unwrap_or(true);
            if surface_is_changed {
                surface_changed.insert(rel_path.clone());
                selected_ref_ids.extend(ref_ids_depending_on(&tx, &rel_path)?);
            }
            own_refresh.insert(rel_path.clone());
            delete_file_rows(&tx, &rel_path)?;
            insert_file_extract(&tx, &self.project_root, &extract)?;
            changed_extracts.insert(rel_path, extract);
        }

        let dependency_selected_refs = selected_ref_ids.len();
        let selected_refs_by_caller = refs_by_caller_for_ref_ids(&tx, &selected_ref_ids)?;
        let mut touched_callers: BTreeSet<String> =
            selected_refs_by_caller.keys().cloned().collect();
        touched_callers.extend(own_refresh.iter().cloned());

        let mut caller_extracts: HashMap<String, FileExtract> = HashMap::new();
        for rel_path in &touched_callers {
            if deleted.contains(rel_path) {
                continue;
            }
            if let Some(extract) = changed_extracts.get(rel_path) {
                caller_extracts.insert(rel_path.clone(), extract.clone());
                continue;
            }
            let abs_path = self.project_root.join(rel_path);
            if abs_path.exists() {
                let extract = build_file_extract(&self.project_root, &abs_path)?;
                caller_extracts.insert(rel_path.clone(), extract);
            }
        }

        let index = ProjectIndex::from_db_and_callers(&tx, &self.project_root, &caller_extracts)?;
        for rel_path in &touched_callers {
            if deleted.contains(rel_path) {
                continue;
            }
            let Some(extract) = caller_extracts.get(rel_path) else {
                continue;
            };
            if own_refresh.contains(rel_path) {
                delete_refs_for_caller(&tx, rel_path)?;
                for raw_ref in &extract.raw_refs {
                    let resolved = resolve_ref(raw_ref.clone(), &index)?;
                    insert_resolved_ref(&tx, &resolved)?;
                }
                continue;
            }

            let selected_for_caller = selected_refs_by_caller
                .get(rel_path)
                .cloned()
                .unwrap_or_default();
            delete_ref_ids(&tx, &selected_for_caller)?;
            for raw_ref in &extract.raw_refs {
                if selected_for_caller.contains(&raw_ref.ref_id) {
                    let resolved = resolve_ref(raw_ref.clone(), &index)?;
                    insert_resolved_ref(&tx, &resolved)?;
                }
            }
        }

        tx.commit()?;
        Ok(IncrementalStats {
            changed_files: changed,
            surface_changed: surface_changed.into_iter().collect(),
            deleted_files: deleted.into_iter().collect(),
            dependency_selected_refs,
            refreshed_own_files: own_refresh.len(),
        })
    }

    pub fn refresh_corpus(&self, current_files: &[PathBuf]) -> Result<ColdBuildStats> {
        self.cold_build(current_files)
    }

    pub fn mark_files_stale(&self, files: &[PathBuf]) -> Result<Vec<String>> {
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let tx = conn.transaction()?;
        let mut marked = Vec::new();
        for path in files {
            let abs_path = normalize_file_path(&self.project_root, path)?;
            let rel_path = relative_path(&self.project_root, &abs_path);
            let freshness = cache_freshness::collect(&abs_path).ok();
            mark_backend_state(
                &tx,
                &self.project_root,
                &rel_path,
                freshness.as_ref().map(|freshness| &freshness.content_hash),
                "stale",
            )?;
            marked.push(rel_path);
        }
        tx.commit()?;
        marked.sort();
        marked.dedup();
        Ok(marked)
    }

    pub fn stale_files(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file_path FROM backend_file_state
             WHERE backend = ?1 AND workspace_root = ?2 AND status = 'stale'
             ORDER BY file_path",
        )?;
        let rows = stmt.query_map(
            params![BACKEND_TREESITTER, self.project_root.display().to_string()],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn backend_status_for_file(&self, file: &Path) -> Result<Option<String>> {
        let rel_path = relative_path(
            &self.project_root,
            &normalize_file_path(&self.project_root, file)?,
        );
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        conn.query_row(
            "SELECT status FROM backend_file_state
             WHERE backend = ?1 AND workspace_root = ?2 AND file_path = ?3
             ORDER BY updated_at DESC LIMIT 1",
            params![
                BACKEND_TREESITTER,
                self.project_root.display().to_string(),
                rel_path
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        edge_snapshot_with_conn(&conn)
    }

    pub fn indexed_file_count(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        indexed_file_count(&conn)
    }

    pub fn node_for(&self, file_rel: &Path, symbol: &str) -> Result<StoreNode> {
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        resolve_node_for_rel(&conn, &rel_path, symbol)
    }

    /// Return all positional nodes matching a legacy symbol query in a file.
    ///
    /// Consumers that need legacy compatibility can collapse these by
    /// `StoreNode::symbol` before deciding whether a query is ambiguous.
    pub fn nodes_for(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreNode>> {
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        nodes_for_file_matching_symbol(&conn, &rel_path, symbol)
    }

    /// Return all positional nodes matching a symbol query anywhere in the store.
    pub fn nodes_matching(&self, symbol: &str) -> Result<Vec<StoreNode>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        nodes_matching_symbol(&conn, symbol)
    }

    /// Return direct callers for an already-resolved `(file, scoped_symbol)` tuple.
    pub fn direct_callers_of(&self, file_rel: &Path, symbol: &str) -> Result<Vec<StoreCallSite>> {
        let abs_path = normalize_file_path(&self.project_root, file_rel)?;
        let rel_path = relative_path(&self.project_root, &abs_path);
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        direct_callers_for_tuple(&conn, &rel_path, symbol)
    }

    pub fn callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreCallersResult> {
        let target = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        let effective_depth = depth.max(1);
        let mut visited = HashSet::new();
        let mut callers = Vec::new();
        let mut depth_limited = false;
        let mut truncated = 0usize;
        collect_callers_recursive(
            &conn,
            &target.file,
            &target.symbol,
            effective_depth,
            0,
            &mut visited,
            &mut callers,
            &mut depth_limited,
            &mut truncated,
        )?;
        Ok(StoreCallersResult {
            target,
            callers,
            scanned_files: indexed_file_count(&conn)?,
            depth_limited,
            truncated,
        })
    }

    pub fn impact_of(
        &self,
        file_rel: &Path,
        symbol: &str,
        depth: usize,
    ) -> Result<StoreImpactResult> {
        let callers = self.callers_of(file_rel, symbol, depth)?;
        let target_parameters = callers
            .target
            .signature
            .as_deref()
            .map(|signature| callgraph::extract_parameters(signature, callers.target.lang))
            .unwrap_or_default();
        let enriched = callers
            .callers
            .iter()
            .map(|site| StoreImpactCaller {
                site: site.clone(),
                signature: site.caller.signature.clone(),
                is_entry_point: site.caller.is_entry_point,
                call_expression: read_source_line(
                    &self.project_root.join(&site.caller.file),
                    site.line,
                ),
                parameters: site
                    .caller
                    .signature
                    .as_deref()
                    .map(|signature| callgraph::extract_parameters(signature, site.caller.lang))
                    .unwrap_or_default(),
            })
            .collect();
        Ok(StoreImpactResult {
            target: callers.target,
            parameters: target_parameters,
            callers: enriched,
            depth_limited: callers.depth_limited,
            truncated: callers.truncated,
        })
    }

    pub fn outgoing_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        outgoing_calls_for_node(&conn, node)
    }

    pub fn unresolved_calls_of(&self, node: &StoreNode) -> Result<Vec<StoreUnresolvedCall>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        unresolved_calls_for_node(&conn, node)
    }

    pub fn call_tree(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::CallTreeNode> {
        let node = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        let mut visited = HashSet::new();
        call_tree_inner(&conn, &node, max_depth, 0, &mut visited)
    }

    pub fn trace_to(
        &self,
        file_rel: &Path,
        symbol: &str,
        max_depth: usize,
    ) -> Result<callgraph::TraceToResult> {
        let target = self.node_for(file_rel, symbol)?;
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        let effective_max = if max_depth == 0 { 10 } else { max_depth };

        #[derive(Clone)]
        struct PathElem {
            node: StoreNode,
        }

        let initial = vec![PathElem {
            node: target.clone(),
        }];
        let mut complete_paths = Vec::new();
        if target.is_entry_point {
            complete_paths.push(initial.clone());
        }

        let mut queue = vec![(initial, 0usize)];
        let mut max_depth_reached = false;
        let mut truncated_paths = 0usize;

        while let Some((path, depth)) = queue.pop() {
            if depth >= effective_max {
                max_depth_reached = true;
                continue;
            }
            let Some(current) = path.last() else {
                continue;
            };
            let callers =
                direct_callers_for_tuple(&conn, &current.node.file, &current.node.symbol)?;
            if callers.is_empty() {
                if path.len() > 1 {
                    truncated_paths += 1;
                }
                continue;
            }

            let mut has_new_path = false;
            for site in callers {
                if path.iter().any(|elem| {
                    elem.node.file == site.caller.file && elem.node.symbol == site.caller.symbol
                }) {
                    continue;
                }
                has_new_path = true;
                let mut new_path = path.clone();
                new_path.push(PathElem {
                    node: site.caller.clone(),
                });
                if site.caller.is_entry_point {
                    complete_paths.push(new_path.clone());
                }
                queue.push((new_path, depth + 1));
            }
            if !has_new_path && path.len() > 1 {
                truncated_paths += 1;
            }
        }

        let mut paths: Vec<callgraph::TracePath> = complete_paths
            .into_iter()
            .map(|mut elems| {
                elems.reverse();
                let hops = elems
                    .iter()
                    .enumerate()
                    .map(|(index, elem)| callgraph::TraceHop {
                        symbol: elem.node.symbol.clone(),
                        file: elem.node.file.clone(),
                        line: elem.node.line,
                        signature: elem.node.signature.clone(),
                        is_entry_point: index == 0 && elem.node.is_entry_point,
                    })
                    .collect();
                callgraph::TracePath { hops }
            })
            .collect();
        paths.sort_by(|left, right| {
            let left_entry = left
                .hops
                .first()
                .map(|hop| hop.symbol.as_str())
                .unwrap_or("");
            let right_entry = right
                .hops
                .first()
                .map(|hop| hop.symbol.as_str())
                .unwrap_or("");
            left_entry
                .cmp(right_entry)
                .then(left.hops.len().cmp(&right.hops.len()))
        });
        let entry_points_found = paths
            .iter()
            .filter_map(|path| path.hops.first())
            .filter(|hop| hop.is_entry_point)
            .map(|hop| (hop.file.clone(), hop.symbol.clone()))
            .collect::<HashSet<_>>()
            .len();

        Ok(callgraph::TraceToResult {
            target_symbol: target.symbol,
            target_file: target.file,
            total_paths: paths.len(),
            paths,
            entry_points_found,
            max_depth_reached,
            truncated_paths,
        })
    }

    pub fn trace_to_symbol_candidates(
        &self,
        to_symbol: &str,
    ) -> Result<Vec<callgraph::TraceToSymbolCandidate>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        let mut candidates_by_file: HashMap<String, u32> = HashMap::new();
        for node in nodes_matching_symbol(&conn, to_symbol)? {
            candidates_by_file
                .entry(node.file)
                .and_modify(|line| *line = (*line).min(node.line))
                .or_insert(node.line);
        }
        let mut candidates: Vec<_> = candidates_by_file
            .into_iter()
            .map(|(file, line)| callgraph::TraceToSymbolCandidate { file, line })
            .collect();
        candidates
            .sort_by(|left, right| left.file.cmp(&right.file).then(left.line.cmp(&right.line)));
        Ok(candidates)
    }

    pub fn trace_to_symbol(
        &self,
        file_rel: &Path,
        symbol: &str,
        to_symbol: &str,
        to_file: Option<&Path>,
        max_depth: usize,
    ) -> Result<callgraph::TraceToSymbolResult> {
        let origin = self.node_for(file_rel, symbol)?;
        let target_file = to_file
            .map(|path| normalize_file_path(&self.project_root, path))
            .transpose()?
            .map(|path| relative_path(&self.project_root, &path));
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        ensure_database_ready(&conn)?;
        let effective_max = if max_depth == 0 {
            10
        } else {
            max_depth.min(16)
        };

        let start_hop = trace_to_symbol_hop(&origin);
        if trace_to_symbol_matches_target(&origin, to_symbol, target_file.as_deref()) {
            return Ok(callgraph::TraceToSymbolResult {
                path: Some(vec![start_hop]),
                complete: true,
                reason: None,
            });
        }

        let mut queue = VecDeque::new();
        queue.push_back((origin.clone(), vec![start_hop], 0usize));
        let mut visited = HashSet::new();
        visited.insert((origin.file.clone(), origin.symbol.clone()));
        let mut max_depth_exhausted = false;

        while let Some((current, path, depth)) = queue.pop_front() {
            let callees = outgoing_calls_for_node(&conn, &current)?
                .into_iter()
                .filter_map(|site| site.target)
                .collect::<Vec<_>>();

            if depth >= effective_max {
                if callees
                    .iter()
                    .any(|node| !visited.contains(&(node.file.clone(), node.symbol.clone())))
                {
                    max_depth_exhausted = true;
                }
                continue;
            }

            for callee in callees {
                if !visited.insert((callee.file.clone(), callee.symbol.clone())) {
                    continue;
                }
                let mut next_path = path.clone();
                next_path.push(trace_to_symbol_hop(&callee));
                if trace_to_symbol_matches_target(&callee, to_symbol, target_file.as_deref()) {
                    return Ok(callgraph::TraceToSymbolResult {
                        path: Some(next_path),
                        complete: true,
                        reason: None,
                    });
                }
                queue.push_back((callee, next_path, depth + 1));
            }
        }

        if max_depth_exhausted {
            Ok(callgraph::TraceToSymbolResult {
                path: None,
                complete: false,
                reason: Some("max_depth_exhausted".to_string()),
            })
        } else {
            Ok(callgraph::TraceToSymbolResult {
                path: None,
                complete: true,
                reason: Some("no_path_found".to_string()),
            })
        }
    }
}

fn indexed_file_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
    Ok(count.max(0) as usize)
}

fn resolve_node_for_rel(conn: &Connection, rel_path: &str, symbol: &str) -> Result<StoreNode> {
    let candidates = nodes_for_file_matching_symbol(conn, rel_path, symbol)?;
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(AftError::SymbolNotFound {
            name: symbol.to_string(),
            file: rel_path.to_string(),
        }
        .into()),
        _ => Err(AftError::AmbiguousSymbol {
            name: symbol.to_string(),
            candidates: candidates
                .iter()
                .map(|candidate| candidate.symbol.clone())
                .collect(),
        }
        .into()),
    }
}

fn nodes_for_file_matching_symbol(
    conn: &Connection,
    rel_path: &str,
    symbol: &str,
) -> Result<Vec<StoreNode>> {
    let qualified_query = symbol.contains("::");
    let sql = if qualified_query {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.file_path = ?1 AND n.scoped_name = ?2
         ORDER BY n.scoped_name, n.start_line, n.start_col"
    } else {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.file_path = ?1 AND (n.scoped_name = ?2 OR n.name = ?2)
         ORDER BY n.scoped_name, n.start_line, n.start_col"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![rel_path, symbol], store_node_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn nodes_matching_symbol(conn: &Connection, symbol: &str) -> Result<Vec<StoreNode>> {
    let qualified_query = symbol.contains("::");
    let sql = if qualified_query {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.scoped_name = ?1
         ORDER BY n.file_path, n.scoped_name, n.start_line, n.start_col"
    } else {
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.scoped_name = ?1 OR n.name = ?1
         ORDER BY n.file_path, n.scoped_name, n.start_line, n.start_col"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![symbol], store_node_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn load_node_by_id(conn: &Connection, node_id: &str) -> Result<Option<StoreNode>> {
    conn.query_row(
        "SELECT n.id, n.file_path, n.scoped_name, n.name, n.kind, n.start_line, n.end_line,
                n.signature, n.exported, n.is_callgraph_entry_point, f.lang
         FROM nodes n JOIN files f ON f.path = n.file_path
         WHERE n.id = ?1",
        params![node_id],
        store_node_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn store_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreNode> {
    let start_line: u32 = row.get::<_, i64>(5)?.max(0) as u32;
    let end_line: u32 = row.get::<_, i64>(6)?.max(0) as u32;
    let lang_label_value: String = row.get(10)?;
    Ok(StoreNode {
        node_id: row.get(0)?,
        file: row.get(1)?,
        symbol: row.get(2)?,
        name: row.get(3)?,
        kind: row.get(4)?,
        line: start_line.saturating_add(1),
        end_line: end_line.saturating_add(1),
        signature: row.get(7)?,
        exported: row.get::<_, i64>(8)? != 0,
        is_entry_point: row.get::<_, i64>(9)? != 0,
        lang: lang_from_label(&lang_label_value).unwrap_or(LangId::TypeScript),
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_callers_recursive(
    conn: &Connection,
    file: &str,
    symbol: &str,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
    result: &mut Vec<StoreCallSite>,
    depth_limited: &mut bool,
    truncated: &mut usize,
) -> Result<()> {
    if current_depth >= max_depth {
        let omitted = direct_callers_for_tuple(conn, file, symbol)?.len();
        if omitted > 0 {
            *depth_limited = true;
            *truncated += omitted;
        }
        return Ok(());
    }

    if !visited.insert((file.to_string(), symbol.to_string())) {
        return Ok(());
    }

    let sites = direct_callers_for_tuple(conn, file, symbol)?;
    for site in sites {
        result.push(site.clone());
        if current_depth + 1 < max_depth {
            collect_callers_recursive(
                conn,
                &site.caller.file,
                &site.caller.symbol,
                max_depth,
                current_depth + 1,
                visited,
                result,
                depth_limited,
                truncated,
            )?;
        } else {
            let omitted =
                direct_callers_for_tuple(conn, &site.caller.file, &site.caller.symbol)?.len();
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
        }
    }
    Ok(())
}

fn direct_callers_for_tuple(
    conn: &Connection,
    target_file: &str,
    target_symbol: &str,
) -> Result<Vec<StoreCallSite>> {
    let mut stmt = conn.prepare(
        "SELECT e.source_node, e.target_node, e.target_file, e.target_symbol, e.line,
                r.byte_start, r.byte_end, r.status
         FROM edges e JOIN refs r ON r.ref_id = e.ref_id
         WHERE e.kind = 'call' AND e.target_file = ?1 AND e.target_symbol = ?2
         ORDER BY e.source_node, r.byte_start, r.line, r.ref_id",
    )?;
    let rows = stmt.query_map(params![target_file, target_symbol], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;

    let mut sites = Vec::new();
    for row in rows {
        let (
            source_node,
            target_node,
            target_file,
            target_symbol,
            line,
            byte_start,
            byte_end,
            status,
        ) = row?;
        let Some(caller) = load_node_by_id(conn, &source_node)? else {
            continue;
        };
        let target = target_node
            .as_deref()
            .map(|node_id| load_node_by_id(conn, node_id))
            .transpose()?
            .flatten();
        sites.push(StoreCallSite {
            caller,
            target_file,
            target_symbol,
            target,
            line: line.max(0) as u32,
            byte_start: byte_start.max(0) as usize,
            byte_end: byte_end.max(0) as usize,
            resolved: status == "resolved",
        });
    }
    Ok(sites)
}

fn outgoing_calls_for_node(conn: &Connection, node: &StoreNode) -> Result<Vec<StoreCallSite>> {
    let mut stmt = conn.prepare(
        "SELECT e.target_node, e.target_file, e.target_symbol, e.line,
                r.byte_start, r.byte_end, r.status
         FROM edges e JOIN refs r ON r.ref_id = e.ref_id
         WHERE e.kind = 'call' AND e.source_node = ?1
         ORDER BY r.byte_start, r.line, r.ref_id",
    )?;
    let rows = stmt.query_map(params![node.node_id], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut calls = Vec::new();
    for row in rows {
        let (target_node, target_file, target_symbol, line, byte_start, byte_end, status) = row?;
        let target = target_node
            .as_deref()
            .map(|node_id| load_node_by_id(conn, node_id))
            .transpose()?
            .flatten();
        calls.push(StoreCallSite {
            caller: node.clone(),
            target_file,
            target_symbol,
            target,
            line: line.max(0) as u32,
            byte_start: byte_start.max(0) as usize,
            byte_end: byte_end.max(0) as usize,
            resolved: status == "resolved",
        });
    }
    Ok(calls)
}

fn unresolved_calls_for_node(
    conn: &Connection,
    node: &StoreNode,
) -> Result<Vec<StoreUnresolvedCall>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(short_name, full_ref, ''), full_ref, line, byte_start, byte_end
         FROM refs
         WHERE caller_node = ?1 AND kind = 'call' AND status = 'unresolved'
         ORDER BY byte_start, line, ref_id",
    )?;
    let rows = stmt.query_map(params![node.node_id], |row| {
        Ok(StoreUnresolvedCall {
            caller: node.clone(),
            symbol: row.get(0)?,
            full_ref: row.get(1)?,
            line: row.get::<_, i64>(2)?.max(0) as u32,
            byte_start: row.get::<_, i64>(3)?.max(0) as usize,
            byte_end: row.get::<_, i64>(4)?.max(0) as usize,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn forward_calls_for_node(conn: &Connection, node: &StoreNode) -> Result<Vec<StoreForwardCall>> {
    let mut calls = Vec::new();
    calls.extend(
        outgoing_calls_for_node(conn, node)?
            .into_iter()
            .map(StoreForwardCall::Resolved),
    );
    calls.extend(
        unresolved_calls_for_node(conn, node)?
            .into_iter()
            .map(StoreForwardCall::Unresolved),
    );
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.line().cmp(&right.line()))
    });
    Ok(calls)
}

fn call_tree_inner(
    conn: &Connection,
    node: &StoreNode,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
) -> Result<callgraph::CallTreeNode> {
    let visit_key = (node.file.clone(), node.symbol.clone());
    if visited.contains(&visit_key) {
        return Ok(callgraph::CallTreeNode {
            name: node.symbol.clone(),
            file: node.file.clone(),
            line: node.line,
            signature: node.signature.clone(),
            resolved: true,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
        });
    }
    visited.insert(visit_key.clone());

    let calls = forward_calls_for_node(conn, node)?;
    let mut children = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    if current_depth < max_depth {
        for call in calls {
            match call {
                StoreForwardCall::Resolved(site) => {
                    if let Some(target) = site.target {
                        let child =
                            call_tree_inner(conn, &target, max_depth, current_depth + 1, visited)?;
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        children.push(callgraph::CallTreeNode {
                            name: site.target_symbol,
                            file: site.target_file,
                            line: site.line,
                            signature: None,
                            resolved: false,
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
                StoreForwardCall::Unresolved(call) => {
                    children.push(callgraph::CallTreeNode {
                        name: call.symbol,
                        file: call.caller.file,
                        line: call.line,
                        signature: None,
                        resolved: false,
                        children: Vec::new(),
                        depth_limited: false,
                        truncated: 0,
                    });
                }
            }
        }
    } else if !calls.is_empty() {
        depth_limited = true;
        truncated = calls.len();
    }

    visited.remove(&visit_key);
    Ok(callgraph::CallTreeNode {
        name: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        signature: node.signature.clone(),
        resolved: true,
        children,
        depth_limited,
        truncated,
    })
}

fn trace_to_symbol_hop(node: &StoreNode) -> callgraph::TraceToSymbolHop {
    callgraph::TraceToSymbolHop {
        symbol: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
    }
}

fn trace_to_symbol_matches_target(
    node: &StoreNode,
    to_symbol: &str,
    to_file: Option<&str>,
) -> bool {
    if !symbol_query_matches(&node.symbol, to_symbol) {
        return false;
    }
    match to_file {
        Some(file) => node.file == file,
        None => true,
    }
}

fn symbol_query_matches(symbol: &str, query: &str) -> bool {
    symbol == query || unqualified_name(symbol) == query
}

fn read_source_line(path: &Path, line: u32) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(|line| line.trim().to_string())
}

#[doc(hidden)]
pub fn live_callgraph_edge_snapshot(
    project_root: &Path,
    files: &[PathBuf],
) -> Result<BTreeSet<StoredEdge>> {
    let files = normalize_file_list(project_root, files)?;
    let mut graph = callgraph::CallGraph::new(project_root.to_path_buf());
    let mut file_data = Vec::new();
    for file in &files {
        let canon = canonicalize_path(file);
        let data = graph.build_file(&canon)?.clone();
        file_data.push((canon, data));
    }

    let mut edges = BTreeSet::new();
    for (caller_file, data) in &file_data {
        for (caller_symbol, call_sites) in &data.calls_by_symbol {
            for call_site in call_sites {
                let resolution = graph.resolve_cross_file_edge(
                    &call_site.full_callee,
                    &call_site.callee_name,
                    caller_file,
                    &data.import_block,
                );
                let (target_file, target_symbol) = match resolution {
                    EdgeResolution::Resolved { file, symbol } => (file, symbol),
                    EdgeResolution::Unresolved { callee_name } => {
                        if !callgraph::is_bare_callee(&call_site.full_callee, &callee_name) {
                            continue;
                        }
                        let Ok(target_symbol) = callgraph::resolve_symbol_query_in_data(
                            data,
                            caller_file,
                            &callee_name,
                        ) else {
                            continue;
                        };
                        (caller_file.clone(), target_symbol)
                    }
                };
                if target_file == *caller_file && target_symbol == *caller_symbol {
                    continue;
                }
                edges.insert(StoredEdge {
                    source_file: relative_path(project_root, caller_file),
                    source_symbol: caller_symbol.clone(),
                    target_file: relative_path(project_root, &target_file),
                    target_symbol,
                    kind: "call".to_string(),
                    line: call_site.line,
                });
            }
        }
    }
    Ok(edges)
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

fn configure_build_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            path                TEXT PRIMARY KEY,
            content_hash        TEXT NOT NULL,
            mtime_ns            INTEGER NOT NULL,
            size                INTEGER NOT NULL,
            lang                TEXT NOT NULL,
            is_dead_code_root   INTEGER NOT NULL DEFAULT 0,
            is_public_api       INTEGER NOT NULL DEFAULT 0,
            surface_fingerprint TEXT NOT NULL,
            indexed_at          INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS nodes (
            id                         TEXT PRIMARY KEY,
            file_path                  TEXT NOT NULL,
            name                       TEXT NOT NULL,
            scoped_name                TEXT NOT NULL,
            kind                       TEXT NOT NULL,
            start_line                 INTEGER NOT NULL,
            start_col                  INTEGER NOT NULL,
            end_line                   INTEGER NOT NULL,
            end_col                    INTEGER NOT NULL,
            range_ordinal              INTEGER NOT NULL,
            signature                  TEXT,
            exported                   INTEGER NOT NULL,
            is_default_export          INTEGER NOT NULL,
            is_type_like               INTEGER NOT NULL,
            is_callgraph_entry_point   INTEGER NOT NULL,
            provenance                 TEXT NOT NULL,
            UNIQUE(file_path, start_line, start_col, end_line, end_col, range_ordinal)
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
        CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
        CREATE INDEX IF NOT EXISTS idx_nodes_scoped ON nodes(scoped_name);

        CREATE TABLE IF NOT EXISTS refs (
            ref_id          TEXT PRIMARY KEY,
            caller_node     TEXT,
            caller_file     TEXT NOT NULL,
            kind            TEXT NOT NULL,
            short_name      TEXT,
            full_ref        TEXT,
            module_path     TEXT,
            import_kind     TEXT,
            local_name      TEXT,
            requested_name  TEXT,
            namespace_alias TEXT,
            wildcard        INTEGER NOT NULL DEFAULT 0,
            line            INTEGER NOT NULL,
            byte_start      INTEGER NOT NULL,
            byte_end        INTEGER NOT NULL,
            status          TEXT NOT NULL,
            target_node     TEXT,
            target_file     TEXT,
            target_symbol   TEXT,
            provenance      TEXT NOT NULL,
            raw_payload     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_refs_short_name ON refs(short_name);
        CREATE INDEX IF NOT EXISTS idx_refs_caller_file ON refs(caller_file);
        CREATE INDEX IF NOT EXISTS idx_refs_caller_node_kind ON refs(caller_node, kind, status);
        CREATE INDEX IF NOT EXISTS idx_refs_target_file ON refs(target_file);

        CREATE TABLE IF NOT EXISTS ref_dependencies (
            ref_id      TEXT NOT NULL,
            dep_file    TEXT NOT NULL,
            PRIMARY KEY(ref_id, dep_file)
        );
        CREATE INDEX IF NOT EXISTS idx_ref_dependencies_dep_file ON ref_dependencies(dep_file);

        CREATE TABLE IF NOT EXISTS edges (
            edge_id       TEXT PRIMARY KEY,
            ref_id        TEXT NOT NULL,
            source_node   TEXT NOT NULL,
            target_node   TEXT,
            target_file   TEXT NOT NULL,
            target_symbol TEXT NOT NULL,
            kind          TEXT NOT NULL,
            line          INTEGER NOT NULL,
            provenance    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source_node, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target_node, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_target_file_symbol ON edges(target_file, target_symbol, kind);

        CREATE TABLE IF NOT EXISTS dispatch_hints (
            id           TEXT PRIMARY KEY,
            method_name  TEXT NOT NULL,
            caller_node  TEXT NOT NULL,
            file         TEXT NOT NULL,
            line         INTEGER NOT NULL,
            byte_start   INTEGER NOT NULL,
            byte_end     INTEGER NOT NULL,
            provenance   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_hints_method ON dispatch_hints(method_name);

        CREATE TABLE IF NOT EXISTS type_ref_names (
            name TEXT PRIMARY KEY
        );

        CREATE TABLE IF NOT EXISTS backend_file_state (
            backend        TEXT NOT NULL,
            workspace_root TEXT NOT NULL,
            file_path      TEXT NOT NULL,
            content_hash   TEXT NOT NULL,
            status         TEXT NOT NULL,
            updated_at     INTEGER NOT NULL,
            PRIMARY KEY(backend, workspace_root, file_path, content_hash)
        );
        CREATE INDEX IF NOT EXISTS idx_backend_file_state_file ON backend_file_state(file_path, backend);

        CREATE TABLE IF NOT EXISTS meta (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
        );",
    )?;
    insert_meta(conn)?;
    Ok(())
}

fn insert_meta(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('fingerprint', ?1)",
        params![schema_fingerprint()],
    )?;
    Ok(())
}

fn set_meta_ready(conn: &Connection, ready: bool) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(k, v) VALUES('ready', ?1)",
        params![if ready { "1" } else { "0" }],
    )?;
    Ok(())
}

fn database_ready(conn: &Connection) -> Result<bool> {
    let schema_version: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'schema_version'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let fingerprint: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'fingerprint'", [], |row| {
            row.get(0)
        })
        .optional()?;
    let ready: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = 'ready'", [], |row| row.get(0))
        .optional()?;

    let expected_schema = SCHEMA_VERSION.to_string();
    let expected_fingerprint = schema_fingerprint();
    Ok(schema_version.as_deref() == Some(expected_schema.as_str())
        && fingerprint.as_deref() == Some(expected_fingerprint.as_str())
        && ready.as_deref() == Some("1"))
}

fn ensure_database_ready(conn: &Connection) -> Result<()> {
    if database_ready(conn)? {
        Ok(())
    } else {
        Err(CallGraphStoreError::Unavailable(
            "database is missing, stale, or mid-build".to_string(),
        ))
    }
}

fn schema_fingerprint() -> String {
    let input = format!("callgraph_store:v{SCHEMA_VERSION}:positional:raw-ref:v4");
    hash_to_hex(blake3::hash(input.as_bytes()))
}

fn clear_tables(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "DELETE FROM edges;
         DELETE FROM ref_dependencies;
         DELETE FROM refs;
         DELETE FROM dispatch_hints;
         DELETE FROM type_ref_names;
         DELETE FROM backend_file_state;
         DELETE FROM nodes;
         DELETE FROM files;",
    )?;
    Ok(())
}

fn remove_sqlite_file_set(path: &Path) {
    let _ = std::fs::remove_file(path);
    remove_sqlite_sidecars(path);
}

fn remove_sqlite_sidecars(path: &Path) {
    let path_text = path.to_string_lossy();
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-wal")));
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-shm")));
    let _ = std::fs::remove_file(PathBuf::from(format!("{path_text}-journal")));
}

fn build_extracts_parallel(project_root: &Path, files: &[PathBuf]) -> BuildExtractsResult {
    let results: Vec<std::result::Result<FileExtract, ExtractFailure>> = files
        .par_iter()
        .map(|path| match build_file_extract(project_root, path) {
            Ok(extract) => Ok(extract),
            Err(error) => {
                let abs_path =
                    normalize_file_path(project_root, path).unwrap_or_else(|_| path.to_path_buf());
                let rel_path = relative_path(project_root, &abs_path);
                let freshness = cache_freshness::collect(&abs_path).ok();
                log::debug!(
                    "callgraph store: skipping {} during cold build: {}",
                    abs_path.display(),
                    error
                );
                Err(ExtractFailure {
                    rel_path,
                    freshness,
                })
            }
        })
        .collect();

    let mut extracts = Vec::new();
    let mut failures = Vec::new();
    for result in results {
        match result {
            Ok(extract) => extracts.push(extract),
            Err(failure) => failures.push(failure),
        }
    }
    BuildExtractsResult { extracts, failures }
}

fn build_file_extract(project_root: &Path, path: &Path) -> Result<FileExtract> {
    let abs_path = normalize_file_path(project_root, path)?;
    let rel_path = relative_path(project_root, &abs_path);
    let source = std::fs::read_to_string(&abs_path)?;
    let freshness = cache_freshness::collect(&abs_path)?;
    let data = callgraph::build_file_data(&abs_path)?;
    let lang = data.lang;
    let mut nodes = build_node_records(&rel_path, &source, &data)?;
    let node_by_scoped: HashMap<String, String> = nodes
        .iter()
        .map(|node| (node.scoped_name.clone(), node.id.clone()))
        .collect();
    let import_dependencies =
        import_dependencies(project_root, &abs_path, &data.import_block.imports);
    let reexports = collect_reexport_refs(project_root, &abs_path, &rel_path, &source);
    let source_less_exports = collect_source_less_export_alias_refs(&rel_path, &source);
    let mut raw_refs = Vec::new();
    raw_refs.extend(build_call_refs(
        &rel_path,
        &data,
        &node_by_scoped,
        &import_dependencies,
    )?);
    raw_refs.extend(build_import_refs(
        project_root,
        &abs_path,
        &rel_path,
        &data.import_block.imports,
    )?);
    let mut surface_parts = reexports.surface_parts;
    surface_parts.extend(source_less_exports.surface_parts);
    raw_refs.extend(reexports.raw_refs);
    raw_refs.extend(source_less_exports.raw_refs);
    let dispatch_hints = build_dispatch_hints(&rel_path, &data, &node_by_scoped);
    let surface_fingerprint = surface_fingerprint(&mut nodes, &data, &surface_parts);

    Ok(FileExtract {
        abs_path,
        rel_path,
        freshness,
        lang,
        data,
        nodes,
        raw_refs,
        dispatch_hints,
        surface_fingerprint,
    })
}

fn build_node_records(
    rel_path: &str,
    source: &str,
    data: &FileCallData,
) -> Result<Vec<NodeRecord>> {
    let mut records = Vec::new();
    let mut ordinal_by_range: BTreeMap<(u32, u32, u32, u32), u32> = BTreeMap::new();
    let mut metadata: Vec<_> = data.symbol_metadata.iter().collect();
    metadata.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (scoped_name, meta) in metadata {
        let name = unqualified_name(scoped_name).to_string();
        let range = selection_range(source, scoped_name, &name, &meta.range);
        let range_key = (
            range.start_line,
            range.start_col,
            range.end_line,
            range.end_col,
        );
        let ordinal = ordinal_by_range.entry(range_key).or_insert(0);
        let range_ordinal = *ordinal;
        *ordinal += 1;
        let id = node_id(rel_path, &range, range_ordinal, scoped_name);
        let exported = meta.exported || data.exported_symbols.iter().any(|item| item == &name);
        let is_default_export = data
            .default_export_symbol
            .as_deref()
            .map(|default| default == scoped_name || default == name)
            .unwrap_or(false);
        records.push(NodeRecord {
            id,
            file_path: rel_path.to_string(),
            name: name.clone(),
            scoped_name: scoped_name.clone(),
            kind: symbol_kind_label(&meta.kind).to_string(),
            range,
            range_ordinal,
            signature: meta.signature.clone(),
            exported,
            is_default_export,
            is_type_like: is_type_like(&meta.kind),
            is_callgraph_entry_point: callgraph::is_entry_point(
                scoped_name,
                &meta.kind,
                exported,
                data.lang,
            ),
        });
    }

    Ok(records)
}

fn selection_range(source: &str, scoped_name: &str, name: &str, fallback: &Range) -> Range {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return Range {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        };
    }
    let Some(line) = source.lines().nth(fallback.start_line as usize) else {
        return fallback.clone();
    };
    let start_col = fallback.start_col as usize;
    let search_start = start_col.min(line.len());
    if let Some(offset) = line[search_start..].find(name) {
        let col = search_start + offset;
        return Range {
            start_line: fallback.start_line,
            start_col: col as u32,
            end_line: fallback.start_line,
            end_col: (col + name.len()) as u32,
        };
    }
    if let Some(offset) = line.find(name) {
        return Range {
            start_line: fallback.start_line,
            start_col: offset as u32,
            end_line: fallback.start_line,
            end_col: (offset + name.len()) as u32,
        };
    }
    Range {
        start_line: fallback.start_line,
        start_col: fallback.start_col,
        end_line: fallback.start_line,
        end_col: fallback.start_col.saturating_add(name.len() as u32),
    }
}

fn node_id(rel_path: &str, range: &Range, ordinal: u32, scoped_name: &str) -> String {
    if scoped_name == TOP_LEVEL_SYMBOL {
        return format!("top:{}", hash_to_hex(blake3::hash(rel_path.as_bytes())));
    }
    let input = format!(
        "{rel_path}:{}:{}:{}:{}:{ordinal}",
        range.start_line, range.start_col, range.end_line, range.end_col
    );
    format!("pos:{}", hash_to_hex(blake3::hash(input.as_bytes())))
}

fn build_call_refs(
    rel_path: &str,
    data: &FileCallData,
    node_by_scoped: &HashMap<String, String>,
    import_dependencies: &BTreeSet<String>,
) -> Result<Vec<RawRef>> {
    let mut refs = Vec::new();
    let mut ordinal = 0usize;
    let mut symbols: Vec<_> = data.calls_by_symbol.iter().collect();
    symbols.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (caller_symbol, call_sites) in symbols {
        let caller_node = node_by_scoped.get(caller_symbol).cloned();
        for call_site in call_sites {
            ordinal += 1;
            let ref_id = ref_id(&[
                rel_path,
                "call",
                caller_symbol,
                &call_site.line.to_string(),
                &call_site.byte_start.to_string(),
                &call_site.byte_end.to_string(),
                &call_site.full_callee,
                &ordinal.to_string(),
            ]);
            let raw_payload = serde_json::to_string(&json!({
                "kind": "call",
                "caller_symbol": caller_symbol,
                "short_name": call_site.callee_name,
                "full_ref": call_site.full_callee,
                "byte_range": {"start": call_site.byte_start, "end": call_site.byte_end}
            }))?;
            refs.push(RawRef {
                ref_id,
                caller_node: caller_node.clone(),
                caller_symbol: Some(caller_symbol.clone()),
                caller_file: rel_path.to_string(),
                kind: "call".to_string(),
                short_name: Some(call_site.callee_name.clone()),
                full_ref: Some(call_site.full_callee.clone()),
                module_path: None,
                import_kind: None,
                local_name: Some(call_site.callee_name.clone()),
                requested_name: Some(call_site.callee_name.clone()),
                namespace_alias: namespace_alias(&call_site.full_callee),
                wildcard: false,
                line: call_site.line,
                byte_start: call_site.byte_start,
                byte_end: call_site.byte_end,
                raw_payload,
                dependencies: import_dependencies.clone(),
            });
        }
    }
    Ok(refs)
}

fn build_import_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    imports: &[ImportStatement],
) -> Result<Vec<RawRef>> {
    let mut refs = Vec::new();
    for (index, import) in imports.iter().enumerate() {
        let payload = import_payload(import)?;
        let import_kind = import_kind_label(import.kind).to_string();
        let local_name = import_local_names(import).join(",");
        let requested_name = import_requested_names(import).join(",");
        let ref_id = ref_id(&[
            rel_path,
            "import",
            &import.byte_range.start.to_string(),
            &import.byte_range.end.to_string(),
            &import.module_path,
            &index.to_string(),
        ]);
        refs.push(RawRef {
            ref_id,
            caller_node: None,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "import".to_string(),
            short_name: None,
            full_ref: Some(import.raw_text.clone()),
            module_path: Some(import.module_path.clone()),
            import_kind: Some(import_kind),
            local_name: empty_to_none(local_name),
            requested_name: empty_to_none(requested_name),
            namespace_alias: import.namespace_import.clone(),
            wildcard: import_is_wildcard(import),
            line: byte_to_line(abs_path, import.byte_range.start).unwrap_or(1),
            byte_start: import.byte_range.start,
            byte_end: import.byte_range.end,
            raw_payload: payload,
            dependencies: module_dependencies(project_root, abs_path, &import.module_path),
        });
    }
    Ok(refs)
}

#[derive(Debug, Clone)]
struct ReexportRefs {
    raw_refs: Vec<RawRef>,
    surface_parts: Vec<String>,
}

fn collect_reexport_refs(
    project_root: &Path,
    abs_path: &Path,
    rel_path: &str,
    source: &str,
) -> ReexportRefs {
    let mut raw_refs = Vec::new();
    let mut surface_parts = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if !statement.contains(" from ") || !statement.contains(['\'', '"']) {
            continue;
        }
        let Some(module_path) = quoted_module_path(statement) else {
            continue;
        };
        ordinal += 1;
        let wildcard = statement.contains('*');
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1;
        let ref_id = ref_id(&[
            rel_path,
            "reexport",
            &start.to_string(),
            &end.to_string(),
            &module_path,
            &ordinal.to_string(),
        ]);
        surface_parts.push(format!("reexport\t{statement}"));
        let raw_payload = serde_json::to_string(&json!({
            "kind": "reexport",
            "module_path": module_path,
            "raw_text": statement,
            "wildcard": wildcard,
            "byte_range": {"start": start, "end": end}
        }))
        .unwrap_or_else(|_| "{}".to_string());
        raw_refs.push(RawRef {
            ref_id,
            caller_node: None,
            caller_symbol: None,
            caller_file: rel_path.to_string(),
            kind: "reexport".to_string(),
            short_name: None,
            full_ref: Some(statement.to_string()),
            module_path: Some(module_path.clone()),
            import_kind: Some("reexport".to_string()),
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard,
            line,
            byte_start: start,
            byte_end: end,
            raw_payload,
            dependencies: module_dependencies(project_root, abs_path, &module_path),
        });
    }
    ReexportRefs {
        raw_refs,
        surface_parts,
    }
}

fn quoted_module_path(statement: &str) -> Option<String> {
    let quote = match (statement.find('\''), statement.find('"')) {
        (Some(single), Some(double)) if single < double => '\'',
        (Some(_), Some(_)) => '"',
        (Some(_), None) => '\'',
        (None, Some(_)) => '"',
        (None, None) => return None,
    };
    let start = statement.find(quote)? + 1;
    let end = statement[start..].find(quote)? + start;
    Some(statement[start..end].to_string())
}

#[derive(Debug, Clone)]
struct SourceLessExportRefs {
    raw_refs: Vec<RawRef>,
    surface_parts: Vec<String>,
}

fn collect_source_less_export_alias_refs(rel_path: &str, source: &str) -> SourceLessExportRefs {
    let mut raw_refs = Vec::new();
    let mut surface_parts = Vec::new();
    let mut search_start = 0usize;
    let mut ordinal = 0usize;
    while let Some(export_offset) = source[search_start..].find("export") {
        let start = search_start + export_offset;
        let Some(statement_end_offset) = source[start..].find(';') else {
            break;
        };
        let end = start + statement_end_offset + 1;
        let statement = &source[start..end];
        search_start = end;
        if statement.contains(" from ") || !statement.contains('{') || !statement.contains('}') {
            continue;
        }
        let aliases = parse_reexport_names(statement);
        if aliases.is_empty() {
            continue;
        }
        let line = source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1;
        for (exported, source_symbol) in aliases {
            ordinal += 1;
            let ref_id = ref_id(&[
                rel_path,
                "export_alias",
                &start.to_string(),
                &end.to_string(),
                &exported,
                &source_symbol,
                &ordinal.to_string(),
            ]);
            surface_parts.push(format!("export_alias\t{source_symbol}\t{exported}"));
            let raw_payload = serde_json::to_string(&json!({
                "kind": "export_alias",
                "source": source_symbol,
                "exported": exported,
                "raw_text": statement,
                "byte_range": {"start": start, "end": end}
            }))
            .unwrap_or_else(|_| "{}".to_string());
            raw_refs.push(RawRef {
                ref_id,
                caller_node: None,
                caller_symbol: None,
                caller_file: rel_path.to_string(),
                kind: "export_alias".to_string(),
                short_name: None,
                full_ref: Some(statement.to_string()),
                module_path: None,
                import_kind: Some("export_alias".to_string()),
                local_name: Some(exported),
                requested_name: Some(source_symbol),
                namespace_alias: None,
                wildcard: false,
                line,
                byte_start: start,
                byte_end: end,
                raw_payload,
                dependencies: BTreeSet::new(),
            });
        }
    }
    SourceLessExportRefs {
        raw_refs,
        surface_parts,
    }
}

fn build_dispatch_hints(
    rel_path: &str,
    data: &FileCallData,
    node_by_scoped: &HashMap<String, String>,
) -> Vec<DispatchHint> {
    let mut hints = Vec::new();
    let mut ordinal = 0usize;
    for (caller_symbol, call_sites) in &data.calls_by_symbol {
        let Some(caller_node) = node_by_scoped.get(caller_symbol) else {
            continue;
        };
        for call_site in call_sites {
            if !(call_site.full_callee.contains('.') || call_site.full_callee.contains("::")) {
                continue;
            }
            ordinal += 1;
            hints.push(DispatchHint {
                id: ref_id(&[
                    rel_path,
                    "dispatch",
                    caller_symbol,
                    &call_site.line.to_string(),
                    &call_site.byte_start.to_string(),
                    &call_site.byte_end.to_string(),
                    &ordinal.to_string(),
                ]),
                method_name: call_site.callee_name.clone(),
                caller_node: caller_node.clone(),
                file: rel_path.to_string(),
                line: call_site.line,
                byte_start: call_site.byte_start,
                byte_end: call_site.byte_end,
            });
        }
    }
    hints
}

fn surface_fingerprint(
    nodes: &mut [NodeRecord],
    data: &FileCallData,
    reexport_parts: &[String],
) -> String {
    nodes.sort_by(|left, right| {
        (left.file_path.as_str(), left.scoped_name.as_str())
            .cmp(&(right.file_path.as_str(), right.scoped_name.as_str()))
    });
    let mut parts = Vec::new();
    for node in nodes.iter() {
        parts.push(format!(
            "node\t{}\t{}\t{}\t{}\t{}:{}:{}:{}:{}\t{}",
            node.scoped_name,
            node.name,
            node.kind,
            node.exported,
            node.range.start_line,
            node.range.start_col,
            node.range.end_line,
            node.range.end_col,
            node.range_ordinal,
            node.signature.as_deref().unwrap_or("")
        ));
    }
    let mut exports = data.exported_symbols.clone();
    exports.sort();
    for export in exports {
        parts.push(format!("export\t{export}"));
    }
    if let Some(default_export) = &data.default_export_symbol {
        parts.push(format!("default\t{default_export}"));
    }
    let mut imports: Vec<String> = data
        .import_block
        .imports
        .iter()
        .map(|import| {
            format!(
                "import\t{}\t{:?}\t{}",
                import.module_path, import.form, import.raw_text
            )
        })
        .collect();
    imports.sort();
    parts.extend(imports);
    parts.extend(reexport_parts.iter().cloned());
    hash_to_hex(blake3::hash(parts.join("\n").as_bytes()))
}

fn resolve_ref(raw: RawRef, index: &ProjectIndex<'_>) -> Result<ResolvedRef> {
    if raw.kind != "call" {
        return Ok(ResolvedRef {
            dependencies: raw.dependencies.clone(),
            raw,
            status: "unresolved".to_string(),
            target_node: None,
            target_file: None,
            target_symbol: None,
            edge: None,
        });
    }

    let caller_file = raw.caller_file.clone();
    let caller_data = index.caller_data.get(&caller_file).ok_or_else(|| {
        CallGraphStoreError::MissingCallerData {
            file: caller_file.clone(),
        }
    })?;
    let full_ref = raw.full_ref.as_deref().unwrap_or_default();
    let short_name = raw.short_name.as_deref().unwrap_or_default();
    let mut dependencies = raw.dependencies.clone();

    let resolved = match index.lang_for(&caller_file) {
        Some(LangId::Rust) => {
            resolve_rust_target(index, &caller_file, full_ref, short_name, caller_data)
        }
        Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript) => {
            resolve_js_ts_target(index, &caller_file, full_ref, short_name, caller_data)
        }
        _ => resolve_local_target(index, &caller_file, full_ref, short_name, caller_data),
    };

    let Some((status, target_file, target_symbol)) = resolved else {
        return Ok(ResolvedRef {
            raw,
            status: "unresolved".to_string(),
            target_node: None,
            target_file: None,
            target_symbol: None,
            dependencies,
            edge: None,
        });
    };

    dependencies.insert(target_file.clone());
    let target_node = index.node_for_symbol(&target_file, &target_symbol);
    let source_node = raw.caller_node.clone();
    let edge = if let Some(source_node) = source_node {
        if target_file == caller_file
            && raw.caller_symbol.as_deref() == Some(target_symbol.as_str())
        {
            None
        } else {
            Some(EdgeRecord {
                edge_id: ref_id(&[&raw.ref_id, "edge"]),
                source_node,
                target_node: target_node.clone(),
                target_file: target_file.clone(),
                target_symbol: target_symbol.clone(),
                kind: "call".to_string(),
                line: raw.line,
            })
        }
    } else {
        None
    };

    Ok(ResolvedRef {
        raw,
        status,
        target_node,
        target_file: Some(target_file),
        target_symbol: Some(target_symbol),
        dependencies,
        edge,
    })
}

fn resolve_js_ts_target(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if let Some((namespace, member)) = full_ref.split_once('.') {
        for import in &caller_data.import_block.imports {
            if import.namespace_import.as_deref() == Some(namespace) {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    if let Some((file, symbol)) =
                        resolve_exported_symbol(index, &target_file, member, 0)
                    {
                        return Some(("resolved".to_string(), file, symbol));
                    }
                }
            }
        }
    }

    for import in &caller_data.import_block.imports {
        for spec in &import.names {
            if crate::imports::specifier_local_name(spec) == short_name {
                if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                    let requested = crate::imports::specifier_imported_name(spec);
                    let (file, symbol) = resolve_exported_symbol(index, &target_file, requested, 0)
                        .unwrap_or_else(|| (target_file, requested.to_string()));
                    return Some(("resolved".to_string(), file, symbol));
                }
            }
        }

        if import.default_import.as_deref() == Some(short_name) {
            if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
                let (file, symbol) = resolve_exported_symbol(index, &target_file, "default", 0)
                    .or_else(|| {
                        index
                            .files
                            .get(&target_file)
                            .and_then(|file| file.default_export.clone())
                            .map(|symbol| (target_file.clone(), symbol))
                    })
                    .unwrap_or_else(|| {
                        let file_name = Path::new(&target_file)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        (target_file, format!("<default:{file_name}>"))
                    });
                return Some(("resolved".to_string(), file, symbol));
            }
        }
    }

    for import in &caller_data.import_block.imports {
        if let Some(target_file) = index.module_target(caller_file, &import.module_path) {
            if index
                .files
                .get(&target_file)
                .map(|file| file.exports.contains(short_name))
                .unwrap_or(false)
            {
                return Some(("resolved".to_string(), target_file, short_name.to_string()));
            }
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn resolve_exported_symbol(
    index: &ProjectIndex<'_>,
    file: &str,
    requested: &str,
    depth: usize,
) -> Option<(String, String)> {
    if depth > 16 {
        return None;
    }
    if requested != "default" {
        if let Some(source_symbol) = index
            .files
            .get(file)
            .and_then(|item| item.export_aliases.get(requested))
        {
            return Some((file.to_string(), source_symbol.clone()));
        }
        if index
            .files
            .get(file)
            .map(|item| item.exports.contains(requested))
            .unwrap_or(false)
        {
            return Some((file.to_string(), requested.to_string()));
        }
    } else if let Some(default) = index
        .files
        .get(file)
        .and_then(|item| item.default_export.clone())
    {
        return Some((file.to_string(), default));
    }

    for reexport in index.reexports_for(file) {
        let mut next_requested = requested.to_string();
        let matches = if reexport.wildcard {
            true
        } else if let Some(source_name) = reexport.named.get(requested) {
            next_requested = source_name.clone();
            true
        } else {
            false
        };
        if !matches {
            continue;
        }
        if let Some(target_file) = &reexport.target_file {
            if let Some(target) =
                resolve_exported_symbol(index, target_file, &next_requested, depth + 1)
            {
                return Some(target);
            }
        }
    }
    None
}

fn resolve_rust_target(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if full_ref.contains("::") {
        if let Some(target_file) = rust_target_for_qualified(index, caller_file, full_ref) {
            return Some((
                "resolved".to_string(),
                target_file,
                rust_target_symbol(full_ref, short_name),
            ));
        }
    }

    for import in &caller_data.import_block.imports {
        if let Some((target_file, target_symbol)) =
            rust_target_for_use(index, caller_file, import, short_name)
        {
            return Some(("resolved".to_string(), target_file, target_symbol));
        }
    }

    resolve_local_target(index, caller_file, full_ref, short_name, caller_data)
}

fn rust_target_for_qualified(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
) -> Option<String> {
    let mut segments: Vec<&str> = full_ref.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    segments.pop();
    if !matches!(segments.first().copied(), Some("crate" | "self" | "super")) {
        if let Some(target) = rust_workspace_file_for_segments(index, &segments) {
            return Some(target);
        }
    }
    let module_segments = rust_resolve_segments(caller_file, &segments)?;
    rust_file_for_segments(index, caller_file, &module_segments)
}

fn rust_target_symbol(full_ref: &str, short_name: &str) -> String {
    full_ref
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(short_name)
        .to_string()
}

fn rust_target_for_use(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    import: &ImportStatement,
    short_name: &str,
) -> Option<(String, String)> {
    let path = import.module_path.trim().trim_end_matches(';');
    if let Some(brace_start) = path.find("::{") {
        let prefix = &path[..brace_start];
        if import.names.iter().any(|name| name == short_name) {
            let prefix_segments: Vec<&str> = prefix.split("::").collect();
            let module_segments = rust_resolve_segments(caller_file, &prefix_segments)?;
            let file = rust_file_for_segments(index, caller_file, &module_segments)?;
            return Some((file, short_name.to_string()));
        }
        return None;
    }

    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments: Vec<&str> = path_without_alias.split("::").collect();
    let imported = alias.or_else(|| segments.last().copied())?;
    if imported != short_name {
        return None;
    }
    if segments.len() < 2 {
        return None;
    }
    let module_segments = rust_resolve_segments(caller_file, &segments[..segments.len() - 1])?;
    let file = rust_file_for_segments(index, caller_file, &module_segments)?;
    Some((file, segments.last().unwrap_or(&short_name).to_string()))
}

fn rust_workspace_file_for_segments(index: &ProjectIndex<'_>, segments: &[&str]) -> Option<String> {
    let crate_name = segments.first().copied()?;
    let src_prefix = index.crate_src_prefix(crate_name)?;
    let module_segments = segments[1..]
        .iter()
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    rust_file_for_src_prefix(index, &src_prefix, &module_segments)
}

/// Walk the project tree once and map every Rust crate name (package name with
/// `-` normalized to `_`, plus any explicit `[lib] name`) to its `src` prefix.
/// Replaces the previous per-ref tree walk: resolving 600k+ qualified refs no
/// longer re-walks the filesystem once per ref.
fn build_workspace_crate_prefixes(project_root: &Path) -> HashMap<String, String> {
    let mut prefixes = HashMap::new();
    let mut stack = vec![project_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if matches!(name, "target" | "node_modules" | ".git") {
            continue;
        }
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let crate_names = rust_manifest_crate_names(&manifest);
            if !crate_names.is_empty() {
                let src_prefix = relative_path(project_root, &canonicalize_path(&dir.join("src")));
                for crate_name in crate_names {
                    prefixes
                        .entry(crate_name)
                        .or_insert_with(|| src_prefix.clone());
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    prefixes
}

/// Extract the crate names a manifest defines: the normalized package name
/// (`-` -> `_`) and any explicit `[lib] name`. Returns both so a crate is
/// reachable by either spelling, matching the previous match semantics.
fn rust_manifest_crate_names(manifest: &Path) -> Vec<String> {
    let Ok(source) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let mut in_lib = false;
    let mut package_name = None;
    let mut lib_name = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_lib = trimmed == "[lib]";
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if in_lib && key == "name" {
            lib_name = Some(value.to_string());
        } else if !in_lib && key == "name" && package_name.is_none() {
            package_name = Some(value.to_string());
        }
    }
    let mut names = Vec::new();
    if let Some(lib) = lib_name {
        names.push(lib);
    }
    if let Some(package) = package_name {
        let normalized = package.replace('-', "_");
        if !names.contains(&normalized) {
            names.push(normalized);
        }
    }
    names
}

fn rust_resolve_segments(caller_file: &str, segments: &[&str]) -> Option<Vec<String>> {
    if segments.is_empty() {
        return Some(Vec::new());
    }
    let caller_segments = rust_module_segments_for_rel(caller_file);
    match segments[0] {
        "crate" => Some(segments[1..].iter().map(|item| item.to_string()).collect()),
        "self" => {
            let mut resolved = caller_segments;
            resolved.extend(segments[1..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        "super" => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments[1..].iter().map(|item| item.to_string()));
            Some(resolved)
        }
        _ => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments.iter().map(|item| item.to_string()));
            Some(resolved)
        }
    }
}

fn rust_file_for_segments(
    index: &ProjectIndex<'_>,
    caller_file: &str,
    segments: &[String],
) -> Option<String> {
    rust_file_for_src_prefix(index, &rust_src_prefix(caller_file), segments)
}

fn rust_file_for_src_prefix(
    index: &ProjectIndex<'_>,
    src_prefix: &str,
    segments: &[String],
) -> Option<String> {
    let candidate = if segments.is_empty() {
        [src_prefix, "lib.rs"].join("/")
    } else {
        format!("{}/{}.rs", src_prefix, segments.join("/"))
    };
    if index.files.contains_key(&candidate) {
        return Some(candidate);
    }
    if !segments.is_empty() {
        let mod_candidate = format!("{}/{}/mod.rs", src_prefix, segments.join("/"));
        if index.files.contains_key(&mod_candidate) {
            return Some(mod_candidate);
        }
    }
    None
}

fn rust_src_prefix(rel_path: &str) -> String {
    rel_path
        .split_once("/src/")
        .map(|(prefix, _)| format!("{prefix}/src"))
        .unwrap_or_else(|| "src".to_string())
}

fn rust_module_segments_for_rel(rel_path: &str) -> Vec<String> {
    let after_src = rel_path
        .split_once("/src/")
        .map(|(_, rest)| rest)
        .or_else(|| rel_path.strip_prefix("src/"))
        .unwrap_or(rel_path);
    if matches!(after_src, "lib.rs" | "main.rs") {
        return Vec::new();
    }
    if let Some(prefix) = after_src.strip_suffix("/mod.rs") {
        return prefix.split('/').map(|item| item.to_string()).collect();
    }
    after_src
        .strip_suffix(".rs")
        .unwrap_or(after_src)
        .split('/')
        .map(|item| item.to_string())
        .collect()
}

fn resolve_local_target(
    _index: &ProjectIndex<'_>,
    caller_file: &str,
    full_ref: &str,
    short_name: &str,
    caller_data: &FileCallData,
) -> Option<(String, String, String)> {
    if !callgraph::is_bare_callee(full_ref, short_name) {
        return None;
    }
    callgraph::resolve_symbol_query_in_data(caller_data, Path::new(caller_file), short_name)
        .ok()
        .map(|symbol| {
            (
                "resolved_local".to_string(),
                caller_file.to_string(),
                symbol,
            )
        })
}

impl<'a> ProjectIndex<'a> {
    fn from_extracts(project_root: &Path, extracts: &'a [FileExtract]) -> Self {
        let mut files = HashMap::new();
        let mut caller_data = HashMap::new();
        for extract in extracts {
            let index = DbFileIndex::from_extract(project_root, extract);
            caller_data.insert(extract.rel_path.clone(), &extract.data);
            files.insert(extract.rel_path.clone(), index);
        }
        Self {
            project_root: project_root.to_path_buf(),
            files,
            caller_data,
            workspace_crate_prefixes: std::sync::OnceLock::new(),
        }
    }

    fn from_db_and_callers(
        tx: &Transaction<'_>,
        project_root: &Path,
        caller_extracts: &'a HashMap<String, FileExtract>,
    ) -> Result<Self> {
        let mut files = load_db_file_indexes(tx, project_root)?;
        let mut caller_data = HashMap::new();
        for (rel_path, extract) in caller_extracts {
            files.insert(
                rel_path.clone(),
                DbFileIndex::from_extract(project_root, extract),
            );
            caller_data.insert(rel_path.clone(), &extract.data);
        }
        Ok(Self {
            project_root: project_root.to_path_buf(),
            files,
            caller_data,
            workspace_crate_prefixes: std::sync::OnceLock::new(),
        })
    }

    fn lang_for(&self, rel_path: &str) -> Option<LangId> {
        self.files.get(rel_path).and_then(|file| file.lang)
    }

    fn module_target(&self, caller_file: &str, module_path: &str) -> Option<String> {
        self.files
            .get(caller_file)
            .and_then(|file| file.module_targets.get(module_path).cloned().flatten())
    }

    fn reexports_for(&self, rel_path: &str) -> &[ReexportIndex] {
        self.files
            .get(rel_path)
            .map(|file| file.reexports.as_slice())
            .unwrap_or(&[])
    }

    fn node_for_symbol(&self, rel_path: &str, symbol: &str) -> Option<String> {
        self.files.get(rel_path).and_then(|file| {
            file.node_by_scoped
                .get(symbol)
                .cloned()
                .or_else(|| file.node_by_bare.get(symbol).cloned())
        })
    }
}

impl DbFileIndex {
    fn from_extract(project_root: &Path, extract: &FileExtract) -> Self {
        let mut node_by_scoped = HashMap::new();
        let mut node_by_bare = HashMap::new();
        for node in &extract.nodes {
            node_by_scoped.insert(node.scoped_name.clone(), node.id.clone());
            node_by_bare
                .entry(node.name.clone())
                .or_insert(node.id.clone());
        }
        let mut export_aliases = HashMap::new();
        for raw_ref in &extract.raw_refs {
            if raw_ref.kind == "export_alias" {
                if let (Some(exported), Some(source_symbol)) =
                    (&raw_ref.local_name, &raw_ref.requested_name)
                {
                    export_aliases.insert(exported.clone(), source_symbol.clone());
                }
            }
        }
        let mut module_targets = HashMap::new();
        for import in &extract.data.import_block.imports {
            module_targets.insert(
                import.module_path.clone(),
                module_target_from_dependencies(
                    project_root,
                    &module_dependencies(project_root, &extract.abs_path, &import.module_path),
                ),
            );
        }
        let mut reexports = Vec::new();
        for raw_ref in &extract.raw_refs {
            if raw_ref.kind == "reexport" {
                if let Some(module_path) = &raw_ref.module_path {
                    let target_file =
                        module_target_from_dependencies(project_root, &raw_ref.dependencies);
                    module_targets.insert(module_path.clone(), target_file.clone());
                    reexports.push(reexport_index_from_raw(raw_ref, target_file));
                }
            }
        }
        Self {
            lang: Some(extract.lang),
            exports: extract.data.exported_symbols.iter().cloned().collect(),
            default_export: extract.data.default_export_symbol.clone(),
            export_aliases,
            node_by_scoped,
            node_by_bare,
            module_targets,
            reexports,
        }
    }
}

fn load_db_file_indexes(
    tx: &Transaction<'_>,
    project_root: &Path,
) -> Result<HashMap<String, DbFileIndex>> {
    let mut files = HashMap::new();
    let mut stmt = tx.prepare("SELECT path, lang FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (rel_path, lang) = row?;
        files.insert(
            rel_path.clone(),
            DbFileIndex {
                lang: lang_from_label(&lang),
                exports: HashSet::new(),
                default_export: None,
                export_aliases: HashMap::new(),
                node_by_scoped: HashMap::new(),
                node_by_bare: HashMap::new(),
                module_targets: HashMap::new(),
                reexports: Vec::new(),
            },
        );
    }

    let mut node_stmt = tx.prepare(
        "SELECT file_path, id, name, scoped_name, exported, is_default_export FROM nodes",
    )?;
    let nodes = node_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)? != 0,
            row.get::<_, i64>(5)? != 0,
        ))
    })?;
    for row in nodes {
        let (file_path, id, name, scoped_name, exported, is_default_export) = row?;
        let file = files
            .entry(file_path.clone())
            .or_insert_with(|| DbFileIndex {
                lang: None,
                exports: HashSet::new(),
                default_export: None,
                export_aliases: HashMap::new(),
                node_by_scoped: HashMap::new(),
                node_by_bare: HashMap::new(),
                module_targets: HashMap::new(),
                reexports: Vec::new(),
            });
        if exported {
            file.exports.insert(name.clone());
            file.exports.insert(scoped_name.clone());
        }
        if is_default_export {
            file.default_export = Some(scoped_name.clone());
        }
        file.node_by_scoped.insert(scoped_name, id.clone());
        file.node_by_bare.entry(name).or_insert(id);
    }
    let file_keys: HashSet<String> = files.keys().cloned().collect();
    let mut ref_stmt = tx.prepare(
        "SELECT ref_id, caller_file, kind, module_path, full_ref, wildcard, local_name, requested_name
         FROM refs WHERE kind IN ('import', 'reexport', 'export_alias')",
    )?;
    let ref_rows = ref_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)? != 0,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    for row in ref_rows {
        let (
            ref_id,
            caller_file,
            kind,
            module_path,
            full_ref,
            wildcard,
            local_name,
            requested_name,
        ) = row?;
        if kind == "export_alias" {
            if let (Some(exported), Some(source_symbol), Some(file)) =
                (local_name, requested_name, files.get_mut(&caller_file))
            {
                file.export_aliases.insert(exported, source_symbol);
            }
            continue;
        }
        let Some(module_path) = module_path else {
            continue;
        };
        let deps = dependencies_for_ref(tx, &ref_id)?;
        let target_file = deps
            .iter()
            .find(|dep| file_keys.contains(*dep))
            .map(|dep| relative_path(project_root, &canonicalize_path(&project_root.join(dep))));
        if let Some(file) = files.get_mut(&caller_file) {
            file.module_targets
                .entry(module_path.clone())
                .or_insert_with(|| target_file.clone());
            if kind == "reexport" {
                let raw = RawRef {
                    ref_id,
                    caller_node: None,
                    caller_symbol: None,
                    caller_file,
                    kind,
                    short_name: None,
                    full_ref,
                    module_path: Some(module_path),
                    import_kind: Some("reexport".to_string()),
                    local_name: None,
                    requested_name: None,
                    namespace_alias: None,
                    wildcard,
                    line: 0,
                    byte_start: 0,
                    byte_end: 0,
                    raw_payload: String::new(),
                    dependencies: deps,
                };
                file.reexports
                    .push(reexport_index_from_raw(&raw, target_file));
            }
        }
    }

    Ok(files)
}

fn insert_file_extract(
    tx: &Transaction<'_>,
    project_root: &Path,
    extract: &FileExtract,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO files(
            path, content_hash, mtime_ns, size, lang, is_dead_code_root,
            is_public_api, surface_fingerprint, indexed_at
        ) VALUES(?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7)",
        params![
            extract.rel_path,
            hash_to_hex(extract.freshness.content_hash),
            system_time_to_ns(extract.freshness.mtime),
            extract.freshness.size as i64,
            lang_label(extract.lang),
            extract.surface_fingerprint,
            unix_seconds_now(),
        ],
    )?;
    for node in &extract.nodes {
        tx.execute(
            "INSERT OR REPLACE INTO nodes(
                id, file_path, name, scoped_name, kind, start_line, start_col,
                end_line, end_col, range_ordinal, signature, exported,
                is_default_export, is_type_like, is_callgraph_entry_point, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                node.id,
                node.file_path,
                node.name,
                node.scoped_name,
                node.kind,
                node.range.start_line as i64,
                node.range.start_col as i64,
                node.range.end_line as i64,
                node.range.end_col as i64,
                node.range_ordinal as i64,
                node.signature,
                bool_int(node.exported),
                bool_int(node.is_default_export),
                bool_int(node.is_type_like),
                bool_int(node.is_callgraph_entry_point),
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    for hint in &extract.dispatch_hints {
        tx.execute(
            "INSERT OR REPLACE INTO dispatch_hints(
                id, method_name, caller_node, file, line, byte_start, byte_end, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                hint.id,
                hint.method_name,
                hint.caller_node,
                hint.file,
                hint.line as i64,
                hint.byte_start as i64,
                hint.byte_end as i64,
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    mark_backend_state(
        tx,
        project_root,
        &extract.rel_path,
        Some(&extract.freshness.content_hash),
        "fresh",
    )?;
    Ok(())
}

fn insert_resolved_ref(tx: &Transaction<'_>, resolved: &ResolvedRef) -> Result<()> {
    let raw = &resolved.raw;
    tx.execute(
        "INSERT OR REPLACE INTO refs(
            ref_id, caller_node, caller_file, kind, short_name, full_ref, module_path,
            import_kind, local_name, requested_name, namespace_alias, wildcard, line,
            byte_start, byte_end, status, target_node, target_file, target_symbol,
            provenance, raw_payload
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            raw.ref_id,
            raw.caller_node,
            raw.caller_file,
            raw.kind,
            raw.short_name,
            raw.full_ref,
            raw.module_path,
            raw.import_kind,
            raw.local_name,
            raw.requested_name,
            raw.namespace_alias,
            bool_int(raw.wildcard),
            raw.line as i64,
            raw.byte_start as i64,
            raw.byte_end as i64,
            resolved.status,
            resolved.target_node,
            resolved.target_file,
            resolved.target_symbol,
            PROVENANCE_TREESITTER,
            raw.raw_payload,
        ],
    )?;
    for dep_file in &resolved.dependencies {
        tx.execute(
            "INSERT OR IGNORE INTO ref_dependencies(ref_id, dep_file) VALUES(?1, ?2)",
            params![raw.ref_id, dep_file],
        )?;
    }
    if let Some(edge) = &resolved.edge {
        tx.execute(
            "INSERT OR REPLACE INTO edges(
                edge_id, ref_id, source_node, target_node, target_file, target_symbol,
                kind, line, provenance
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                edge.edge_id,
                raw.ref_id,
                edge.source_node,
                edge.target_node,
                edge.target_file,
                edge.target_symbol,
                edge.kind,
                edge.line as i64,
                PROVENANCE_TREESITTER,
            ],
        )?;
    }
    Ok(())
}

fn mark_backend_state(
    tx: &Transaction<'_>,
    project_root: &Path,
    rel_path: &str,
    content_hash: Option<&blake3::Hash>,
    status: &str,
) -> Result<()> {
    let hash = content_hash
        .map(|hash| hash_to_hex(*hash))
        .unwrap_or_else(|| hash_to_hex(cache_freshness::zero_hash()));
    tx.execute(
        "INSERT OR REPLACE INTO backend_file_state(
            backend, workspace_root, file_path, content_hash, status, updated_at
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            BACKEND_TREESITTER,
            project_root.display().to_string(),
            rel_path,
            hash,
            status,
            unix_seconds_now(),
        ],
    )?;
    Ok(())
}

fn load_file_row(tx: &Transaction<'_>, rel_path: &str) -> Result<Option<FileRow>> {
    tx.query_row(
        "SELECT surface_fingerprint, content_hash, mtime_ns, size FROM files WHERE path = ?1",
        params![rel_path],
        |row| {
            let hash_text: String = row.get(1)?;
            Ok(FileRow {
                surface_fingerprint: row.get(0)?,
                freshness: FileFreshness {
                    content_hash: hash_from_hex(&hash_text)
                        .unwrap_or_else(cache_freshness::zero_hash),
                    mtime: ns_to_system_time(row.get::<_, i64>(2)?),
                    size: row.get::<_, i64>(3)? as u64,
                },
            })
        },
    )
    .optional()
    .map_err(CallGraphStoreError::from)
}

fn update_file_fresh_metadata(
    tx: &Transaction<'_>,
    rel_path: &str,
    hash: &blake3::Hash,
    mtime: SystemTime,
    size: u64,
) -> Result<()> {
    tx.execute(
        "UPDATE files SET mtime_ns = ?2, size = ?3, indexed_at = ?4 WHERE path = ?1",
        params![
            rel_path,
            system_time_to_ns(mtime),
            size as i64,
            unix_seconds_now()
        ],
    )?;
    tx.execute(
        "UPDATE backend_file_state SET status = 'fresh', updated_at = ?4
         WHERE backend = ?1 AND file_path = ?2 AND content_hash = ?3",
        params![
            BACKEND_TREESITTER,
            rel_path,
            hash_to_hex(*hash),
            unix_seconds_now(),
        ],
    )?;
    Ok(())
}

fn ref_ids_depending_on(tx: &Transaction<'_>, rel_path: &str) -> Result<Vec<String>> {
    let mut stmt = tx.prepare("SELECT ref_id FROM ref_dependencies WHERE dep_file = ?1")?;
    let rows = stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

fn refs_by_caller_for_ref_ids(
    tx: &Transaction<'_>,
    ref_ids: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut by_caller: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stmt = tx.prepare("SELECT caller_file FROM refs WHERE ref_id = ?1")?;
    for ref_id in ref_ids {
        if let Some(caller) = stmt
            .query_row(params![ref_id], |row| row.get::<_, String>(0))
            .optional()?
        {
            by_caller.entry(caller).or_default().insert(ref_id.clone());
        }
    }
    Ok(by_caller)
}

fn delete_file_rows(tx: &Transaction<'_>, rel_path: &str) -> Result<()> {
    delete_refs_for_caller(tx, rel_path)?;
    tx.execute(
        "DELETE FROM dispatch_hints WHERE file = ?1",
        params![rel_path],
    )?;
    tx.execute("DELETE FROM nodes WHERE file_path = ?1", params![rel_path])?;
    tx.execute("DELETE FROM files WHERE path = ?1", params![rel_path])?;
    Ok(())
}

fn delete_refs_for_caller(tx: &Transaction<'_>, rel_path: &str) -> Result<()> {
    let mut stmt = tx.prepare("SELECT ref_id FROM refs WHERE caller_file = ?1")?;
    let rows = stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?;
    let mut ids = BTreeSet::new();
    for row in rows {
        ids.insert(row?);
    }
    delete_ref_ids(tx, &ids)
}

fn delete_ref_ids(tx: &Transaction<'_>, ref_ids: &BTreeSet<String>) -> Result<()> {
    for ref_id in ref_ids {
        tx.execute("DELETE FROM edges WHERE ref_id = ?1", params![ref_id])?;
        tx.execute(
            "DELETE FROM ref_dependencies WHERE ref_id = ?1",
            params![ref_id],
        )?;
        tx.execute("DELETE FROM refs WHERE ref_id = ?1", params![ref_id])?;
    }
    Ok(())
}

fn edge_snapshot_with_conn(conn: &Connection) -> Result<BTreeSet<StoredEdge>> {
    let mut stmt = conn.prepare(
        "SELECT source.file_path, source.scoped_name, edges.target_file,
                edges.target_symbol, edges.kind, edges.line
         FROM edges
         JOIN nodes AS source ON source.id = edges.source_node
         ORDER BY source.file_path, source.scoped_name, edges.target_file,
                  edges.target_symbol, edges.kind, edges.line",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(StoredEdge {
            source_file: row.get(0)?,
            source_symbol: row.get(1)?,
            target_file: row.get(2)?,
            target_symbol: row.get(3)?,
            kind: row.get(4)?,
            line: row.get::<_, i64>(5)? as u32,
        })
    })?;
    let mut edges = BTreeSet::new();
    for row in rows {
        edges.insert(row?);
    }
    Ok(edges)
}

fn module_target_from_dependencies(
    project_root: &Path,
    dependencies: &BTreeSet<String>,
) -> Option<String> {
    dependencies.iter().find_map(|dep| {
        let path = project_root.join(dep);
        if path.is_file() {
            Some(relative_path(project_root, &canonicalize_path(&path)))
        } else {
            None
        }
    })
}

fn reexport_index_from_raw(raw_ref: &RawRef, target_file: Option<String>) -> ReexportIndex {
    let mut named = HashMap::new();
    if let Some(full_ref) = &raw_ref.full_ref {
        named = parse_reexport_names(full_ref);
    }
    ReexportIndex {
        target_file,
        named,
        wildcard: raw_ref.wildcard,
    }
}

fn parse_reexport_names(statement: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(open) = statement.find('{') else {
        return names;
    };
    let Some(close) = statement[open + 1..]
        .find('}')
        .map(|offset| open + 1 + offset)
    else {
        return names;
    };
    for spec in statement[open + 1..close].split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        if let Some((source, local)) = spec.split_once(" as ") {
            names.insert(local.trim().to_string(), source.trim().to_string());
        } else {
            names.insert(spec.to_string(), spec.to_string());
        }
    }
    names
}

fn dependencies_for_ref(tx: &Transaction<'_>, ref_id: &str) -> Result<BTreeSet<String>> {
    let mut stmt = tx.prepare("SELECT dep_file FROM ref_dependencies WHERE ref_id = ?1")?;
    let rows = stmt.query_map(params![ref_id], |row| row.get::<_, String>(0))?;
    let mut deps = BTreeSet::new();
    for row in rows {
        deps.insert(row?);
    }
    Ok(deps)
}

fn import_dependencies(
    project_root: &Path,
    abs_path: &Path,
    imports: &[ImportStatement],
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    for import in imports {
        deps.extend(module_dependencies(
            project_root,
            abs_path,
            &import.module_path,
        ));
    }
    deps
}

fn module_dependencies(
    project_root: &Path,
    abs_path: &Path,
    module_path: &str,
) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let caller_dir = abs_path.parent().unwrap_or(project_root);
    if let Some(resolved) = callgraph::resolve_module_path(caller_dir, module_path) {
        deps.insert(relative_path(project_root, &resolved));
    }
    if module_path.starts_with('.') {
        let base = caller_dir.join(module_path);
        for candidate in relative_module_candidates(&base) {
            deps.insert(relative_path(project_root, &candidate));
        }
    }
    deps
}

fn relative_module_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if base.extension().is_some() {
        candidates.push(base.to_path_buf());
        return candidates;
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.with_extension(ext));
    }
    for ext in JS_TS_EXTENSIONS {
        candidates.push(base.join(format!("index.{ext}")));
    }
    candidates
}

fn import_payload(import: &ImportStatement) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "module_path": import.module_path,
        "names": import.names,
        "default_import": import.default_import,
        "namespace_import": import.namespace_import,
        "kind": import_kind_label(import.kind),
        "group": import.group.label(),
        "byte_range": {"start": import.byte_range.start, "end": import.byte_range.end},
        "raw_text": import.raw_text,
        "form": import_form_payload(&import.form),
    }))?)
}

fn import_form_payload(form: &ImportForm) -> serde_json::Value {
    match form {
        ImportForm::Es {
            default_import,
            namespace_import,
            named,
            type_only,
            side_effect,
        } => json!({
            "tag": "es",
            "default_import": default_import,
            "namespace_import": namespace_import,
            "named": named,
            "type_only": type_only,
            "side_effect": side_effect,
        }),
        ImportForm::Python { from_import, named } => json!({
            "tag": "python",
            "from_import": from_import,
            "named": named,
        }),
        ImportForm::RustUse { visibility, named } => json!({
            "tag": "rust_use",
            "visibility": visibility,
            "named": named,
        }),
        ImportForm::Go { alias } => json!({
            "tag": "go",
            "alias": alias,
        }),
        ImportForm::Solidity {
            named,
            namespace,
            alias,
        } => json!({
            "tag": "solidity",
            "named": named,
            "namespace": namespace,
            "alias": alias,
        }),
        ImportForm::Structured {
            named,
            namespace,
            alias,
            modifiers,
            import_kind,
        } => json!({
            "tag": "structured",
            "named": named,
            "namespace": namespace,
            "alias": alias,
            "modifiers": modifiers,
            "import_kind": import_kind,
        }),
    }
}

fn import_local_names(import: &ImportStatement) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(default) = &import.default_import {
        names.push(default.clone());
    }
    if let Some(namespace) = &import.namespace_import {
        names.push(namespace.clone());
    }
    for name in &import.names {
        names.push(crate::imports::specifier_local_name(name).to_string());
    }
    names
}

fn import_requested_names(import: &ImportStatement) -> Vec<String> {
    import
        .names
        .iter()
        .map(|name| crate::imports::specifier_imported_name(name).to_string())
        .collect()
}

fn import_is_wildcard(import: &ImportStatement) -> bool {
    import.namespace_import.is_some() || import.raw_text.contains('*')
}

fn namespace_alias(full_ref: &str) -> Option<String> {
    full_ref
        .split_once('.')
        .map(|(namespace, _)| namespace.to_string())
}

fn import_kind_label(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Value => "value",
        ImportKind::Type => "type",
        ImportKind::SideEffect => "side_effect",
    }
}

fn symbol_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn is_type_like(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
    )
}

fn lang_label(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
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
        LangId::Yaml => "yaml",
    }
}

fn lang_from_label(label: &str) -> Option<LangId> {
    match label {
        "typescript" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "javascript" => Some(LangId::JavaScript),
        "python" => Some(LangId::Python),
        "rust" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" => Some(LangId::C),
        "cpp" => Some(LangId::Cpp),
        "zig" => Some(LangId::Zig),
        "csharp" => Some(LangId::CSharp),
        "bash" => Some(LangId::Bash),
        "html" => Some(LangId::Html),
        "markdown" => Some(LangId::Markdown),
        "solidity" => Some(LangId::Solidity),
        "scss" => Some(LangId::Scss),
        "vue" => Some(LangId::Vue),
        "json" => Some(LangId::Json),
        "scala" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "ruby" => Some(LangId::Ruby),
        "kotlin" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "perl" => Some(LangId::Perl),
        "yaml" => Some(LangId::Yaml),
        _ => None,
    }
}

fn normalize_file_list(project_root: &Path, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut normalized = if files.is_empty() {
        callgraph::walk_project_files(project_root).collect::<Vec<_>>()
    } else {
        files
            .iter()
            .map(|path| normalize_file_path(project_root, path))
            .collect::<Result<Vec<_>>>()?
    };
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn normalize_file_path(project_root: &Path, path: &Path) -> Result<PathBuf> {
    let full_path = if path.is_relative() {
        project_root.join(path)
    } else {
        path.to_path_buf()
    };
    Ok(canonicalize_path(&full_path))
}

fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    if let Ok(stripped) = path.strip_prefix(project_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    let canon_root = canonicalize_path(project_root);
    let canon_path = canonicalize_path(path);
    if let Ok(stripped) = canon_path.strip_prefix(&canon_root) {
        return stripped.to_string_lossy().replace('\\', "/");
    }
    canon_path.to_string_lossy().replace('\\', "/")
}

fn unqualified_name(scoped: &str) -> &str {
    if scoped == TOP_LEVEL_SYMBOL {
        return scoped;
    }
    scoped
        .rsplit("::")
        .next()
        .unwrap_or(scoped)
        .rsplit('.')
        .next()
        .unwrap_or(scoped)
        .rsplit('#')
        .next()
        .unwrap_or(scoped)
}

fn ref_id(parts: &[&str]) -> String {
    let joined = parts.join("\0");
    hash_to_hex(blake3::hash(joined.as_bytes()))
}

fn hash_to_hex(hash: blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn hash_from_hex(value: &str) -> Option<blake3::Hash> {
    let bytes = hex_to_bytes(value)?;
    Some(blake3::Hash::from_bytes(bytes))
}

fn hex_to_bytes(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        let end = start + 2;
        *slot = u8::from_str_radix(&value[start..end], 16).ok()?;
    }
    Some(bytes)
}

fn byte_to_line(path: &Path, byte_offset: usize) -> Option<u32> {
    let source = std::fs::read_to_string(path).ok()?;
    Some(
        source[..byte_offset.min(source.len())]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32
            + 1,
    )
}

fn empty_to_none(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn system_time_to_ns(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn ns_to_system_time(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(value.max(0) as u64)
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
