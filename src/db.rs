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
    r#"
-- Cleanup plans (see plan.rs): files to move to a volume's trash, each with the copy that stays.
CREATE TABLE plans (
    id          TEXT PRIMARY KEY,       -- also names the plan's trash folder on the machine
    machine     TEXT NOT NULL,
    kind        TEXT NOT NULL,          -- 'drain' or 'keep'
    folders     TEXT NOT NULL,          -- JSON array of machine:volume/path; for 'keep', [keeper, over]
    state       TEXT NOT NULL,          -- draft, applying, applied, undone, emptied
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE plan_items (
    plan_id        TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    volume_id      TEXT NOT NULL,       -- the file to remove
    path           TEXT NOT NULL,
    size           INTEGER NOT NULL,
    mtime          INTEGER NOT NULL,
    fingerprint    TEXT NOT NULL,
    keep_volume_id TEXT NOT NULL,       -- the copy that stays
    keep_path      TEXT NOT NULL,
    keep_size      INTEGER NOT NULL,
    keep_mtime     INTEGER NOT NULL,
    outcome        TEXT,                -- NULL until applied: 'moved' or 'skipped'
    reason         TEXT,                -- why it was skipped
    PRIMARY KEY (plan_id, seq)
);
CREATE INDEX plan_items_file ON plan_items (volume_id, path);
-- Files in the plan's folders that it leaves alone, and why.
CREATE TABLE plan_left (
    plan_id   TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    volume_id TEXT NOT NULL,
    path      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    reason    TEXT NOT NULL
);
CREATE INDEX plan_left_plan ON plan_left (plan_id);
"#,
    r#"
-- Folders inside a plan's folders that it leaves out entirely (JSON array, like folders).
ALTER TABLE plans ADD COLUMN except_folders TEXT NOT NULL DEFAULT '[]';
"#,
    r#"
-- Plans from a supplied list (kind 'list'): moves, and removals a producer vouches for.
-- SQLite can't drop NOT NULL, so plan_items is rebuilt: a move has no kept copy, and a listed
-- file may not be fingerprinted yet. Existing rows become plain removals.
CREATE TABLE plan_items_new (
    plan_id        TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    action         TEXT NOT NULL DEFAULT 'remove',  -- 'remove' or 'move'
    volume_id      TEXT NOT NULL,       -- the file to remove or move
    path           TEXT NOT NULL,
    size           INTEGER NOT NULL,
    mtime          INTEGER NOT NULL,
    fingerprint    TEXT,
    vouched        INTEGER NOT NULL DEFAULT 0,  -- removal the list's producer vouched for
    note           TEXT,                -- the producer's reason
    keep_volume_id TEXT,                -- removal: the copy that stays
    keep_path      TEXT,
    keep_size      INTEGER,
    keep_mtime     INTEGER,
    dest_volume_id TEXT,                -- move: where it goes
    dest_path      TEXT,
    outcome        TEXT,                -- NULL until applied: 'moved' or 'skipped'
    reason         TEXT,                -- why it was skipped
    PRIMARY KEY (plan_id, seq)
);
INSERT INTO plan_items_new (plan_id, seq, volume_id, path, size, mtime, fingerprint,
    keep_volume_id, keep_path, keep_size, keep_mtime, outcome, reason)
SELECT plan_id, seq, volume_id, path, size, mtime, fingerprint,
    keep_volume_id, keep_path, keep_size, keep_mtime, outcome, reason
FROM plan_items;
DROP TABLE plan_items;
ALTER TABLE plan_items_new RENAME TO plan_items;
CREATE INDEX plan_items_file ON plan_items (volume_id, path);
-- Entries of a supplied list that were rejected, and why.
CREATE TABLE plan_rejected (
    plan_id TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    line    INTEGER NOT NULL,           -- 1-based line in the list
    entry   TEXT NOT NULL,              -- the line as given
    reason  TEXT NOT NULL
);
CREATE INDEX plan_rejected_plan ON plan_rejected (plan_id);
"#,
    r#"
-- What a plan's move carries to the file's new path: the fingerprint and full hash the
-- catalog had, kept until the next scan of that volume gives them to the new entry (when
-- its size and mtime still match), so moved files aren't read again and keep their hashes.
CREATE TABLE carried (
    volume_id    TEXT NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mtime        INTEGER NOT NULL,
    fingerprint  TEXT,
    content_hash TEXT,
    PRIMARY KEY (volume_id, path)
);
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

    #[test]
    fn plans_saved_before_lists_keep_their_items() {
        let mut conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..4] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute_batch(
            "INSERT INTO plans (id, machine, kind, folders, state, created_at, updated_at)
                 VALUES ('p', 'm', 'drain', '[\"m:v/todo\"]', 'applied', 1, 2);
             INSERT INTO plan_items (plan_id, seq, volume_id, path, size, mtime, fingerprint,
                 keep_volume_id, keep_path, keep_size, keep_mtime, outcome, reason)
                 VALUES ('p', 0, 'v', 'todo/a', 3, 4, 'p1:x', 'v', 'a', 3, 5, 'moved', NULL),
                        ('p', 1, 'v', 'todo/b', 6, 7, 'p1:y', 'v', 'b', 6, 8, 'skipped', 'why');",
        )
        .unwrap();

        setup(&mut conn).unwrap();
        type Row = (i64, String, String, bool, Option<String>, Option<String>);
        let rows: Vec<Row> = conn
            .prepare(
                "SELECT seq, action, keep_path, vouched, outcome, reason FROM plan_items
                 WHERE plan_id = 'p' ORDER BY seq",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    0,
                    "remove".into(),
                    "a".into(),
                    false,
                    Some("moved".into()),
                    None
                ),
                (
                    1,
                    "remove".into(),
                    "b".into(),
                    false,
                    Some("skipped".into()),
                    Some("why".into())
                ),
            ]
        );
        // Deleting the plan still deletes its items.
        conn.execute("DELETE FROM plans", []).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM plan_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
