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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 1;
const BACKEND_TREESITTER: &str = "treesitter";
const PROVENANCE_TREESITTER: &str = "treesitter+resolver";
const TOP_LEVEL_SYMBOL: &str = "<top-level>";
const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

#[derive(Debug)]
pub enum CallGraphStoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Aft(AftError),
    Lock(crate::fs_lock::AcquireError),
    MissingCallerData { file: String },
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
    files: HashMap<String, DbFileIndex>,
    caller_data: HashMap<String, &'a FileCallData>,
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
        let conn = Connection::open(&sqlite_path)?;
        configure_connection(&conn)?;
        initialize_schema(&conn)?;
        Ok(Self::from_connection(
            project_root,
            project_key,
            sqlite_path,
            conn,
        ))
    }

    pub fn open_readonly(callgraph_dir: PathBuf, project_root: PathBuf) -> Result<Option<Self>> {
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = callgraph_dir.join(format!("{project_key}.sqlite"));
        if !sqlite_path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&sqlite_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
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
        let lock_path = callgraph_dir.join(format!("{project_key}.build.lock"));
        let _guard = crate::fs_lock::try_acquire(&lock_path, Duration::from_secs(30))?;
        let store = Self::open(callgraph_dir, project_root)?;
        let stats = store.cold_build(files)?;
        Ok((store, stats))
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
        let files = normalize_file_list(&self.project_root, files)?;
        let extracts = build_extracts_parallel(&self.project_root, &files)?;
        let node_count = extracts.iter().map(|extract| extract.nodes.len()).sum();

        let index = ProjectIndex::from_extracts(&self.project_root, &extracts);
        let mut resolved_refs = Vec::new();
        for extract in &extracts {
            for raw_ref in &extract.raw_refs {
                resolved_refs.push(resolve_ref(raw_ref.clone(), &index)?);
            }
        }
        let ref_count = resolved_refs.len();
        let edge_count = resolved_refs
            .iter()
            .filter(|item| item.edge.is_some())
            .count();

        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let tx = conn.transaction()?;
        clear_tables(&tx)?;
        insert_meta(&tx)?;
        for extract in &extracts {
            insert_file_extract(&tx, &self.project_root, extract)?;
        }
        for resolved in &resolved_refs {
            insert_resolved_ref(&tx, resolved)?;
        }
        tx.commit()?;

        Ok(ColdBuildStats {
            files: extracts.len(),
            nodes: node_count,
            refs: ref_count,
            edges: edge_count,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub fn refresh_files(&self, changed_files: &[PathBuf]) -> Result<IncrementalStats> {
        let mut conn = self.conn.lock().expect("callgraph store mutex poisoned");
        let tx = conn.transaction()?;
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

    pub fn edge_snapshot(&self) -> Result<BTreeSet<StoredEdge>> {
        let conn = self.conn.lock().expect("callgraph store mutex poisoned");
        edge_snapshot_with_conn(&conn)
    }
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

fn schema_fingerprint() -> String {
    let input = format!("callgraph_store:v{SCHEMA_VERSION}:positional:raw-ref:v3");
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

fn build_extracts_parallel(project_root: &Path, files: &[PathBuf]) -> Result<Vec<FileExtract>> {
    let results: Vec<Result<FileExtract>> = files
        .par_iter()
        .map(|path| build_file_extract(project_root, path))
        .collect();
    results.into_iter().collect()
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
    raw_refs.extend(reexports.raw_refs);
    let dispatch_hints = build_dispatch_hints(&rel_path, &data, &node_by_scoped);
    let surface_fingerprint = surface_fingerprint(&mut nodes, &data, &reexports.surface_parts);

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
                &name, &meta.kind, exported, data.lang,
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
            return Some(("resolved".to_string(), target_file, short_name.to_string()));
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
    let module_segments = rust_resolve_segments(caller_file, &segments)?;
    rust_file_for_segments(index, caller_file, &module_segments)
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
    let src_prefix = rust_src_prefix(caller_file);
    let candidate = if segments.is_empty() {
        [src_prefix.as_str(), "lib.rs"].join("/")
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
        Self { files, caller_data }
    }

    fn from_db_and_callers(
        tx: &Transaction<'_>,
        project_root: &Path,
        caller_extracts: &'a HashMap<String, FileExtract>,
    ) -> Result<Self> {
        let mut files = load_db_file_indexes(tx)?;
        let mut caller_data = HashMap::new();
        for (rel_path, extract) in caller_extracts {
            files.insert(
                rel_path.clone(),
                DbFileIndex::from_extract(project_root, extract),
            );
            caller_data.insert(rel_path.clone(), &extract.data);
        }
        Ok(Self { files, caller_data })
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
            node_by_scoped,
            node_by_bare,
            module_targets,
            reexports,
        }
    }
}

fn load_db_file_indexes(tx: &Transaction<'_>) -> Result<HashMap<String, DbFileIndex>> {
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
        "SELECT ref_id, caller_file, kind, module_path, full_ref, wildcard FROM refs WHERE kind IN ('import', 'reexport')",
    )?;
    let ref_rows = ref_stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)? != 0,
        ))
    })?;
    for row in ref_rows {
        let (ref_id, caller_file, kind, module_path, full_ref, wildcard) = row?;
        let Some(module_path) = module_path else {
            continue;
        };
        let deps = dependencies_for_ref(tx, &ref_id)?;
        let target_file = deps.iter().find(|dep| file_keys.contains(*dep)).cloned();
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
    dependencies
        .iter()
        .find(|dep| project_root.join(dep).is_file())
        .cloned()
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
