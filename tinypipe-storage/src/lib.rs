//! `tinypipe-storage` — SQLite implementation of `GraphStorage` trait.
//!
//! Tables: `graphs`, `graph_versions`, `executions`, `execution_steps`.
//! Migrations run automatically on first connection.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json;

use tinypipe_api::storage::{GraphDefinition, GraphStorage, GraphTreeNode};
use tinypipe_api::types::{
    Execution, ExecutionStatus, ExecutionStep, GraphId, StorageError, Value, Version,
};

/// SQLite-backed GraphStorage implementation.
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

/// A lightweight graph listing item for CLI display.
#[derive(Debug, Clone)]
pub struct ListGraphItem {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub status: String,
    pub code_len: u64,
}

/// Full-text search result item.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    pub name: String,
    pub code: String,
    pub rank: f64,
}

impl SqliteStorage {
    /// Open (or create) a SQLite database at `path` and run migrations.
    /// Use `:memory:` for in-memory testing.
    pub fn open(path: &str) -> Result<Self, StorageError> {
        let conn = Connection::open(path)
            .map_err(|e| StorageError::Internal(format!("failed to open db: {}", e)))?;
        let store = SqliteStorage { conn: Mutex::new(conn) };
        store.run_migrations()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database (for testing).
    pub fn in_memory() -> Result<Self, StorageError> {
        Self::open(":memory:")
    }

    fn run_migrations(&self) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS graphs (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                version         INTEGER NOT NULL DEFAULT 1,
                status          TEXT NOT NULL DEFAULT 'draft',
                code            TEXT NOT NULL,
                execution_plan  BLOB,
                active_version  INTEGER,
                parent_id       TEXT,
                fork_node       TEXT,
                fork_label      TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS graph_versions (
                graph_id        TEXT NOT NULL,
                version         INTEGER NOT NULL,
                code            TEXT NOT NULL,
                execution_plan  BLOB,
                created_at      TEXT NOT NULL,
                PRIMARY KEY (graph_id, version)
            );

            CREATE TABLE IF NOT EXISTS executions (
                id              TEXT PRIMARY KEY,
                graph_id        TEXT NOT NULL,
                graph_version   INTEGER NOT NULL,
                input           TEXT NOT NULL,
                output          TEXT,
                status          TEXT NOT NULL DEFAULT 'running',
                error           TEXT,
                started_at      TEXT NOT NULL,
                completed_at    TEXT,
                duration_us     INTEGER,
                context         TEXT
            );

            CREATE TABLE IF NOT EXISTS execution_steps (
                id              TEXT PRIMARY KEY,
                execution_id    TEXT NOT NULL,
                node_id         TEXT NOT NULL,
                node_op         TEXT NOT NULL,
                status          TEXT NOT NULL DEFAULT 'running',
                error           TEXT,
                started_at      TEXT NOT NULL,
                completed_at    TEXT,
                duration_us     INTEGER,
                context_before  TEXT,
                context_after   TEXT,
                parent_step_id  TEXT
            );
            "
        ).map_err(|e| StorageError::Internal(format!("migration failed: {}", e)))?;

        // v2.1 migration: add fork_node + fork_label columns if missing
        let has_fork_node: bool = conn
            .prepare("SELECT fork_node FROM graphs LIMIT 0")
            .is_ok();
        if !has_fork_node {
            conn.execute_batch(
                "ALTER TABLE graphs ADD COLUMN fork_node TEXT;
                 ALTER TABLE graphs ADD COLUMN fork_label TEXT;"
            ).map_err(|e| StorageError::Internal(format!("migration v2.1 failed: {}", e)))?;
        }

        // v2.3 migration: FTS5 virtual table for full-text search
        let has_fts: bool = conn
            .prepare("SELECT name FROM graphs_fts WHERE 0=1")
            .is_ok();
        if !has_fts {
            // FTS5 may not be available in all SQLite builds — try gracefully
            let fts_result = conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS graphs_fts USING fts5(
                     id UNINDEXED,
                     name,
                     code,
                     content='graphs',
                     content_rowid='rowid'
                 );
                 -- Triggers to keep FTS in sync
                 CREATE TRIGGER IF NOT EXISTS graphs_ai AFTER INSERT ON graphs BEGIN
                     INSERT INTO graphs_fts(rowid, id, name, code)
                     VALUES (new.rowid, new.id, new.name, new.code);
                 END;
                 CREATE TRIGGER IF NOT EXISTS graphs_ad AFTER DELETE ON graphs BEGIN
                     INSERT INTO graphs_fts(graphs_fts, rowid, id, name, code)
                     VALUES ('delete', old.rowid, old.id, old.name, old.code);
                 END;
                 CREATE TRIGGER IF NOT EXISTS graphs_au AFTER UPDATE ON graphs BEGIN
                     INSERT INTO graphs_fts(graphs_fts, rowid, id, name, code)
                     VALUES ('delete', old.rowid, old.id, old.name, old.code);
                     INSERT INTO graphs_fts(rowid, id, name, code)
                     VALUES (new.rowid, new.id, new.name, new.code);
                 END;"
            );
            if let Err(e) = fts_result {
                // FTS5 not available — log warning and continue
                tracing::warn!("FTS5 not available (graphs search disabled): {}", e);
            }
        }

