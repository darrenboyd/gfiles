//! Writing a volume's entries into the catalog. Shared by local scans and by `ingest`, which
//! receives other machines' volumes, so both keep the catalog the same way.
//!
//! A write session bumps the volume's `scan_gen`, stamps every entry it is given with it, and
//! at the end deletes entries left with an older stamp: those weren't seen this time, so they
//! no longer exist. Run it inside a transaction so a failed or interrupted write changes
//! nothing.
//!
//! Ingest writes what another machine sent, so every entry and volume root is checked here
//! first: a stored path must stay inside its volume, whoever wrote it.

use anyhow::{Result, bail};
use rusqlite::{Connection, Statement, params};
use serde::{Deserialize, Serialize};

use crate::classify;

/// One cataloged item, as stored and as sent between machines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative to the volume root, `/`-separated.
    pub path: String,
    pub name: String,
    pub ext: Option<String>,
    pub kind: String,
    pub size: i64,
    /// Unix seconds.
    pub mtime: i64,
}

impl Entry {
    /// Refuses an entry whose path could point outside its volume, or whose name or kind
    /// doesn't match what a scan would have produced.
    pub fn validate(&self) -> Result<()> {
        let path = &self.path;
        if path.is_empty() || path.contains('\0') {
            bail!("bad entry path {path:?}");
        }
        if path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            bail!("entry path {path:?} must be relative, with no empty, '.' or '..' parts");
        }
        if path.rsplit('/').next() != Some(self.name.as_str()) {
            bail!("entry name {:?} doesn't match its path {path:?}", self.name);
        }
        if !classify::all_kinds().any(|k| k == self.kind) {
            bail!("entry {path:?} has unknown kind {:?}", self.kind);
        }
        Ok(())
    }
}

/// A volume row: which machine it's on and where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub id: String,
    pub name: String,
    pub machine: String,
    pub root: String,
    pub last_scan_at: Option<i64>,
}

pub struct VolumeWriter<'t> {
    conn: &'t Connection,
    upsert: Statement<'t>,
    volume_id: String,
    generation: i64,
}

