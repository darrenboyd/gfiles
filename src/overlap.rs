//! Duplicate space by folder, within one machine: which folder pairs share the most content,
//! and what draining one folder would free. The folder-level view that cleanup plans (see
//! `plan`) act on.
//!
//! Two copies of a file pair up at the first folder where their paths differ: copies at
//! `todo/2012/a.jpg` and `Photos/2012/a.jpg` make the pair (`todo`, `Photos`). Copies on
//! different volumes pair at the top folder of each. Copies in the same folder make that folder
//! a pair with itself. Each content counts once per pair, however many copies it has there.
//! Only the first 50 copies of a content (by volume and path) are paired, so a file copied
//! hundreds of times can't make tens of thousands of pairs; its other copies don't count
//! toward any pair.
//!
//! Only same-machine copies count, and only files a plan could remove: fingerprinted, not
//! empty, not a repository, not in a mirror, and not already moved by an applied plan. Copies
//! match as duplicates do (see `dupes::content`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::dupes::content;
use crate::mirror::{FolderRef, JOIN};
use crate::plan::{self, NOT_MOVED, Tally};

/// Copies of one content beyond this many are left out of pairing (see the module doc).
const MAX_COPIES: usize = 50;

/// Two folders on one machine that hold the same content.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Pair {
    pub machine: String,
    pub a: FolderRef,
    /// Equal to `a` when the copies are inside one folder.
    pub b: FolderRef,
    /// Distinct contents with a copy in both (bytes count each content once).
    pub shared: Tally,
    /// Everything cataloged under each folder.
    pub a_total: Tally,
    pub b_total: Tally,
    /// What's cataloged under each folder with no copy in the other one: what would be lost
    /// keeping only the other folder (includes files that aren't fingerprinted yet).
    pub a_only: Tally,
    pub b_only: Tally,
}

/// What a pair accumulates while grouping: shared contents, and the files under each side
/// whose content the other side holds.
#[derive(Default)]
struct Overlap {
    shared: Tally,
    a_covered: Tally,
    b_covered: Tally,
}

/// Whether the file at (volume, path) is under the folder `spot`.
fn under(spot: &Spot, volume: &str, path: &str) -> bool {
    spot.0 == volume
        && (spot.1.is_empty()
            || path
                .strip_prefix(spot.1.as_str())
                .is_some_and(|rest| rest.starts_with('/')))
}

/// A folder as (volume, path in it).
type Spot = (String, String);

/// The folder pair two copies of a file make (see the module doc).
fn pair_of(a: (&str, &str), b: (&str, &str)) -> (Spot, Spot) {
    let dirs = |p: &str| -> Vec<String> {
        let mut parts: Vec<String> = p.split('/').map(str::to_string).collect();
        parts.pop();
        parts
    };
    let (da, db) = (dirs(a.1), dirs(b.1));
    if a.0 != b.0 {
        let top = |d: &[String]| d.first().cloned().unwrap_or_default();
        return ((a.0.to_string(), top(&da)), (b.0.to_string(), top(&db)));
    }
    let common = da.iter().zip(&db).take_while(|(x, y)| x == y).count();
    let upto = |d: &[String]| d[..(common + 1).min(d.len())].join("/");
    ((a.0.to_string(), upto(&da)), (b.0.to_string(), upto(&db)))
}

/// Folder pairs, most shared bytes first, cut to the limit.
#[derive(Debug, Serialize)]
pub struct Pairs {
    /// How many pairs there are in all, before the limit.
    pub total: usize,
    pub pairs: Vec<Pair>,
}