        Ok(())
    }

    fn now() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros().to_string())
            .unwrap_or_else(|_| "0".into())
    }

    fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// List all graphs with metadata (for CLI display).
    /// Supports optional pagination via `limit` and `offset`.
    pub fn list_all_graphs(&self, limit: Option<u64>, offset: Option<u64>) -> Result<Vec<ListGraphItem>, StorageError> {
        let conn = self.conn.lock().unwrap();
        let sql = if limit.is_some() || offset.is_some() {
            "SELECT id, name, version, status, LENGTH(code) FROM graphs ORDER BY created_at DESC LIMIT ?1 OFFSET ?2"
        } else {
            "SELECT id, name, version, status, LENGTH(code) FROM graphs ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)
            .map_err(|e| StorageError::Internal(format!("prepare list_graphs: {}", e)))?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ListGraphItem> {
            Ok(ListGraphItem {
                id: row.get::<_, String>(0)?,
                name: row.get::<_, String>(1)?,
                version: row.get::<_, i64>(2)? as u64,
                status: row.get::<_, String>(3)?,
                code_len: row.get::<_, i64>(4)? as u64,
            })
        };
        let (lim, off) = (limit.unwrap_or(1000), offset.unwrap_or(0));
        let mapped = if limit.is_some() || offset.is_some() {
            stmt.query_map(params![lim as i64, off as i64], map)
                .map_err(|e| StorageError::Internal(format!("query list_graphs: {}", e)))?
        } else {
            stmt.query_map([], map)
                .map_err(|e| StorageError::Internal(format!("query list_graphs: {}", e)))?
        };
        let mut rows = Vec::new();
        for row in mapped {
            rows.push(row.map_err(|e| StorageError::Internal(format!("row list_graphs: {}", e)))?);
        }
        Ok(rows)
    }

    /// Full-text search across graph names and code.
    /// Uses FTS5 if available; falls back to LIKE search.
    /// Returns up to `limit` results (default 20).
    pub fn search_graphs(&self, query: &str, limit: Option<u64>) -> Result<Vec<SearchResult>, StorageError> {
        let conn = self.conn.lock().unwrap();
        let lim = limit.unwrap_or(20).min(100) as i64;

        // Try FTS5 first
        let has_fts: bool = conn.prepare("SELECT name FROM graphs_fts WHERE 0=1").is_ok();
        if has_fts {
            let mut stmt = conn.prepare(
                "SELECT g.id, g.name, g.code, rank
                 FROM graphs_fts
                 JOIN graphs g ON g.rowid = graphs_fts.rowid
                 WHERE graphs_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2"
            ).map_err(|e| StorageError::Internal(format!("prepare search_fts: {}", e)))?;
            let mapped = stmt.query_map(params![query, lim], |row| {
                Ok(SearchResult {
                    id: row.get::<_, String>(0)?,
                    name: row.get::<_, String>(1)?,
                    code: row.get::<_, String>(2)?,
                    rank: row.get::<_, f64>(3)?,
                })
            }).map_err(|e| StorageError::Internal(format!("query search_fts: {}", e)))?;
            let mut results = Vec::new();
            for row in mapped {
                results.push(row.map_err(|e| StorageError::Internal(format!("row search_fts: {}", e)))?);
            }
            return Ok(results);
        }

        // Fallback: LIKE search
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT id, name, code FROM graphs
             WHERE name LIKE ?1 ESCAPE '\\' OR code LIKE ?1 ESCAPE '\\'
             LIMIT ?2"
        ).map_err(|e| StorageError::Internal(format!("prepare search_like: {}", e)))?;
        let mapped = stmt.query_map(params![pattern, lim], |row| {
            Ok(SearchResult {
                id: row.get::<_, String>(0)?,
                name: row.get::<_, String>(1)?,
                code: row.get::<_, String>(2)?,
                rank: 0.0,
            })
        }).map_err(|e| StorageError::Internal(format!("query search_like: {}", e)))?;
        let mut results = Vec::new();
        for row in mapped {
            results.push(row.map_err(|e| StorageError::Internal(format!("row search_like: {}", e)))?);
        }
        Ok(results)
    }

    /// Internal helper: create a graph with full fork metadata (used by fork_graph).
    pub fn create_fork_graph(
        &self,
        name: &str,
        code: &str,
        parent_id: &str,
        fork_node: &str,
        fork_label: Option<&str>,
    ) -> Result<GraphId, StorageError> {
        let conn = self.conn.lock().unwrap();
        let id = Self::new_id();
        let now = Self::now();
        conn.execute(
            "INSERT INTO graphs (id, name, version, status, code, parent_id, fork_node, fork_label, created_at, updated_at)
             VALUES (?1, ?2, 1, 'draft', ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, name, code, parent_id, fork_node, fork_label, now],
        ).map_err(|e| StorageError::Internal(format!("create_fork_graph failed: {}", e)))?;
        // Also insert into graph_versions
        conn.execute(
            "INSERT INTO graph_versions (graph_id, version, code, created_at)
             VALUES (?1, 1, ?2, ?3)",
            params![id, code, now],
        ).map_err(|e| StorageError::Internal(format!("create_fork_graph version failed: {}", e)))?;
        Ok(GraphId::new(&id))
    }
}

