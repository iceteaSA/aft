use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use rusqlite::{params, Connection};

pub struct CompressionEventRow<'a> {
    pub harness: &'a str,
    pub session_id: Option<&'a str>,
    pub project_key: &'a str,
    pub tool: &'a str,
    pub task_id: Option<&'a str>,
    pub command: Option<&'a str>,
    pub compressor: &'a str,
    pub original_bytes: i64,
    pub compressed_bytes: i64,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CompressionAggregate {
    pub events: u64,
    pub original_tokens: u64,
    pub compressed_tokens: u64,
}

impl CompressionAggregate {
    pub fn savings_tokens(&self) -> u64 {
        self.original_tokens.saturating_sub(self.compressed_tokens)
    }

    fn add_event(&mut self, row: &CompressionEventRow<'_>) {
        self.events = self.events.saturating_add(1);
        self.original_tokens = self
            .original_tokens
            .saturating_add(u64::from(row.original_tokens));
        self.compressed_tokens = self
            .compressed_tokens
            .saturating_add(u64::from(row.compressed_tokens));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectAggregateKey {
    harness: String,
    project_key: String,
}

impl ProjectAggregateKey {
    fn new(harness: &str, project_key: &str) -> Self {
        Self {
            harness: harness.to_string(),
            project_key: project_key.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionAggregateKey {
    project: ProjectAggregateKey,
    session_id: String,
}

impl SessionAggregateKey {
    fn new(harness: &str, project_key: &str, session_id: &str) -> Self {
        Self {
            project: ProjectAggregateKey::new(harness, project_key),
            session_id: session_id.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CachedAggregate {
    aggregate: CompressionAggregate,
    watermark: i64,
}

#[derive(Debug, Default)]
struct CompressionAggregateCacheInner {
    connection_identity: Option<usize>,
    projects: HashMap<ProjectAggregateKey, CachedAggregate>,
    sessions: HashMap<SessionAggregateKey, CachedAggregate>,
}

/// Process-local compression totals backed by the durable event table.
///
/// Status reads validate entries with the table's maximum row id, an indexed
/// lookup that detects writes from other AFT processes. Full aggregate scans run
/// only for a cold or stale key. Successful local inserts advance warm entries
/// directly while the caller still owns the database connection mutex.
#[derive(Debug, Default)]
pub struct CompressionAggregateCache {
    inner: Mutex<CompressionAggregateCacheInner>,
    #[cfg(test)]
    aggregate_scan_count: AtomicUsize,
}

impl CompressionAggregateCache {
    pub fn aggregates_for_session(
        &self,
        conn: &Connection,
        harness: &str,
        project_key: &str,
        session_id: &str,
    ) -> rusqlite::Result<(CompressionAggregate, CompressionAggregate)> {
        let watermark = compression_event_watermark(conn)?;
        let project_key = ProjectAggregateKey::new(harness, project_key);
        let session_key = SessionAggregateKey::new(harness, &project_key.project_key, session_id);
        let mut inner = self.inner.lock();
        reset_for_connection_change(&mut inner, conn);

        let project = match inner.projects.get(&project_key) {
            Some(cached) if cached.watermark == watermark => cached.aggregate,
            _ => {
                self.note_aggregate_scan();
                let aggregate = aggregate_for_project(conn, harness, &project_key.project_key)?;
                inner.projects.insert(
                    project_key.clone(),
                    CachedAggregate {
                        aggregate,
                        watermark,
                    },
                );
                aggregate
            }
        };

        let session = match inner.sessions.get(&session_key) {
            Some(cached) if cached.watermark == watermark => cached.aggregate,
            _ => {
                self.note_aggregate_scan();
                let aggregate =
                    aggregate_for_session(conn, harness, &project_key.project_key, session_id)?;
                inner.sessions.insert(
                    session_key,
                    CachedAggregate {
                        aggregate,
                        watermark,
                    },
                );
                aggregate
            }
        };

        Ok((project, session))
    }

    /// Apply a row that was inserted successfully on `conn`.
    ///
    /// A warm entry is advanced only when its watermark matches the row that
    /// immediately preceded `inserted_row_id`. If another process wrote first,
    /// the entry remains stale and the next status read rebuilds it from SQL.
    pub fn record_successful_insert(
        &self,
        conn: &Connection,
        row: &CompressionEventRow<'_>,
        inserted_row_id: i64,
    ) {
        let previous_watermark = compression_event_watermark_before(conn, inserted_row_id);
        let project_key = ProjectAggregateKey::new(row.harness, row.project_key);
        let session_key = row
            .session_id
            .map(|session_id| SessionAggregateKey::new(row.harness, row.project_key, session_id));
        let mut inner = self.inner.lock();
        reset_for_connection_change(&mut inner, conn);

        let Ok(previous_watermark) = previous_watermark else {
            *inner = CompressionAggregateCacheInner {
                connection_identity: inner.connection_identity,
                ..CompressionAggregateCacheInner::default()
            };
            return;
        };

        for (key, cached) in &mut inner.projects {
            if cached.watermark != previous_watermark {
                continue;
            }
            if key == &project_key {
                cached.aggregate.add_event(row);
            }
            cached.watermark = inserted_row_id;
        }
        for (key, cached) in &mut inner.sessions {
            if cached.watermark != previous_watermark {
                continue;
            }
            if session_key.as_ref() == Some(key) {
                cached.aggregate.add_event(row);
            }
            cached.watermark = inserted_row_id;
        }
    }

    pub fn clear(&self) {
        *self.inner.lock() = CompressionAggregateCacheInner::default();
    }

    #[cfg(test)]
    fn aggregate_scan_count_for_test(&self) -> usize {
        self.aggregate_scan_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn note_aggregate_scan(&self) {
        self.aggregate_scan_count.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn note_aggregate_scan(&self) {}
}

/// Insert one event and return its row id. Duplicate identities are ignored and
/// return `None`, allowing in-process aggregates to advance only for durable rows.
pub fn insert_compression_event(
    conn: &Connection,
    row: &CompressionEventRow<'_>,
) -> rusqlite::Result<Option<i64>> {
    let inserted = conn.execute(
        r#"
        INSERT OR IGNORE INTO compression_events (
            harness, session_id, project_key, tool, task_id, command, compressor,
            original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            row.harness,
            row.session_id,
            row.project_key,
            row.tool,
            row.task_id,
            row.command,
            row.compressor,
            row.original_bytes,
            row.compressed_bytes,
            row.original_tokens,
            row.compressed_tokens,
            row.created_at,
        ],
    )?;
    Ok((inserted > 0).then(|| conn.last_insert_rowid()))
}

pub fn aggregate_for_project(
    conn: &Connection,
    harness: &str,
    project_key: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT
            COUNT(*) AS events,
            COALESCE(SUM(original_tokens), 0) AS original,
            COALESCE(SUM(compressed_tokens), 0) AS compressed
        FROM compression_events
        WHERE harness = ?1 AND project_key = ?2
        "#,
        params![harness, project_key],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}

pub fn aggregate_for_session(
    conn: &Connection,
    harness: &str,
    project_key: &str,
    session_id: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT
            COUNT(*) AS events,
            COALESCE(SUM(original_tokens), 0) AS original,
            COALESCE(SUM(compressed_tokens), 0) AS compressed
        FROM compression_events
        WHERE harness = ?1 AND project_key = ?2 AND session_id = ?3
        "#,
        params![harness, project_key, session_id],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}

fn reset_for_connection_change(inner: &mut CompressionAggregateCacheInner, conn: &Connection) {
    let identity = conn as *const Connection as usize;
    if inner.connection_identity != Some(identity) {
        *inner = CompressionAggregateCacheInner {
            connection_identity: Some(identity),
            ..CompressionAggregateCacheInner::default()
        };
    }
}

fn compression_event_watermark(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM compression_events",
        [],
        |row| row.get(0),
    )
}

fn compression_event_watermark_before(
    conn: &Connection,
    inserted_row_id: i64,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM compression_events WHERE id < ?1",
        [inserted_row_id],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn duplicate_identity_is_ignored_without_cross_project_suppression() {
        let dir = tempdir().expect("tempdir");
        let conn = crate::db::open(&dir.path().join("aft.db")).expect("open db");

        assert!(
            insert_compression_event(&conn, &row("project-a", "task-1", 100, 40, 1))
                .expect("insert first")
                .is_some()
        );
        assert!(
            insert_compression_event(&conn, &row("project-a", "task-1", 900, 10, 2))
                .expect("ignore duplicate")
                .is_none()
        );
        assert!(
            insert_compression_event(&conn, &row("project-b", "task-1", 200, 80, 3))
                .expect("insert same task id for other project")
                .is_some()
        );

        let project_a = aggregate_for_project(&conn, "opencode", "project-a").unwrap();
        assert_eq!(project_a.events, 1);
        assert_eq!(project_a.original_tokens, 100);
        assert_eq!(project_a.compressed_tokens, 40);

        let project_b = aggregate_for_project(&conn, "opencode", "project-b").unwrap();
        assert_eq!(project_b.events, 1);
        assert_eq!(project_b.original_tokens, 200);
        assert_eq!(project_b.compressed_tokens, 80);
    }

    #[test]
    fn cached_aggregates_match_sql_after_generated_inserts_and_duplicates() {
        let dir = tempdir().expect("tempdir");
        let conn = crate::db::open(&dir.path().join("aft.db")).expect("open db");
        let cache = CompressionAggregateCache::default();
        let (project, session) = cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-1")
            .expect("warm cache");
        assert_eq!(project, CompressionAggregate::default());
        assert_eq!(session, CompressionAggregate::default());
        cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-2")
            .expect("warm sibling session");
        cache
            .aggregates_for_session(&conn, "opencode", "project-b", "session-1")
            .expect("warm sibling project");
        assert_eq!(cache.aggregate_scan_count_for_test(), 5);

        let mut previous_task = String::new();
        for index in 0..64u32 {
            let task_id = if index % 5 == 4 {
                previous_task.clone()
            } else {
                let task_id = format!("task-{index}");
                previous_task = task_id.clone();
                task_id
            };
            let row = row(
                "project-a",
                &task_id,
                100 + index,
                40 + (index % 17),
                i64::from(index),
            );
            if let Some(row_id) = insert_compression_event(&conn, &row).expect("insert event") {
                cache.record_successful_insert(&conn, &row, row_id);
            }

            for (project_key, session_id) in [
                ("project-a", "session-1"),
                ("project-a", "session-2"),
                ("project-b", "session-1"),
            ] {
                let cached = cache
                    .aggregates_for_session(&conn, "opencode", project_key, session_id)
                    .expect("read cache");
                let scanned = (
                    aggregate_for_project(&conn, "opencode", project_key).expect("scan project"),
                    aggregate_for_session(&conn, "opencode", project_key, session_id)
                        .expect("scan session"),
                );
                assert_eq!(cached, scanned, "aggregate mismatch after step {index}");
            }
            assert_eq!(
                cache.aggregate_scan_count_for_test(),
                5,
                "local inserts must advance warm entries without rescanning"
            );
        }
    }

    fn row<'a>(
        project_key: &'a str,
        task_id: &'a str,
        original_tokens: u32,
        compressed_tokens: u32,
        created_at: i64,
    ) -> CompressionEventRow<'a> {
        CompressionEventRow {
            harness: "opencode",
            session_id: Some("session-1"),
            project_key,
            tool: "bash",
            task_id: Some(task_id),
            command: Some("echo ok"),
            compressor: "zstd",
            original_bytes: i64::from(original_tokens) * 4,
            compressed_bytes: i64::from(compressed_tokens) * 4,
            original_tokens,
            compressed_tokens,
            created_at,
        }
    }
}
