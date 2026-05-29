use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

use crate::cache_freshness::{FileFreshness, FreshnessVerdict};

use super::job::{FileContribution, InspectCategory, JobKey};

#[derive(Debug, Default)]
pub(crate) struct Tier2ContributionUpdates {
    pub upserts: Vec<FileContribution>,
    pub deletes: Vec<PathBuf>,
    pub metadata_updates: Vec<(PathBuf, FileFreshness)>,
}

#[derive(Debug)]
pub enum InspectCacheError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    LockPoisoned(&'static str),
    InvalidHash(String),
}

impl fmt::Display for InspectCacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InspectCacheError::Io(error) => write!(formatter, "inspect cache io error: {error}"),
            InspectCacheError::Sql(error) => {
                write!(formatter, "inspect cache sqlite error: {error}")
            }
            InspectCacheError::Json(error) => {
                write!(formatter, "inspect cache json error: {error}")
            }
            InspectCacheError::LockPoisoned(name) => {
                write!(formatter, "inspect cache lock poisoned: {name}")
            }
            InspectCacheError::InvalidHash(hash) => {
                write!(formatter, "inspect cache invalid blake3 hash: {hash}")
            }
        }
    }
}

impl std::error::Error for InspectCacheError {}

impl From<std::io::Error> for InspectCacheError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for InspectCacheError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<serde_json::Error> for InspectCacheError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Debug, Clone)]
pub struct ContributionRecord {
    pub category: InspectCategory,
    pub file_path: PathBuf,
    pub freshness: FileFreshness,
    pub contribution: serde_json::Value,
}

#[derive(Debug, Clone)]
struct MemoryAggregate {
    payload: serde_json::Value,
    generated_at: i64,
}

const TIER1_FILE_MEMO_MAX_ENTRIES: usize = 4_096;

#[derive(Debug, Clone)]
struct Tier1MemoEntry<T> {
    freshness: FileFreshness,
    value: T,
}

#[derive(Debug)]
struct Tier1MemoState<T> {
    entries: HashMap<PathBuf, Tier1MemoEntry<T>>,
    lru: std::collections::VecDeque<PathBuf>,
}

impl<T> Default for Tier1MemoState<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            lru: std::collections::VecDeque::new(),
        }
    }
}

impl<T> Tier1MemoState<T> {
    fn insert(&mut self, path: PathBuf, entry: Tier1MemoEntry<T>) {
        self.entries.insert(path.clone(), entry);
        self.touch(&path);
        self.evict_lru();
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
        self.remove_from_lru(path);
    }

    fn touch(&mut self, path: &Path) {
        if !self.entries.contains_key(path) {
            return;
        }
        self.remove_from_lru(path);
        self.lru.push_back(path.to_path_buf());
    }

    fn remove_from_lru(&mut self, path: &Path) {
        self.lru.retain(|cached_path| cached_path.as_path() != path);
    }

    fn evict_lru(&mut self) {
        while self.entries.len() > TIER1_FILE_MEMO_MAX_ENTRIES {
            let Some(path) = self.lru.pop_front() else {
                break;
            };
            self.entries.remove(&path);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Tier1FileMemo<T> {
    state: Mutex<Tier1MemoState<T>>,
}

impl<T> Default for Tier1FileMemo<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(Tier1MemoState::default()),
        }
    }
}

impl<T: Clone> Tier1FileMemo<T> {
    pub(crate) fn get_or_insert_with<F>(&self, path: &Path, scan: F) -> T
    where
        F: FnOnce(&Path) -> (Option<FileFreshness>, T),
    {
        if let Some(cached) = self.cached_value(path) {
            return cached;
        }

        let (freshness, value) = scan(path);
        if let Ok(mut state) = self.state.lock() {
            if let Some(freshness) = freshness {
                state.insert(
                    path.to_path_buf(),
                    Tier1MemoEntry {
                        freshness,
                        value: value.clone(),
                    },
                );
            } else {
                state.remove(path);
            }
        }
        value
    }