impl GraphStorage for SqliteStorage {
    fn create_graph(&self, name: &str, code: &str) -> Result<GraphId, StorageError> {
        let conn = self.conn.lock().unwrap();
        let id = Self::new_id();
        let now = Self::now();
        conn.execute(
            "INSERT INTO graphs (id, name, version, status, code, created_at, updated_at)
             VALUES (?1, ?2, 1, 'draft', ?3, ?4, ?4)",
            params![id, name, code, now],
        ).map_err(|e| StorageError::Internal(format!("create_graph failed: {}", e)))?;
        // Also insert into graph_versions
        conn.execute(
            "INSERT INTO graph_versions (graph_id, version, code, created_at)
             VALUES (?1, 1, ?2, ?3)",
            params![id, code, now],
        ).map_err(|e| StorageError::Internal(format!("create_graph version failed: {}", e)))?;
        Ok(GraphId::new(&id))
    }

    fn update_graph(&self, id: &GraphId, code: &str) -> Result<Version, StorageError> {
        let conn = self.conn.lock().unwrap();
        let now = Self::now();
        // Get current version
        let current: i64 = conn.query_row(
            "SELECT version FROM graphs WHERE id = ?1",
            params![id.0],
            |row| row.get(0),
        ).map_err(|_| StorageError::GraphNotFound(id.clone()))?;
        let new_version = current + 1;

        // Update graph header
        conn.execute(
            "UPDATE graphs SET code = ?1, version = ?2, updated_at = ?3 WHERE id = ?4",
            params![code, new_version, now, id.0],
        ).map_err(|e| StorageError::Internal(format!("update_graph failed: {}", e)))?;

        // Insert new version snapshot
        conn.execute(
            "INSERT INTO graph_versions (graph_id, version, code, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id.0, new_version, code, now],
        ).map_err(|e| StorageError::Internal(format!("update_graph version failed: {}", e)))?;

        Ok(Version(new_version as u64))
    }

