//! Plans from a supplied list. Another tool (a script, or an agent) works out what should
//! change, for reasons gfiles doesn't know, and hands gfiles a list; gfiles checks each entry
//! against the catalog and saves the valid ones as a draft plan, which Darren applies like any
//! other (see `plan` and `apply`).
//!
//! A list is JSON lines, one entry per line, naming files as `machine:volume/path`:
//!
//! ```text
//! {"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF", "vouched": true, "reason": "same shot"}
//! {"move": "elrond:darren/todo/b.JPG", "to": "elrond:photo/2013/08/13/b.JPG", "reason": "new"}
//! ```
//!
//! A removal names the copy that stays. Unless it's `vouched`, apply compares their full
//! content and skips it if they differ; a vouched removal is the producer's word that the two
//! hold the same thing although their bytes may differ, and needs a `reason`. Entries are
//! untrusted: each is checked here, and again on the machine when the plan is applied. A bad
//! entry is rejected with its reason and the rest of the list still makes the plan.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::mirror::{FolderRef, JOIN};
use crate::plan::{Built, Item, Move, Rejected, Spec};

/// The longest producer reason kept, in characters.
const MAX_REASON: usize = 500;
/// The longest list line kept with a rejected entry, in characters.
const MAX_SHOWN: usize = 1000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    remove: Option<String>,
    keep: Option<String>,
    #[serde(rename = "move")]
    move_: Option<String>,
    to: Option<String>,
    #[serde(default)]
    vouched: bool,
    reason: Option<String>,
}

/// A cataloged file a list names.
struct Found {
    volume_id: String,
    volume: String,
    path: String,
    size: i64,
    mtime: i64,
    kind: String,
    fingerprint: Option<String>,
    content: Option<String>,
    mirror: Option<String>,
    moved_by: Option<String>,
}

/// An entry that passed the checks on its own.
enum Valid {
    Remove(Item),
    Move(Move),
}

impl Valid {
    fn source(&self) -> (&str, &str) {
        match self {
            Valid::Remove(i) => (&i.volume_id, &i.path),
            Valid::Move(m) => (&m.volume_id, &m.path),
        }
    }
}

/// Checks a list (JSON lines) against the catalog (with mirrors loaded) and builds the plan
/// from its valid entries. Fails when no entry is valid.
pub fn build(conn: &Connection, list: &str) -> Result<Built> {
    let mut checker = Checker::new(conn)?;
    let mut rejected = Vec::new();
    let mut valid: Vec<(i64, String, Valid)> = Vec::new();
    for (n, text) in list.lines().enumerate() {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let line = n as i64 + 1;
        match checker.check(text) {
            Ok(v) => valid.push((line, text.to_string(), v)),
            Err(reason) => rejected.push(reject(line, text, reason)),
        }
    }

    // Across entries: no file is removed or moved while another entry keeps it, a file is
    // listed once, and a destination is used once. (A destination can't be another entry's
    // file: that file is cataloged, which already rules the destination out.)
    let kept: HashMap<(String, String), i64> = valid
        .iter()
        .filter_map(|(line, _, v)| match v {
            Valid::Remove(i) => Some(((i.keep_volume_id.clone(), i.keep_path.clone()), *line)),
            Valid::Move(_) => None,
        })
        .collect();
    let sources: HashMap<(String, String), i64> = valid
        .iter()
        .rev()
        .map(|(line, _, v)| {
            let (vol, path) = v.source();
            ((vol.to_string(), path.to_string()), *line)
        })
        .collect();
    let mut listed: HashSet<(String, String)> = HashSet::new();
    let mut dests: HashMap<(String, String), i64> = HashMap::new();
    let mut items = Vec::new();
    let mut moves = Vec::new();
    for (line, text, v) in valid {
        let (vol, path) = v.source();
        let key = (vol.to_string(), path.to_string());
        let why = if let Some(k) = kept.get(&key) {
            Some(format!(
                "line {k} keeps this file as the copy that stays, so it can't be removed or moved \
                 in the same plan (to rename it, import the move as a second plan once this one \
                 is applied and emptied: emptying keeps a file whose kept copy has moved)"
            ))
        } else if !listed.insert(key.clone()) {
            Some(format!("line {} already lists this file", sources[&key]))
        } else if let Valid::Move(m) = &v {
            let to = (m.dest_volume_id.clone(), m.dest_path.clone());
            if let Some(d) = dests.get(&to) {
                Some(format!("line {d} moves a file to the same destination"))
            } else {
                dests.insert(to, line);
                None
            }
        } else {
            None
        };
        match (why, v) {
            (Some(reason), _) => rejected.push(reject(line, &text, reason)),
            (None, Valid::Remove(i)) => items.push(i),
            (None, Valid::Move(m)) => moves.push(m),
        }
    }
    rejected.sort_by_key(|r| r.line);

    let Some(machine) = checker.machine.clone() else {
        bail!("{}", no_plan(&rejected, "the list has no entries"));
    };
    if items.is_empty() && moves.is_empty() {
        bail!("{}", no_plan(&rejected, "no entry of the list is valid"));
    }
    let folders = touched_folders(&machine, &items, &moves);
    Ok(Built {
        machine,
        spec: Spec::List(folders),
        except: Vec::new(),
        items,
        moves,
        left: Vec::new(),
        rejected,
    })
}

