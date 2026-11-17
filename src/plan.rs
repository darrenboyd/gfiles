//! Cleanup plans: which duplicate files to remove from a machine, and which copy of each stays.
//! Plans are built from the hub's catalog and stored there for Darren to review; `apply`
//! carries them out on the machine holding the files.
//!
//! Two kinds:
//! - **drain** empties folders of every file that has a copy elsewhere on the same machine.
//!   When every copy is inside the drained folders, one of them stays.
//! - **keep** (keep A over B) removes from B each file that A also holds.
//!
//! Only same-machine copies count: a copy on another machine is a deliberate local copy, never a
//! reason to remove one. Mirror files (see `mirror`) are never removed or kept, since a sync tool
//! replicates deletes, and repositories are one cataloged item, never removed. Matching uses
//! the sampled fingerprint; the machine applying a plan compares full content before moving
//! anything.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::mirror::{self, FolderRef, JOIN};

/// FROM and WHERE (on `e`) for the plans that moved entry `e` to the trash. Only the file as
/// the plan saw it (same size, mtime and fingerprint) counts: a new file that later lands at
/// the same path is a different file.
macro_rules! moved_by {
    () => {
        "FROM plan_items pi JOIN plans p ON p.id = pi.plan_id
         WHERE pi.volume_id = e.volume_id AND pi.path = e.path AND pi.outcome = 'moved'
           AND pi.size = e.size AND pi.mtime = e.mtime AND pi.fingerprint IS e.fingerprint
           AND p.state IN ('applying', 'applied', 'emptied')"
    };
}

/// SQL condition (on `e`) for an entry no applied plan has moved to the trash. Such entries
/// stay in the catalog until the next scan, but they're gone from their folder.
pub const NOT_MOVED: &str = concat!("NOT EXISTS (SELECT 1 ", moved_by!(), ")");

/// Folder names that sync tools own: a file removed there is removed from every other copy the
/// tool keeps, so plans refuse them unless they're configured as mirrors or listed as not
/// synced.
const SYNC_TOOL_FOLDERS: &[&str] = &["Dropbox", "OneDrive", "SynologyDrive"];

#[derive(Debug, Clone, PartialEq)]
pub enum Spec {
    /// Remove every file in these folders that has a copy elsewhere on the machine.
    Drain(Vec<FolderRef>),
    /// Remove from `over` every file that `keeper` also holds.
    Keep { keeper: FolderRef, over: FolderRef },
}

impl Spec {
    pub fn kind(&self) -> &'static str {
        match self {
            Spec::Drain(_) => "drain",
            Spec::Keep { .. } => "keep",
        }
    }

    pub fn folders(&self) -> Vec<FolderRef> {
        match self {
            Spec::Drain(f) => f.clone(),
            Spec::Keep { keeper, over } => vec![keeper.clone(), over.clone()],
        }
    }

    fn from_stored(kind: &str, folders: &[FolderRef]) -> Result<Spec> {
        Ok(match (kind, folders) {
            ("drain", f) => Spec::Drain(f.to_vec()),
            ("keep", [keeper, over]) => Spec::Keep {
                keeper: keeper.clone(),
                over: over.clone(),
            },
            _ => bail!("plan of unknown kind {kind:?}"),
        })
    }
}

/// A file a plan removes, and the copy that stays.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Item {
    pub volume_id: String,
    pub volume: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub fingerprint: String,
    pub keep_volume_id: String,
    pub keep_volume: String,
    pub keep_path: String,
    pub keep_size: i64,
    pub keep_mtime: i64,
}

/// A file in the plan's folders that it leaves alone, and why.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Left {
    pub volume_id: String,
    pub volume: String,
    pub path: String,
    pub size: i64,
    pub reason: String,
}

/// A plan before it's saved: what it would remove and what it leaves.
#[derive(Debug)]
pub struct Built {
    pub machine: String,
    pub spec: Spec,
    pub items: Vec<Item>,
    pub left: Vec<Left>,
}

/// One catalog entry under a target folder.
struct TargetEntry {
    volume_id: String,
    volume: String,
    path: String,
    size: i64,
    kind: String,
    fingerprint: Option<String>,
    mirror: Option<String>,
    moved_by: Option<String>,
}

/// One same-machine copy of a content.
#[derive(Clone)]
struct Member {
    volume_id: String,
    volume: String,
    path: String,
    size: i64,
    mtime: i64,
}

/// A folder resolved against the catalog.
struct Resolved {
    folder: FolderRef,
    volume_id: String,
}

impl Resolved {
    fn holds(&self, volume_id: &str, path: &str) -> bool {
        self.volume_id == volume_id && self.folder.holds(path)
    }
}