    fn deploy(&self, id: &GraphId, version: Version) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        let ver = version.0 as i64;
        // Verify version exists
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM graph_versions WHERE graph_id = ?1 AND version = ?2",
            params![id.0, ver],
            |row| row.get::<_, i64>(0),
        ).map(|c| c > 0).unwrap_or(false);
        if !exists {
            return Err(StorageError::VersionNotFound(version, id.clone()));
        }
        conn.execute(
            "UPDATE graphs SET active_version = ?1, status = 'deployed' WHERE id = ?2",
            params![ver, id.0],
        ).map_err(|e| StorageError::Internal(format!("deploy failed: {}", e)))?;
        Ok(())
    }

    fn load_plan(&self, id: &GraphId) -> Result<Vec<u8>, StorageError> {
        let conn = self.conn.lock().unwrap();
        let plan: Vec<u8> = conn.query_row(
            "SELECT execution_plan FROM graphs WHERE id = ?1",
            params![id.0],
            |row| row.get(0),
        ).map_err(|_| StorageError::GraphNotFound(id.clone()))?;
        Ok(plan)
    }

    fn load_graph(&self, id: &GraphId) -> Result<GraphDefinition, StorageError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, version, status, code, active_version, parent_id, fork_node, fork_label, created_at, updated_at
             FROM graphs WHERE id = ?1",
            params![id.0],
            |row| {
                let parent_id: Option<String> = row.get(6)?;
                Ok(GraphDefinition {
                    id: GraphId::new(&row.get::<_, String>(0)?),
                    name: row.get(1)?,
                    version: Version(row.get::<_, i64>(2)? as u64),
                    status: row.get(3)?,
                    code: row.get(4)?,
                    execution_plan: None, // loaded separately via load_plan
                    active: row.get::<_, Option<i64>>(5)?.is_some(),
                    parent_id: parent_id.map(|p| GraphId::new(&p)),
                    fork_node: row.get(7)?,
                    fork_label: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        ).map_err(|_| StorageError::GraphNotFound(id.clone()))
    }

    fn save_execution(&self, exec: &Execution) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        let input_json = serde_json::to_string(&exec.input)
            .map_err(|e| StorageError::Internal(format!("serialize input: {}", e)))?;
        let output_json = exec.output.as_ref().map(|v| serde_json::to_string(v).unwrap_or_default());
        let context_json = exec.context.as_ref().map(|c| serde_json::to_string(c).unwrap_or_default());

        // Serialize status as lowercase string
        let status_str = match exec.status {
            ExecutionStatus::Running => "running",
            ExecutionStatus::Paused => "paused",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
        };
        conn.execute(
            "INSERT OR REPLACE INTO executions
             (id, graph_id, graph_version, input, output, status, error, started_at, completed_at, duration_us, context)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                exec.id, exec.graph_id.0, exec.graph_version.0 as i64,
                input_json, output_json, status_str,
                exec.error, exec.started_at, exec.completed_at,
                exec.duration_us.map(|d| d as i64), context_json,
            ],
        ).map_err(|e| StorageError::Internal(format!("save_execution failed: {}", e)))?;
        Ok(())
    }

    fn save_step(&self, step: &ExecutionStep) -> Result<(), StorageError> {
        let conn = self.conn.lock().unwrap();
        let before_json = step.context_before.as_ref()
            .map(|c| serde_json::to_string(c).unwrap_or_default());
        let after_json = step.context_after.as_ref()
            .map(|c| serde_json::to_string(c).unwrap_or_default());

        conn.execute(
            "INSERT OR REPLACE INTO execution_steps
             (id, execution_id, node_id, node_op, status, error, started_at, completed_at, duration_us, context_before, context_after, parent_step_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                step.id, step.execution_id, step.node_id, step.node_op,
                step.status, step.error, step.started_at, step.completed_at,
                step.duration_us.map(|d| d as i64), before_json, after_json,
                step.parent_step_id,
            ],
        ).map_err(|e| StorageError::Internal(format!("save_step failed: {}", e)))?;
        Ok(())
    }

    fn load_execution(&self, id: &str) -> Result<Execution, StorageError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, graph_id, graph_version, input, output, status, error, started_at, completed_at, duration_us, context
             FROM executions WHERE id = ?1",
            params![id],
            |row| {
                let input_json: String = row.get(3)?;
                let input: tinypipe_api::types::Context = serde_json::from_str(&input_json)
                    .unwrap_or_default();
                let output: Option<Value> = row.get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok());
                let context: Option<tinypipe_api::types::Context> = row.get::<_, Option<String>>(10)?
                    .and_then(|s| serde_json::from_str(&s).ok());
                let status_str: String = row.get(5)?;
                let status = match status_str.as_str() {
                    "running" => ExecutionStatus::Running,
                    "paused" => ExecutionStatus::Paused,
                    "completed" => ExecutionStatus::Completed,
                    "failed" => ExecutionStatus::Failed,
                    _ => ExecutionStatus::Running,
                };
                Ok(Execution {
                    id: row.get(0)?,
                    graph_id: GraphId::new(&row.get::<_, String>(1)?),
                    graph_version: Version(row.get::<_, i64>(2)? as u64),
                    input,
                    output,
                    status,
                    error: row.get(6)?,
                    started_at: row.get(7)?,
                    completed_at: row.get(8)?,
                    duration_us: row.get::<_, Option<i64>>(9)?.map(|d| d as u64),
                    context,
                })
            },
        ).map_err(|_| StorageError::ExecutionNotFound(id.into()))
    }

    fn list_paused_executions(&self) -> Result<Vec<Execution>, StorageError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, graph_id, graph_version, input, output, status, error, started_at, completed_at, duration_us, context
             FROM executions WHERE status = 'paused'"
        ).map_err(|e| StorageError::Internal(format!("list_paused failed: {}", e)))?;

        let rows = stmt.query_map([], |row| {
            let input_json: String = row.get(3)?;
            let input: tinypipe_api::types::Context = serde_json::from_str(&input_json)
                .unwrap_or_default();
            let output: Option<Value> = row.get::<_, Option<String>>(4)?
                .and_then(|s| serde_json::from_str(&s).ok());
            let context: Option<tinypipe_api::types::Context> = row.get::<_, Option<String>>(10)?
                .and_then(|s| serde_json::from_str(&s).ok());
            Ok(Execution {
                id: row.get(0)?,
                graph_id: GraphId::new(&row.get::<_, String>(1)?),
                graph_version: Version(row.get::<_, i64>(2)? as u64),
                input,
                output,
                status: ExecutionStatus::Paused,
                error: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
                duration_us: row.get::<_, Option<i64>>(9)?.map(|d| d as u64),
                context,
            })
        }).map_err(|e| StorageError::Internal(format!("list_paused query: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| StorageError::Internal(format!("row error: {}", e)))?);
        }
        Ok(results)
    }

    // ==================== Branch Explore (v2.1) ====================

    fn fork_graph(
        &self,
        id: &GraphId,
        fork_node: &str,
        code: &str,
        label: Option<&str>,
    ) -> Result<GraphId, StorageError> {
        // Load parent to get name
        let parent = self.load_graph(id)?;
        let child_id = self.create_fork_graph(
            &parent.name,
            code,
            &id.0,
            fork_node,
            label,
        )?;
        Ok(child_id)
    }

    fn list_children(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, version, status, code, active_version, parent_id, fork_node, fork_label, created_at, updated_at
             FROM graphs WHERE parent_id = ?1 ORDER BY created_at ASC"
        ).map_err(|e| StorageError::Internal(format!("list_children prepare: {}", e)))?;

        let rows = stmt.query_map(params![id.0], |row| {
            let parent_id: Option<String> = row.get(6)?;
            Ok(GraphDefinition {
                id: GraphId::new(&row.get::<_, String>(0)?),
                name: row.get(1)?,
                version: Version(row.get::<_, i64>(2)? as u64),
                status: row.get(3)?,
                code: row.get(4)?,
                execution_plan: None,
                active: row.get::<_, Option<i64>>(5)?.is_some(),
                parent_id: parent_id.map(|p| GraphId::new(&p)),
                fork_node: row.get(7)?,
                fork_label: row.get(8)?,
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        }).map_err(|e| StorageError::Internal(format!("list_children query: {}", e)))?;

        let mut children = Vec::new();
        for row in rows {
            children.push(row.map_err(|e| StorageError::Internal(format!("list_children row: {}", e)))?);
        }
        Ok(children)
    }

    fn graph_lineage(&self, id: &GraphId) -> Result<Vec<GraphDefinition>, StorageError> {
        let mut lineage: Vec<GraphDefinition> = Vec::new();
        let mut current_id = Some(id.clone());

        // Walk up parent chain (root first, then ordered to current)
        while let Some(ref cid) = current_id {
            let graph = self.load_graph(cid)?;
            current_id = graph.parent_id.clone();
            lineage.push(graph);
        }
        // Reverse so root is first, current is last
        lineage.reverse();
        Ok(lineage)
    }

    fn graph_tree(&self, id: &GraphId) -> Result<GraphTreeNode, StorageError> {
        let graph = self.load_graph(id)?;
        let children = self.list_children(id)?;
        let mut tree_children = Vec::new();
        for child in &children {
            let subtree = self.graph_tree(&child.id)?;
            tree_children.push(subtree);
        }
        Ok(GraphTreeNode {
            graph,
            children: tree_children,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> SqliteStorage {
        SqliteStorage::in_memory().expect("in-memory db")
    }

    #[test]
    fn test_create_and_load_graph() {
        let store = setup();
        let id = store.create_graph("test_graph", "def graph(): pass").unwrap();
        let g = store.load_graph(&id).unwrap();
        assert_eq!(g.name, "test_graph");
        assert_eq!(g.code, "def graph(): pass");
        assert_eq!(g.version, Version(1));
        assert_eq!(g.status, "draft");
    }

    #[test]
    fn test_update_graph_creates_new_version() {
        let store = setup();
        let id = store.create_graph("test", "def graph(): pass").unwrap();
        let v = store.update_graph(&id, "def graph():\n    return 1").unwrap();
        assert_eq!(v, Version(2));
        let g = store.load_graph(&id).unwrap();
        assert_eq!(g.version, Version(2));
        assert_eq!(g.code, "def graph():\n    return 1");
    }

    #[test]
    fn test_deploy_graph() {
        let store = setup();
        let id = store.create_graph("test", "def graph(): pass").unwrap();
        store.deploy(&id, Version(1)).unwrap();
        let g = store.load_graph(&id).unwrap();
        assert_eq!(g.status, "deployed");
        assert!(g.active);
    }

    #[test]
    fn test_deploy_nonexistent_version() {
        let store = setup();
        let id = store.create_graph("test", "def graph(): pass").unwrap();
        let result = store.deploy(&id, Version(99));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_nonexistent_graph() {
        let store = setup();
        let id = GraphId::new("nonexistent");
        let result = store.load_graph(&id);
        assert!(matches!(result, Err(StorageError::GraphNotFound(_))));
    }

    #[test]
    fn test_save_and_load_execution() {
        let store = setup();
        let graph_id = store.create_graph("test", "def graph(): pass").unwrap();
        let exec = Execution {
            id: "exec-1".into(),
            graph_id: graph_id.clone(),
            graph_version: Version(1),
            input: tinypipe_api::types::Context::new(),
            output: Some(Value::Int(42)),
            status: ExecutionStatus::Completed,
            error: None,
            started_at: "1000".into(),
            completed_at: Some("2000".into()),
            duration_us: Some(1000),
            context: None,
        };
        store.save_execution(&exec).unwrap();
        let loaded = store.load_execution("exec-1").unwrap();
        assert_eq!(loaded.id, "exec-1");
        assert_eq!(loaded.output, Some(Value::Int(42)));
        assert!(matches!(loaded.status, ExecutionStatus::Completed));
    }

    #[test]
    fn test_list_paused_executions() {
        let store = setup();
        let graph_id = store.create_graph("test", "def graph(): pass").unwrap();
        let exec = Execution {
            id: "paused-1".into(),
            graph_id: graph_id.clone(),
            graph_version: Version(1),
            input: tinypipe_api::types::Context::new(),
            output: None,
            status: ExecutionStatus::Paused,
            error: None,
            started_at: "1000".into(),
            completed_at: None,
            duration_us: None,
            context: None,
        };
        store.save_execution(&exec).unwrap();
        let paused = store.list_paused_executions().unwrap();
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].id, "paused-1");
    }

    #[test]
    fn test_save_and_load_step() {
        let store = setup();
        let step = ExecutionStep {
            id: "step-1".into(),
            execution_id: "exec-1".into(),
            node_id: "n0".into(),
            node_op: "Input".into(),
            status: "completed".into(),
            error: None,
            started_at: "1000".into(),
            completed_at: Some("1010".into()),
            duration_us: Some(10),
            context_before: None,
            context_after: None,
            parent_step_id: None,
        };
        store.save_step(&step).unwrap();
    }

    // ==================== Branch Explore Tests (v2.1) ====================

    #[test]
    fn test_fork_graph_creates_child() {
        let store = setup();
        let parent_id = store.create_graph("parent", "def graph(): pass").unwrap();
        let child_id = store.fork_graph(&parent_id, "n0", "def graph():\n    return 42", Some("modified_return"))
            .expect("fork should succeed");

        // Child exists and has correct parent
        let child = store.load_graph(&child_id).unwrap();
        assert_eq!(child.name, "parent");  // inherits name
        assert_eq!(child.code, "def graph():\n    return 42");
        assert_eq!(child.parent_id, Some(parent_id.clone()));
        assert_eq!(child.fork_node.as_deref(), Some("n0"));
        assert_eq!(child.fork_label.as_deref(), Some("modified_return"));
        assert_eq!(child.version.0, 1);  // fresh version
    }

    #[test]
    fn test_fork_graph_without_label() {
        let store = setup();
        let parent_id = store.create_graph("parent", "def graph(): pass").unwrap();
        let child_id = store.fork_graph(&parent_id, "n1", "def graph():\n    return 1", None)
            .expect("fork should succeed");
        let child = store.load_graph(&child_id).unwrap();
        assert_eq!(child.fork_node.as_deref(), Some("n1"));
        assert_eq!(child.fork_label, None);
    }

    #[test]
    fn test_list_children_returns_forked_graphs() {
        let store = setup();
        let parent_id = store.create_graph("parent", "def g(): pass").unwrap();

        // No children initially
        let children = store.list_children(&parent_id).unwrap();
        assert!(children.is_empty());

        // Fork twice
        let child1 = store.fork_graph(&parent_id, "n0", "code1", Some("branch_a")).unwrap();
        let child2 = store.fork_graph(&parent_id, "n1", "code2", Some("branch_b")).unwrap();

        let children = store.list_children(&parent_id).unwrap();
        assert_eq!(children.len(), 2);

        // Children are ordered by created_at ASC
        assert!(children.iter().any(|c| c.id == child1));
        assert!(children.iter().any(|c| c.id == child2));

        // Verify child metadata
        let c1 = children.iter().find(|c| c.id == child1).unwrap();
        assert_eq!(c1.fork_label.as_deref(), Some("branch_a"));
    }

    #[test]
    fn test_graph_lineage_returns_ancestors() {
        let store = setup();
        let parent_id = store.create_graph("root", "def root(): pass").unwrap();
        let child1_id = store.fork_graph(&parent_id, "n0", "def c1(): pass", None).unwrap();
        let child2_id = store.fork_graph(&child1_id, "n1", "def c2(): pass", Some("deep")).unwrap();

        // Lineage of child2: [root, child1, child2]
        let lineage = store.graph_lineage(&child2_id).unwrap();
        assert_eq!(lineage.len(), 3);
        assert_eq!(lineage[0].id, parent_id);
        assert_eq!(lineage[1].id, child1_id);
        assert_eq!(lineage[2].id, child2_id);
    }

    #[test]
    fn test_graph_lineage_single_node() {
        let store = setup();
        let id = store.create_graph("solo", "def g(): pass").unwrap();
        let lineage = store.graph_lineage(&id).unwrap();
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].id, id);
    }

    #[test]
    fn test_graph_tree_builds_recursive_structure() {
        let store = setup();
        let root_id = store.create_graph("root", "def root(): pass").unwrap();
        let child1 = store.fork_graph(&root_id, "n0", "def c1(): pass", None).unwrap();
        let child2 = store.fork_graph(&root_id, "n1", "def c2(): pass", Some("alt")).unwrap();
        let grandchild = store.fork_graph(&child1, "n2", "def gc(): pass", None).unwrap();

        // Build tree from root
        let tree = store.graph_tree(&root_id).unwrap();
        assert_eq!(tree.graph.id, root_id);
        assert_eq!(tree.children.len(), 2);

        // Find child1 in tree
        let c1_node = tree.children.iter().find(|n| n.graph.id == child1).unwrap();
        assert_eq!(c1_node.children.len(), 1);
        assert_eq!(c1_node.children[0].graph.id, grandchild);

        // child2 has no children
        let c2_node = tree.children.iter().find(|n| n.graph.id == child2).unwrap();
        assert_eq!(c2_node.children.len(), 0);
    }

    #[test]
    fn test_fork_nonexistent_graph_returns_error() {
        let store = setup();
        let fake_id = GraphId::new("nonexistent-graph-id");
        let result = store.fork_graph(&fake_id, "n0", "code", None);
        assert!(matches!(result, Err(StorageError::GraphNotFound(_))));
    }
}