/// Folder pairs on each machine (or one), most shared bytes first. Refuses a machine the
/// catalog doesn't know.
pub fn pairs(conn: &Connection, machine: Option<&str>, limit: Option<usize>) -> Result<Pairs> {
    crate::search::check_filters(
        conn,
        &crate::search::Query {
            machine: machine.map(str::to_string),
            ..Default::default()
        },
    )?;
    let (content, o_content) = (content("e"), content("o"));
    let sql = format!(
        "SELECT v.machine, v.name, e.path, e.size, {content} AS content
         FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         WHERE e.size > 0 AND e.fingerprint LIKE 'p1:%' AND e.kind != 'repo'
           AND mf.mirror IS NULL AND (?1 IS NULL OR v.machine = ?1) AND {NOT_MOVED}
           AND EXISTS (SELECT 1 FROM entries o JOIN volumes ov ON ov.id = o.volume_id
                       WHERE o.size = e.size AND o.fingerprint = e.fingerprint
                         AND {o_content} = {content}
                         AND ov.machine = v.machine AND o.id != e.id)
         ORDER BY v.machine, e.size, content"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![machine])?;
    // (machine, (volume, folder), (volume, folder)) → shared tally
    let mut found: HashMap<(String, Spot, Spot), Overlap> = HashMap::new();
    let mut group: Vec<(String, String)> = Vec::new();
    let mut key: Option<(String, i64, String)> = None;
    let mut flush = |key: &Option<(String, i64, String)>, group: &mut Vec<(String, String)>| {
        if let Some((machine, size, _)) = key
            && group.len() >= 2
        {
            group.truncate(MAX_COPIES);
            let mut seen = BTreeSet::new();
            for i in 0..group.len() {
                for j in i + 1..group.len() {
                    let (x, y) = pair_of((&group[i].0, &group[i].1), (&group[j].0, &group[j].1));
                    seen.insert(if x <= y { (x, y) } else { (y, x) });
                }
            }
            for (x, y) in seen {
                let count =
                    |spot: &Spot| group.iter().filter(|(v, p)| under(spot, v, p)).count() as i64;
                let (in_x, in_y) = (count(&x), count(&y));
                let o = found.entry((machine.clone(), x, y)).or_default();
                o.shared.files += 1;
                o.shared.bytes += size;
                o.a_covered.files += in_x;
                o.a_covered.bytes += in_x * size;
                o.b_covered.files += in_y;
                o.b_covered.bytes += in_y * size;
            }
        }
        group.clear();
    };
    while let Some(r) = rows.next()? {
        let k: (String, i64, String) = (r.get(0)?, r.get(3)?, r.get(4)?);
        if key.as_ref() != Some(&k) {
            flush(&key, &mut group);
            key = Some(k);
        }
        group.push((r.get(1)?, r.get(2)?));
    }
    flush(&key, &mut group);

    let mut list: Vec<_> = found.into_iter().collect();
    list.sort_by(|a, b| {
        b.1.shared
            .bytes
            .cmp(&a.1.shared.bytes)
            .then_with(|| a.0.cmp(&b.0))
    });
    let total = list.len();
    if let Some(l) = limit {
        list.truncate(l);
    }
    let mut totals = Totals::new(conn)?;
    let pairs = list
        .into_iter()
        .map(|((machine, a, b), o)| {
            let a = FolderRef {
                machine: machine.clone(),
                volume: a.0,
                path: a.1,
            };
            let b = FolderRef {
                machine: machine.clone(),
                volume: b.0,
                path: b.1,
            };
            let (a_total, b_total) = (totals.of(&a)?, totals.of(&b)?);
            let less = |t: Tally, c: Tally| Tally {
                files: (t.files - c.files).max(0),
                bytes: (t.bytes - c.bytes).max(0),
            };
            Ok(Pair {
                a_only: less(a_total, o.a_covered),
                b_only: less(b_total, o.b_covered),
                a_total,
                b_total,
                machine,
                a,
                b,
                shared: o.shared,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Pairs { total, pairs })
}

/// Files and bytes cataloged under folders, cached.
struct Totals<'c> {
    stmt: rusqlite::Statement<'c>,
    cache: HashMap<FolderRef, Tally>,
}

impl<'c> Totals<'c> {
    fn new(conn: &'c Connection) -> Result<Self> {
        Ok(Totals {
            stmt: conn.prepare(
                "SELECT count(*), coalesce(sum(e.size), 0)
                 FROM entries e JOIN volumes v ON v.id = e.volume_id
                 WHERE v.machine = ?1 AND v.name = ?2
                   AND (?3 = '' OR substr(e.path, 1, length(?3) + 1) = ?3 || '/')",
            )?,
            cache: HashMap::new(),
        })
    }

    fn of(&mut self, f: &FolderRef) -> Result<Tally> {
        if let Some(t) = self.cache.get(f) {
            return Ok(*t);
        }
        let t = self
            .stmt
            .query_row(params![f.machine, f.volume, f.path], |r| {
                Ok(Tally {
                    files: r.get(0)?,
                    bytes: r.get(1)?,
                })
            })?;
        self.cache.insert(f.clone(), t);
        Ok(t)
    }
}

/// Files a drain would remove whose kept copies are in one folder.
#[derive(Debug, Serialize)]
pub struct Elsewhere {
    pub folder: FolderRef,
    #[serde(flatten)]
    pub tally: Tally,
}

/// Files a drain would leave, for one reason.
#[derive(Debug, Serialize)]
pub struct Leftover {
    pub reason: String,
    #[serde(flatten)]
    pub tally: Tally,
}

/// What draining a folder would do, without saving a plan.
#[derive(Debug, Serialize)]
pub struct Preview {
    pub folder: FolderRef,
    /// Folders inside it the drain would leave out entirely.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<FolderRef>,
    /// Everything cataloged in the folder, less the folders left out.
    pub total: Tally,
    /// Files with a copy elsewhere on the machine: what a drain plan removes.
    pub removable: Tally,
    /// Where the copies that stay are, most bytes first, cut to the limit.
    pub copies_in: Vec<Elsewhere>,
    /// How many folders hold copies that stay, before the limit.
    pub copies_in_total: usize,
    /// Files the drain leaves, by reason, most bytes first.
    pub left: Vec<Leftover>,
}

pub fn drain_preview(
    conn: &Connection,
    folder: &FolderRef,
    except: &[FolderRef],
    limit: Option<usize>,
) -> Result<Preview> {
    let built = plan::build(conn, plan::Spec::Drain(vec![folder.clone()]), except)?;
    let mut copies: BTreeMap<FolderRef, Tally> = BTreeMap::new();
    let mut removable = Tally::default();
    for i in &built.items {
        let (_, (volume, path)) = pair_of((&i.volume, &i.path), (&i.keep_volume, &i.keep_path));
        let t = copies
            .entry(FolderRef {
                machine: built.machine.clone(),
                volume,
                path,
            })
            .or_default();
        t.files += 1;
        t.bytes += i.size;
        removable.files += 1;
        removable.bytes += i.size;
    }
    let mut left: BTreeMap<String, Tally> = BTreeMap::new();
    for l in &built.left {
        let t = left.entry(l.reason.clone()).or_default();
        t.files += 1;
        t.bytes += l.size;
    }
    let mut copies_in: Vec<Elsewhere> = copies
        .into_iter()
        .map(|(folder, tally)| Elsewhere { folder, tally })
        .collect();
    copies_in.sort_by_key(|c| std::cmp::Reverse(c.tally.bytes));
    let copies_in_total = copies_in.len();
    if let Some(l) = limit {
        copies_in.truncate(l);
    }
    let mut left: Vec<Leftover> = left
        .into_iter()
        .map(|(reason, tally)| Leftover { reason, tally })
        .collect();
    left.sort_by_key(|l| std::cmp::Reverse(l.tally.bytes));
    let mut totals = Totals::new(conn)?;
    let mut total = totals.of(folder)?;
    for e in &built.except {
        let t = totals.of(e)?;
        total.files -= t.files;
        total.bytes -= t.bytes;
    }
    Ok(Preview {
        total,
        folder: folder.clone(),
        except: built.except.clone(),
        removable,
        copies_in,
        copies_in_total,
        left,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{A, B, C, put};

    fn s(v: &str, p: &str) -> (String, String) {
        (v.to_string(), p.to_string())
    }

    #[test]
    fn copies_pair_where_their_paths_part() {
        assert_eq!(
            pair_of(("d", "todo/2012/a.jpg"), ("d", "Photos/2012/a.jpg")),
            (s("d", "todo"), s("d", "Photos"))
        );
        assert_eq!(
            pair_of(("d", "Photos/2012/a.jpg"), ("d", "Photos/2013/x/a.jpg")),
            (s("d", "Photos/2012"), s("d", "Photos/2013"))
        );
        // One copy in a parent of the other's folder.
        assert_eq!(
            pair_of(("d", "Photos/a.jpg"), ("d", "Photos/old/a.jpg")),
            (s("d", "Photos"), s("d", "Photos/old"))
        );
        assert_eq!(
            pair_of(("d", "Photos/a.jpg"), ("d", "Photos/a copy.jpg")),
            (s("d", "Photos"), s("d", "Photos"))
        );
        assert_eq!(
            pair_of(("d", "a.jpg"), ("d", "b.jpg")),
            (s("d", ""), s("d", ""))
        );
        assert_eq!(
            pair_of(("darren", "todo/x/a.jpg"), ("photo", "2012/a.jpg")),
            (s("darren", "todo"), s("photo", "2012"))
        );
    }

    #[test]
    fn pairs_count_each_content_once_and_stay_on_one_machine() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.jpg", 10, Some(A)),
                ("todo/sub/a.jpg", 10, Some(A)),
                ("Photos/a.jpg", 10, Some(A)),
                ("todo/b.jpg", 20, Some(B)),
                ("Photos/2012/b.jpg", 20, Some(B)),
                ("todo/c.jpg", 5, Some(C)),
                (
                    "todo/u.jpg",
                    7,
                    Some("p1:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"),
                ),
            ],
        );
        put(&conn, "gaffer", "darren", &[("Pictures/c.jpg", 5, Some(C))]);
        let p = pairs(&conn, None, Some(2)).unwrap();
        assert_eq!(p.total, 2);
        let p = p.pairs;
        let shown: Vec<(String, String, Tally)> = p
            .iter()
            .map(|p| (p.a.to_string(), p.b.to_string(), p.shared))
            .collect();
        assert_eq!(
            shown,
            vec![
                (
                    "elrond:darren/Photos".into(),
                    "elrond:darren/todo".into(),
                    Tally {
                        files: 2,
                        bytes: 30
                    }
                ),
                (
                    "elrond:darren/todo".into(),
                    "elrond:darren/todo/sub".into(),
                    Tally {
                        files: 1,
                        bytes: 10
                    }
                ),
            ]
        );
        assert_eq!(
            p[0].b_total,
            Tally {
                files: 5,
                bytes: 52
            }
        );
        // Photos has nothing todo lacks; todo has c.jpg and u.jpg (12 bytes) with no copy in
        // Photos, and both of its copies of a.jpg's content count as covered.
        assert_eq!(p[0].a_only, Tally { files: 0, bytes: 0 });
        assert_eq!(
            p[0].b_only,
            Tally {
                files: 2,
                bytes: 12
            }
        );
        assert!(pairs(&conn, Some("gaffer"), None).unwrap().pairs.is_empty());
        let one = pairs(&conn, None, Some(1)).unwrap();
        assert_eq!((one.total, one.pairs.len()), (2, 1));
        let err = pairs(&conn, Some("bilbo"), None).unwrap_err().to_string();
        assert!(
            err.contains("unknown machine") && err.contains("elrond"),
            "{err}"
        );
    }

    #[test]
    fn drain_preview_says_where_copies_are_and_what_stays() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.jpg", 10, Some(A)),
                ("Photos/2012/a.jpg", 10, Some(A)),
                ("todo/b.jpg", 20, Some(B)),
                ("todo/n.jpg", 3, None),
            ],
        );
        let p = drain_preview(
            &conn,
            &crate::mirror::parse_folder("elrond:darren/todo").unwrap(),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            p.total,
            Tally {
                files: 3,
                bytes: 33
            }
        );
        assert_eq!(
            p.removable,
            Tally {
                files: 1,
                bytes: 10
            }
        );
        assert_eq!(p.copies_in[0].folder.to_string(), "elrond:darren/Photos");
        assert_eq!(p.copies_in_total, 1);
        let reasons: Vec<&str> = p.left.iter().map(|l| l.reason.as_str()).collect();
        assert_eq!(reasons, vec!["unique", "not fingerprinted yet"]);
    }

    #[test]
    fn a_drain_preview_can_leave_out_a_folder() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/Pictures/p.jpg", 10, Some(A)),
                ("Rayla/p.jpg", 10, Some(A)),
                ("todo/Pictures/u.jpg", 4, Some(B)),
                ("todo/x/q.jpg", 5, Some(C)),
                ("todo/Pictures/q.jpg", 5, Some(C)),
            ],
        );
        let f = |s: &str| crate::mirror::parse_folder(s).unwrap();
        let p = drain_preview(
            &conn,
            &f("elrond:darren/todo"),
            &[f("elrond:darren/todo/Pictures")],
            None,
        )
        .unwrap();
        // Only todo/x/q.jpg is in scope; its copy in the left-out folder is the one that stays.
        assert_eq!(p.total, Tally { files: 1, bytes: 5 });
        assert_eq!(p.removable, Tally { files: 1, bytes: 5 });
        assert_eq!(
            p.copies_in[0].folder.to_string(),
            "elrond:darren/todo/Pictures"
        );
        assert!(p.left.is_empty());
        assert_eq!(p.except, vec![f("elrond:darren/todo/Pictures")]);
    }
}
