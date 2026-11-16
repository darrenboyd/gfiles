//! The SQLite catalog: opening it and keeping its schema current.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Schema migrations, applied in order. `PRAGMA user_version` records how many have run.
/// Never edit a migration that has shipped; add a new one.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE volumes (
    id           TEXT PRIMARY KEY,      -- UUID from the machine's config
    name         TEXT NOT NULL,
    machine      TEXT NOT NULL,
    root         TEXT NOT NULL,         -- absolute path on that machine
    scan_gen     INTEGER NOT NULL DEFAULT 0,
    last_scan_at INTEGER                -- unix seconds
);

-- One row per cataloged item: a file, or a whole git repository (kind = 'repo').
CREATE TABLE entries (
    id        INTEGER PRIMARY KEY,
    volume_id TEXT NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    path      TEXT NOT NULL,            -- relative to the volume root, '/'-separated
    name      TEXT NOT NULL,
    ext       TEXT,                     -- lowercase, without the dot
    kind      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,         -- unix seconds
    seen_gen  INTEGER NOT NULL,         -- volumes.scan_gen of the last scan that saw it
    UNIQUE (volume_id, path)
);
CREATE INDEX entries_kind ON entries (kind);

CREATE VIRTUAL TABLE entries_fts USING fts5 (
    name, path,
    content = 'entries', content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts (rowid, name, path) VALUES (new.id, new.name, new.path);
END;
CREATE TRIGGER entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, name, path)
        VALUES ('delete', old.id, old.name, old.path);
END;
CREATE TRIGGER entries_au AFTER UPDATE OF name, path ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, name, path)
        VALUES ('delete', old.id, old.name, old.path);
    INSERT INTO entries_fts (rowid, name, path) VALUES (new.id, new.name, new.path);
END;
"#,
    r#"
-- Content fingerprints, computed by the machine holding the file (see fingerprint.rs).
ALTER TABLE entries ADD COLUMN fingerprint TEXT;   -- 'p1:<hex>' sampled, or 'git:<sha>' for a repo
ALTER TABLE entries ADD COLUMN content_hash TEXT;  -- 'b3:<hex>' of the whole file, on request
CREATE INDEX entries_fingerprint ON entries (size, fingerprint) WHERE fingerprint IS NOT NULL;
"#,
];

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut conn =
        Connection::open(path).with_context(|| format!("opening catalog {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    setup(&mut conn)?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    setup(&mut conn)?;
    Ok(conn)
}

fn setup(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Scans and ingests are single long transactions; wait for one to finish rather than fail.
    conn.busy_timeout(std::time::Duration::from_secs(600))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    let applied: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    conn.execute_batch(crate::mirror::CREATE_TABLE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_a_first_version_catalog_keeping_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO volumes (id, name, machine, root) VALUES ('v', 'v', 'm', '/v');
             INSERT INTO entries (volume_id, path, name, kind, size, mtime, seen_gen)
                 VALUES ('v', 'a.txt', 'a.txt', 'document', 3, 0, 1);",
        )
        .unwrap();

        setup(&mut conn).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let row: (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT path, fingerprint, content_hash FROM entries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("a.txt".into(), None, None));
    }
}
