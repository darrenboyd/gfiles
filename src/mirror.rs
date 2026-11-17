//! Mirrors: folders on different volumes kept in sync by a tool such as Insync or Synology
//! Cloud Sync, declared on the hub as `[[mirror]]`. A mirror is one logical folder: files at
//! the same path inside its folders are one logical file with several replicas. They're shown
//! once, aren't duplicates of each other, and count as one place for backup coverage, since a
//! sync tool copies deletes and damage from one replica to the others.
//!
//! When a command opens the catalog, `load` resolves each mirror folder to a volume and a path
//! prefix in the per-connection table `temp.mirror_folders`. Queries join it through `JOIN` and
//! use `KEY` and `PLACE`, so every interface collapses replicas the same way.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorConfig {
    pub name: String,
    /// Each `machine:volume/path` (the path may be empty for the whole volume).
    pub folders: Vec<String>,
    /// Gitignore-style patterns, relative to the mirror's folders, for files the sync tools
    /// write differently on each side. Health reports leave them out.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// Creates the per-connection tables that mirror-aware queries read. Empty until `load`.
pub(crate) const CREATE_TABLE: &str = "CREATE TEMP TABLE IF NOT EXISTS mirror_folders (
    mirror    TEXT NOT NULL,
    volume_id TEXT NOT NULL,
    prefix    TEXT NOT NULL  -- folder relative to the volume root; '' for the whole volume
);
CREATE TEMP TABLE IF NOT EXISTS mirror_ignores (mirror TEXT NOT NULL, pattern TEXT NOT NULL);
CREATE TEMP TABLE IF NOT EXISTS mirror_warnings (message TEXT NOT NULL);";

/// Joins each entry `e` to the mirror folder holding it, if any (as `mf`).
pub const JOIN: &str = " LEFT JOIN temp.mirror_folders mf ON mf.volume_id = e.volume_id
    AND (mf.prefix = '' OR substr(e.path, 1, length(mf.prefix) + 1) = mf.prefix || '/')";

/// An entry's path inside its mirror folder (needs `JOIN`); NULL outside a mirror.
pub const MIRROR_PATH: &str = "(CASE WHEN mf.mirror IS NULL THEN NULL
    WHEN mf.prefix = '' THEN e.path ELSE substr(e.path, length(mf.prefix) + 2) END)";

/// The logical item an entry belongs to (needs `JOIN`): replicas of a mirror file share one.
pub const KEY: &str = "(CASE WHEN mf.mirror IS NULL THEN 'e:' || e.id
    ELSE 'm:' || mf.mirror || ':' || (CASE WHEN mf.prefix = '' THEN e.path
        ELSE substr(e.path, length(mf.prefix) + 2) END) END)";

/// Where a copy lives for backup coverage (needs `JOIN` and volumes as `v`): its machine, or
/// its mirror as a whole.
pub const PLACE: &str =
    "(CASE WHEN mf.mirror IS NULL THEN 'machine:' || v.machine ELSE 'mirror:' || mf.mirror END)";

/// A parsed `machine:volume/path`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderRef {
    pub machine: String,
    pub volume: String,
    pub path: String,
}

impl FolderRef {
    /// Whether `path` (relative to this folder's volume root) is this folder or inside it.
    pub fn holds(&self, path: &str) -> bool {
        self.path.is_empty()
            || path == self.path
            || path
                .strip_prefix(self.path.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Whether `other` is this folder or inside it.
    pub fn contains(&self, other: &FolderRef) -> bool {
        self.machine == other.machine && self.volume == other.volume && self.holds(&other.path)
    }
}

impl Serialize for FolderRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl std::fmt::Display for FolderRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.machine, self.volume)?;
        if !self.path.is_empty() {
            write!(f, "/{}", self.path)?;
        }
        Ok(())
    }
}

pub fn parse_folder(s: &str) -> Result<FolderRef> {
    let bad = || anyhow::anyhow!("folder {s:?} must look like machine:volume/path");
    let (machine, rest) = s.split_once(':').ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    let (volume, path) = rest.split_once('/').unwrap_or((rest, ""));
    if machine.is_empty() || volume.is_empty() {
        return Err(bad());
    }
    if !path.is_empty()
        && path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        bail!("folder {s:?}: the path must not contain empty, '.' or '..' parts");
    }
    Ok(FolderRef {
        machine: machine.into(),
        volume: volume.into(),
        path: path.into(),
    })
}

/// A mirror's folders that resolved to cataloged volumes, as loaded into
/// `temp.mirror_folders`.
struct Loaded {
    name: String,
    folders: Vec<(FolderRef, String)>, // folder, volume id
    ignore: Vec<String>,
}

