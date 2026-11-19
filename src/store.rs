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
use rusqlite::{Connection, OptionalExtension, Statement, params};
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
    /// Sampled content fingerprint (`p1:<hex>`) or a repo's HEAD commit (`git:<sha>`), when
    /// the holding machine has computed it.
    #[serde(default, rename = "fp", skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Hash of the whole file (`b3:<hex>`), computed on request.
    #[serde(default, rename = "hash", skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

impl Entry {
    /// Refuses an entry whose path could point outside its volume, or whose name or kind
    /// doesn't match what a scan would have produced.
    pub fn validate(&self) -> Result<()> {
        let path = &self.path;
        check_rel_path(path)?;
        if path.rsplit('/').next() != Some(self.name.as_str()) {
            bail!("entry name {:?} doesn't match its path {path:?}", self.name);
        }
        if self.ext.is_some() && self.ext != ext_of(&self.name) {
            bail!(
                "entry {path:?} has extension {:?}, which isn't its name's",
                self.ext
            );
        }
        if !classify::all_kinds().any(|k| k == self.kind) {
            bail!("entry {path:?} has unknown kind {:?}", self.kind);
        }
        if let Some(fp) = &self.fingerprint
            && !(is_tagged_hex(fp, "p1:", &[64]) || is_tagged_hex(fp, "git:", &[40, 64]))
        {
            bail!("entry {path:?} has a malformed fingerprint {fp:?}");
        }
        if let Some(hash) = &self.content_hash
            && !is_tagged_hex(hash, "b3:", &[64])
        {
            bail!("entry {path:?} has a malformed content hash {hash:?}");
        }
        Ok(())
    }
}

/// Refuses a path relative to a volume root that could point outside it: empty, absolute, or
/// with empty, `.` or `..` parts.
pub fn check_rel_path(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('\0') {
        bail!("bad entry path {path:?}");
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("entry path {path:?} must be relative, with no empty, '.' or '..' parts");
    }
    Ok(())
}

/// A file name's extension as the catalog stores it: lowercase, without the dot.
pub fn ext_of(name: &str) -> Option<String> {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
}