    fn cached_value(&self, path: &Path) -> Option<T> {
        let mut cached = self
            .state
            .lock()
            .ok()
            .and_then(|state| state.entries.get(path).cloned())?;

        match crate::cache_freshness::verify_file(path, &cached.freshness) {
            FreshnessVerdict::HotFresh => {
                if let Ok(mut state) = self.state.lock() {
                    state.touch(path);
                }
                Some(cached.value)
            }
            FreshnessVerdict::ContentFresh {
                new_mtime,
                new_size,
            } => {
                cached.freshness.mtime = new_mtime;
                cached.freshness.size = new_size;
                let value = cached.value.clone();
                if let Ok(mut state) = self.state.lock() {
                    state.insert(path.to_path_buf(), cached);
                }
                Some(value)
            }
            FreshnessVerdict::Stale => None,
            FreshnessVerdict::Deleted => {
                if let Ok(mut state) = self.state.lock() {
                    state.remove(path);
                }
                None
            }
        }
    }
}

#[derive(Debug)]
pub struct InspectCache {
    project_root: PathBuf,
    project_key: String,
    sqlite_path: PathBuf,
    conn: Mutex<Connection>,
    memory: RwLock<HashMap<JobKey, MemoryAggregate>>,
}

impl InspectCache {
    pub fn open(inspect_dir: PathBuf, project_root: PathBuf) -> Result<Self, InspectCacheError> {
        std::fs::create_dir_all(&inspect_dir)?;
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = inspect_dir.join(format!("{project_key}.sqlite"));
        let conn = Connection::open(&sqlite_path)?;
        configure_connection(&conn)?;
        initialize_schema(&conn)?;
        Ok(Self {
            project_root,
            project_key,
            sqlite_path,
            conn: Mutex::new(conn),
            memory: RwLock::new(HashMap::new()),
        })
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

    pub fn store_aggregated(
        &self,
        key: JobKey,
        payload: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        self.memory
            .write()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .insert(
                key,
                MemoryAggregate {
                    payload,
                    generated_at: unix_seconds_now(),
                },
            );
        Ok(())
    }

    pub fn get_aggregated(
        &self,
        key: &JobKey,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        if let Some(entry) = self
            .memory
            .read()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .get(key)
            .cloned()
        {
            return Ok(Some(entry.payload));
        }

        if !key.category.is_tier2() {
            return Ok(None);
        }

        let payload = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            conn.query_row(
                "SELECT aggregate FROM tier2_aggregates WHERE category = ?1 AND project_key = ?2",
                params![key.category.as_str(), self.project_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        };

        match payload {
            Some(bytes) => {
                let value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
                self.store_aggregated(key.clone(), value.clone())?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn store_tier2_result(
        &self,
        key: JobKey,
        scanned_files: &[PathBuf],
        contributions: &[FileContribution],
        aggregate: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        if !key.category.is_tier2() {
            self.store_aggregated(key, aggregate)?;
            return Ok(());
        }

        let now = unix_seconds_now();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;

        let scanned_relative = scanned_files
            .iter()
            .map(|path| relative_string(&self.project_root, path))
            .collect::<BTreeSet<_>>();
        let existing = existing_contribution_paths(&tx, key.category, &self.project_key)?;
        for file_path in existing {
            if !scanned_relative.contains(&file_path) {
                tx.execute(
                    "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                    params![key.category.as_str(), self.project_key, file_path],
                )?;
            }
        }

        for contribution in contributions {
            let file_path = relative_string(&self.project_root, &contribution.file_path);
            let blob = serde_json::to_vec(&contribution.contribution)?;
            tx.execute(
                "INSERT INTO tier2_contributions \
                 (category, project_key, file_path, file_mtime_ns, file_size, file_hash, contribution, generated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(category, project_key, file_path) DO UPDATE SET \
                 file_mtime_ns = excluded.file_mtime_ns, \
                 file_size = excluded.file_size, \
                 file_hash = excluded.file_hash, \
                 contribution = excluded.contribution, \
                 generated_at = excluded.generated_at",
                params![
                    contribution.category.as_str(),
                    self.project_key,
                    file_path,
                    system_time_to_ns(contribution.freshness.mtime),
                    contribution.freshness.size as i64,
                    hash_to_hex(contribution.freshness.content_hash),
                    blob,
                    now,
                ],
            )?;
        }

        let contribution_set_hash =
            contribution_set_hash_with_conn(&tx, key.category, &self.project_key)?;
        let aggregate_blob = serde_json::to_vec(&aggregate)?;
        tx.execute(
            "INSERT INTO tier2_aggregates \
             (category, project_key, contribution_set_hash, aggregate, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(category, project_key) DO UPDATE SET \
             contribution_set_hash = excluded.contribution_set_hash, \
             aggregate = excluded.aggregate, \
             generated_at = excluded.generated_at",
            params![
                key.category.as_str(),
                self.project_key,
                contribution_set_hash,
                aggregate_blob,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3) \
             ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![key.category.as_str(), self.project_key, now],
        )?;
        tx.commit()?;

        self.store_aggregated(key, aggregate)
    }

    pub(crate) fn apply_contribution_updates(
        &self,
        category: InspectCategory,
        updates: Tier2ContributionUpdates,
    ) -> Result<String, InspectCacheError> {
        let now = unix_seconds_now();
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;

        for relative_file in updates.deletes {
            tx.execute(
                "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                params![
                    category.as_str(),
                    self.project_key,
                    relative_file.to_string_lossy().to_string()
                ],
            )?;
        }

        for (relative_file, freshness) in updates.metadata_updates {
            tx.execute(
                "UPDATE tier2_contributions \
                 SET file_mtime_ns = ?4, file_size = ?5, file_hash = ?6 \
                 WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
                params![
                    category.as_str(),
                    self.project_key,
                    relative_file.to_string_lossy().to_string(),
                    system_time_to_ns(freshness.mtime),
                    freshness.size as i64,
                    hash_to_hex(freshness.content_hash),
                ],
            )?;
        }

        for contribution in updates.upserts {
            let file_path = relative_string(&self.project_root, &contribution.file_path);
            let blob = serde_json::to_vec(&contribution.contribution)?;
            tx.execute(
                "INSERT INTO tier2_contributions \
                 (category, project_key, file_path, file_mtime_ns, file_size, file_hash, contribution, generated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(category, project_key, file_path) DO UPDATE SET \
                 file_mtime_ns = excluded.file_mtime_ns, \
                 file_size = excluded.file_size, \
                 file_hash = excluded.file_hash, \
                 contribution = excluded.contribution, \
                 generated_at = excluded.generated_at",
                params![
                    contribution.category.as_str(),
                    self.project_key,
                    file_path,
                    system_time_to_ns(contribution.freshness.mtime),
                    contribution.freshness.size as i64,
                    hash_to_hex(contribution.freshness.content_hash),
                    blob,
                    now,
                ],
            )?;
        }

        let contribution_set_hash =
            contribution_set_hash_with_conn(&tx, category, &self.project_key)?;
        tx.commit()?;

        self.memory
            .write()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .remove(&JobKey::for_project_category(category));

        Ok(contribution_set_hash)
    }

    pub(crate) fn load_aggregate_if_hash_matches(
        &self,
        category: InspectCategory,
        contribution_set_hash: &str,
    ) -> Result<Option<serde_json::Value>, InspectCacheError> {
        let payload = {
            let conn = self
                .conn
                .lock()
                .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
            conn.query_row(
                "SELECT aggregate FROM tier2_aggregates \
                 WHERE category = ?1 AND project_key = ?2 AND contribution_set_hash = ?3",
                params![category.as_str(), self.project_key, contribution_set_hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        };

        match payload {
            Some(bytes) => {
                let value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
                self.store_aggregated(JobKey::for_project_category(category), value.clone())?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn touch_tier2_last_full_run(
        &self,
        category: InspectCategory,
    ) -> Result<i64, InspectCacheError> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;
        let previous = tx
            .query_row(
                "SELECT last_full_run FROM tier2_meta WHERE category = ?1 AND project_key = ?2",
                params![category.as_str(), self.project_key],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let now = unix_seconds_now();
        let last_full_run = previous.map_or(now, |previous| now.max(previous.saturating_add(1)));
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3)              ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![category.as_str(), self.project_key, last_full_run],
        )?;
        tx.commit()?;
        Ok(last_full_run)
    }

    pub(crate) fn store_tier2_aggregate(
        &self,
        key: JobKey,
        contribution_set_hash: &str,
        aggregate: serde_json::Value,
    ) -> Result<(), InspectCacheError> {
        if !key.category.is_tier2() {
            self.store_aggregated(key, aggregate)?;
            return Ok(());
        }

        let now = unix_seconds_now();
        let aggregate_blob = serde_json::to_vec(&aggregate)?;
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tier2_aggregates \
             (category, project_key, contribution_set_hash, aggregate, generated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(category, project_key) DO UPDATE SET \
             contribution_set_hash = excluded.contribution_set_hash, \
             aggregate = excluded.aggregate, \
             generated_at = excluded.generated_at",
            params![
                key.category.as_str(),
                self.project_key,
                contribution_set_hash,
                aggregate_blob,
                now,
            ],
        )?;
        tx.execute(
            "INSERT INTO tier2_meta (category, project_key, last_full_run) VALUES (?1, ?2, ?3) \
             ON CONFLICT(category, project_key) DO UPDATE SET last_full_run = excluded.last_full_run",
            params![key.category.as_str(), self.project_key, now],
        )?;
        tx.commit()?;

        self.store_aggregated(key, aggregate)
    }

    pub fn load_tier2_contributions(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<ContributionRecord>, InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut stmt = conn.prepare(
            "SELECT file_path, file_mtime_ns, file_size, file_hash, contribution \
             FROM tier2_contributions \
             WHERE category = ?1 AND project_key = ?2 \
             ORDER BY file_path ASC",
        )?;
        let rows = stmt.query_map(params![category.as_str(), self.project_key], |row| {
            let file_path: String = row.get(0)?;
            let mtime_ns: i64 = row.get(1)?;
            let file_size: i64 = row.get(2)?;
            let file_hash: String = row.get(3)?;
            let contribution: Vec<u8> = row.get(4)?;
            Ok((file_path, mtime_ns, file_size, file_hash, contribution))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (file_path, mtime_ns, file_size, file_hash, contribution) = row?;
            records.push(ContributionRecord {
                category,
                file_path: PathBuf::from(file_path),
                freshness: FileFreshness {
                    mtime: ns_to_system_time(mtime_ns),
                    size: file_size.max(0) as u64,
                    content_hash: hash_from_hex(&file_hash)?,
                },
                contribution: serde_json::from_slice(&contribution)?,
            });
        }
        Ok(records)
    }

    pub fn delete_tier2_contribution(
        &self,
        category: InspectCategory,
        relative_file: &Path,
    ) -> Result<(), InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.execute(
            "DELETE FROM tier2_contributions WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
            params![
                category.as_str(),
                self.project_key,
                relative_file.to_string_lossy().to_string()
            ],
        )?;
        Ok(())
    }

    pub fn update_content_fresh_metadata(
        &self,
        category: InspectCategory,
        relative_file: &Path,
        freshness: &FileFreshness,
    ) -> Result<(), InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.execute(
            "UPDATE tier2_contributions \
             SET file_mtime_ns = ?4, file_size = ?5, file_hash = ?6 \
             WHERE category = ?1 AND project_key = ?2 AND file_path = ?3",
            params![
                category.as_str(),
                self.project_key,
                relative_file.to_string_lossy().to_string(),
                system_time_to_ns(freshness.mtime),
                freshness.size as i64,
                hash_to_hex(freshness.content_hash),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn contribution_fingerprint(
        &self,
        category: InspectCategory,
    ) -> Result<(usize, String), InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut stmt = conn.prepare(
            "SELECT file_path, file_mtime_ns, file_size \
             FROM tier2_contributions \
             WHERE category = ?1 AND project_key = ?2 \
             ORDER BY file_path ASC",
        )?;
        let rows = stmt.query_map(params![category.as_str(), self.project_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut count = 0usize;
        let mut hasher = blake3::Hasher::new();
        for row in rows {
            let (file_path, mtime_ns, file_size) = row?;
            count += 1;
            update_contribution_fingerprint_hash(
                &mut hasher,
                &file_path,
                mtime_ns.max(0),
                file_size.max(0) as u64,
            );
        }

        Ok((count, hasher.finalize().to_hex().to_string()))
    }

    pub(crate) fn contribution_freshness(
        &self,
        category: InspectCategory,
    ) -> Result<Vec<(PathBuf, FileFreshness)>, InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        let mut stmt = conn.prepare(
            "SELECT file_path, file_mtime_ns, file_size, file_hash \
             FROM tier2_contributions \
             WHERE category = ?1 AND project_key = ?2 \
             ORDER BY file_path ASC",
        )?;
        let rows = stmt.query_map(params![category.as_str(), self.project_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (file_path, mtime_ns, file_size, file_hash) = row?;
            records.push((
                PathBuf::from(file_path),
                FileFreshness {
                    mtime: ns_to_system_time(mtime_ns),
                    size: file_size.max(0) as u64,
                    content_hash: hash_from_hex(&file_hash)?,
                },
            ));
        }
        Ok(records)
    }

    pub fn contribution_set_hash(
        &self,
        category: InspectCategory,
    ) -> Result<String, InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        contribution_set_hash_with_conn(&conn, category, &self.project_key)
    }

    pub fn last_full_run(
        &self,
        category: InspectCategory,
    ) -> Result<Option<i64>, InspectCacheError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| InspectCacheError::LockPoisoned("connection"))?;
        conn.query_row(
            "SELECT last_full_run FROM tier2_meta WHERE category = ?1 AND project_key = ?2",
            params![category.as_str(), self.project_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(InspectCacheError::from)
    }

    pub fn memory_generated_at(&self, key: &JobKey) -> Result<Option<i64>, InspectCacheError> {
        Ok(self
            .memory
            .read()
            .map_err(|_| InspectCacheError::LockPoisoned("memory"))?
            .get(key)
            .map(|entry| entry.generated_at))
    }
}

fn configure_connection(conn: &Connection) -> Result<(), InspectCacheError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<(), InspectCacheError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tier2_contributions (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            file_path       TEXT NOT NULL,
            file_mtime_ns   INTEGER NOT NULL,
            file_size       INTEGER NOT NULL,
            file_hash       TEXT NOT NULL,
            contribution    BLOB NOT NULL,
            generated_at    INTEGER NOT NULL,
            PRIMARY KEY (category, project_key, file_path)
        );

        CREATE TABLE IF NOT EXISTS tier2_aggregates (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            contribution_set_hash TEXT NOT NULL,
            aggregate       BLOB NOT NULL,
            generated_at    INTEGER NOT NULL,
            PRIMARY KEY (category, project_key)
        );

        CREATE TABLE IF NOT EXISTS tier2_meta (
            category        TEXT NOT NULL,
            project_key     TEXT NOT NULL,
            last_full_run   INTEGER NOT NULL,
            PRIMARY KEY (category, project_key)
        );",
    )?;
    Ok(())
}

fn existing_contribution_paths(
    conn: &Connection,
    category: InspectCategory,
    project_key: &str,
) -> Result<Vec<String>, InspectCacheError> {
    let mut stmt = conn.prepare(
        "SELECT file_path FROM tier2_contributions WHERE category = ?1 AND project_key = ?2",
    )?;
    let rows = stmt.query_map(params![category.as_str(), project_key], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(InspectCacheError::from)
}

fn contribution_set_hash_with_conn(
    conn: &Connection,
    category: InspectCategory,
    project_key: &str,
) -> Result<String, InspectCacheError> {
    let mut stmt = conn.prepare(
        "SELECT file_path, file_hash FROM tier2_contributions \
         WHERE category = ?1 AND project_key = ?2 ORDER BY file_path ASC",
    )?;
    let rows = stmt.query_map(params![category.as_str(), project_key], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut hasher = blake3::Hasher::new();
    for row in rows {
        let (file_path, file_hash) = row?;
        hasher.update(file_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(file_hash.as_bytes());
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn update_contribution_fingerprint_hash(
    hasher: &mut blake3::Hasher,
    relative_path: &str,
    mtime_ns: i64,
    file_size: u64,
) {
    hasher.update(relative_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(&mtime_ns.to_le_bytes());
    hasher.update(&file_size.to_le_bytes());
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn system_time_to_ns(time: SystemTime) -> i64 {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    nanos.min(i64::MAX as u128) as i64
}

fn ns_to_system_time(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(value.max(0) as u64)
}

fn hash_to_hex(hash: blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn hash_from_hex(value: &str) -> Result<blake3::Hash, InspectCacheError> {
    if value.len() != 64 {
        return Err(InspectCacheError::InvalidHash(value.to_string()));
    }
    let mut bytes = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk)
            .map_err(|_| InspectCacheError::InvalidHash(value.to_string()))?;
        bytes[index] = u8::from_str_radix(hex, 16)
            .map_err(|_| InspectCacheError::InvalidHash(value.to_string()))?;
    }
    Ok(blake3::Hash::from_bytes(bytes))
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;

    fn collect_freshness(path: &Path) -> FileFreshness {
        crate::cache_freshness::collect(path).unwrap()
    }

    #[test]
    fn tier1_file_memo_evicts_lru_and_keeps_recent_hits() {
        let temp = tempfile::tempdir().unwrap();
        let memo = Tier1FileMemo::<usize>::default();
        let mut paths = Vec::with_capacity(TIER1_FILE_MEMO_MAX_ENTRIES);

        for index in 0..TIER1_FILE_MEMO_MAX_ENTRIES {
            let path = temp.path().join(format!("file-{index}.txt"));
            fs::write(&path, index.to_string()).unwrap();
            let value =
                memo.get_or_insert_with(&path, |path| (Some(collect_freshness(path)), index));
            assert_eq!(value, index);
            paths.push(path);
        }

        let recent_path = paths[0].clone();
        let recent_value = memo.get_or_insert_with(&recent_path, |_| {
            panic!("recently inserted entry should hit before eviction")
        });
        assert_eq!(recent_value, 0);

        let evicting_path = temp.path().join("new-file.txt");
        fs::write(&evicting_path, "new").unwrap();
        let evicting_value = memo.get_or_insert_with(&evicting_path, |path| {
            (Some(collect_freshness(path)), TIER1_FILE_MEMO_MAX_ENTRIES)
        });
        assert_eq!(evicting_value, TIER1_FILE_MEMO_MAX_ENTRIES);

        let state = memo.state.lock().unwrap();
        assert_eq!(state.entries.len(), TIER1_FILE_MEMO_MAX_ENTRIES);
        assert!(state.entries.contains_key(&recent_path));
        assert!(state.entries.contains_key(&evicting_path));
        assert!(!state.entries.contains_key(&paths[1]));
        drop(state);

        let recent_value = memo.get_or_insert_with(&recent_path, |_| {
            panic!("recently used entry should survive eviction")
        });
        assert_eq!(recent_value, 0);
    }

    #[test]
    fn tier1_file_memo_reuses_fresh_entries_and_rescans_stale_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("memo.txt");
        fs::write(&path, "first").unwrap();

        let memo = Tier1FileMemo::<String>::default();
        let scans = Cell::new(0);

        let first = memo.get_or_insert_with(&path, |path| {
            scans.set(scans.get() + 1);
            (Some(collect_freshness(path)), "first scan".to_string())
        });
        assert_eq!(first, "first scan");
        assert_eq!(scans.get(), 1);

        let unchanged =
            memo.get_or_insert_with(&path, |_| panic!("unchanged file should reuse Tier-1 memo"));
        assert_eq!(unchanged, "first scan");
        assert_eq!(scans.get(), 1);

        fs::write(&path, "changed file contents").unwrap();
        let changed = memo.get_or_insert_with(&path, |path| {
            scans.set(scans.get() + 1);
            (Some(collect_freshness(path)), "second scan".to_string())
        });
        assert_eq!(changed, "second scan");
        assert_eq!(scans.get(), 2);

        let fresh_after_rescan = memo.get_or_insert_with(&path, |_| {
            panic!("rescanned file should reuse refreshed Tier-1 memo")
        });
        assert_eq!(fresh_after_rescan, "second scan");
        assert_eq!(scans.get(), 2);
    }
}