/// Resolves the config's mirrors against the catalog and loads them for this connection.
/// A folder on an unknown machine (neither this one, a remote, nor in the catalog) is an
/// error. A folder whose volume isn't in the catalog (perhaps not pulled yet) is left out with
/// a warning, and the mirror's other folders still load: dropping the whole mirror would make
/// its replicas look like independent backups of each other. Returns the warnings, which are
/// also kept for this connection (see `warnings`) so results can carry them.
pub fn load(conn: &Connection, config: &Config) -> Result<Vec<String>> {
    conn.execute_batch(
        "DELETE FROM temp.mirror_folders; DELETE FROM temp.mirror_ignores;
         DELETE FROM temp.mirror_warnings;",
    )?;
    let mut machines: Vec<String> = conn
        .prepare("SELECT DISTINCT machine FROM volumes")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    machines.push(config.machine_name());
    machines.extend(config.remotes.iter().map(|r| r.name.clone()));
    machines.sort();
    machines.dedup();

    let mut loaded: Vec<Loaded> = Vec::new();
    let mut warnings = Vec::new();
    for (i, m) in config.mirrors.iter().enumerate() {
        if config.mirrors[..i].iter().any(|o| o.name == m.name) {
            bail!("two mirrors are named {:?}", m.name);
        }
        if m.folders.len() < 2 {
            bail!("mirror {:?} needs at least two folders", m.name);
        }
        let mut folders = Vec::new();
        let mut missing = Vec::new();
        for f in &m.folders {
            let folder = parse_folder(f)?;
            if !machines.contains(&folder.machine) {
                bail!(
                    "mirror {:?}: unknown machine {:?}; use one of: {}",
                    m.name,
                    folder.machine,
                    machines.join(", ")
                );
            }
            let id: Option<String> = conn
                .query_row(
                    "SELECT id FROM volumes WHERE machine = ?1 AND name = ?2",
                    params![folder.machine, folder.volume],
                    |r| r.get(0),
                )
                .ok();
            match id {
                Some(id) => folders.push((folder, id)),
                None => missing.push(folder),
            }
        }
        for folder in &missing {
            let known: Vec<String> = conn
                .prepare("SELECT name FROM volumes WHERE machine = ?1 ORDER BY name")?
                .query_map([&folder.machine], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            warnings.push(format!(
                "mirror {:?}: folder {folder} left out, no volume {:?} on {} in the catalog \
                 (not pulled yet?); its volumes are: {}",
                m.name,
                folder.volume,
                folder.machine,
                if known.is_empty() {
                    "none yet".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        if folders.is_empty() {
            continue;
        }
        for (i, (a, id)) in folders.iter().enumerate() {
            if folders[..i].iter().any(|(_, other)| other == id) {
                bail!(
                    "mirror {:?}: two folders on volume {}:{}; a mirror's folders must be on different volumes",
                    m.name,
                    a.machine,
                    a.volume
                );
            }
        }
        // A file must belong to one mirror at most.
        for other in &loaded {
            for (a, id) in &folders {
                for (b, other_id) in &other.folders {
                    if id == other_id && (nests(&a.path, &b.path) || nests(&b.path, &a.path)) {
                        bail!(
                            "mirrors {:?} and {:?} overlap at {a} and {b}",
                            m.name,
                            other.name
                        );
                    }
                }
            }
        }
        loaded.push(Loaded {
            name: m.name.clone(),
            folders,
            ignore: m.ignore.clone(),
        });
    }
    let mut insert = conn.prepare(
        "INSERT INTO temp.mirror_folders (mirror, volume_id, prefix) VALUES (?1, ?2, ?3)",
    )?;
    let mut insert_ignore =
        conn.prepare("INSERT INTO temp.mirror_ignores (mirror, pattern) VALUES (?1, ?2)")?;
    for m in &loaded {
        ignores(&m.ignore)?; // a bad pattern is a config error now, not a failed report later
        for (folder, id) in &m.folders {
            insert.execute(params![m.name, id, folder.path])?;
        }
        for pattern in &m.ignore {
            insert_ignore.execute(params![m.name, pattern])?;
        }
    }
    let mut insert_warning =
        conn.prepare("INSERT INTO temp.mirror_warnings (message) VALUES (?1)")?;
    for w in &warnings {
        insert_warning.execute([w])?;
    }
    Ok(warnings)
}

/// The warnings from this connection's last `load`: mirror folders left out because their
/// volume isn't in the catalog. Results that depend on mirrors carry them.
pub fn warnings(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT message FROM temp.mirror_warnings ORDER BY rowid")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Whether relative folder `inner` is `outer` or inside it ('' is the whole volume).
fn nests(inner: &str, outer: &str) -> bool {
    outer.is_empty()
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Extensions sync tools give the stand-ins they write for Google Docs, Sheets and other
/// Google-native files: Insync (`.gddoc` ...) and Google Drive or Synology Cloud Sync
/// (`.gdoc` ...). Each tool writes its own, so they never match across replicas.
pub const PLACEHOLDER_EXTS: &[&str] = &[
    "gddoc",
    "gdsheet",
    "gdslides",
    "gdmap",
    "gdform",
    "gddraw",
    "gdsite",
    "gdjam",
    "gdlink",
    "gdoc",
    "gsheet",
    "gslides",
    "gmap",
    "gform",
    "gdraw",
    "gsite",
    "gjam",
    "glink",
    "gshortcut",
];

pub fn is_placeholder(ext: Option<&str>) -> bool {
    ext.is_some_and(|e| PLACEHOLDER_EXTS.contains(&e))
}

/// Conflict copies sync tools make when both sides changed: Synology Cloud Sync
/// (`name_DiskStation_Sep-20-1538-2021_Conflict.ext`) and Dropbox-style
/// (`name (Someone's conflicted copy 2010-12-19).ext`).
pub fn is_conflict(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["_conflict", "conflicted copy"]
        .iter()
        .any(|m| lower.contains(m))
}

/// Cloud Sync's mark on a name that had a `/` in Google Drive (where Insync writes `_` alone).
const SLASH_MARK: &str = "(slash conflict)";

/// A mirror path as compared across replicas: sync tools' renaming marks removed.
fn comparable(path: &str) -> String {
    path.replace(SLASH_MARK, "")
}

fn ignores(patterns: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new("/");
    for p in patterns {
        builder
            .add_line(None, p)
            .with_context(|| format!("bad mirror ignore pattern {p:?}"))?;
    }
    Ok(builder.build()?)
}

/// A capped list with the full count, so a cut-off list says so.
#[derive(Debug, Default, Serialize)]
pub struct Listed {
    pub total: usize,
    pub items: Vec<String>,
}

impl Listed {
    fn from(mut items: Vec<String>, limit: usize) -> Listed {
        items.sort();
        let total = items.len();
        items.truncate(limit);
        Listed { total, items }
    }
}

#[derive(Debug, Serialize)]
pub struct Replica {
    /// `machine:volume/path`, as in the config.
    pub folder: String,
    pub abs_folder: String,
    pub files: usize,
    /// The newest file's mtime: replicas far apart here suggest the sync has stopped.
    pub newest_mtime: Option<i64>,
    /// When the replica's volume was last scanned.
    pub last_scan_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct OnlyIn {
    /// The replica (`machine:volume/path`) that has these files and another lacks.
    pub replica: String,
    pub files: Listed,
}

/// How well a mirror's replicas agree.
#[derive(Debug, Serialize)]
pub struct Health {
    pub mirror: String,
    pub replicas: Vec<Replica>,
    /// Files that some replicas lack, under each replica that has them.
    pub only_in: Vec<OnlyIn>,
    /// Same content at different paths: the sync tools named the file differently.
    pub named_differently: Listed,
    /// Same path, different content.
    pub differing: Listed,
    /// Conflict copies a sync tool made when both sides changed.
    pub conflicts: Listed,
    /// Files that are missing from, or differ in, another replica but changed after that
    /// replica was last scanned (as `replica/path`): the catalog can't tell yet whether the
    /// sync tool copied them. A rescan of the other replica settles it. Not counted as drift.
    pub not_yet_seen: Listed,
    /// Google-native stand-ins (and files matching the mirror's ignore patterns), left out.
    pub skipped_placeholders: usize,
    pub in_sync: bool,
}

/// One file of a mirror, as seen in one replica.
struct Seen {
    replica: usize,
    path: String,
    size: i64,
    mtime: i64,
    fingerprint: Option<String>,
}

/// Health of every loaded mirror. Lists are capped at `limit` items each, with totals.
pub fn health(conn: &Connection, limit: usize) -> Result<Vec<Health>> {
    // The replicas of each mirror, in config order.
    let mut mirrors: BTreeMap<String, Vec<(String, Replica)>> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT mf.mirror, mf.volume_id, mf.prefix, v.machine, v.name, v.root, v.last_scan_at
         FROM temp.mirror_folders mf JOIN volumes v ON v.id = mf.volume_id ORDER BY mf.rowid",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let prefix: String = r.get(2)?;
        let root: String = r.get(5)?;
        let folder = FolderRef {
            machine: r.get(3)?,
            volume: r.get(4)?,
            path: prefix.clone(),
        };
        mirrors.entry(r.get(0)?).or_default().push((
            r.get(1)?,
            Replica {
                folder: folder.to_string(),
                abs_folder: crate::search::join_root(&root, &prefix),
                files: 0,
                newest_mtime: None,
                last_scan_at: r.get(6)?,
            },
        ));
    }

    let mut out = Vec::new();
    for (name, replicas) in mirrors {
        let patterns: Vec<String> = conn
            .prepare("SELECT pattern FROM temp.mirror_ignores WHERE mirror = ?1")?
            .query_map([&name], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let ignore = ignores(&patterns)?;
        let (ids, mut replicas): (Vec<String>, Vec<Replica>) = replicas.into_iter().unzip();

        let mut by_path: BTreeMap<String, Vec<Seen>> = BTreeMap::new();
        let mut conflicts = Vec::new();
        let mut skipped = 0;
        let sql = format!(
            "SELECT e.volume_id, {MIRROR_PATH}, e.name, e.ext, e.size, e.mtime, e.fingerprint
             FROM entries e{JOIN} WHERE mf.mirror = ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&name])?;
        while let Some(r) = rows.next()? {
            let volume_id: String = r.get(0)?;
            let Some(replica) = ids.iter().position(|id| *id == volume_id) else {
                continue;
            };
            let path: String = r.get(1)?;
            let file_name: String = r.get(2)?;
            let ext: Option<String> = r.get(3)?;
            let mtime: i64 = r.get(5)?;
            let rep = &mut replicas[replica];
            rep.files += 1;
            rep.newest_mtime = rep.newest_mtime.max(Some(mtime));
            if is_placeholder(ext.as_deref())
                || ignore
                    .matched_path_or_any_parents(Path::new("/").join(&path), false)
                    .is_ignore()
            {
                skipped += 1;
                continue;
            }
            if is_conflict(&file_name) {
                conflicts.push(format!("{}/{path}", rep.folder));
                continue;
            }
            let path = comparable(&path);
            by_path.entry(path.clone()).or_default().push(Seen {
                replica,
                path,
                size: r.get(4)?,
                mtime,
                fingerprint: r.get(6)?,
            });
        }

        let n = replicas.len();
        // Whether a file that changed at `mtime` can't have reached replica `i` in the catalog
        // yet: that replica was last scanned before the change (or never).
        let unseen_by = |i: usize, mtime: i64| replicas[i].last_scan_at.is_none_or(|t| t < mtime);
        let mut differing = Vec::new();
        let mut not_yet_seen = Vec::new();
        let mut one_sided: Vec<Seen> = Vec::new();
        for (path, seen) in by_path {
            let present: HashSet<usize> = seen.iter().map(|s| s.replica).collect();
            let first = &seen[0];
            if seen.iter().any(|s| {
                s.size != first.size
                    || (s.fingerprint.is_some()
                        && first.fingerprint.is_some()
                        && s.fingerprint != first.fingerprint)
            }) {
                let newest = seen.iter().max_by_key(|s| s.mtime).unwrap();
                if seen
                    .iter()
                    .any(|s| s.replica != newest.replica && unseen_by(s.replica, newest.mtime))
                {
                    not_yet_seen.push(format!("{}/{}", replicas[newest.replica].folder, path));
                } else {
                    differing.push(path);
                }
                continue;
            }
            if present.len() < n {
                for s in seen {
                    let missing_from = (0..n).filter(|i| !present.contains(i));
                    if missing_from.clone().any(|i| unseen_by(i, s.mtime)) {
                        not_yet_seen.push(format!("{}/{}", replicas[s.replica].folder, s.path));
                    } else {
                        one_sided.push(s);
                    }
                }
            }
        }

        // Pair one-sided files across replicas: same content, or same size with one path being
        // the other plus an extension (one tool added `.pdf`, the other didn't).
        // The path without its extension, taken from the path itself (the stored ext is
        // lowercased, and a pulled one is only checked against the name, so its length can't
        // be trusted for slicing).
        let stem = |s: &Seen| match Path::new(&s.path).extension().and_then(|e| e.to_str()) {
            Some(ext) => s.path[..s.path.len() - ext.len() - 1].to_string(),
            None => s.path.clone(),
        };
        let mut paired = vec![false; one_sided.len()];
        let mut named_differently = Vec::new();
        let mut by_content: HashMap<(i64, &str), Vec<usize>> = HashMap::new();
        let mut by_stem: HashMap<(i64, String), Vec<usize>> = HashMap::new();
        for (i, s) in one_sided.iter().enumerate() {
            if let Some(fp) = &s.fingerprint {
                by_content.entry((s.size, fp.as_str())).or_default().push(i);
            }
            by_stem.entry((s.size, stem(s))).or_default().push(i);
        }
        for i in 0..one_sided.len() {
            if paired[i] {
                continue;
            }
            let s = &one_sided[i];
            let candidates = s
                .fingerprint
                .as_deref()
                .and_then(|fp| by_content.get(&(s.size, fp)))
                .into_iter()
                .flatten()
                .chain(by_stem.get(&(s.size, stem(s))).into_iter().flatten());
            let mate = candidates
                .copied()
                .find(|&j| !paired[j] && one_sided[j].replica != s.replica);
            if let Some(j) = mate {
                paired[i] = true;
                paired[j] = true;
                let o = &one_sided[j];
                named_differently.push(format!(
                    "{}/{} = {}/{}",
                    replicas[s.replica].folder, s.path, replicas[o.replica].folder, o.path
                ));
            }
        }
        let mut only: Vec<Vec<String>> = vec![Vec::new(); n];
        for (s, _) in one_sided.iter().zip(&paired).filter(|(_, p)| !**p) {
            only[s.replica].push(s.path.clone());
        }
        let only_in: Vec<OnlyIn> = only
            .into_iter()
            .enumerate()
            .filter(|(_, files)| !files.is_empty())
            .map(|(i, files)| OnlyIn {
                replica: replicas[i].folder.clone(),
                files: Listed::from(files, limit),
            })
            .collect();
        let in_sync = only_in.is_empty() && differing.is_empty() && conflicts.is_empty();
        out.push(Health {
            mirror: name,
            replicas,
            only_in,
            named_differently: Listed::from(named_differently, limit),
            differing: Listed::from(differing, limit),
            conflicts: Listed::from(conflicts, limit),
            not_yet_seen: Listed::from(not_yet_seen, limit),
            skipped_placeholders: skipped,
            in_sync,
        });
    }
    Ok(out)
}

/// Folder pairs that look like an unconfigured mirror.
#[derive(Debug, Serialize, PartialEq)]
pub struct Suggestion {
    pub folders: [String; 2],
    /// Files present in both, at the same relative path with the same content.
    pub matching: usize,
    /// Files in each folder.
    pub files: [usize; 2],
    /// `matching` over the larger folder's file count.
    pub ratio: f64,
    /// A `[[mirror]]` section to paste into the hub's config.
    pub config: String,
}

/// Least files two folders must share to be suggested, and the share of the larger folder.
const SUGGEST_MIN_FILES: usize = 50;
const SUGGEST_MIN_RATIO: f64 = 0.9;
/// Candidate folder pairs checked in full, most-shared first.
const SUGGEST_CANDIDATES: usize = 40;

/// Suggests folder pairs, outside any configured mirror, whose files mostly match: candidate
/// pairs come from duplicate files (the folders above two copies of a file, level by level),
/// and the best-voted are compared in full.
pub fn suggestions(conn: &Connection) -> Result<Vec<Suggestion>> {
    // Every fingerprinted file whose content is also in another volume, outside mirrors.
    let sql = format!(
        "SELECT e.volume_id, e.path, e.size, e.fingerprint
         FROM entries e{JOIN}
         WHERE mf.mirror IS NULL AND e.size > 0 AND e.fingerprint LIKE 'p1:%'
           AND (e.size, e.fingerprint) IN (
               SELECT size, fingerprint FROM entries
               WHERE size > 0 AND fingerprint LIKE 'p1:%'
               GROUP BY size, fingerprint HAVING count(DISTINCT volume_id) > 1)
         ORDER BY e.size, e.fingerprint"
    );
    let rows: Vec<(String, String, i64, String)> = conn
        .prepare(&sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    type Side = (String, String); // volume id, folder
    let mut votes: HashMap<(Side, Side), usize> = HashMap::new();
    for group in rows.chunk_by(|a, b| (a.2, &a.3) == (b.2, &b.3)) {
        let group = &group[..group.len().min(8)];
        for (i, a) in group.iter().enumerate() {
            for b in &group[i + 1..] {
                if a.0 == b.0 {
                    continue;
                }
                let pa: Vec<&str> = a.1.split('/').collect();
                let pb: Vec<&str> = b.1.split('/').collect();
                // Every level of the shared tail, starting with the file name itself.
                let mut k = 0;
                while k < pa.len().min(pb.len()) && pa[pa.len() - 1 - k] == pb[pb.len() - 1 - k] {
                    k += 1;
                    let x = (a.0.clone(), pa[..pa.len() - k].join("/"));
                    let y = (b.0.clone(), pb[..pb.len() - k].join("/"));
                    let key = if x < y { (x, y) } else { (y, x) };
                    *votes.entry(key).or_default() += 1;
                }
            }
        }
    }
    let mut candidates: Vec<((Side, Side), usize)> = votes
        .into_iter()
        .filter(|(_, v)| *v >= SUGGEST_MIN_FILES)
        .collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    candidates.truncate(SUGGEST_CANDIDATES);

    let mut accepted: Vec<((Side, Side), Suggestion)> = Vec::new();
    for ((a, b), votes) in candidates {
        let (fa, fb) = (files_under(conn, &a)?, files_under(conn, &b)?);
        let larger = fa.len().max(fb.len());
        if (votes as f64) < larger as f64 * SUGGEST_MIN_RATIO / 2.0 {
            continue;
        }
        let matching = fa
            .iter()
            .filter(|(rel, (size, fp))| {
                fb.get(*rel)
                    .is_some_and(|(s, f)| s == size && (fp.is_none() || f.is_none() || f == fp))
            })
            .count();
        let ratio = matching as f64 / larger as f64;
        if matching < SUGGEST_MIN_FILES || ratio < SUGGEST_MIN_RATIO {
            continue;
        }
        let names = [folder_name(conn, &a)?, folder_name(conn, &b)?];
        let name =
            a.1.rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or("mirror");
        let config = format!(
            "[[mirror]]\nname = {:?}\nfolders = [{:?}, {:?}]\n",
            name.to_lowercase(),
            names[0],
            names[1]
        );
        accepted.push((
            (a, b),
            Suggestion {
                folders: names,
                matching,
                files: [fa.len(), fb.len()],
                ratio: (ratio * 1000.0).round() / 1000.0,
                config,
            },
        ));
    }
    // Keep the outermost of nested suggestions (Drive, not also Drive/Photos).
    accepted.sort_by_key(|((a, b), _)| a.1.len() + b.1.len());
    let mut kept: Vec<((Side, Side), Suggestion)> = Vec::new();
    for (sides, s) in accepted {
        let inside = kept.iter().any(|((ka, kb), _)| {
            ka.0 == sides.0.0
                && kb.0 == sides.1.0
                && nests(&sides.0.1, &ka.1)
                && nests(&sides.1.1, &kb.1)
        });
        if !inside {
            kept.push((sides, s));
        }
    }
    let mut out: Vec<Suggestion> = kept.into_iter().map(|(_, s)| s).collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.matching));
    Ok(out)
}

/// The files under a folder of a volume, keyed by path relative to it.
fn files_under(
    conn: &Connection,
    (volume_id, folder): &(String, String),
) -> Result<HashMap<String, (i64, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT path, size, fingerprint FROM entries
         WHERE volume_id = ?1 AND (?2 = '' OR (path > ?2 || '/' AND path < ?2 || '0'))",
    )?;
    let skip = if folder.is_empty() {
        0
    } else {
        folder.len() + 1
    };
    let files = stmt
        .query_map(params![volume_id, folder], |r| {
            let path: String = r.get(0)?;
            Ok((path[skip..].to_string(), (r.get(1)?, r.get(2)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(files)
}

fn folder_name(conn: &Connection, (volume_id, folder): &(String, String)) -> Result<String> {
    let (machine, volume): (String, String) = conn.query_row(
        "SELECT machine, name FROM volumes WHERE id = ?1",
        [volume_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(FolderRef {
        machine,
        volume,
        path: folder.clone(),
    }
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entry, Volume, VolumeWriter};

    /// A hub catalog with gaffer:darren holding `gaffer` and elrond:darren holding `elrond`.
    fn catalog(gaffer: &[&str], elrond: &[&str]) -> Connection {
        let conn = crate::db::open_in_memory().unwrap();
        for (machine, paths) in [("gaffer", gaffer), ("elrond", elrond)] {
            let vol = Volume {
                id: format!("{machine}-darren"),
                name: "darren".into(),
                machine: machine.into(),
                root: format!("/{machine}"),
                last_scan_at: None,
            };
            let mut w = VolumeWriter::begin(&conn, &vol).unwrap();
            for p in paths {
                w.put(&Entry {
                    path: p.to_string(),
                    name: p.rsplit('/').next().unwrap().into(),
                    ext: None,
                    kind: "other".into(),
                    size: 1,
                    mtime: 0,
                    fingerprint: None,
                    content_hash: None,
                })
                .unwrap();
            }
            w.finish(0, &[]).unwrap();
        }
        conn
    }

    fn config(mirrors: &str) -> Config {
        toml::from_str(&format!("machine = \"gaffer\"\n{mirrors}")).unwrap()
    }

    const DRIVE: &str = "[[mirror]]\nname = \"drive\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:darren/Drive\"]\n";

    fn keys(conn: &Connection) -> Vec<(String, String, String)> {
        let sql = format!(
            "SELECT v.machine, e.path, {KEY} FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
             ORDER BY e.path, v.machine"
        );
        conn.prepare(&sql)
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn replicas_share_a_key_and_nothing_else_does() {
        let conn = catalog(
            &["Drive/a.pdf", "Drive Old/a.pdf", "Drive"],
            &["Drive/a.pdf", "Drive/sub/b.pdf"],
        );
        assert!(load(&conn, &config(DRIVE)).unwrap().is_empty());
        let keys = keys(&conn);
        let key = |machine: &str, path: &str| {
            keys.iter()
                .find(|k| k.0 == machine && k.1 == path)
                .unwrap()
                .2
                .clone()
        };
        assert_eq!(key("gaffer", "Drive/a.pdf"), "m:drive:a.pdf");
        assert_eq!(key("elrond", "Drive/a.pdf"), "m:drive:a.pdf");
        assert_eq!(key("elrond", "Drive/sub/b.pdf"), "m:drive:sub/b.pdf");
        // Whole path components only: "Drive Old" and a file named "Drive" aren't in it.
        assert!(key("gaffer", "Drive Old/a.pdf").starts_with("e:"));
        assert!(key("gaffer", "Drive").starts_with("e:"));
    }

    #[test]
    fn bad_mirrors_are_config_errors() {
        let conn = catalog(&["Drive/a"], &["Drive/a"]);
        for (mirrors, msg) in [
            (
                "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\"]\n",
                "at least two",
            ),
            (
                "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"Drive\"]\n",
                "machine:volume/path",
            ),
            (
                "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"gaffer:darren/Backup\"]\n",
                "different volumes",
            ),
            (
                "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"bilbo:darren/Drive\"]\n",
                "unknown machine \"bilbo\"; use one of: elrond, gaffer",
            ),
            (
                "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/x/../y\", \"elrond:darren/Drive\"]\n",
                "'..'",
            ),
        ] {
            let err = load(&conn, &config(mirrors)).unwrap_err().to_string();
            assert!(err.contains(msg), "{mirrors}: {err}");
        }
        let overlap = format!(
            "{DRIVE}[[mirror]]\nname = \"photos\"\nfolders = [\"gaffer:darren/Drive/Photos\", \"elrond:darren/Photos\"]\n"
        );
        let err = load(&conn, &config(&overlap)).unwrap_err().to_string();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn a_volume_not_pulled_yet_is_left_out_with_a_warning() {
        let conn = catalog(&["Drive/a"], &["Drive/a"]);
        let mirrors = "[[remote]]\nname = \"nas2\"\n[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:darren/Drive\", \"nas2:darren/Drive\"]\n";
        let warnings = load(&conn, &config(mirrors)).unwrap();
        assert!(
            warnings[0].contains("no volume \"darren\" on nas2"),
            "{warnings:?}"
        );
        assert_eq!(super::warnings(&conn).unwrap(), warnings);
        // The folders that did resolve still make one logical file.
        let keys = keys(&conn);
        assert!(keys.iter().all(|k| k.2 == "m:m:a"), "{keys:?}");

        let mirrors = "[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:Media/Drive\"]\n";
        let warnings = load(&conn, &config(mirrors)).unwrap();
        assert!(
            warnings[0].contains("its volumes are: darren"),
            "{warnings:?}"
        );
        // Reloading replaces the old warnings.
        assert_eq!(super::warnings(&conn).unwrap(), warnings);
    }

    /// Writes (path, size, fingerprint) entries into machine:volume, replacing what was there.
    fn put(
        conn: &Connection,
        machine: &str,
        volume: &str,
        entries: &[(&str, i64, Option<String>)],
    ) {
        let entries: Vec<_> = entries
            .iter()
            .map(|(p, s, f)| (*p, *s, 0, f.clone()))
            .collect();
        put_at(conn, machine, volume, &entries, 0);
    }

    /// Like `put`, with each entry's mtime and the time of the scan.
    fn put_at(
        conn: &Connection,
        machine: &str,
        volume: &str,
        entries: &[(&str, i64, i64, Option<String>)],
        scanned_at: i64,
    ) {
        let vol = Volume {
            id: format!("{machine}-{volume}"),
            name: volume.into(),
            machine: machine.into(),
            root: format!("/{machine}/{volume}"),
            last_scan_at: Some(100),
        };
        let mut w = VolumeWriter::begin(conn, &vol).unwrap();
        for (path, size, mtime, fp) in entries {
            let name = path.rsplit('/').next().unwrap();
            w.put(&Entry {
                path: path.to_string(),
                name: name.into(),
                ext: name.rsplit_once('.').map(|(_, e)| e.to_lowercase()),
                kind: "other".into(),
                size: *size,
                mtime: *mtime,
                fingerprint: fp.clone(),
                content_hash: None,
            })
            .unwrap();
        }
        w.finish(scanned_at, &[]).unwrap();
    }

    fn fp(n: u32) -> Option<String> {
        Some(format!("p1:{n:064x}"))
    }

    #[test]
    fn health_reports_drift_and_skips_placeholders() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Drive/same.pdf", 10, fp(1)),
                ("Drive/Notes.gddoc", 209, None),
                ("Drive/only-here.jpg", 5, fp(2)),
                ("Drive/changed.txt", 3, fp(3)),
                ("Drive/OP103 Vapor Agreement.pdf", 691, fp(4)),
                ("Drive/Bills _ Receipts/june.pdf", 8, fp(7)),
                ("Drive/é.İ", 4, fp(8)),
            ],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/same.pdf", 10, fp(1)),
                ("Drive/Notes.gmap", 79, None),
                ("Drive/changed.txt", 3, fp(5)),
                ("Drive/OP103 Vapor Agreement", 691, None),
                (
                    "Drive/Budget_DiskStation_Sep-20-1538-2021_Conflict.xlsx",
                    7,
                    fp(6),
                ),
                ("Drive/.sync/state", 1, None),
                ("Drive/Bills _ Receipts(slash conflict)/june.pdf", 8, fp(7)),
            ],
        );
        let config = config(&format!("{DRIVE}ignore = [\".sync/\"]\n"));
        load(&conn, &config).unwrap();
        let h = &health(&conn, 50).unwrap()[0];
        assert_eq!(h.mirror, "drive");
        assert!(!h.in_sync);
        assert_eq!(
            h.skipped_placeholders, 3,
            "two stand-ins and one ignored file"
        );
        assert_eq!(h.only_in.len(), 1);
        assert_eq!(h.only_in[0].replica, "gaffer:darren/Drive");
        assert_eq!(h.only_in[0].files.items, vec!["only-here.jpg", "é.İ"]);
        assert_eq!(h.differing.items, vec!["changed.txt"]);
        assert_eq!(
            h.conflicts.items,
            vec!["elrond:darren/Drive/Budget_DiskStation_Sep-20-1538-2021_Conflict.xlsx"]
        );
        assert_eq!(
            h.named_differently.items,
            vec![
                "elrond:darren/Drive/OP103 Vapor Agreement = gaffer:darren/Drive/OP103 Vapor Agreement.pdf"
            ]
        );
        assert_eq!(h.replicas[0].files, 7);
        assert_eq!(h.replicas[0].last_scan_at, Some(0));
    }

    #[test]
    fn changes_after_the_other_replicas_scan_are_not_drift() {
        let conn = crate::db::open_in_memory().unwrap();
        // elrond was scanned at 1000; gaffer at 2000, after new.md appeared and notes.md changed.
        put_at(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/notes.md", 3, 500, fp(1)),
                ("Drive/old.md", 3, 500, fp(2)),
            ],
            1000,
        );
        put_at(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Drive/notes.md", 4, 1500, fp(3)),
                ("Drive/new.md", 3, 1500, fp(4)),
                ("Drive/stuck.md", 3, 900, fp(5)),
            ],
            2000,
        );
        load(&conn, &config(DRIVE)).unwrap();
        let h = &health(&conn, 50).unwrap()[0];
        assert_eq!(
            h.not_yet_seen.items,
            vec!["gaffer:darren/Drive/new.md", "gaffer:darren/Drive/notes.md"]
        );
        assert_eq!(h.differing.total, 0);
        // Older than elrond's scan, so elrond really lacks it; old.md really left gaffer.
        assert_eq!(h.only_in.len(), 2);
        assert!(!h.in_sync);
    }

    #[test]
    fn a_mirror_in_step_is_in_sync() {
        let conn = crate::db::open_in_memory().unwrap();
        put(&conn, "gaffer", "darren", &[("Drive/a", 1, fp(1))]);
        put(&conn, "elrond", "darren", &[("Drive/a", 1, fp(1))]);
        load(&conn, &config(DRIVE)).unwrap();
        assert!(health(&conn, 50).unwrap()[0].in_sync);
    }

    /// `n` files `name{i}` under `folder`, with fingerprints `fp(offset + i)`.
    fn files(folder: &str, n: u32, offset: u32) -> Vec<(String, i64, Option<String>)> {
        (0..n)
            .map(|i| {
                (
                    format!("{folder}/sub{}/f{i}.jpg", i % 3),
                    100 + i as i64,
                    fp(offset + i),
                )
            })
            .collect()
    }

    fn put_owned(
        conn: &Connection,
        machine: &str,
        volume: &str,
        list: &[(String, i64, Option<String>)],
    ) {
        let borrowed: Vec<(&str, i64, Option<String>)> = list
            .iter()
            .map(|(p, s, f)| (p.as_str(), *s, f.clone()))
            .collect();
        put(conn, machine, volume, &borrowed);
    }

    #[test]
    fn suggests_folders_that_match() {
        let conn = crate::db::open_in_memory().unwrap();
        // 60 files each side, 58 the same; plus unrelated files and a small shared folder.
        let mut gaffer = files("Drive", 58, 0);
        gaffer.extend(files("Drive/extra", 2, 1000));
        gaffer.extend(files("Pictures", 10, 2000));
        gaffer.extend(files("Junk", 30, 3000));
        let mut elrond = files("Sync/Drive", 58, 0);
        elrond.extend(files("Sync/Drive/other", 2, 4000));
        elrond.extend(files("Photos", 10, 2000));
        put_owned(&conn, "gaffer", "darren", &gaffer);
        put_owned(&conn, "elrond", "darren", &elrond);
        load(&conn, &config("")).unwrap();

        let s = suggestions(&conn).unwrap();
        assert_eq!(s.len(), 1, "{s:?}");
        let mut folders = s[0].folders.to_vec();
        folders.sort();
        assert_eq!(
            folders,
            vec!["elrond:darren/Sync/Drive", "gaffer:darren/Drive"]
        );
        assert_eq!((s[0].matching, s[0].files), (58, [60, 60]));
        assert!((s[0].ratio - 0.967).abs() < 0.001);
        assert!(s[0].config.contains("[[mirror]]"));

        // Once configured, it isn't suggested again.
        let configured = "[[mirror]]\nname = \"drive\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:darren/Sync/Drive\"]\n";
        load(&conn, &config(configured)).unwrap();
        assert!(suggestions(&conn).unwrap().is_empty());
    }

    #[test]
    fn placeholders_and_conflicts() {
        assert!(is_placeholder(Some("gddoc")));
        assert!(is_placeholder(Some("gmap")));
        assert!(!is_placeholder(Some("pdf")));
        assert!(!is_placeholder(None));
        assert!(is_conflict(
            "Aug 21 Open Houses_DiskStation_Sep-20-1538-2021_Conflict.gmap"
        ));
        assert!(is_conflict(
            "grid (Shamino MP's conflicted copy 2010-12-19).png"
        ));
        assert!(!is_conflict("Conflict Resolution.pdf"));
    }
}