/// Whether `value` is `prefix` followed by lowercase hex of one of the allowed lengths.
fn is_tagged_hex(value: &str, prefix: &str, lengths: &[usize]) -> bool {
    value.strip_prefix(prefix).is_some_and(|hex| {
        lengths.contains(&hex.len())
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
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
        //
        // A fingerprint or hash the entry brings (a pull) is stored. Without one (a scan), the
        // stored values stay while size and mtime are unchanged, so unchanged files are never
        // read again, and are cleared when either changed.
        let upsert = conn.prepare(
            "INSERT INTO entries
               (volume_id, path, name, ext, kind, size, mtime, seen_gen, fingerprint, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (volume_id, path) DO UPDATE SET
               ext = excluded.ext, kind = excluded.kind, size = excluded.size,
               mtime = excluded.mtime, seen_gen = excluded.seen_gen,
               fingerprint = CASE
                 WHEN excluded.fingerprint IS NOT NULL THEN excluded.fingerprint
                 WHEN entries.size = excluded.size AND entries.mtime = excluded.mtime
                   THEN entries.fingerprint
               END,
               content_hash = CASE
                 WHEN excluded.content_hash IS NOT NULL THEN excluded.content_hash
                 WHEN entries.size = excluded.size AND entries.mtime = excluded.mtime
                   THEN entries.content_hash
               END",
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
            self.generation,
            e.fingerprint,
            e.content_hash
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

/// Whether `path` is `dir` or inside it (both relative, `/`-separated). The empty `dir` is
/// the volume root, which holds everything.
fn is_within(path: &str, dir: &str) -> bool {
    dir.is_empty()
        || path
            .strip_prefix(dir)
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
    let mut stmt = conn.prepare(
        "SELECT path, name, ext, kind, size, mtime, fingerprint, content_hash
         FROM entries WHERE volume_id = ?1",
    )?;
    let mut rows = stmt.query([volume_id])?;
    while let Some(r) = rows.next()? {
        f(Entry {
            path: r.get(0)?,
            name: r.get(1)?,
            ext: r.get(2)?,
            kind: r.get(3)?,
            size: r.get(4)?,
            mtime: r.get(5)?,
            fingerprint: r.get(6)?,
            content_hash: r.get(7)?,
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

/// A full content hash read from a file outside a scan (by `plan apply`), with the size and
/// mtime the file had when it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Hashed {
    pub volume_id: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub hash: String,
}

/// Stores full hashes on the entries they were read from, only where the entry still has the
/// same size and mtime (a file that changed since is left alone, as `gfiles hash` does).
/// Returns how many were stored.
pub fn record_hashes(conn: &Connection, hashes: &[Hashed]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut stored = 0;
    {
        let mut update = tx.prepare(
            "UPDATE entries SET content_hash = ?5
             WHERE volume_id = ?1 AND path = ?2 AND size = ?3 AND mtime = ?4",
        )?;
        for h in hashes {
            stored += update.execute(params![h.volume_id, h.path, h.size, h.mtime, h.hash])?;
        }
    }
    tx.commit()?;
    Ok(stored)
}

/// A file a plan run moved on this machine, from one path to another (the same path for a
/// file undo restored from the trash), with the size and mtime it kept. `fingerprint` and
/// `hash` are what the run already knew: what the catalog held before the file moved
/// (`known_content`), or a full hash the run read itself (checking a copy, or the trash
/// journal's on undo).
#[derive(Debug, Clone, PartialEq)]
pub struct Carried {
    pub from_volume_id: String,
    pub from_path: String,
    pub to_volume_id: String,
    pub to_path: String,
    pub size: i64,
    pub mtime: i64,
    pub fingerprint: Option<String>,
    pub hash: Option<String>,
}

const KNOWN_CONTENT: &str = "SELECT fingerprint, content_hash FROM entries
     WHERE volume_id = ?1 AND path = ?2 AND size = ?3 AND mtime = ?4
     UNION ALL
     SELECT fingerprint, content_hash FROM carried
     WHERE volume_id = ?1 AND path = ?2 AND size = ?3 AND mtime = ?4
     LIMIT 1";

/// The fingerprint and full hash the catalog has for the file at a path while its size and
/// mtime match: its entry's, else what a move carried there and no scan has taken yet.
pub fn known_content(
    conn: &Connection,
    volume_id: &str,
    path: &str,
    size: i64,
    mtime: i64,
) -> Result<(Option<String>, Option<String>)> {
    Ok(conn
        .prepare_cached(KNOWN_CONTENT)?
        .query_row(params![volume_id, path, size, mtime], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?
        .unwrap_or_default())
}

/// Records what each moved file carries to its new path: what the run already knew, else the
/// fingerprint and full hash its old entry has (or a record carried there by an earlier move,
/// when no scan came between), guarded by size and mtime. The next scan
/// of the destination volume gives them to the new entry (`take_carried`); an entry already
/// there with the same size and mtime gets them now. Returns how many were recorded.
pub fn record_carried(conn: &Connection, moves: &[Carried]) -> Result<usize> {
    // Immediate: a deferred transaction that reads first can't wait out a scan's commit.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let mut recorded = 0;
    {
        let mut drop = tx.prepare("DELETE FROM carried WHERE volume_id = ?1 AND path = ?2")?;
        let mut put = tx.prepare(
            "INSERT OR REPLACE INTO carried (volume_id, path, size, mtime, fingerprint, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut fill = tx.prepare(
            "UPDATE entries SET fingerprint = coalesce(fingerprint, ?5),
                                content_hash = coalesce(content_hash, ?6)
             WHERE volume_id = ?1 AND path = ?2 AND size = ?3 AND mtime = ?4",
        )?;
        for m in moves {
            let (fingerprint, hash) =
                known_content(&tx, &m.from_volume_id, &m.from_path, m.size, m.mtime)?;
            let fingerprint = m.fingerprint.clone().or(fingerprint);
            let hash = m.hash.clone().or(hash);
            // The file left its old path; for a restored file, from and to are the same, so
            // this goes before the new record.
            drop.execute(params![m.from_volume_id, m.from_path])?;
            if fingerprint.is_none() && hash.is_none() {
                continue;
            }
            let to = params![
                m.to_volume_id,
                m.to_path,
                m.size,
                m.mtime,
                fingerprint,
                hash
            ];
            put.execute(to)?;
            fill.execute(to)?;
            recorded += 1;
        }
    }
    tx.commit()?;
    Ok(recorded)
}

/// Gives a volume's carried records to the entries a scan just wrote at their paths (where
/// size and mtime match, filling only what an entry lacks), then forgets the volume's
/// records, except those under `keep` (folders the scan couldn't read). Returns how many
/// entries got values.
pub fn take_carried(conn: &Connection, volume_id: &str, keep: &[String]) -> Result<usize> {
    // Almost every scan has none: skip the join over the whole volume.
    let any: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM carried WHERE volume_id = ?1)",
        [volume_id],
        |r| r.get(0),
    )?;
    if !any {
        return Ok(0);
    }
    let given = conn.execute(
        "UPDATE entries SET
           fingerprint = coalesce(entries.fingerprint, c.fingerprint),
           content_hash = coalesce(entries.content_hash, c.content_hash)
         FROM carried c
         WHERE entries.volume_id = ?1 AND c.volume_id = ?1 AND c.path = entries.path
           AND c.size = entries.size AND c.mtime = entries.mtime",
        [volume_id],
    )?;
    if keep.is_empty() {
        conn.execute("DELETE FROM carried WHERE volume_id = ?1", [volume_id])?;
    } else {
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM carried WHERE volume_id = ?1")?
            .query_map([volume_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut delete = conn.prepare("DELETE FROM carried WHERE volume_id = ?1 AND path = ?2")?;
        for path in paths {
            if !keep.iter().any(|k| is_within(&path, k)) {
                delete.execute(params![volume_id, path])?;
            }
        }
    }
    Ok(given)
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
            fingerprint: None,
            content_hash: None,
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
        // A pulled extension must be the name's own, lowercased.
        let mut e = entry("a", "a");
        e.ext = Some("zzzz".into());
        assert!(e.validate().is_err());
        let mut e = entry("Photo.JPG", "Photo.JPG");
        e.ext = Some("jpg".into());
        assert!(e.validate().is_ok());
    }

    const FP: &str = "p1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const HASH: &str = "b3:ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    fn vol() -> Volume {
        Volume {
            id: "v".into(),
            name: "v".into(),
            machine: "m".into(),
            root: "/v".into(),
            last_scan_at: None,
        }
    }

    /// Writes `entries` as one scan of the test volume.
    fn write(conn: &Connection, entries: &[Entry]) {
        let mut w = VolumeWriter::begin(conn, &vol()).unwrap();
        for e in entries {
            w.put(e).unwrap();
        }
        w.finish(0, &[]).unwrap();
    }

    fn stored(conn: &Connection, path: &str) -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT fingerprint, content_hash FROM entries WHERE path = ?1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn an_unchanged_file_keeps_its_fingerprint() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut e = entry("a.txt", "a.txt");
        e.size = 10;
        e.mtime = 100;
        e.fingerprint = Some(FP.into());
        e.content_hash = Some(HASH.into());
        write(&conn, &[e.clone()]);

        // A scan sends no fingerprint; size and mtime match, so the stored ones stay.
        let mut rescanned = e.clone();
        rescanned.fingerprint = None;
        rescanned.content_hash = None;
        write(&conn, &[rescanned.clone()]);
        assert_eq!(stored(&conn, "a.txt"), (Some(FP.into()), Some(HASH.into())));

        // A changed mtime means the content may have changed: both are cleared.
        rescanned.mtime = 101;
        write(&conn, &[rescanned.clone()]);
        assert_eq!(stored(&conn, "a.txt"), (None, None));

        // An entry that brings its own fingerprint (a pull) stores it.
        let mut pulled = rescanned;
        pulled.fingerprint = Some(FP.into());
        write(&conn, &[pulled]);
        assert_eq!(stored(&conn, "a.txt"), (Some(FP.into()), None));
    }

    const HASH2: &str = "b3:0000000000000000000000000000000000000000000000000000000000000002";

    fn file(path: &str, mtime: i64, fp: Option<&str>, hash: Option<&str>) -> Entry {
        Entry {
            size: 10,
            mtime,
            fingerprint: fp.map(Into::into),
            content_hash: hash.map(Into::into),
            ..entry(path, path.rsplit('/').next().unwrap())
        }
    }

    fn moved(from: &str, to: &str, mtime: i64, hash: Option<&str>) -> Carried {
        Carried {
            from_volume_id: "v".into(),
            from_path: from.into(),
            to_volume_id: "v".into(),
            to_path: to.into(),
            size: 10,
            mtime,
            fingerprint: None,
            hash: hash.map(Into::into),
        }
    }

    fn carried(conn: &Connection) -> Vec<(String, Option<String>, Option<String>)> {
        conn.prepare("SELECT path, fingerprint, content_hash FROM carried ORDER BY path")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_move_carries_what_the_catalog_knew() {
        let conn = crate::db::open_in_memory().unwrap();
        write(
            &conn,
            &[
                file("a/x.png", 100, Some(FP), Some(HASH)),
                file("a/y.png", 100, Some(FP), None),
                file("a/z.png", 100, None, None),
            ],
        );
        let n = record_carried(
            &conn,
            &[
                moved("a/x.png", "b/x.png", 100, None),
                // The move read a different hash itself: it wins.
                moved("a/y.png", "b/y.png", 100, Some(HASH2)),
                // Changed since the catalog saw it: nothing to carry.
                moved("a/x.png", "b/old.png", 101, None),
                // Nothing known at all.
                moved("a/z.png", "b/z.png", 100, None),
            ],
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            carried(&conn),
            vec![
                ("b/x.png".into(), Some(FP.into()), Some(HASH.into())),
                ("b/y.png".into(), Some(FP.into()), Some(HASH2.into())),
            ]
        );
    }

    #[test]
    fn a_move_keeps_what_was_known_before_a_scan_forgot_its_old_entry() {
        // A scan during the run saw the file gone from its old path and dropped that entry.
        let conn = crate::db::open_in_memory().unwrap();
        write(&conn, &[]);
        let mut m = moved("a.png", "b.png", 100, None);
        m.fingerprint = Some(FP.into());
        record_carried(&conn, &[m]).unwrap();
        assert_eq!(
            carried(&conn),
            vec![("b.png".into(), Some(FP.into()), None)]
        );
    }

    #[test]
    fn a_scan_that_read_nothing_keeps_everything() {
        // A walk error with no path: every entry and record stays, as if under `keep`.
        let conn = crate::db::open_in_memory().unwrap();
        write(&conn, &[file("a.png", 100, Some(FP), Some(HASH))]);
        record_carried(&conn, &[moved("a.png", "b.png", 100, None)]).unwrap();
        let finished = VolumeWriter::begin(&conn, &vol())
            .unwrap()
            .finish(0, &[String::new()])
            .unwrap();
        assert_eq!((finished.removed, finished.kept), (0, 1));
        assert_eq!(take_carried(&conn, "v", &[String::new()]).unwrap(), 0);
        assert_eq!(stored(&conn, "a.png"), (Some(FP.into()), Some(HASH.into())));
        assert_eq!(carried(&conn).len(), 1);
    }

    #[test]
    fn a_file_moved_twice_before_a_scan_carries_through() {
        let conn = crate::db::open_in_memory().unwrap();
        write(&conn, &[file("a.png", 100, Some(FP), Some(HASH))]);
        record_carried(&conn, &[moved("a.png", "b.png", 100, None)]).unwrap();
        record_carried(&conn, &[moved("b.png", "c.png", 100, None)]).unwrap();
        assert_eq!(
            carried(&conn),
            vec![("c.png".into(), Some(FP.into()), Some(HASH.into()))]
        );
    }

    #[test]
    fn a_restored_file_keeps_its_journal_hash() {
        // Undo restores a removed file to its own path, whose entry a scan already forgot.
        let conn = crate::db::open_in_memory().unwrap();
        write(&conn, &[]);
        record_carried(&conn, &[moved("a.png", "a.png", 100, Some(HASH))]).unwrap();
        assert_eq!(
            carried(&conn),
            vec![("a.png".into(), None, Some(HASH.into()))]
        );
    }

    #[test]
    fn recording_fills_an_entry_a_scan_already_wrote() {
        let conn = crate::db::open_in_memory().unwrap();
        write(
            &conn,
            &[
                file("a.png", 100, Some(FP), Some(HASH)),
                file("b.png", 100, None, None),
                file("c.png", 100, Some(FP), Some(HASH)),
                file("d.png", 200, None, None),
            ],
        );
        record_carried(
            &conn,
            &[
                moved("a.png", "b.png", 100, None),
                // Different mtime at the destination: left alone.
                moved("c.png", "d.png", 100, None),
            ],
        )
        .unwrap();
        assert_eq!(stored(&conn, "b.png"), (Some(FP.into()), Some(HASH.into())));
        assert_eq!(stored(&conn, "d.png"), (None, None));
    }

    #[test]
    fn a_scan_takes_matching_records_and_forgets_the_rest() {
        let conn = crate::db::open_in_memory().unwrap();
        write(
            &conn,
            &[
                file("a.png", 100, Some(FP), Some(HASH)),
                file("c.png", 100, Some(FP), Some(HASH)),
                file("e.png", 100, Some(FP), Some(HASH)),
            ],
        );
        record_carried(
            &conn,
            &[
                moved("a.png", "new/b.png", 100, None),
                moved("c.png", "new/d.png", 100, None),
                moved("e.png", "locked/f.png", 100, None),
            ],
        )
        .unwrap();
        // The scan finds b unchanged, d edited since the move, and can't read `locked`.
        write(
            &conn,
            &[
                file("new/b.png", 100, None, None),
                file("new/d.png", 150, None, None),
            ],
        );
        let given = take_carried(&conn, "v", &["locked".into()]).unwrap();
        assert_eq!(given, 1);
        assert_eq!(
            stored(&conn, "new/b.png"),
            (Some(FP.into()), Some(HASH.into()))
        );
        assert_eq!(stored(&conn, "new/d.png"), (None, None));
        assert_eq!(
            carried(&conn),
            vec![("locked/f.png".into(), Some(FP.into()), Some(HASH.into()))]
        );
        forget_volume(&conn, "v").unwrap();
        assert!(carried(&conn).is_empty());
    }

    #[test]
    fn validate_checks_fingerprints() {
        let ok = |fp: &str| {
            let mut e = entry("a", "a");
            e.fingerprint = Some(fp.into());
            e.validate().is_ok()
        };
        assert!(ok(FP));
        assert!(ok("git:0123456789abcdef0123456789abcdef01234567"));
        for bad in [
            "p1:xyz",
            "p1:00112233",
            "md5:00112233445566778899aabbccddeeff",
            "p1:00112233445566778899AABBCCDDEEFF00112233445566778899aabbccddeeff",
            HASH, // a full hash isn't a fingerprint
            "",
        ] {
            assert!(!ok(bad), "{bad:?} accepted");
        }
        let mut e = entry("a", "a");
        e.content_hash = Some(FP.into());
        assert!(e.validate().is_err(), "a fingerprint isn't a full hash");
        e.content_hash = Some(HASH.into());
        assert!(e.validate().is_ok());
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
        assert!(is_within("a/b", ""), "the root holds everything");
    }

    #[test]
    fn hashes_are_stored_only_on_unchanged_entries() {
        let conn = crate::db::open_in_memory().unwrap();
        let vol = Volume {
            id: "v1".into(),
            name: "darren".into(),
            machine: "elrond".into(),
            root: "/elrond/darren".into(),
            last_scan_at: None,
        };
        let mut w = VolumeWriter::begin(&conn, &vol).unwrap();
        for path in ["same.bmp", "changed.bmp"] {
            w.put(&Entry {
                path: path.into(),
                name: path.into(),
                ext: Some("bmp".into()),
                kind: "photo".into(),
                size: 10,
                mtime: 7,
                fingerprint: None,
                content_hash: None,
            })
            .unwrap();
        }
        w.finish(0, &[]).unwrap();
        let hashed = |path: &str, mtime: i64| Hashed {
            volume_id: "v1".into(),
            path: path.into(),
            size: 10,
            mtime,
            hash: HASH.into(),
        };
        let stored = record_hashes(
            &conn,
            &[
                hashed("same.bmp", 7),
                hashed("changed.bmp", 8),
                hashed("gone.bmp", 7),
            ],
        )
        .unwrap();
        assert_eq!(stored, 1);
        let hashes: Vec<(String, Option<String>)> = conn
            .prepare("SELECT path, content_hash FROM entries ORDER BY path")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            hashes,
            vec![
                ("changed.bmp".to_string(), None),
                ("same.bmp".to_string(), Some(HASH.to_string()))
            ]
        );
    }
}