fn reject(line: i64, text: &str, reason: String) -> Rejected {
    Rejected {
        line,
        entry: text.chars().take(MAX_SHOWN).collect(),
        reason,
    }
}

fn no_plan(rejected: &[Rejected], why: &str) -> String {
    let mut msg = format!("{why}; no plan was saved");
    for r in rejected.iter().take(20) {
        msg.push_str(&format!("\n  line {}: {}", r.line, r.reason));
    }
    if rejected.len() > 20 {
        msg.push_str(&format!("\n  ... and {} more", rejected.len() - 20));
    }
    msg
}

/// The deepest folder, per volume, holding every file the plan removes, keeps or moves to.
fn touched_folders(machine: &str, items: &[Item], moves: &[Move]) -> Vec<FolderRef> {
    let parent = |p: &str| p.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    let mut paths: Vec<(&str, String)> = Vec::new();
    for i in items {
        paths.push((&i.volume, parent(&i.path)));
        paths.push((&i.keep_volume, parent(&i.keep_path)));
    }
    for m in moves {
        paths.push((&m.volume, parent(&m.path)));
        paths.push((&m.dest_volume, parent(&m.dest_path)));
    }
    let mut by_volume: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (volume, dir) in &paths {
        by_volume.entry(volume).or_default().push(dir);
    }
    by_volume
        .into_iter()
        .map(|(volume, dirs)| {
            let mut common: Vec<&str> = dirs[0].split('/').filter(|p| !p.is_empty()).collect();
            for d in &dirs[1..] {
                let parts: Vec<&str> = d.split('/').filter(|p| !p.is_empty()).collect();
                let same = common
                    .iter()
                    .zip(&parts)
                    .take_while(|(a, b)| a == b)
                    .count();
                common.truncate(same);
            }
            FolderRef {
                machine: machine.to_string(),
                volume: volume.to_string(),
                path: common.join("/"),
            }
        })
        .collect()
}

/// Checks entries one at a time; the first valid file reference decides the list's machine.
struct Checker<'c> {
    conn: &'c Connection,
    machine: Option<String>,
    find: rusqlite::Statement<'c>,
}

