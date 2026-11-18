//! A plan's effect folder by folder: one tree of the folders it takes files out of or puts
//! files into, each with what leaves, what arrives and what stays (or that it empties). The
//! CLI (`plan show --folders`) and the MCP server render the same tree.
//!
//! What stays comes from the leftovers a drain or keep-over plan recorded when it was drafted.
//! A list plan records none, so for it the catalog's files are counted when the tree is built,
//! leaving out the plan's own sources and destinations, and adding files other applied plans
//! moved in that the catalog hasn't scanned yet. Either way, files any applied plan moved to
//! the trash don't stay. Folders a plan left out (`--except`) aren't in either count, so a
//! folder holding one never claims to empty.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::mirror::{self, FolderRef};
use crate::plan::{self, NOT_MOVED, Summary, Tally};

/// A plan item: its volume, path and size, and a move's destination volume and path.
type Item = (String, String, i64, Option<String>, Option<String>);

/// Levels shown below each top folder unless the caller asks for another depth.
pub const DEFAULT_DEPTH: usize = 3;

/// A plan's folder tree.
#[derive(Debug, Serialize)]
pub struct FolderTree {
    #[serde(flatten)]
    pub summary: Summary,
    /// The folder the tree is limited to, when the caller gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub under: Option<FolderRef>,
    /// Levels shown below each top folder; none means every level.
    pub depth: Option<usize>,
    /// The top folders, by name.
    pub folders: Vec<FolderNode>,
    /// Whether some folders were deeper than `depth`; their numbers are in their ancestor's.
    pub cut_at_depth: bool,
}

/// One folder the plan changes. Its numbers include everything below it.
#[derive(Debug, Serialize)]
pub struct FolderNode {
    pub folder: FolderRef,
    /// The name to show: a top folder's full name, else its path below its parent's line
    /// (several levels when a folder holding only one subfolder shares its line).
    pub name: String,
    /// Files the plan takes out of it (removed, or moved away).
    pub out: Tally,
    /// Files the plan moves into it.
    #[serde(rename = "in")]
    pub into: Tally,
    /// Files left in it after the plan.
    pub stays: Tally,
    /// Nothing is left in it and nothing arrives.
    pub empties: bool,
    /// It holds a folder the plan left out, whose files aren't counted in `stays`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_left_out: bool,
    /// Subfolders the plan changes, by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<FolderNode>,
}

/// A folder while the tree is built: its totals and every subfolder seen.
#[derive(Default)]
struct Dir {
    out: Tally,
    into: Tally,
    stays: Tally,
    left_out: bool,
    subs: BTreeMap<String, Dir>,
}

impl Dir {
    fn changed(&self) -> bool {
        self.out.files + self.into.files > 0
    }
}

struct Roots(Vec<(FolderRef, Dir)>);

impl Roots {
    /// Applies `f` to every folder from the deepest top folder holding `path` (a file on
    /// `volume`) down to the file's own folder. A file outside every top folder is ignored.
    fn add(&mut self, volume: &str, path: &str, f: impl Fn(&mut Dir)) {
        let Some((root, dir)) = self
            .0
            .iter_mut()
            .filter(|(r, _)| r.volume == volume && r.holds(path) && r.path != path)
            .max_by_key(|(r, _)| r.path.len())
        else {
            return;
        };
        let rest = if root.path.is_empty() {
            path
        } else {
            &path[root.path.len() + 1..]
        };
        let mut at = dir;
        f(at);
        let parts: Vec<&str> = rest.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            at = at.subs.entry(part.to_string()).or_default();
            f(at);
        }
    }

    /// Marks the folders from each top folder down to the left-out folder `f`.
    fn mark_left_out(&mut self, f: &FolderRef) {
        for (root, dir) in &mut self.0 {
            if !root.contains(f) {
                continue;
            }
            dir.left_out = true;
            let rest = f.path[root.path.len()..].trim_start_matches('/');
            let mut at = dir;
            for part in rest.split('/').filter(|p| !p.is_empty()) {
                at = at.subs.entry(part.to_string()).or_default();
                at.left_out = true;
            }
        }
    }
}

fn count(t: &mut Tally, size: i64) {
    t.files += 1;
    t.bytes += size;
}