/// Builds a plan from the catalog (with mirrors loaded). Refuses folders that aren't in the
/// catalog, that span machines, or that are inside a configured mirror.
pub fn build(conn: &Connection, spec: Spec) -> Result<Built> {
    let folders = spec.folders();
    let Some(first) = folders.first() else {
        bail!("name at least one folder");
    };
    let machine = first.machine.clone();
    let mut resolved = Vec::new();
    for f in &folders {
        if f.machine != machine {
            bail!(
                "{f} and {first} are on different machines; a plan only removes copies on one machine"
            );
        }
        resolved.push(resolve(conn, f)?);
    }
    if let Spec::Keep { keeper, over } = &spec
        && (keeper.contains(over) || over.contains(keeper))
    {
        bail!("{keeper} and {over} contain one another; pick two separate folders");
    }
    // The folders files are removed from, and (for keep) the folder whose copies stay.
    let (targets, keeper): (Vec<&Resolved>, Option<&Resolved>) = match &spec {
        Spec::Drain(_) => (resolved.iter().collect(), None),
        Spec::Keep { .. } => (vec![&resolved[1]], Some(&resolved[0])),
    };

    let mut items = Vec::new();
    let mut left = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut candidates: BTreeMap<(i64, String), ()> = BTreeMap::new();
    for t in &targets {
        for e in target_entries(conn, t)? {
            if !seen.insert((e.volume_id.clone(), e.path.clone())) {
                continue;
            }
            let reason = if e.kind == crate::classify::REPO {
                Some("repository".to_string())
            } else if let Some(m) = &e.mirror {
                Some(format!("in mirror {m}"))
            } else if let Some(p) = &e.moved_by {
                Some(format!("already moved by plan {p}"))
            } else if e.size == 0 {
                Some("empty file".to_string())
            } else if e.fingerprint.is_none() {
                Some("not fingerprinted yet".to_string())
            } else {
                None
            };
            match (reason, &e.fingerprint) {
                (Some(reason), _) => left.push(Left {
                    volume_id: e.volume_id,
                    volume: e.volume,
                    path: e.path,
                    size: e.size,
                    reason,
                }),
                (None, Some(fp)) => {
                    candidates.insert((e.size, fp.clone()), ());
                }
                (None, None) => unreachable!("unfingerprinted entries have a reason"),
            }
        }
    }

    let in_targets = |m: &Member| targets.iter().any(|t| t.holds(&m.volume_id, &m.path));
    let mut members_stmt = conn.prepare(&format!(
        "SELECT e.volume_id, v.name, e.path, e.size, e.mtime
         FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         WHERE v.machine = ?1 AND e.size = ?2 AND e.fingerprint = ?3
           AND mf.mirror IS NULL AND {NOT_MOVED}"
    ))?;
    let mut mirror_stmt = conn.prepare(&format!(
        "SELECT mf.mirror FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         WHERE v.machine = ?1 AND e.size = ?2 AND e.fingerprint = ?3 AND mf.mirror IS NOT NULL
         LIMIT 1"
    ))?;
    let mut machines_stmt = conn.prepare(
        "SELECT DISTINCT v.machine FROM entries e JOIN volumes v ON v.id = e.volume_id
         WHERE v.machine != ?1 AND e.size = ?2 AND e.fingerprint = ?3 ORDER BY v.machine",
    )?;
    for (size, fp) in candidates.keys() {
        let members: Vec<Member> = members_stmt
            .query_map(params![machine, size, fp], |r| {
                Ok(Member {
                    volume_id: r.get(0)?,
                    volume: r.get(1)?,
                    path: r.get(2)?,
                    size: r.get(3)?,
                    mtime: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let (inside, outside): (Vec<&Member>, Vec<&Member>) =
            members.iter().partition(|m| in_targets(m));
        let kept: Option<&Member> = match keeper {
            Some(k) => best(
                outside
                    .iter()
                    .copied()
                    .filter(|m| k.holds(&m.volume_id, &m.path)),
            ),
            None => best(outside.iter().copied()).or_else(|| {
                if inside.len() >= 2 {
                    best(inside.iter().copied())
                } else {
                    None
                }
            }),
        };
        match kept {
            Some(kept) => {
                for m in inside.iter().filter(|m| !same(m, kept)) {
                    items.push(Item {
                        volume_id: m.volume_id.clone(),
                        volume: m.volume.clone(),
                        path: m.path.clone(),
                        size: m.size,
                        mtime: m.mtime,
                        fingerprint: fp.clone(),
                        keep_volume_id: kept.volume_id.clone(),
                        keep_volume: kept.volume.clone(),
                        keep_path: kept.path.clone(),
                        keep_size: kept.size,
                        keep_mtime: kept.mtime,
                    });
                }
            }
            None => {
                let reason = if let Some(k) = keeper {
                    format!("not in {}", k.folder)
                } else if let Some(m) = mirror_stmt
                    .query_row(params![machine, size, fp], |r| r.get::<_, String>(0))
                    .optional()?
                {
                    format!("other copy only in mirror {m}")
                } else {
                    let others: Vec<String> = machines_stmt
                        .query_map(params![machine, size, fp], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    if others.is_empty() {
                        "unique".to_string()
                    } else {
                        format!("other copies only on {}", others.join(", "))
                    }
                };
                for m in &inside {
                    left.push(Left {
                        volume_id: m.volume_id.clone(),
                        volume: m.volume.clone(),
                        path: m.path.clone(),
                        size: m.size,
                        reason: reason.clone(),
                    });
                }
            }
        }
    }
    items.sort_by(|a, b| (&a.volume, &a.path).cmp(&(&b.volume, &b.path)));
    left.sort_by(|a, b| (&a.volume, &a.path).cmp(&(&b.volume, &b.path)));
    Ok(Built {
        machine,
        spec,
        items,
        left,
    })
}

/// The copy to keep among `members`: the shortest path, then alphabetical.
fn best<'a>(members: impl Iterator<Item = &'a Member>) -> Option<&'a Member> {
    members
        .min_by(|a, b| (a.path.len(), &a.volume, &a.path).cmp(&(b.path.len(), &b.volume, &b.path)))
}

fn same(a: &Member, b: &Member) -> bool {
    a.volume_id == b.volume_id && a.path == b.path
}

/// Finds a folder's volume in the catalog, and refuses one inside a configured mirror.
fn resolve(conn: &Connection, f: &FolderRef) -> Result<Resolved> {
    let volume_id: String = conn
        .query_row(
            "SELECT id FROM volumes WHERE machine = ?1 AND name = ?2",
            params![f.machine, f.volume],
            |r| r.get(0),
        )
        .optional()?
        .with_context(|| format!("no volume {:?} on {} in the catalog", f.volume, f.machine))?;
    let mirror: Option<String> = conn
        .query_row(
            "SELECT mirror FROM temp.mirror_folders
             WHERE volume_id = ?1
               AND (prefix = '' OR ?2 = prefix OR substr(?2, 1, length(prefix) + 1) = prefix || '/')",
            params![volume_id, f.path],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(m) = mirror {
        bail!(
            "{f} is in mirror {m:?}: removing a file there would delete it from every replica, \
             so gfiles never cleans up inside a mirror"
        );
    }
    let exists: bool = f.path.is_empty()
        || conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM entries WHERE volume_id = ?1
                 AND substr(path, 1, length(?2) + 1) = ?2 || '/')",
            params![volume_id, f.path],
            |r| r.get(0),
        )?;
    if !exists {
        bail!("no cataloged files under {f}");
    }
    Ok(Resolved {
        folder: f.clone(),
        volume_id,
    })
}

fn target_entries(conn: &Connection, t: &Resolved) -> Result<Vec<TargetEntry>> {
    let sql = format!(
        "SELECT e.volume_id, v.name, e.path, e.size, e.kind, e.fingerprint, mf.mirror,
                (SELECT p.id {moved_by} LIMIT 1)
         FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         WHERE e.volume_id = ?1
           AND (?2 = '' OR e.path = ?2 OR substr(e.path, 1, length(?2) + 1) = ?2 || '/')",
        moved_by = moved_by!()
    );
    Ok(conn
        .prepare(&sql)?
        .query_map(params![t.volume_id, t.folder.path], |r| {
            Ok(TargetEntry {
                volume_id: r.get(0)?,
                volume: r.get(1)?,
                path: r.get(2)?,
                size: r.get(3)?,
                kind: r.get(4)?,
                fingerprint: r.get(5)?,
                mirror: r.get(6)?,
                moved_by: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Refuses a plan that touches a folder a sync tool seems to manage without being configured
/// as a mirror: one `gfiles mirrors` suggests, or one named like a sync tool's folder. Removing
/// a file there would make the tool remove its other copies too. `not_synced` lists folders
/// Darren has said no tool syncs.
pub fn check_not_synced(conn: &Connection, built: &Built, not_synced: &[FolderRef]) -> Result<()> {
    let suggested: Vec<(FolderRef, String)> = mirror::suggestions(conn)?
        .into_iter()
        .flat_map(|s| {
            let [a, b] = s.folders;
            [(a.clone(), b.clone()), (b, a)]
        })
        .filter_map(|(f, other)| Some((mirror::parse_folder(&f).ok()?, other)))
        .filter(|(f, _)| f.machine == built.machine)
        .collect();
    let roots: HashMap<String, String> = conn
        .prepare("SELECT name, root FROM volumes WHERE machine = ?1")?
        .query_map([&built.machine], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let at = |volume: &str, path: &str| FolderRef {
        machine: built.machine.clone(),
        volume: volume.to_string(),
        path: path.to_string(),
    };
    let mut places: Vec<FolderRef> = built.spec.folders();
    for i in &built.items {
        places.push(at(&i.volume, &i.path));
        places.push(at(&i.keep_volume, &i.keep_path));
    }
    let mut problems: Vec<String> = Vec::new();
    for p in &places {
        if not_synced.iter().any(|n| n.contains(p)) {
            continue;
        }
        let problem = if let Some((f, other)) = suggested.iter().find(|(f, _)| f.contains(p)) {
            Some(format!("{f} (gfiles mirrors suggests it mirrors {other})"))
        } else {
            let root = roots.get(&p.volume).map_or("", String::as_str);
            let mut parts = std::path::Path::new(root)
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .chain(p.path.split('/'));
            parts
                .find(|c| SYNC_TOOL_FOLDERS.contains(c))
                .map(|name| format!("{p} (inside a {name} folder)"))
        };
        if let Some(problem) = problem
            && !problems.contains(&problem)
        {
            problems.push(problem);
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = problems.iter().take(5).map(String::as_str).collect();
    bail!(
        "this plan touches folders a sync tool may manage: {}{}. Removing a file there could \
         make the tool delete its other copies. If a tool keeps a folder in sync, add it to \
         [[mirror]] in the hub's config; if not, list it in not_synced = [\"machine:volume/dir\"].",
        shown.join("; "),
        if problems.len() > shown.len() {
            format!(" and {} more", problems.len() - shown.len())
        } else {
            String::new()
        }
    )
}

/// Saves a built plan as a draft. Returns its id.
pub fn save(conn: &Connection, built: &Built, now: i64) -> Result<String> {
    let id = new_id(now);
    let folders: Vec<String> = built.spec.folders().iter().map(|f| f.to_string()).collect();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO plans (id, machine, kind, folders, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'draft', ?5, ?5)",
        params![
            id,
            built.machine,
            built.spec.kind(),
            serde_json::to_string(&folders)?,
            now
        ],
    )?;
    {
        let mut item = tx.prepare(
            "INSERT INTO plan_items (plan_id, seq, volume_id, path, size, mtime, fingerprint,
                 keep_volume_id, keep_path, keep_size, keep_mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for (seq, i) in built.items.iter().enumerate() {
            item.execute(params![
                id,
                seq as i64,
                i.volume_id,
                i.path,
                i.size,
                i.mtime,
                i.fingerprint,
                i.keep_volume_id,
                i.keep_path,
                i.keep_size,
                i.keep_mtime
            ])?;
        }
        let mut left = tx.prepare(
            "INSERT INTO plan_left (plan_id, volume_id, path, size, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for l in &built.left {
            left.execute(params![id, l.volume_id, l.path, l.size, l.reason])?;
        }
    }
    tx.commit()?;
    Ok(id)
}

/// A new plan id: the creation time (UTC) and a random suffix, safe as a folder name.
fn new_id(now: i64) -> String {
    let secs = now.rem_euclid(86_400);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!(
        "{}-{:02}{:02}{:02}-{}",
        crate::fmt::date(now).replace('-', ""),
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        &suffix[..4]
    )
}

/// Whether `id` has the shape `new_id` makes (so it's safe to use as a folder name).
pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    matches!(parts.as_slice(), [d, t, r]
        if d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())
        && t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit())
        && r.len() == 4 && r.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Files and their bytes.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq)]
pub struct Tally {
    pub files: i64,
    pub bytes: i64,
}

/// A plan's state and totals.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub id: String,
    pub machine: String,
    pub kind: String,
    pub folders: Vec<String>,
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Files the plan removes, and the bytes that frees once the trash is emptied.
    pub remove: Tally,
    /// Files left in the plan's folders.
    pub left: Tally,
    pub moved: Tally,
    pub skipped: Tally,
    /// Bytes this plan has in the trash (reclaimable by emptying it).
    pub in_trash: i64,
}

const SUMMARY_SQL: &str = "SELECT p.id, p.machine, p.kind, p.folders, p.state, p.created_at,
        p.updated_at,
        (SELECT count(*) FROM plan_items WHERE plan_id = p.id),
        (SELECT coalesce(sum(size), 0) FROM plan_items WHERE plan_id = p.id),
        (SELECT count(*) FROM plan_left WHERE plan_id = p.id),
        (SELECT coalesce(sum(size), 0) FROM plan_left WHERE plan_id = p.id),
        (SELECT count(*) FROM plan_items WHERE plan_id = p.id AND outcome = 'moved'),
        (SELECT coalesce(sum(size), 0) FROM plan_items WHERE plan_id = p.id AND outcome = 'moved'),
        (SELECT count(*) FROM plan_items WHERE plan_id = p.id AND outcome = 'skipped'),
        (SELECT coalesce(sum(size), 0) FROM plan_items WHERE plan_id = p.id AND outcome = 'skipped')
    FROM plans p";

fn read_summary(r: &rusqlite::Row) -> rusqlite::Result<Summary> {
    let folders: String = r.get(3)?;
    let state: String = r.get(4)?;
    let moved = Tally {
        files: r.get(11)?,
        bytes: r.get(12)?,
    };
    Ok(Summary {
        id: r.get(0)?,
        machine: r.get(1)?,
        kind: r.get(2)?,
        folders: serde_json::from_str(&folders).unwrap_or_default(),
        in_trash: if matches!(state.as_str(), "applying" | "applied") {
            moved.bytes
        } else {
            0
        },
        state,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        remove: Tally {
            files: r.get(7)?,
            bytes: r.get(8)?,
        },
        left: Tally {
            files: r.get(9)?,
            bytes: r.get(10)?,
        },
        moved,
        skipped: Tally {
            files: r.get(13)?,
            bytes: r.get(14)?,
        },
    })
}

/// Every plan, newest first.
pub fn list(conn: &Connection) -> Result<Vec<Summary>> {
    Ok(conn
        .prepare(&format!(
            "{SUMMARY_SQL} ORDER BY p.created_at DESC, p.id DESC"
        ))?
        .query_map([], read_summary)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn summary(conn: &Connection, id: &str) -> Result<Summary> {
    conn.query_row(
        &format!("{SUMMARY_SQL} WHERE p.id = ?1"),
        [id],
        read_summary,
    )
    .optional()?
    .with_context(|| format!("no plan {id:?}; `gfiles plan list` shows them"))
}

/// The plan's stored spec (kind and folders).
pub fn spec(conn: &Connection, id: &str) -> Result<Spec> {
    let s = summary(conn, id)?;
    let folders = s
        .folders
        .iter()
        .map(|f| mirror::parse_folder(f))
        .collect::<Result<Vec<_>>>()?;
    Spec::from_stored(&s.kind, &folders)
}

/// A file a plan removes, for display.
#[derive(Debug, Serialize)]
pub struct ItemView {
    pub volume: String,
    pub path: String,
    pub abs_path: String,
    pub size: i64,
    pub keep_volume: String,
    pub keep_path: String,
    pub keep_abs_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Files left behind for one reason.
#[derive(Debug, Serialize)]
pub struct LeftGroup {
    pub reason: String,
    pub files: i64,
    pub bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct LeftView {
    pub volume: String,
    pub path: String,
    pub abs_path: String,
    pub size: i64,
    pub reason: String,
}

/// A plan in full: its totals, the files it removes (up to a limit, by folder then name) and
/// the files it leaves.
#[derive(Debug, Serialize)]
pub struct View {
    #[serde(flatten)]
    pub summary: Summary,
    pub items: Vec<ItemView>,
    pub left_by_reason: Vec<LeftGroup>,
    pub left_files: Vec<LeftView>,
}

pub fn show(conn: &Connection, id: &str, limit: Option<usize>) -> Result<View> {
    let summary = summary(conn, id)?;
    let lim = limit.map_or(-1, |l| l as i64);
    let items = conn
        .prepare(
            "SELECT coalesce(v.name, i.volume_id), i.path, coalesce(v.root, ''), i.size,
                    coalesce(k.name, i.keep_volume_id), i.keep_path, coalesce(k.root, ''),
                    i.outcome, i.reason
             FROM plan_items i
             LEFT JOIN volumes v ON v.id = i.volume_id
             LEFT JOIN volumes k ON k.id = i.keep_volume_id
             WHERE i.plan_id = ?1 ORDER BY i.seq LIMIT ?2",
        )?
        .query_map(params![id, lim], |r| {
            let (root, path): (String, String) = (r.get(2)?, r.get(1)?);
            let (keep_root, keep_path): (String, String) = (r.get(6)?, r.get(5)?);
            Ok(ItemView {
                volume: r.get(0)?,
                abs_path: crate::search::join_root(&root, &path),
                path,
                size: r.get(3)?,
                keep_volume: r.get(4)?,
                keep_abs_path: crate::search::join_root(&keep_root, &keep_path),
                keep_path,
                outcome: r.get(7)?,
                reason: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let left_by_reason = conn
        .prepare(
            "SELECT reason, count(*), sum(size) FROM plan_left WHERE plan_id = ?1
             GROUP BY reason ORDER BY sum(size) DESC, reason",
        )?
        .query_map([id], |r| {
            Ok(LeftGroup {
                reason: r.get(0)?,
                files: r.get(1)?,
                bytes: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let left_files = conn
        .prepare(
            "SELECT coalesce(v.name, l.volume_id), l.path, coalesce(v.root, ''), l.size, l.reason
             FROM plan_left l LEFT JOIN volumes v ON v.id = l.volume_id
             WHERE l.plan_id = ?1 ORDER BY l.size DESC, l.path LIMIT ?2",
        )?
        .query_map(params![id, lim], |r| {
            let (root, path): (String, String) = (r.get(2)?, r.get(1)?);
            Ok(LeftView {
                volume: r.get(0)?,
                abs_path: crate::search::join_root(&root, &path),
                path,
                size: r.get(3)?,
                reason: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(View {
        summary,
        items,
        left_by_reason,
        left_files,
    })
}

/// Deletes a plan that hasn't been applied.
pub fn drop(conn: &Connection, id: &str) -> Result<()> {
    let s = summary(conn, id)?;
    if s.state != "draft" {
        bail!(
            "plan {id} is {}; only a draft can be dropped (undo it instead)",
            s.state
        );
    }
    conn.execute("DELETE FROM plans WHERE id = ?1", [id])?;
    Ok(())
}

pub fn set_state(conn: &Connection, id: &str, state: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE plans SET state = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, state, now],
    )?;
    Ok(())
}

/// The plan's items that have no outcome yet, as the applying machine receives them.
pub fn exec_items(conn: &Connection, id: &str) -> Result<Vec<crate::apply::Item>> {
    Ok(conn
        .prepare(
            "SELECT seq, volume_id, path, size, mtime, keep_volume_id, keep_path, keep_size,
                    keep_mtime
             FROM plan_items WHERE plan_id = ?1 AND outcome IS NULL ORDER BY seq",
        )?
        .query_map([id], |r| {
            Ok(crate::apply::Item {
                seq: r.get(0)?,
                volume_id: r.get(1)?,
                path: r.get(2)?,
                size: r.get(3)?,
                mtime: r.get(4)?,
                keep_volume_id: r.get(5)?,
                keep_path: r.get(6)?,
                keep_size: r.get(7)?,
                keep_mtime: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The volumes that may hold the plan's files in the trash: those of every item not skipped.
pub fn trash_volumes(conn: &Connection, id: &str) -> Result<Vec<String>> {
    Ok(conn
        .prepare(
            "SELECT DISTINCT volume_id FROM plan_items
             WHERE plan_id = ?1 AND outcome IS NOT 'skipped' ORDER BY volume_id",
        )?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Records what happened to items, in one transaction.
pub fn record(conn: &mut Connection, id: &str, outcomes: &[crate::apply::Outcome]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE plan_items SET outcome = ?3, reason = ?4 WHERE plan_id = ?1 AND seq = ?2",
        )?;
        for o in outcomes {
            stmt.execute(params![
                id,
                o.seq,
                if o.moved { "moved" } else { "skipped" },
                o.reason
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::{Entry, Volume, VolumeWriter};

    pub(crate) const A: &str =
        "p1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const B: &str =
        "p1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    pub(crate) const C: &str =
        "p1:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    /// Writes (path, size, fingerprint) entries into machine:volume.
    pub(crate) fn put(
        conn: &Connection,
        machine: &str,
        volume: &str,
        entries: &[(&str, i64, Option<&str>)],
    ) {
        let vol = Volume {
            id: format!("{machine}-{volume}"),
            name: volume.into(),
            machine: machine.into(),
            root: format!("/{machine}/{volume}"),
            last_scan_at: None,
        };
        let mut w = VolumeWriter::begin(conn, &vol).unwrap();
        for (path, size, fp) in entries {
            let kind = if fp.is_some_and(|f| f.starts_with("git:")) {
                "repo"
            } else {
                "photo"
            };
            w.put(&Entry {
                path: path.to_string(),
                name: path.rsplit('/').next().unwrap().into(),
                ext: None,
                kind: kind.into(),
                size: *size,
                mtime: 7,
                fingerprint: fp.map(Into::into),
                content_hash: None,
            })
            .unwrap();
        }
        w.finish(0, &[]).unwrap();
    }

    fn folder(s: &str) -> FolderRef {
        mirror::parse_folder(s).unwrap()
    }

    fn drain(conn: &Connection, folders: &[&str]) -> Result<Built> {
        build(
            conn,
            Spec::Drain(folders.iter().map(|f| folder(f)).collect()),
        )
    }

    fn removed(b: &Built) -> Vec<(String, String)> {
        b.items
            .iter()
            .map(|i| (i.path.clone(), i.keep_path.clone()))
            .collect()
    }

    fn left(b: &Built) -> Vec<(String, String)> {
        b.left
            .iter()
            .map(|l| (l.path.clone(), l.reason.clone()))
            .collect()
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn draining_two_folders_keeps_one_of_their_only_copies() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/IMG_1.jpg", 10, Some(A)),
                ("PhotoMgmt/2012/IMG_1.jpg", 10, Some(A)),
            ],
        );
        let b = drain(&conn, &["elrond:darren/todo", "elrond:darren/PhotoMgmt"]).unwrap();
        assert_eq!(
            removed(&b),
            vec![pair("PhotoMgmt/2012/IMG_1.jpg", "todo/IMG_1.jpg")]
        );
        assert!(b.left.is_empty());
    }

    #[test]
    fn a_copy_outside_the_targets_stays_and_every_target_copy_goes() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.jpg", 10, Some(A)),
                ("todo/sub/a copy.jpg", 10, Some(A)),
                ("Photos/2012/x/a.jpg", 10, Some(A)),
                ("Photos/b.jpg", 10, Some(A)),
            ],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        // The shortest path outside the target stays.
        assert_eq!(
            removed(&b),
            vec![
                pair("todo/a.jpg", "Photos/b.jpg"),
                pair("todo/sub/a copy.jpg", "Photos/b.jpg")
            ]
        );
    }

    #[test]
    fn ties_break_alphabetically() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/x.jpg", 10, Some(A)),
                ("b/x.jpg", 10, Some(A)),
                ("a/x.jpg", 10, Some(A)),
            ],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert_eq!(removed(&b), vec![pair("todo/x.jpg", "a/x.jpg")]);
    }

    #[test]
    fn unique_files_stay_and_say_where_else_they_are() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/report.pdf", 10, Some(A)),
                ("todo/lonely.jpg", 10, Some(B)),
                ("todo/new.jpg", 10, None),
                ("todo/empty.txt", 0, None),
            ],
        );
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Documents/report.pdf", 10, Some(A))],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert!(b.items.is_empty());
        assert_eq!(
            left(&b),
            vec![
                pair("todo/empty.txt", "empty file"),
                pair("todo/lonely.jpg", "unique"),
                pair("todo/new.jpg", "not fingerprinted yet"),
                pair("todo/report.pdf", "other copies only on gaffer"),
            ]
        );
    }

    #[test]
    fn keep_over_removes_only_what_the_keeper_holds() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("A/deep/x.jpg", 10, Some(A)),
                ("B/x.jpg", 10, Some(A)),
                ("B/y.jpg", 10, Some(B)),
                ("C/y.jpg", 10, Some(B)),
            ],
        );
        let b = build(
            &conn,
            Spec::Keep {
                keeper: folder("elrond:darren/A"),
                over: folder("elrond:darren/B"),
            },
        )
        .unwrap();
        assert_eq!(removed(&b), vec![pair("B/x.jpg", "A/deep/x.jpg")]);
        // y's other copy is in a third folder, which a keep-over plan doesn't count.
        assert_eq!(left(&b), vec![pair("B/y.jpg", "not in elrond:darren/A")]);
    }

    #[test]
    fn nested_keep_over_folders_are_refused() {
        let conn = crate::db::open_in_memory().unwrap();
        put(&conn, "elrond", "darren", &[("A/B/x.jpg", 10, Some(A))]);
        let err = build(
            &conn,
            Spec::Keep {
                keeper: folder("elrond:darren/A"),
                over: folder("elrond:darren/A/B"),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("contain one another"), "{err}");
    }

    fn with_drive(conn: &Connection) {
        let config: Config = toml::from_str(
            "machine = \"gaffer\"\n[[mirror]]\nname = \"drive\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:darren/Drive\"]\n",
        )
        .unwrap();
        mirror::load(conn, &config).unwrap();
    }

    #[test]
    fn mirrors_are_never_drained_or_kept() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/a.pdf", 10, Some(A)),
                ("Drive/old/a.pdf", 10, Some(A)),
                ("todo/a.pdf", 10, Some(A)),
            ],
        );
        put(&conn, "gaffer", "darren", &[("Drive/a.pdf", 10, Some(A))]);
        with_drive(&conn);
        let err = drain(&conn, &["elrond:darren/Drive/old"]).unwrap_err();
        assert!(err.to_string().contains("in mirror \"drive\""), "{err}");
        // A file whose only other copy is in the mirror stays.
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert!(b.items.is_empty());
        assert_eq!(
            left(&b),
            vec![pair("todo/a.pdf", "other copy only in mirror drive")]
        );
        // A drained folder holding a mirror leaves the mirror's files alone.
        let b = drain(&conn, &["elrond:darren"]).unwrap();
        assert_eq!(removed(&b), Vec::<(String, String)>::new());
        assert!(left(&b).contains(&pair("Drive/a.pdf", "in mirror drive")));
    }

    #[test]
    fn repositories_and_unknown_folders() {
        let conn = crate::db::open_in_memory().unwrap();
        let git = "git:0123456789abcdef0123456789abcdef01234567";
        put(
            &conn,
            "elrond",
            "darren",
            &[("todo/proj", 0, Some(git)), ("code/proj", 0, Some(git))],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert!(b.items.is_empty());
        assert_eq!(left(&b), vec![pair("todo/proj", "repository")]);
        assert!(drain(&conn, &["elrond:darren/nope"]).is_err());
        assert!(drain(&conn, &["elrond:media/todo"]).is_err());
        let err = drain(&conn, &["elrond:darren/todo", "gaffer:darren/todo"]).unwrap_err();
        assert!(err.to_string().contains("different machines"), "{err}");
    }

    #[test]
    fn every_plan_keeps_a_copy_of_every_content() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("a/1.jpg", 10, Some(A)),
                ("b/1.jpg", 10, Some(A)),
                ("a/x/1.jpg", 10, Some(A)),
                ("b/2.jpg", 20, Some(B)),
                ("c/2.jpg", 20, Some(B)),
                ("a/3.jpg", 30, Some(C)),
            ],
        );
        let b = drain(
            &conn,
            &["elrond:darren/a", "elrond:darren/b", "elrond:darren/c"],
        )
        .unwrap();
        let gone: HashSet<&str> = b.items.iter().map(|i| i.path.as_str()).collect();
        for (fp, paths) in [
            (A, vec!["a/1.jpg", "b/1.jpg", "a/x/1.jpg"]),
            (B, vec!["b/2.jpg", "c/2.jpg"]),
        ] {
            assert!(
                paths.iter().any(|p| !gone.contains(p)),
                "{fp} lost every copy"
            );
        }
        assert!(b.items.iter().all(|i| !gone.contains(i.keep_path.as_str())));
    }

    #[test]
    fn plans_save_show_and_drop() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("todo/a.jpg", 10, Some(A)),
                ("Photos/a.jpg", 10, Some(A)),
                ("todo/b.jpg", 5, Some(B)),
            ],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        let id = save(&conn, &b, 1_763_300_000).unwrap();
        assert!(valid_id(&id), "{id}");
        assert!(id.starts_with("20251116-"), "{id}");
        let v = show(&conn, &id, None).unwrap();
        assert_eq!(v.summary.state, "draft");
        assert_eq!(
            v.summary.remove,
            Tally {
                files: 1,
                bytes: 10
            }
        );
        assert_eq!(v.summary.left, Tally { files: 1, bytes: 5 });
        assert_eq!(v.items[0].keep_abs_path, "/elrond/darren/Photos/a.jpg");
        assert_eq!(v.left_by_reason[0].reason, "unique");
        assert_eq!(spec(&conn, &id).unwrap(), b.spec);
        assert_eq!(list(&conn).unwrap().len(), 1);
        set_state(&conn, &id, "applied", 2).unwrap();
        assert!(drop(&conn, &id).is_err());
        set_state(&conn, &id, "draft", 3).unwrap();
        drop(&conn, &id).unwrap();
        assert!(list(&conn).unwrap().is_empty());
        assert!(!valid_id("../x"));
    }

    #[test]
    fn files_an_applied_plan_moved_are_left_out() {
        let mut conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[("todo/a.jpg", 10, Some(A)), ("Photos/a.jpg", 10, Some(A))],
        );
        let built = drain(&conn, &["elrond:darren/todo"]).unwrap();
        let id = save(&conn, &built, 0).unwrap();
        record(
            &mut conn,
            &id,
            &[crate::apply::Outcome {
                seq: 0,
                moved: true,
                reason: None,
            }],
        )
        .unwrap();
        set_state(&conn, &id, "applied", 1).unwrap();
        // Draining Photos now must not count the moved copy in todo as the one that stays.
        let b = drain(&conn, &["elrond:darren/Photos"]).unwrap();
        assert!(b.items.is_empty());
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert_eq!(
            left(&b),
            vec![pair("todo/a.jpg", &format!("already moved by plan {id}"))]
        );
        assert_eq!(summary(&conn, &id).unwrap().in_trash, 10);
        // A different file that later lands at the moved path is a file like any other.
        put(
            &conn,
            "elrond",
            "darren",
            &[("todo/a.jpg", 20, Some(B)), ("Photos/b.jpg", 20, Some(B))],
        );
        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert_eq!(removed(&b), vec![pair("todo/a.jpg", "Photos/b.jpg")]);
    }

    #[test]
    fn folders_that_look_synced_are_refused() {
        let conn = crate::db::open_in_memory().unwrap();
        // 60 photos at the same paths in two volumes: gfiles mirrors suggests them as a mirror.
        let photos: Vec<(String, String)> = (0..60)
            .map(|i| (format!("{i:064x}"), format!("p1:{:064x}", i + 1000)))
            .collect();
        let todo: Vec<(String, i64, Option<&str>)> = photos
            .iter()
            .map(|(n, fp)| (format!("todo/Digikam/{n}.jpg"), 10, Some(fp.as_str())))
            .collect();
        let todo: Vec<(&str, i64, Option<&str>)> =
            todo.iter().map(|(p, s, f)| (p.as_str(), *s, *f)).collect();
        put(&conn, "elrond", "darren", &todo);
        let share: Vec<(String, i64, Option<&str>)> = photos
            .iter()
            .map(|(n, fp)| (format!("{n}.jpg"), 10, Some(fp.as_str())))
            .collect();
        let share: Vec<(&str, i64, Option<&str>)> =
            share.iter().map(|(p, s, f)| (p.as_str(), *s, *f)).collect();
        put(&conn, "elrond", "photo", &share);

        let b = drain(&conn, &["elrond:darren/todo"]).unwrap();
        assert_eq!(b.items.len(), 60);
        let err = check_not_synced(&conn, &b, &[]).unwrap_err().to_string();
        assert!(err.contains("elrond:darren/todo/Digikam"), "{err}");
        assert!(err.contains("not_synced"), "{err}");
        let ok = [folder("elrond:darren/todo/Digikam"), folder("elrond:photo")];
        check_not_synced(&conn, &b, &ok).unwrap();

        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Dropbox/Shared/a.jpg", 10, Some(A)),
                ("Pictures/a.jpg", 10, Some(A)),
            ],
        );
        let b = drain(&conn, &["gaffer:darren/Dropbox"]).unwrap();
        let err = check_not_synced(&conn, &b, &[]).unwrap_err().to_string();
        assert!(err.contains("inside a Dropbox folder"), "{err}");
    }
}