impl<'c> Checker<'c> {
    fn new(conn: &'c Connection) -> Result<Checker<'c>> {
        let find = conn.prepare(&format!(
            "SELECT e.volume_id, v.name, e.path, e.size, e.mtime, e.kind, e.fingerprint,
                    {content}, mf.mirror, (SELECT p.id {moved_by} LIMIT 1)
             FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
             WHERE v.machine = ?1 AND v.name = ?2 AND e.path = ?3",
            content = crate::dupes::content("e"),
            moved_by = crate::plan::moved_by_sql(),
        ))?;
        Ok(Checker {
            conn,
            machine: None,
            find,
        })
    }

    fn check(&mut self, text: &str) -> std::result::Result<Valid, String> {
        let e: Entry =
            serde_json::from_str(text).map_err(|err| format!("not a valid entry: {err}"))?;
        let reason = match e.reason.as_deref().map(str::trim) {
            Some(r) if r.chars().count() > MAX_REASON => {
                return Err(format!("its reason is longer than {MAX_REASON} characters"));
            }
            Some("") | None => None,
            Some(r) => Some(r.to_string()),
        };
        match (&e.remove, &e.keep, &e.move_, &e.to) {
            (Some(file), Some(keep), None, None) => {
                if e.vouched && reason.is_none() {
                    return Err("a vouched removal needs a reason".into());
                }
                self.removal(file, keep, e.vouched, reason)
            }
            (None, None, Some(file), Some(to)) => {
                if e.vouched {
                    return Err("only a removal can be vouched for".into());
                }
                self.relocation(file, to, reason)
            }
            _ => Err(
                "an entry is either {\"remove\": file, \"keep\": file} or {\"move\": file, \
                 \"to\": path}"
                    .into(),
            ),
        }
    }

    fn removal(
        &mut self,
        file: &str,
        keep: &str,
        vouched: bool,
        note: Option<String>,
    ) -> std::result::Result<Valid, String> {
        let f = self.source(file)?;
        let k = self
            .cataloged(keep)
            .map_err(|why| format!("the kept copy {keep}: {why}"))?;
        if k.kind == crate::classify::REPO {
            return Err(format!("the kept copy {keep} is a repository"));
        }
        if let Some(m) = &k.mirror {
            return Err(format!(
                "the kept copy {keep} is in mirror {m}, whose sync tool may delete it"
            ));
        }
        if let Some(p) = &k.moved_by {
            return Err(format!("the kept copy {keep} was moved by plan {p}"));
        }
        if (&k.volume_id, &k.path) == (&f.volume_id, &f.path) {
            return Err("the kept copy is the file itself".into());
        }
        if !vouched {
            let differ = "the catalog shows it and the kept copy differ; mark it vouched if \
                          they hold the same thing";
            if f.size != k.size {
                return Err(format!("{differ} (their sizes differ)"));
            }
            if f.fingerprint.is_none() || k.fingerprint.is_none() {
                return Err("not fingerprinted yet (scan its volume, or vouch for it)".into());
            }
            if f.fingerprint != k.fingerprint || f.content != k.content {
                return Err(differ.into());
            }
        }
        Ok(Valid::Remove(Item {
            volume_id: f.volume_id,
            volume: f.volume,
            path: f.path,
            size: f.size,
            mtime: f.mtime,
            fingerprint: f.fingerprint,
            keep_volume_id: k.volume_id,
            keep_volume: k.volume,
            keep_path: k.path,
            keep_size: k.size,
            keep_mtime: k.mtime,
            vouched,
            note,
        }))
    }

    fn relocation(
        &mut self,
        file: &str,
        to: &str,
        note: Option<String>,
    ) -> std::result::Result<Valid, String> {
        let f = self.source(file)?;
        let d = self
            .reference(to)
            .map_err(|why| format!("its destination: {why}"))?;
        let bad = |why: &str| Err(format!("its destination {to} {why}"));
        if d.path.split('/').next() == Some(crate::apply::TRASH) {
            return bad("is in the trash");
        }
        if crate::scan::ignored_by_default(&d.path) {
            return bad(
                "is somewhere scans skip (a hidden folder, #recycle, @eaDir, ...): the file \
                 would drop out of the catalog",
            );
        }
        let dest_volume_id: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM volumes WHERE machine = ?1 AND name = ?2",
                params![d.machine, d.volume],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(dest_volume_id) = dest_volume_id else {
            return bad(&format!("isn't in a volume of {}", d.machine));
        };
        if (&dest_volume_id, &d.path) == (&f.volume_id, &f.path) {
            return bad("is where it already is");
        }
        let q = |sql: &str| -> std::result::Result<Option<String>, String> {
            self.conn
                .query_row(sql, params![dest_volume_id, d.path], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())
        };
        if q("SELECT path FROM entries WHERE volume_id = ?1 AND path = ?2")?.is_some() {
            return bad("is already a cataloged file");
        }
        if q("SELECT path FROM entries WHERE volume_id = ?1
              AND substr(path, 1, length(?2) + 1) = ?2 || '/' LIMIT 1")?
        .is_some()
        {
            return bad("is a cataloged folder");
        }
        // A cataloged file (or repository) at one of the folders above it.
        if let Some(p) = q("SELECT path FROM entries WHERE volume_id = ?1
             AND substr(?2, 1, length(path) + 1) = path || '/' LIMIT 1")?
        {
            return bad(&format!("is inside {p}, which isn't a plain folder"));
        }
        if let Some(m) = q("SELECT mirror FROM temp.mirror_folders WHERE volume_id = ?1
              AND (prefix = '' OR ?2 = prefix OR substr(?2, 1, length(prefix) + 1) = prefix || '/')")?
        {
            return bad(&format!("is in mirror {m}"));
        }
        Ok(Valid::Move(Move {
            volume_id: f.volume_id,
            volume: f.volume,
            path: f.path,
            size: f.size,
            mtime: f.mtime,
            fingerprint: f.fingerprint,
            dest_volume_id,
            dest_volume: d.volume,
            dest_path: d.path,
            note,
        }))
    }

    /// The file an entry removes or moves.
    fn source(&mut self, file: &str) -> std::result::Result<Found, String> {
        let f = self.cataloged(file)?;
        if f.kind == crate::classify::REPO {
            return Err("it's a repository, which plans never touch".into());
        }
        if let Some(m) = &f.mirror {
            return Err(format!(
                "it's in mirror {m}: removing or moving it there would change every replica"
            ));
        }
        if let Some(p) = &f.moved_by {
            return Err(format!("already moved by plan {p}"));
        }
        Ok(f)
    }

    fn cataloged(&mut self, file: &str) -> std::result::Result<Found, String> {
        let r = self.reference(file)?;
        self.find
            .query_row(params![r.machine, r.volume, r.path], |r| {
                Ok(Found {
                    volume_id: r.get(0)?,
                    volume: r.get(1)?,
                    path: r.get(2)?,
                    size: r.get(3)?,
                    mtime: r.get(4)?,
                    kind: r.get(5)?,
                    fingerprint: r.get(6)?,
                    content: r.get(7)?,
                    mirror: r.get(8)?,
                    moved_by: r.get(9)?,
                })
            })
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{file} is not in the catalog"))
    }

    /// Parses a `machine:volume/path` file reference on the list's machine.
    fn reference(&mut self, s: &str) -> std::result::Result<FolderRef, String> {
        let r = crate::mirror::parse_folder(s).map_err(|e| {
            e.to_string()
                .replace("folder", "file")
                .replace("machine:volume/path", "machine:volume/path/to/file")
        })?;
        if r.path.is_empty() {
            return Err(format!("{s} names a volume, not a file"));
        }
        crate::store::check_rel_path(&r.path).map_err(|e| e.to_string())?;
        match &self.machine {
            Some(m) if *m != r.machine => Err(format!(
                "{s} is on {}, but the list is for {m} (its first entry): a plan changes one \
                 machine",
                r.machine
            )),
            Some(_) => Ok(r),
            None => {
                self.machine = Some(r.machine.clone());
                Ok(r)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{A, B, put};

    fn catalog() -> Connection {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.NEF", 10, Some(A)),
                ("todo/b.NEF", 10, Some(B)),
                ("todo/c.JPG", 10, Some(A)),
                ("todo/new.JPG", 5, Some(B)),
                ("todo/raw.JPG", 5, None),
                ("Drive/d.NEF", 10, Some(A)),
                (
                    "code",
                    0,
                    Some("git:0123456789abcdef0123456789abcdef01234567"),
                ),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[
                ("2007/12/31/a.NEF", 10, Some(A)),
                ("2007/12/31/b.NEF", 11, Some(B)),
                ("2008/x.JPG", 5, None),
            ],
        );
        put(&conn, "gaffer", "darren", &[("todo/a.NEF", 10, Some(A))]);
        conn
    }

    fn reasons(b: &Built) -> Vec<(i64, String)> {
        b.rejected
            .iter()
            .map(|r| (r.line, r.reason.clone()))
            .collect()
    }

    fn rejected_with(b: &Built, line: i64, text: &str) {
        assert!(
            b.rejected
                .iter()
                .any(|r| r.line == line && r.reason.contains(text)),
            "line {line} not rejected with {text:?}: {:?}",
            reasons(b)
        );
    }

    #[test]
    fn valid_entries_make_the_plan_and_bad_ones_are_listed() {
        let conn = catalog();
        let list = r#"
{"remove": "elrond:darren/todo/b.NEF", "keep": "elrond:photo/2007/12/31/b.NEF", "vouched": true, "reason": "same shot; XMP"}
{"remove": "elrond:darren/todo/gone.NEF", "keep": "elrond:photo/2007/12/31/b.NEF", "vouched": true, "reason": "same shot"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/2013/08/13/new.JPG", "reason": "new"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
"#;
        let b = build(&conn, list).unwrap();
        assert_eq!(b.machine, "elrond");
        assert_eq!(
            b.items
                .iter()
                .map(|i| (i.path.as_str(), i.vouched, i.note.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("todo/b.NEF", true, Some("same shot; XMP")),
                ("todo/a.NEF", false, None)
            ]
        );
        assert_eq!(b.moves.len(), 1);
        assert_eq!(b.moves[0].dest_volume, "photo");
        assert_eq!(b.moves[0].dest_path, "2013/08/13/new.JPG");
        assert_eq!(
            reasons(&b),
            vec![(
                3,
                "elrond:darren/todo/gone.NEF is not in the catalog".to_string()
            )]
        );
        assert_eq!(
            b.spec,
            Spec::List(vec![
                crate::mirror::parse_folder("elrond:darren/todo").unwrap(),
                crate::mirror::parse_folder("elrond:photo").unwrap(),
            ])
        );
    }

    #[test]
    fn removals_not_vouched_for_must_match_by_the_catalog() {
        let conn = catalog();
        let list = r#"{"remove": "elrond:darren/todo/b.NEF", "keep": "elrond:photo/2007/12/31/b.NEF"}
{"remove": "elrond:darren/todo/c.JPG", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"remove": "elrond:darren/todo/raw.JPG", "keep": "elrond:photo/2008/x.JPG"}
{"remove": "elrond:darren/todo/b.NEF", "keep": "elrond:photo/2007/12/31/b.NEF", "vouched": true}
{"remove": "elrond:darren/todo/b.NEF", "keep": "elrond:darren/todo/b.NEF", "vouched": true, "reason": "x"}"#;
        let b = build(&conn, list).unwrap();
        assert_eq!(b.items.len(), 1);
        assert_eq!(b.items[0].path, "todo/c.JPG");
        rejected_with(&b, 1, "sizes differ");
        rejected_with(&b, 3, "not fingerprinted yet");
        rejected_with(&b, 4, "needs a reason");
        rejected_with(&b, 5, "the file itself");
    }

    #[test]
    fn full_hashes_that_differ_reject_a_removal() {
        let conn = catalog();
        put(
            &conn,
            "elrond",
            "photo",
            &[
                ("2007/12/31/a.NEF", 10, Some(A)),
                ("2009/a.NEF", 10, Some(A)),
            ],
        );
        let hashed = |path: &str, hash: &str| crate::store::Hashed {
            volume_id: "elrond-photo".into(),
            path: path.into(),
            size: 10,
            mtime: 7,
            hash: hash.into(),
        };
        crate::store::record_hashes(
            &conn,
            &[
                hashed("2007/12/31/a.NEF", "b3:1"),
                hashed("2009/a.NEF", "b3:2"),
            ],
        )
        .unwrap();
        let b = build(
            &conn,
            r#"{"remove": "elrond:photo/2009/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/new.JPG"}"#,
        )
        .unwrap();
        rejected_with(&b, 1, "differ");
    }

    #[test]
    fn entries_that_conflict_with_each_other_are_rejected() {
        let conn = catalog();
        let list = r#"{"remove": "elrond:darren/todo/c.JPG", "keep": "elrond:darren/todo/a.NEF"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"move": "elrond:darren/todo/a.NEF", "to": "elrond:photo/a.NEF"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/n/new.JPG"}
{"move": "elrond:darren/todo/b.NEF", "to": "elrond:photo/n/new.JPG"}
{"move": "elrond:darren/todo/b.NEF", "to": "elrond:darren/todo/c.JPG"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/other.JPG"}"#;
        let b = build(&conn, list).unwrap();
        // Removing or moving the file line 1 keeps.
        rejected_with(&b, 2, "line 1 keeps this file");
        rejected_with(&b, 3, "line 1 keeps this file");
        rejected_with(&b, 3, "once this one is applied and emptied");
        rejected_with(&b, 5, "line 4 moves a file to the same destination");
        // A destination that is another entry's file (and is cataloged anyway).
        rejected_with(&b, 6, "already a cataloged file");
        rejected_with(&b, 7, "line 4 already lists this file");
        assert_eq!(b.items.len(), 1);
        assert_eq!(b.moves.len(), 1);
    }

    #[test]
    fn a_destination_another_entry_moves_away_from_is_rejected() {
        let conn = catalog();
        // b.NEF's catalog path is free once moved, but its own move is listed later.
        let list = r#"{"move": "elrond:darren/todo/new.JPG", "to": "elrond:darren/todo/b2.NEF"}
{"move": "elrond:darren/todo/b.NEF", "to": "elrond:darren/todo/b2.NEF"}"#;
        let b = build(&conn, list).unwrap();
        rejected_with(&b, 2, "line 1 moves a file to the same destination");
        // Moving a file to where another entry moves one away from: that path is cataloged.
        let b = build(
            &conn,
            r#"{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/b.NEF"}
{"move": "elrond:photo/2008/x.JPG", "to": "elrond:darren/todo/new.JPG"}"#,
        )
        .unwrap();
        rejected_with(&b, 2, "already a cataloged file");
    }

    #[test]
    fn destinations_must_be_free_plain_places() {
        let conn = catalog();
        crate::plan::tests::with_drive(&conn);
        let entry = |to: &str| format!(r#"{{"move": "elrond:darren/todo/new.JPG", "to": "{to}"}}"#);
        let list = [
            entry("elrond:photo/2007/12/31/a.NEF"),
            entry("elrond:photo/2007"),
            entry("elrond:photo/2007/12/31/a.NEF/x.JPG"),
            entry("elrond:darren/code/x.JPG"),
            entry("elrond:darren/.gfiles-trash/x.JPG"),
            entry("elrond:darren/Drive/x.JPG"),
            entry("elrond:nope/x.JPG"),
            entry("elrond:photo/../x.JPG"),
            entry("elrond:darren/todo/new.JPG"),
            entry("elrond:photo/#recycle/new.JPG"),
            entry("elrond:darren/.ssh/new.JPG"),
            entry("elrond:photo/2013/@eaDir/new.JPG"),
            entry("elrond:photo/ok/new.JPG"),
        ]
        .join("\n");
        let b = build(&conn, &list).unwrap();
        rejected_with(&b, 1, "already a cataloged file");
        rejected_with(&b, 2, "cataloged folder");
        rejected_with(&b, 3, "inside 2007/12/31/a.NEF");
        rejected_with(&b, 4, "inside code");
        rejected_with(&b, 5, "in the trash");
        rejected_with(&b, 6, "in mirror drive");
        rejected_with(&b, 7, "isn't in a volume of elrond");
        rejected_with(&b, 8, "'..'");
        rejected_with(&b, 9, "where it already is");
        for line in 10..=12 {
            rejected_with(&b, line, "somewhere scans skip");
        }
        assert_eq!(b.moves.len(), 1);
    }

    #[test]
    fn sources_and_kept_copies_must_be_plain_files_on_one_machine() {
        let conn = catalog();
        crate::plan::tests::with_drive(&conn);
        let list = r#"{"remove": "elrond:darren/code", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"remove": "elrond:darren/Drive/d.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:darren/Drive/d.NEF"}
{"remove": "gaffer:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF", "vouch": true}
{"move": "elrond:darren/todo/new.JPG"}
not json
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/n.JPG", "vouched": true, "reason": "x"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}"#;
        let b = build(&conn, list).unwrap();
        rejected_with(&b, 1, "repository");
        rejected_with(&b, 2, "in mirror drive");
        rejected_with(
            &b,
            3,
            "the kept copy elrond:darren/Drive/d.NEF is in mirror drive",
        );
        rejected_with(&b, 4, "the list is for elrond");
        rejected_with(&b, 5, "unknown field `vouch`");
        rejected_with(&b, 6, "either");
        rejected_with(&b, 7, "not a valid entry");
        rejected_with(&b, 8, "only a removal can be vouched");
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn kept_copies_reasons_and_references_are_checked() {
        let conn = catalog();
        let long = "x".repeat(MAX_REASON + 1);
        let list = format!(
            r#"{{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:darren/code"}}
{{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF", "vouched": true, "reason": "{long}"}}
{{"move": "elrond:darren", "to": "elrond:photo/x.NEF"}}
{{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/n.JPG", "pad": "{pad}"}}
{{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}}"#,
            pad = "y".repeat(MAX_SHOWN)
        );
        let b = build(&conn, &list).unwrap();
        rejected_with(&b, 1, "the kept copy elrond:darren/code is a repository");
        rejected_with(&b, 2, "longer than 500 characters");
        rejected_with(&b, 3, "names a volume, not a file");
        rejected_with(&b, 4, "unknown field `pad`");
        let shown = &b.rejected.iter().find(|r| r.line == 4).unwrap().entry;
        assert_eq!(shown.chars().count(), MAX_SHOWN);
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn a_kept_copy_an_applied_plan_moved_is_rejected() {
        let conn = catalog();
        let b = build(
            &conn,
            r#"{"remove": "elrond:photo/2007/12/31/a.NEF", "keep": "elrond:darren/todo/a.NEF"}"#,
        )
        .unwrap();
        let id = crate::plan::save(&conn, &b, 0).unwrap();
        conn.execute(
            "UPDATE plan_items SET outcome = 'moved' WHERE plan_id = ?1",
            [&id],
        )
        .unwrap();
        crate::plan::set_state(&conn, &id, "applied", 1).unwrap();
        let b = build(
            &conn,
            r#"{"remove": "elrond:darren/todo/c.JPG", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/n.JPG"}"#,
        )
        .unwrap();
        rejected_with(&b, 1, "was moved by plan");
    }

    #[test]
    fn a_file_an_applied_plan_moved_is_rejected() {
        let conn = catalog();
        let b = build(
            &conn,
            r#"{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}"#,
        )
        .unwrap();
        let id = crate::plan::save(&conn, &b, 0).unwrap();
        conn.execute(
            "UPDATE plan_items SET outcome = 'moved' WHERE plan_id = ?1",
            [&id],
        )
        .unwrap();
        crate::plan::set_state(&conn, &id, "applied", 1).unwrap();
        let err = build(
            &conn,
            r#"{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already moved by plan"), "{err}");
    }

    #[test]
    fn a_list_with_nothing_valid_saves_no_plan() {
        let conn = catalog();
        let err = build(
            &conn,
            r#"{"remove": "elrond:darren/nope", "keep": "elrond:photo/2007/12/31/a.NEF"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no entry of the list is valid"), "{err}");
        assert!(
            err.contains("line 1: elrond:darren/nope is not in the catalog"),
            "{err}"
        );
        assert!(build(&conn, "\n\n").is_err());
    }

    #[test]
    fn a_saved_list_plan_shows_each_kind_and_its_rejects() {
        let conn = catalog();
        let b = build(
            &conn,
            r#"{"remove": "elrond:darren/todo/b.NEF", "keep": "elrond:photo/2007/12/31/b.NEF", "vouched": true, "reason": "same shot"}
{"remove": "elrond:darren/todo/a.NEF", "keep": "elrond:photo/2007/12/31/a.NEF"}
{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/2013/new.JPG", "reason": "new"}
{"remove": "elrond:darren/todo/gone", "keep": "elrond:photo/2007/12/31/a.NEF"}"#,
        )
        .unwrap();
        let id = crate::plan::save(&conn, &b, 0).unwrap();
        let v = crate::plan::show(&conn, &id, None, None).unwrap();
        let s = &v.summary;
        assert_eq!(s.kind, "list");
        assert_eq!((s.remove.files, s.remove.bytes), (2, 20));
        assert_eq!((s.vouched.files, s.vouched.bytes), (1, 10));
        assert_eq!((s.moves.files, s.moves.bytes), (1, 5));
        assert_eq!(s.confirmed.files, 0);
        assert_eq!(s.rejected, 1);
        assert_eq!(v.rejected[0].line, 4);
        let kinds: Vec<(&str, bool, Option<&str>, Option<&str>)> = v
            .items
            .iter()
            .map(|i| {
                (
                    i.action.as_str(),
                    i.vouched,
                    i.note.as_deref(),
                    i.to_path.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("remove", true, Some("same shot"), None),
                ("remove", false, None, None),
                ("move", false, Some("new"), Some("2013/new.JPG")),
            ]
        );
        assert_eq!(
            v.items[2].to_abs_path.as_deref(),
            Some("/elrond/photo/2013/new.JPG")
        );
        // The applying machine gets moves with their destination and no kept copy.
        let exec = crate::plan::exec_items(&conn, &id).unwrap();
        assert!(exec[0].vouched && exec[0].keep.is_some());
        assert_eq!(exec[2].action, crate::apply::Action::Move);
        assert!(exec[2].keep.is_none());
        assert_eq!(exec[2].dest.as_ref().unwrap().path, "2013/new.JPG");
        assert_eq!(
            crate::plan::trash_volumes(&conn, &id).unwrap(),
            vec!["elrond-darren".to_string(), "elrond-photo".to_string()]
        );
        // And the plan's spec reads back.
        assert!(matches!(
            crate::plan::spec(&conn, &id).unwrap(),
            Spec::List(_)
        ));
    }

    #[test]
    fn a_destination_in_a_synced_folder_refuses_the_plan() {
        let conn = catalog();
        let b = build(
            &conn,
            r#"{"move": "elrond:darren/todo/new.JPG", "to": "elrond:photo/Dropbox/new.JPG"}"#,
        )
        .unwrap();
        let err = crate::plan::check_not_synced(&conn, &b, &[]).unwrap_err();
        assert!(err.to_string().contains("Dropbox"), "{err}");
    }
}