/// The folder tree of plan `id`, `depth` levels below each top folder (`None` for all), and
/// with `under` (a folder inside the plan) rooted there.
pub fn build(
    conn: &Connection,
    id: &str,
    depth: Option<usize>,
    under: Option<&FolderRef>,
) -> Result<FolderTree> {
    let summary = plan::summary(conn, id)?;
    let folders = summary
        .folders
        .iter()
        .map(|f| mirror::parse_folder(f))
        .collect::<Result<Vec<_>>>()?;
    if let Some(u) = under
        && !folders.iter().any(|f| f.contains(u))
    {
        bail!(
            "{u} isn't inside plan {id}'s folders ({})",
            summary.folders.join(", ")
        );
    }

    let items: Vec<Item> = conn
        .prepare(
            "SELECT coalesce(v.name, i.volume_id), i.path, i.size,
                    coalesce(d.name, i.dest_volume_id), i.dest_path
             FROM plan_items i
             LEFT JOIN volumes v ON v.id = i.volume_id
             LEFT JOIN volumes d ON d.id = i.dest_volume_id
             WHERE i.plan_id = ?1",
        )?
        .query_map([id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let tops = match under {
        Some(u) => vec![u.clone()],
        None => match summary.kind.as_str() {
            // Removals happen only in the folder kept over; the keeper doesn't change.
            "keep" => folders[1..].to_vec(),
            "list" => list_tops(&summary.machine, &items),
            // A folder named inside another is covered by the outer one, whose numbers must
            // include it.
            _ => folders
                .iter()
                .filter(|f| !folders.iter().any(|o| o != *f && o.contains(f)))
                .cloned()
                .collect(),
        },
    };
    let mut roots = Roots(tops.into_iter().map(|f| (f, Dir::default())).collect());

    for (volume, path, size, dest_volume, dest_path) in &items {
        roots.add(volume, path, |d| count(&mut d.out, *size));
        if let (Some(dv), Some(dp)) = (dest_volume, dest_path) {
            roots.add(dv, dp, |d| count(&mut d.into, *size));
        }
    }

    if summary.kind == "list" {
        let plan_files: HashSet<(&str, &str)> = items
            .iter()
            .flat_map(|(v, p, _, dv, dp)| {
                let dest = dv.as_deref().zip(dp.as_deref());
                std::iter::once((v.as_str(), p.as_str())).chain(dest)
            })
            .collect();
        let tops: Vec<FolderRef> = roots.0.iter().map(|(f, _)| f.clone()).collect();
        let mut stmt = conn.prepare(&format!(
            "SELECT e.path, e.size FROM entries e JOIN volumes v ON v.id = e.volume_id
             WHERE v.machine = ?1 AND v.name = ?2
               AND (?3 = '' OR (e.path > ?3 || '/' AND e.path < ?3 || '0'))
               AND {NOT_MOVED}"
        ))?;
        for top in &tops {
            let mut rows = stmt.query(params![top.machine, top.volume, top.path])?;
            while let Some(r) = rows.next()? {
                let (path, size): (String, i64) = (r.get(0)?, r.get(1)?);
                if !plan_files.contains(&(top.volume.as_str(), path.as_str())) {
                    roots.add(&top.volume, &path, |d| count(&mut d.stays, size));
                }
            }
        }
        // Files other applied plans moved in, which the catalog sees only after the next scan.
        let mut stmt = conn.prepare(
            "SELECT coalesce(d.name, pi.dest_volume_id), pi.dest_path, pi.size
             FROM plan_items pi JOIN plans p ON p.id = pi.plan_id
             LEFT JOIN volumes d ON d.id = pi.dest_volume_id
             WHERE pi.plan_id <> ?1 AND pi.action = 'move' AND pi.outcome = 'moved'
               AND p.state IN ('applying', 'applied', 'emptied')
               AND NOT EXISTS (SELECT 1 FROM entries e
                               WHERE e.volume_id = pi.dest_volume_id AND e.path = pi.dest_path)",
        )?;
        let mut rows = stmt.query([id])?;
        while let Some(r) = rows.next()? {
            let (volume, path, size): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            if !plan_files.contains(&(volume.as_str(), path.as_str())) {
                roots.add(&volume, &path, |d| count(&mut d.stays, size));
            }
        }
    } else {
        // A leftover an applied plan moved to the trash (before this plan was drafted, or
        // since) no longer stays.
        let mut stmt = conn.prepare(&format!(
            "SELECT coalesce(v.name, l.volume_id), l.path, l.size
             FROM plan_left l LEFT JOIN volumes v ON v.id = l.volume_id
             LEFT JOIN entries e ON e.volume_id = l.volume_id AND e.path = l.path
             WHERE l.plan_id = ?1 AND {NOT_MOVED}"
        ))?;
        let mut rows = stmt.query([id])?;
        while let Some(r) = rows.next()? {
            let (volume, path, size): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            roots.add(&volume, &path, |d| count(&mut d.stays, size));
        }
    }

    for f in &summary.except {
        roots.mark_left_out(&mirror::parse_folder(f)?);
    }

    let mut cut = false;
    let mut tops: Vec<FolderNode> = roots
        .0
        .into_iter()
        .filter(|(_, d)| d.changed())
        .map(|(folder, dir)| {
            let mut node = finish(folder, dir);
            node.name = node.folder.to_string();
            cut_below(&mut node, depth, 0, &mut cut);
            node
        })
        .collect();
    tops.sort_by(|a, b| a.folder.cmp(&b.folder));
    Ok(FolderTree {
        summary,
        under: under.cloned(),
        depth,
        folders: tops,
        cut_at_depth: cut,
    })
}

/// A list plan's top folders: on each volume, the deepest folder holding every file it moves
/// or removes and every place it moves one to.
fn list_tops(machine: &str, items: &[Item]) -> Vec<FolderRef> {
    let mut by_volume: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (volume, path, _, dest_volume, dest_path) in items {
        by_volume.entry(volume).or_default().push(parent(path));
        if let (Some(dv), Some(dp)) = (dest_volume, dest_path) {
            by_volume.entry(dv).or_default().push(parent(dp));
        }
    }
    by_volume
        .into_iter()
        .map(|(volume, dirs)| {
            let mut common: Vec<&str> = dirs[0].split('/').filter(|p| !p.is_empty()).collect();
            for d in &dirs[1..] {
                let same = common
                    .iter()
                    .zip(d.split('/').filter(|p| !p.is_empty()))
                    .take_while(|(a, b)| *a == b)
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

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Turns a built folder into its node: only subfolders the plan changes become children, and
/// a folder whose only content is one such subfolder, with the same numbers, shares its line.
fn finish(folder: FolderRef, dir: Dir) -> FolderNode {
    let child_ref = |name: &str| FolderRef {
        machine: folder.machine.clone(),
        volume: folder.volume.clone(),
        path: if folder.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", folder.path)
        },
    };
    let children: Vec<FolderNode> = dir
        .subs
        .into_iter()
        .filter(|(_, d)| d.changed())
        .map(|(name, d)| {
            let mut child = finish(child_ref(&name), d);
            child.name = if child.name.is_empty() {
                name
            } else {
                format!("{name}/{}", child.name)
            };
            child
        })
        .collect();
    let node = FolderNode {
        empties: dir.stays.files == 0 && dir.into.files == 0 && !dir.left_out,
        folder,
        name: String::new(),
        out: dir.out,
        into: dir.into,
        stays: dir.stays,
        has_left_out: dir.left_out,
        children,
    };
    collapse(node)
}

/// Merges a node into its one child when the child accounts for all of it. The merged node
/// keeps the child's folder; its name (relative, filled in by the caller) gets the child's
/// name appended.
fn collapse(mut node: FolderNode) -> FolderNode {
    if node.children.len() == 1 {
        let c = &node.children[0];
        if c.out == node.out
            && c.into == node.into
            && c.stays == node.stays
            && c.has_left_out == node.has_left_out
        {
            let child = node.children.pop().expect("one child");
            return FolderNode {
                name: child.name,
                ..child
            };
        }
    }
    node
}

/// Drops the levels below `depth` (counting a shared line as one level).
fn cut_below(node: &mut FolderNode, depth: Option<usize>, level: usize, cut: &mut bool) {
    if depth.is_some_and(|d| level >= d) {
        *cut |= !node.children.is_empty();
        node.children.clear();
        return;
    }
    for c in &mut node.children {
        cut_below(c, depth, level + 1, cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{A, B, C, put};
    use crate::plan::{Spec, save, set_state};

    fn folder(s: &str) -> FolderRef {
        mirror::parse_folder(s).unwrap()
    }

    fn tally(t: Tally) -> (i64, i64) {
        (t.files, t.bytes)
    }

    /// A node as (name, depth, out, in, stays, empties).
    type Line = (String, usize, (i64, i64), (i64, i64), (i64, i64), bool);

    /// Each node, in tree order.
    fn lines(t: &FolderTree) -> Vec<Line> {
        fn walk(n: &FolderNode, level: usize, out: &mut Vec<Line>) {
            out.push((
                n.name.clone(),
                level,
                tally(n.out),
                tally(n.into),
                tally(n.stays),
                n.empties,
            ));
            for c in &n.children {
                walk(c, level + 1, out);
            }
        }
        let mut out = Vec::new();
        for n in &t.folders {
            walk(n, 0, &mut out);
        }
        out
    }

    fn names(t: &FolderTree) -> Vec<(String, usize)> {
        lines(t).into_iter().map(|l| (l.0, l.1)).collect()
    }

    fn drain(conn: &Connection, folders: &[&str], except: &[&str]) -> String {
        let except: Vec<FolderRef> = except.iter().map(|f| folder(f)).collect();
        let b = plan::build(
            conn,
            Spec::Drain(folders.iter().map(|f| folder(f)).collect()),
            &except,
        )
        .unwrap();
        save(conn, &b, 1).unwrap()
    }

    /// A drain of darren:todo: x.jpg and y.jpg have copies in photo; the rest are unique.
    fn todo_drain() -> (Connection, String) {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a/deep/x.jpg", 3, Some(A)),
                ("todo/b/y.jpg", 4, Some(B)),
                ("todo/b/keep.txt", 9, None),
                ("todo/c/only.txt", 6, None),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[("x.jpg", 3, Some(A)), ("y.jpg", 4, Some(B))],
        );
        let id = drain(&conn, &["elrond:darren/todo"], &[]);
        (conn, id)
    }

    #[test]
    fn a_drain_shows_what_leaves_and_stays_in_each_folder() {
        let (conn, id) = todo_drain();
        let t = build(&conn, &id, None, None).unwrap();
        assert_eq!(
            lines(&t),
            vec![
                // c holds only a leftover, so it has no line, but counts in todo's stays.
                (
                    "elrond:darren/todo".into(),
                    0,
                    (2, 7),
                    (0, 0),
                    (2, 15),
                    false
                ),
                // a holds only deep: one line.
                ("a/deep".into(), 1, (1, 3), (0, 0), (0, 0), true),
                ("b".into(), 1, (1, 4), (0, 0), (1, 9), false),
            ]
        );
        assert!(!t.cut_at_depth);
    }

    #[test]
    fn a_folder_with_files_of_its_own_keeps_its_line() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a/b/c/x.jpg", 3, Some(A)),
                ("todo/a/note.txt", 2, None),
                ("todo/z.jpg", 4, Some(B)),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[("x.jpg", 3, Some(A)), ("z.jpg", 4, Some(B))],
        );
        let id = drain(&conn, &["elrond:darren/todo"], &[]);
        let t = build(&conn, &id, None, None).unwrap();
        assert_eq!(
            names(&t),
            vec![
                ("elrond:darren/todo".into(), 0),
                ("a".into(), 1),
                ("b/c".into(), 2),
            ]
        );
        assert_eq!(
            t.folders[0].children[0].children[0].folder.path,
            "todo/a/b/c"
        );
    }

    #[test]
    fn depth_keeps_totals_and_drops_deeper_lines() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/2001/06/28/a.jpg", 3, Some(A)),
                ("todo/2001/07/01/b.jpg", 4, Some(B)),
                ("todo/2002/c.jpg", 5, Some(C)),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[
                ("a.jpg", 3, Some(A)),
                ("b.jpg", 4, Some(B)),
                ("c.jpg", 5, Some(C)),
            ],
        );
        let id = drain(&conn, &["elrond:darren/todo"], &[]);
        let all = build(&conn, &id, None, None).unwrap();
        assert_eq!(
            names(&all),
            vec![
                ("elrond:darren/todo".into(), 0),
                ("2001".into(), 1),
                ("06/28".into(), 2),
                ("07/01".into(), 2),
                ("2002".into(), 1),
            ]
        );
        let one = build(&conn, &id, Some(1), None).unwrap();
        assert_eq!(
            names(&one),
            vec![
                ("elrond:darren/todo".into(), 0),
                ("2001".into(), 1),
                ("2002".into(), 1),
            ]
        );
        assert!(one.cut_at_depth);
        assert_eq!(tally(one.folders[0].children[0].out), (2, 7));
        let top = build(&conn, &id, Some(0), None).unwrap();
        assert_eq!(names(&top), vec![("elrond:darren/todo".into(), 0)]);
        assert_eq!(tally(top.folders[0].out), (3, 12));
    }

    #[test]
    fn a_folder_holding_a_left_out_folder_never_empties() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/in/x.jpg", 3, Some(A)),
                ("todo/in/keep/k.jpg", 9, None),
                ("todo/other/y.jpg", 4, Some(B)),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[("x.jpg", 3, Some(A)), ("y.jpg", 4, Some(B))],
        );
        let id = drain(
            &conn,
            &["elrond:darren/todo"],
            &["elrond:darren/todo/in/keep"],
        );
        let t = build(&conn, &id, None, None).unwrap();
        let todo = &t.folders[0];
        assert!(todo.has_left_out && !todo.empties);
        let marks: Vec<(String, bool, bool)> = todo
            .children
            .iter()
            .map(|c| (c.name.clone(), c.has_left_out, c.empties))
            .collect();
        assert_eq!(
            marks,
            vec![("in".into(), true, false), ("other".into(), false, true)]
        );
    }

    #[test]
    fn a_keep_over_plan_is_rooted_at_the_folder_it_removes_from() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[("keeper/x.jpg", 3, Some(A)), ("over/sub/x.jpg", 3, Some(A))],
        );
        let b = plan::build(
            &conn,
            Spec::Keep {
                keeper: folder("elrond:darren/keeper"),
                over: folder("elrond:darren/over"),
            },
            &[],
        )
        .unwrap();
        let id = save(&conn, &b, 1).unwrap();
        let t = build(&conn, &id, None, None).unwrap();
        assert_eq!(names(&t), vec![("elrond:darren/over/sub".into(), 0)]);
    }

    #[test]
    fn a_drain_of_a_folder_and_one_inside_it_counts_both_in_the_outer() {
        let (conn, _) = todo_drain();
        let id = drain(&conn, &["elrond:darren/todo", "elrond:darren/todo/b"], &[]);
        let t = build(&conn, &id, Some(0), None).unwrap();
        assert_eq!(
            lines(&t),
            vec![(
                "elrond:darren/todo".into(),
                0,
                (2, 7),
                (0, 0),
                (2, 15),
                false
            )]
        );
    }

    /// Marks a list plan moving `from` to `to` as applied, as if the files had moved.
    fn applied_move(conn: &Connection, from: &str, to: &str) {
        let b = crate::listplan::build(conn, &format!(r#"{{"move": "{from}", "to": "{to}"}}"#))
            .unwrap();
        let id = save(conn, &b, 1).unwrap();
        conn.execute(
            "UPDATE plan_items SET outcome = 'moved' WHERE plan_id = ?1",
            [&id],
        )
        .unwrap();
        set_state(conn, &id, "applied", 2).unwrap();
    }

    #[test]
    fn a_leftover_another_plan_moved_to_the_trash_doesnt_stay() {
        let (conn, id) = todo_drain();
        applied_move(
            &conn,
            "elrond:darren/todo/b/keep.txt",
            "elrond:darren/elsewhere/keep.txt",
        );
        let t = build(&conn, &id, None, None).unwrap();
        let b = &t.folders[0].children[1];
        assert_eq!(
            (b.name.as_str(), tally(b.stays), b.empties),
            ("b", (0, 0), true)
        );
        assert_eq!(tally(t.folders[0].stays), (1, 6));
    }

    #[test]
    fn under_roots_the_tree_and_is_checked() {
        let (conn, id) = todo_drain();
        let t = build(&conn, &id, None, Some(&folder("elrond:darren/todo/b"))).unwrap();
        assert_eq!(names(&t), vec![("elrond:darren/todo/b".into(), 0)]);
        let none = build(&conn, &id, None, Some(&folder("elrond:darren/todo/c"))).unwrap();
        assert!(none.folders.is_empty());
        let err = build(&conn, &id, None, Some(&folder("elrond:photo")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("isn't inside plan"), "{err}");
    }

    #[test]
    fn an_applied_plan_shows_what_it_set_out_to_do() {
        let (conn, id) = todo_drain();
        let before = lines(&build(&conn, &id, None, None).unwrap());
        conn.execute(
            "UPDATE plan_items SET outcome = 'skipped', reason = 'changed' WHERE plan_id = ?1",
            [&id],
        )
        .unwrap();
        set_state(&conn, &id, "applied", 2).unwrap();
        assert_eq!(lines(&build(&conn, &id, None, None).unwrap()), before);
    }

    fn list_catalog() -> Connection {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/new.jpg", 5, Some(A)),
                ("todo/dup.jpg", 7, Some(B)),
                ("todo/other.txt", 2, None),
            ],
        );
        put(
            &conn,
            "elrond",
            "photo",
            &[("2013/old.jpg", 6, Some(C)), ("2013/dup.jpg", 7, Some(B))],
        );
        conn
    }

    fn list_plan(conn: &Connection) -> String {
        let b = crate::listplan::build(
            conn,
            r#"{"move": "elrond:darren/todo/new.jpg", "to": "elrond:photo/2013/08/new.jpg"}
{"remove": "elrond:darren/todo/dup.jpg", "keep": "elrond:photo/2013/dup.jpg"}"#,
        )
        .unwrap();
        save(conn, &b, 1).unwrap()
    }

    #[test]
    fn a_list_plan_shows_both_ends_of_a_move() {
        let conn = list_catalog();
        let id = list_plan(&conn);
        let t = build(&conn, &id, None, None).unwrap();
        assert_eq!(
            lines(&t),
            vec![
                (
                    "elrond:darren/todo".into(),
                    0,
                    (2, 12),
                    (0, 0),
                    (1, 2),
                    false
                ),
                // The destination folder is new: nothing stays there but it gains a file.
                (
                    "elrond:photo/2013/08".into(),
                    0,
                    (0, 0),
                    (1, 5),
                    (0, 0),
                    false
                ),
            ]
        );
    }

    #[test]
    fn a_list_plans_stays_count_files_another_plan_moved_in_before_a_scan() {
        let conn = list_catalog();
        let id = list_plan(&conn);
        applied_move(
            &conn,
            "elrond:darren/todo/other.txt",
            "elrond:photo/2013/08/other.txt",
        );
        assert_eq!(
            lines(&build(&conn, &id, None, None).unwrap()),
            vec![
                (
                    "elrond:darren/todo".into(),
                    0,
                    (2, 12),
                    (0, 0),
                    (0, 0),
                    true
                ),
                (
                    "elrond:photo/2013/08".into(),
                    0,
                    (0, 0),
                    (1, 5),
                    (1, 2),
                    false
                ),
            ]
        );
    }

    #[test]
    fn a_list_plans_stays_ignore_its_own_files_and_trashed_ones() {
        let conn = list_catalog();
        let id = list_plan(&conn);
        // As after applying and rescanning: the moved file is at its destination.
        put(
            &conn,
            "elrond",
            "photo",
            &[
                ("2013/old.jpg", 6, Some(C)),
                ("2013/dup.jpg", 7, Some(B)),
                ("2013/08/new.jpg", 5, Some(A)),
            ],
        );
        put(&conn, "elrond", "darren", &[("todo/other.txt", 2, None)]);
        set_state(&conn, &id, "applied", 2).unwrap();
        assert_eq!(
            lines(&build(&conn, &id, None, None).unwrap()),
            vec![
                (
                    "elrond:darren/todo".into(),
                    0,
                    (2, 12),
                    (0, 0),
                    (1, 2),
                    false
                ),
                (
                    "elrond:photo/2013/08".into(),
                    0,
                    (0, 0),
                    (1, 5),
                    (0, 0),
                    false
                ),
            ]
        );

        // Another applied plan moved todo/other.txt to the trash; the catalog still has it
        // until the next scan.
        let conn = list_catalog();
        let id = list_plan(&conn);
        put(
            &conn,
            "elrond",
            "photo",
            &[
                ("x.txt", 2, None),
                ("2013/dup.jpg", 7, Some(B)),
                ("2013/old.jpg", 6, Some(C)),
            ],
        );
        let other = crate::listplan::build(
            &conn,
            r#"{"move": "elrond:darren/todo/other.txt", "to": "elrond:darren/elsewhere/other.txt"}"#,
        )
        .unwrap();
        let other = save(&conn, &other, 1).unwrap();
        conn.execute(
            "UPDATE plan_items SET outcome = 'moved' WHERE plan_id = ?1",
            [&other],
        )
        .unwrap();
        set_state(&conn, &other, "applied", 2).unwrap();
        let t = build(&conn, &id, None, None).unwrap();
        assert_eq!(tally(t.folders[0].stays), (0, 0));
        assert!(t.folders[0].empties);
    }
}
