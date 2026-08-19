//! SQLite-backed index store.
//!
//! Replaces the previous postcard whole-index snapshot with a real on-disk
//! store. Symbols and call-graph edges live in SQLite tables with secondary
//! indexes, so point lookups and adjacency traversal never require loading
//! the entire index into memory. WAL mode keeps readers from blocking the
//! writer and supports safe multi-process access to a shared index path.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::symbols::{Symbol, SymbolKind};

/// A directed call-graph edge between two symbols (by symbol id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub caller_id: i64,
    pub callee_id: i64,
    pub edge_type: String,
    pub resolved: bool,
}

/// File-level metadata used for change detection (content hash, mtime, size).
///
/// Kept separate from [`Symbol`] so the store does not depend on the
/// higher-level persistence types in `crate::persist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: String,
    pub content_hash: String,
    pub modified_time: u64,
    pub size: u64,
}

const SCHEMA_VERSION: i64 = 2;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS symbols (
    id             INTEGER PRIMARY KEY,
    repo           TEXT NOT NULL,
    name           TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    kind           TEXT NOT NULL,
    qualified_name TEXT,
    symbol         BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbols_repo      ON symbols(repo);
CREATE INDEX IF NOT EXISTS idx_symbols_name      ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file      ON symbols(repo, file_path);
CREATE INDEX IF NOT EXISTS idx_symbols_qualified ON symbols(qualified_name);

CREATE TABLE IF NOT EXISTS edges (
    id        INTEGER PRIMARY KEY,
    caller_id INTEGER NOT NULL,
    callee_id INTEGER NOT NULL,
    edge_type TEXT NOT NULL,
    resolved  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_edges_caller ON edges(caller_id);
CREATE INDEX IF NOT EXISTS idx_edges_callee ON edges(callee_id);

CREATE TABLE IF NOT EXISTS files (
    repo          TEXT NOT NULL,
    path          TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    modified_time INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    PRIMARY KEY (repo, path)
);
CREATE INDEX IF NOT EXISTS idx_files_repo ON files(repo);

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// SQLite-backed index store, shared across the engine's threads.
///
/// `Connection` is `!Sync`, so access is serialized behind a `Mutex`; WAL mode
/// additionally keeps read transactions from blocking the single writer.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (or create) the store at `path`, applying the schema if needed.
    pub fn open(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("open sqlite store {:?}", path))?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA_SQL)?;

        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match current {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => {
                let parsed: i64 = v.parse().context("parse stored schema version")?;
                if parsed != SCHEMA_VERSION {
                    bail!("store schema version {parsed} != {SCHEMA_VERSION}");
                }
            }
        }
        Ok(())
    }

    // === symbols ===

    /// Insert symbols in a single transaction. Returns the number inserted.
    pub fn insert_symbols(&self, repo: &str, symbols: &[Symbol]) -> Result<usize> {
        if symbols.is_empty() {
            return Ok(0);
        }
        let mut guard = self.conn.lock().unwrap();
        let tx = guard.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO symbols (repo, name, file_path, kind, qualified_name, symbol)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for sym in symbols {
                let blob = postcard::to_stdvec(sym).context("serialize symbol")?;
                stmt.execute(params![
                    repo,
                    sym.name,
                    sym.file_path,
                    kind_str(&sym.kind),
                    sym.qualified_name,
                    blob,
                ])?;
            }
        }
        tx.commit()?;
        Ok(symbols.len())
    }

    /// Delete every symbol belonging to a file. Returns the number removed.
    pub fn delete_symbols_by_file(&self, repo: &str, file_path: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM symbols WHERE repo=?1 AND file_path=?2",
            params![repo, file_path],
        )?)
    }

    /// Delete every symbol belonging to a repo. Returns the number removed.
    pub fn clear_repo(&self, repo: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM symbols WHERE repo=?1", params![repo])?)
    }

    pub fn symbol_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn symbol_by_id(&self, id: i64) -> Result<Option<Symbol>> {
        let conn = self.conn.lock().unwrap();
        let blob: Option<Vec<u8>> = conn
            .query_row("SELECT symbol FROM symbols WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        blob.map(deserialize_symbol).transpose()
    }

    pub fn symbols_by_name(&self, name: &str) -> Result<Vec<Symbol>> {
        self.query_symbols(
            "SELECT symbol FROM symbols WHERE name=?1 ORDER BY id",
            params![name],
        )
    }

    pub fn symbols_by_file(&self, repo: &str, file_path: &str) -> Result<Vec<Symbol>> {
        self.query_symbols(
            "SELECT symbol FROM symbols WHERE repo=?1 AND file_path=?2 ORDER BY id",
            params![repo, file_path],
        )
    }

    /// Stream every symbol in `repo` through `f` without materializing them.
    /// Used by the load path so the index is not read wholesale into memory.
    pub fn for_each_symbol(&self, repo: &str, mut f: impl FnMut(Symbol)) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT symbol FROM symbols WHERE repo=?1 ORDER BY id")?;
        let rows = stmt.query_map(params![repo], |r| r.get::<_, Vec<u8>>(0))?;
        for row in rows {
            f(deserialize_symbol(row?)?);
        }
        Ok(())
    }

    fn query_symbols(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Symbol>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(deserialize_symbol(row?)?);
        }
        Ok(out)
    }

    // === edges ===

    /// Insert edges in a single transaction. Returns the number inserted.
    pub fn add_edges(&self, edges: &[Edge]) -> Result<usize> {
        if edges.is_empty() {
            return Ok(0);
        }
        let mut guard = self.conn.lock().unwrap();
        let tx = guard.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO edges (caller_id, callee_id, edge_type, resolved)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in edges {
                stmt.execute(params![
                    e.caller_id,
                    e.callee_id,
                    e.edge_type,
                    e.resolved as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(edges.len())
    }

    pub fn clear_edges(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM edges", [])?)
    }

    /// Symbol ids that call `symbol_id`.
    pub fn callers(&self, symbol_id: i64) -> Result<Vec<i64>> {
        self.query_ids(
            "SELECT caller_id FROM edges WHERE callee_id=?1 ORDER BY id",
            params![symbol_id],
        )
    }

    /// Symbol ids that `symbol_id` calls.
    pub fn callees(&self, symbol_id: i64) -> Result<Vec<i64>> {
        self.query_ids(
            "SELECT callee_id FROM edges WHERE caller_id=?1 ORDER BY id",
            params![symbol_id],
        )
    }

    pub fn edge_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    fn query_ids(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    // === files ===

    /// Stream every file metadata row in `repo` through `f`.
    pub fn for_each_file(&self, repo: &str, mut f: impl FnMut(FileRow)) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, content_hash, modified_time, size FROM files WHERE repo=?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![repo], |r| {
            Ok(FileRow {
                path: r.get(0)?,
                content_hash: r.get(1)?,
                modified_time: r.get::<_, i64>(2)? as u64,
                size: r.get::<_, i64>(3)? as u64,
            })
        })?;
        for row in rows {
            f(row?);
        }
        Ok(())
    }

    pub fn file_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    // === meta ===

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row("SELECT value FROM meta WHERE key=?1", params![key], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(v)
    }

    // === bulk replace ===

    /// Atomically replace every symbol and file row belonging to `repo`.
    ///
    /// Runs in a single transaction so a concurrent reader never observes a
    /// half-written repo (WAL keeps the writer from blocking readers).
    pub fn replace_repo(&self, repo: &str, files: &[FileRow], symbols: &[Symbol]) -> Result<()> {
        let mut guard = self.conn.lock().unwrap();
        let tx = guard.transaction()?;
        {
            tx.execute("DELETE FROM symbols WHERE repo=?1", params![repo])?;
            tx.execute("DELETE FROM files WHERE repo=?1", params![repo])?;

            let mut fstmt = tx.prepare(
                "INSERT INTO files (repo, path, content_hash, modified_time, size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in files {
                fstmt.execute(params![
                    repo,
                    f.path,
                    f.content_hash,
                    f.modified_time as i64,
                    f.size as i64,
                ])?;
            }

            let mut sstmt = tx.prepare(
                "INSERT INTO symbols (repo, name, file_path, kind, qualified_name, symbol)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for sym in symbols {
                let blob = postcard::to_stdvec(sym).context("serialize symbol")?;
                sstmt.execute(params![
                    repo,
                    sym.name,
                    sym.file_path,
                    kind_str(&sym.kind),
                    sym.qualified_name,
                    blob,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn deserialize_symbol(blob: Vec<u8>) -> Result<Symbol> {
    postcard::from_bytes(&blob).context("deserialize symbol blob")
}

fn kind_str(kind: &SymbolKind) -> &'static str {
    use SymbolKind::*;
    match kind {
        Struct => "Struct",
        Class => "Class",
        Enum => "Enum",
        Interface => "Interface",
        Trait => "Trait",
        TypeAlias => "TypeAlias",
        Function => "Function",
        Method => "Method",
        Constructor => "Constructor",
        Module => "Module",
        Namespace => "Namespace",
        Package => "Package",
        Constant => "Constant",
        Variable => "Variable",
        Field => "Field",
        Parameter => "Parameter",
        Implementation => "Implementation",
        Macro => "Macro",
        Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, file: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            file_path: file.to_string(),
            start_line: line,
            end_line: line,
            signature: Some(format!("fn {name}()")),
            qualified_name: Some(format!("mod::{name}")),
            doc_comment: None,
        }
    }

    fn temp_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("test.db")).unwrap();
        (store, dir)
    }

    #[test]
    fn test_insert_and_count() {
        let (store, _d) = temp_store();
        let symbols = vec![
            sym("foo", "a.rs", 1),
            sym("bar", "a.rs", 5),
            sym("baz", "b.rs", 1),
        ];
        let n = store.insert_symbols("repo", &symbols).unwrap();
        assert_eq!(n, 3);
        assert_eq!(store.symbol_count(), 3);
    }

    #[test]
    fn test_query_by_name_and_file() {
        let (store, _d) = temp_store();
        store
            .insert_symbols(
                "repo",
                &[
                    sym("foo", "a.rs", 1),
                    sym("foo", "b.rs", 2),
                    sym("bar", "a.rs", 3),
                ],
            )
            .unwrap();

        let by_name = store.symbols_by_name("foo").unwrap();
        assert_eq!(by_name.len(), 2);
        assert!(by_name.iter().all(|s| s.name == "foo"));

        let by_file = store.symbols_by_file("repo", "a.rs").unwrap();
        assert_eq!(by_file.len(), 2);
        assert!(by_file.iter().all(|s| s.file_path == "a.rs"));

        assert_eq!(
            store.symbols_by_file("repo", "missing.rs").unwrap().len(),
            0
        );
    }

    #[test]
    fn test_delete_by_file() {
        let (store, _d) = temp_store();
        store
            .insert_symbols(
                "repo",
                &[
                    sym("foo", "a.rs", 1),
                    sym("bar", "a.rs", 2),
                    sym("baz", "b.rs", 1),
                ],
            )
            .unwrap();
        assert_eq!(store.delete_symbols_by_file("repo", "a.rs").unwrap(), 2);
        assert_eq!(store.symbol_count(), 1);
    }

    #[test]
    fn test_for_each_symbol_streams() {
        let (store, _d) = temp_store();
        store
            .insert_symbols(
                "repo",
                &[
                    sym("a", "f.rs", 1),
                    sym("b", "f.rs", 2),
                    sym("c", "g.rs", 1),
                ],
            )
            .unwrap();

        let mut names = Vec::new();
        store
            .for_each_symbol("repo", |s| names.push(s.name))
            .unwrap();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_edges_adjacency() {
        let (store, _d) = temp_store();
        store
            .insert_symbols(
                "repo",
                &[
                    sym("a", "f.rs", 1),
                    sym("b", "f.rs", 2),
                    sym("c", "f.rs", 3),
                ],
            )
            .unwrap();

        // ids are 1,2,3 in insertion order
        let edges = vec![
            Edge {
                caller_id: 1,
                callee_id: 2,
                edge_type: "call".into(),
                resolved: true,
            },
            Edge {
                caller_id: 1,
                callee_id: 3,
                edge_type: "call".into(),
                resolved: false,
            },
            Edge {
                caller_id: 2,
                callee_id: 3,
                edge_type: "call".into(),
                resolved: true,
            },
        ];
        assert_eq!(store.add_edges(&edges).unwrap(), 3);

        assert_eq!(store.callees(1).unwrap(), vec![2, 3]);
        assert_eq!(store.callers(3).unwrap(), vec![1, 2]);
        assert_eq!(store.callees(3).unwrap().len(), 0);
        assert_eq!(store.edge_count(), 3);
    }

    #[test]
    fn test_reopen_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        {
            let store = SqliteStore::open(&path).unwrap();
            store
                .insert_symbols("repo", &[sym("foo", "a.rs", 1)])
                .unwrap();
        }
        // Re-open: data must survive and be queryable.
        let store = SqliteStore::open(&path).unwrap();
        assert_eq!(store.symbol_count(), 1);
        assert_eq!(store.symbols_by_name("foo").unwrap().len(), 1);
    }

    #[test]
    fn test_replace_repo_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("test.db")).unwrap();

        let files = vec![
            FileRow {
                path: "a.rs".into(),
                content_hash: "abc".into(),
                modified_time: 1000,
                size: 10,
            },
            FileRow {
                path: "b.rs".into(),
                content_hash: "def".into(),
                modified_time: 2000,
                size: 20,
            },
        ];
        let symbols = vec![sym("foo", "a.rs", 1), sym("bar", "b.rs", 2)];

        store.replace_repo("repo", &files, &symbols).unwrap();
        assert_eq!(store.file_count(), 2);
        assert_eq!(store.symbol_count(), 2);

        let mut paths = Vec::new();
        store.for_each_file("repo", |f| paths.push(f.path)).unwrap();
        paths.sort();
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);

        // Replace with a single file — old rows must be gone.
        store
            .replace_repo(
                "repo",
                &[FileRow {
                    path: "c.rs".into(),
                    content_hash: "ghi".into(),
                    modified_time: 3000,
                    size: 30,
                }],
                &[sym("baz", "c.rs", 1)],
            )
            .unwrap();
        assert_eq!(store.file_count(), 1);
        assert_eq!(store.symbol_count(), 1);

        // Meta round-trip.
        store.set_meta("repo_root", "/tmp/x").unwrap();
        assert_eq!(
            store.get_meta("repo_root").unwrap().as_deref(),
            Some("/tmp/x")
        );
    }
}