impl<'t> VolumeWriter<'t> {
    /// Creates or updates the volume row and starts a new generation. `conn` should be a
    /// transaction.
    pub fn begin(conn: &'t Connection, volume: &Volume) -> Result<VolumeWriter<'t>> {
        if !is_absolute(&volume.root) {
            bail!(
                "volume {:?}: root {:?} is not an absolute path",
                volume.name,
                volume.root
            );
        }
        conn.execute(
            "INSERT INTO volumes (id, name, machine, root) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET
               name = excluded.name, machine = excluded.machine, root = excluded.root",
            params![volume.id, volume.name, volume.machine, volume.root],
        )?;
        let generation = conn.query_row(
            "UPDATE volumes SET scan_gen = scan_gen + 1 WHERE id = ?1 RETURNING scan_gen",
            [&volume.id],
            |r| r.get(0),
        )?;
        // Name and path aren't updated on conflict: they are the key (path) or derived from it
        // (name), and leaving them out of SET keeps the FTS update trigger from firing.
        let upsert = conn.prepare(
            "INSERT INTO entries (volume_id, path, name, ext, kind, size, mtime, seen_gen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (volume_id, path) DO UPDATE SET
               ext = excluded.ext, kind = excluded.kind, size = excluded.size,
               mtime = excluded.mtime, seen_gen = excluded.seen_gen",
        )?;
        Ok(VolumeWriter {
            conn,
            upsert,
            volume_id: volume.id.clone(),
            generation,
        })
    }

    pub fn put(&mut self, e: &Entry) -> Result<()> {
        e.validate()?;
        self.upsert.execute(params![
            self.volume_id,
            e.path,
            e.name,
            e.ext,
            e.kind,
            e.size,
            e.mtime,
            self.generation
        ])?;
        Ok(())
    }

    /// Deletes entries that weren't put this time, except those at or under a path in `keep`
    /// (folders the scan couldn't read: their entries are unknown, not gone), and records when
    /// the volume was scanned. Returns how many were deleted and how many were kept.
    pub fn finish(self, scanned_at: i64, keep: &[String]) -> Result<Finished> {
        let mut finished = Finished::default();
        if keep.is_empty() {
            finished.removed = self.conn.execute(
                "DELETE FROM entries WHERE volume_id = ?1 AND seen_gen < ?2",
                params![self.volume_id, self.generation],
            )? as u64;
        } else {
            let mut stale = self
                .conn
                .prepare("SELECT id, path FROM entries WHERE volume_id = ?1 AND seen_gen < ?2")?;
            let stale: Vec<(i64, String)> = stale
                .query_map(params![self.volume_id, self.generation], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut delete = self.conn.prepare("DELETE FROM entries WHERE id = ?1")?;
            for (id, path) in stale {
                if keep.iter().any(|k| is_within(&path, k)) {
                    finished.kept += 1;
                } else {
                    delete.execute([id])?;
                    finished.removed += 1;
                }
            }
        }
        self.conn.execute(
            "UPDATE volumes SET last_scan_at = ?2 WHERE id = ?1",
            params![self.volume_id, scanned_at],
        )?;
        Ok(finished)
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Finished {
    pub removed: u64,
    /// Not seen this time, but kept because they're under a folder that couldn't be read.
    pub kept: u64,
}

/// Whether `path` is `dir` or inside it (both relative, `/`-separated).
fn is_within(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// An absolute path on any platform a volume can live on: `/...` on Unix, `C:\...` or
/// `\\server\...` on Windows. Checked on the text, since a pulled root comes from another
/// machine and may not be absolute by this platform's rules.
fn is_absolute(root: &str) -> bool {
    let b = root.as_bytes();
    !root.contains('\0')
        && (root.starts_with('/')
            || root.starts_with("\\\\")
            || (b.len() >= 3
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && (b[2] == b'\\' || b[2] == b'/')))
}

pub fn volume(conn: &Connection, id: &str) -> Result<Option<Volume>> {
    let mut stmt =
        conn.prepare("SELECT id, name, machine, root, last_scan_at FROM volumes WHERE id = ?1")?;
    let mut rows = stmt.query_map([id], |r| {
        Ok(Volume {
            id: r.get(0)?,
            name: r.get(1)?,
            machine: r.get(2)?,
            root: r.get(3)?,
            last_scan_at: r.get(4)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

/// Calls `f` with each of a volume's entries, without loading them all into memory.
pub fn for_each_entry(
    conn: &Connection,
    volume_id: &str,
    mut f: impl FnMut(Entry) -> Result<()>,
) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT path, name, ext, kind, size, mtime FROM entries WHERE volume_id = ?1")?;
    let mut rows = stmt.query([volume_id])?;
    while let Some(r) = rows.next()? {
        f(Entry {
            path: r.get(0)?,
            name: r.get(1)?,
            ext: r.get(2)?,
            kind: r.get(3)?,
            size: r.get(4)?,
            mtime: r.get(5)?,
        })?;
    }
    Ok(())
}

/// Removes the volumes (and entries) of every machine not in `machines`, returning each
/// removed volume as (machine, name). The hub uses it to drop remotes taken out of its config.
pub fn forget_machines_except(
    conn: &Connection,
    machines: &[String],
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, machine, name FROM volumes ORDER BY machine, name")?;
    let gone: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, machine, _)| !machines.contains(machine))
        .collect();
    let mut forgotten = Vec::new();
    for (id, machine, name) in gone {
        forget_volume(conn, &id)?;
        forgotten.push((machine, name));
    }
    Ok(forgotten)
}

/// Removes a volume and all its entries.
pub fn forget_volume(conn: &Connection, volume_id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM volumes WHERE id = ?1", [volume_id])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, name: &str) -> Entry {
        Entry {
            path: path.into(),
            name: name.into(),
            ext: None,
            kind: "other".into(),
            size: 0,
            mtime: 0,
        }
    }

    #[test]
    fn validate_keeps_paths_inside_the_volume() {
        assert!(entry("a.txt", "a.txt").validate().is_ok());
        assert!(entry("Books/Le Guin/x.epub", "x.epub").validate().is_ok());
        assert!(entry("odd\\name..txt", "odd\\name..txt").validate().is_ok());
        for (path, name) in [
            ("", ""),
            ("/etc/shadow", "shadow"),
            ("../../.ssh/id_ed25519", "id_ed25519"),
            ("a/../../b", "b"),
            ("./a", "a"),
            ("a//b", "b"),
            ("a/", ""),
            ("a\0b", "a\0b"),
            ("a/b.txt", "c.txt"),
        ] {
            assert!(entry(path, name).validate().is_err(), "{path:?} accepted");
        }
        let mut e = entry("a.txt", "a.txt");
        e.kind = "virus".into();
        assert!(e.validate().is_err());
    }

    #[test]
    fn absolute_roots() {
        for root in [
            "/",
            "/home/darren/Books",
            "C:\\Users\\d",
            "d:/x",
            "\\\\nas\\share",
        ] {
            assert!(is_absolute(root), "{root}");
        }
        for root in ["", "Books", "./x", "~/x", "C:x", "/a\0b"] {
            assert!(!is_absolute(root), "{root}");
        }
    }

    #[test]
    fn within() {
        assert!(is_within("a/b", "a"));
        assert!(is_within("a", "a"));
        assert!(!is_within("ab/c", "a"));
        assert!(!is_within("b", "a"));
    }
}
