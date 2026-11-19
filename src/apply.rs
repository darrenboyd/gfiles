//! Carrying out plans (see `plan`) on the machine that holds the files: moving files to the
//! trash or to a new place, undoing that, and emptying the trash. This is the only code in
//! gfiles that writes under a volume root, and it only:
//!
//! - creates and writes inside the volume's `.gfiles-trash` folder,
//! - renames a file into it, or back out of it on undo,
//! - moves a file to its destination (and back on undo), creating the destination's missing
//!   folders and removing them on undo once empty,
//! - recreates a restored file's missing parent folders on undo,
//! - deletes a plan's files from the trash when emptying it.
//!
//! A plan's trash is `<volume root>/.gfiles-trash/<plan id>/`: `files/` holds each removed
//! file at its path relative to the volume root, and `journal.jsonl` records each change
//! (with the copy that stays, or where a file went). Renames never replace anything.
//!
//! A removal checks that the file and the copy that stays are unchanged since the scan (size
//! and mtime) and aren't the same file, then compares their full content, unless the list
//! that made the plan vouched for the pair (their bytes may differ). Any doubt skips the file.
//! The full hashes it reads of files that stay (kept copies, and files whose content differs)
//! go into this machine's catalog afterwards (`record_learned`), so the catalog stops matching
//! files apply found different. So does each file it moves (and each file undo puts back):
//! the catalog records the fingerprint and full hash the file had, and the next scan of its
//! new path gives them to the new entry, so a move doesn't cost a file its hashes. All of this
//! stays on the machine that ran the plan, even when a run stops early; the hub's next pull
//! brings it.
//!
//! A move checks the file is unchanged and nothing is at its destination, then renames it.
//! Across filesystems a rename fails, so it copies instead: to a temporary name next to the
//! destination, synced, read back from disk and compared by full hash, renamed into place,
//! and then the original goes to the trash, where undo and empty find it. A copy that doesn't
//! match is removed and the file skipped. Undo puts a copied file's original back and removes
//! the copy only if the copy is unchanged; empty deletes the original only while the copy
//! still has the journaled hash.
//!
//! Re-running a plan resumes it: a file already in the trash, or already at its destination,
//! counts as done.
//!
//! The hub talks to another machine's executor through `gfiles plan exec` over SSH, a
//! newline-delimited JSON exchange:
//!
//! ```text
//! machine → hub   {"gfiles_actions":2,"machine":"elrond"}       before reading anything
//! hub → machine   {"gfiles_actions":2,"op":"apply","plan":"<id>","volumes":[..],"items":N}
//! hub → machine   {"seq":0,"action":"remove","volume_id":..,"path":..,"keep":{..}}
//!                 {"seq":1,"action":"move",..,"dest":{"volume_id":..,"path":..}}
//!                                                                × N (apply only), then EOF
//! machine → hub   {"item":{"seq":0,"moved":true}}                × N, as each is done
//! machine → hub   {"done":{...}}                                 the totals
//! machine → hub   {"error":"..."}                                instead, when it refuses
//! ```
//!
//! The machine reads the whole request before working, and the hub sends it all before
//! reading results, so a large plan can't deadlock on full pipes. A request with fewer item
//! lines than `items` (a hub that died while sending) is refused. `volumes` names the plan's
//! volumes that may hold its files, so undo and empty notice one that isn't there. Each outcome
//! is sent as soon as it's decided, so a hub that has gone away stops the run at the next file.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{fingerprint, plan, platform, store};

/// Bump when the exchange changes incompatibly; both machines must then be updated together.
/// Separate from the catalog stream's `sync::PROTOCOL`.
pub const PROTOCOL: u32 = 2;

/// The trash folder at the top of each volume. Scans always skip it.
pub const TRASH: &str = ".gfiles-trash";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Apply,
    Undo,
    Empty,
}

/// What an item does with its file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Move it to the trash, keeping another copy.
    #[default]
    Remove,
    /// Move it to another path on this machine.
    Move,
}

/// A file to remove (move to the trash, with the copy that must stay) or to move elsewhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub seq: i64,
    pub action: Action,
    pub volume_id: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    /// A removal whose content the list's producer vouches for: it may differ from the kept
    /// copy's, so apply only checks that both are unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vouched: bool,
    /// A removal's copy that must stay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Kept>,
    /// A move's destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<Dest>,
}

/// The copy that stays, as the catalog saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub volume_id: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
}

/// Where a move puts its file: a path that must not exist yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dest {
    pub volume_id: String,
    pub path: String,
}

/// What happened to one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub seq: i64,
    pub moved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Totals of a run, for every op.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub moved: u64,
    pub skipped: u64,
    pub restored: u64,
    pub deleted: u64,
    pub deleted_bytes: u64,
    /// Files (or whole volumes' trash folders) left in the trash by undo or empty, each with
    /// why.
    pub left_in_trash: Vec<String>,
}

/// What a plan run learned for this machine's catalog (see `record_learned`). It stays on
/// the machine that ran the plan, never sent to the hub: the hub's next pull brings it.
#[derive(Debug, Default, Clone)]
pub struct Learned {
    /// Full hashes apply read of files that stay (kept copies, and files it skipped).
    pub hashes: Vec<store::Hashed>,
    /// Files moved to a new path (by apply's moves, and back by undo), so the next scan
    /// gives them the fingerprint and hash they had (`store::record_carried`).
    pub carried: Vec<store::Carried>,
}

impl Learned {
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty() && self.carried.is_empty()
    }
}

/// What the catalog knew of each file an apply moves, by (volume id, path): its fingerprint
/// and full hash.
pub type Known = HashMap<(String, String), (Option<String>, Option<String>)>;

/// Reads what the catalog knows of the files `items` move, before any of them moves, so a scan
/// that runs alongside the apply (and forgets their old entries) can't take it away. A catalog
/// that can't be read only means less is carried.
pub fn known_before(conn: &Connection, items: &[Item]) -> Known {
    let mut known = Known::new();
    for i in items.iter().filter(|i| i.action == Action::Move) {
        match store::known_content(conn, &i.volume_id, &i.path, i.size, i.mtime) {
            Ok(content) => {
                known.insert((i.volume_id.clone(), i.path.clone()), content);
            }
            Err(e) => {
                eprintln!("warning: couldn't read the catalog before moving files: {e:#}");
                break;
            }
        }
    }
    known
}

#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    gfiles_actions: u32,
    machine: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    gfiles_actions: u32,
    op: Op,
    plan: String,
    /// The plan's volumes that may hold its files in the trash.
    volumes: Vec<String>,
    /// How many item lines follow.
    items: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Item(Outcome),
    Done(Done),
    Error(String),
}

/// Holds the per-machine lock that keeps applies, undos and empties from overlapping.
pub struct Lock(#[allow(dead_code)] File);

impl Lock {
    pub fn take(path: &Path) -> Result<Lock> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening lock {}", path.display()))?;
        if !platform::try_lock(&file)? {
            bail!("another gfiles plan apply, undo or empty is running on this machine");
        }
        Ok(Lock(file))
    }
}

/// Runs `op` for plan `id` on this machine. `items` are the files to move (apply only);
/// `volumes` are the plan's volumes that may hold its files in the trash (undo and empty);
/// `known` is what the catalog knew of the files apply moves (`known_before`); `report`
/// receives each item's outcome as it's decided. What the run learned for this machine's
/// catalog goes into `learned`, also when the run stops early. The caller holds the lock.
#[allow(clippy::too_many_arguments)]
pub fn run(
    config: &Config,
    op: Op,
    id: &str,
    items: &[Item],
    volumes: &[String],
    known: &Known,
    learned: &mut Learned,
    mut report: impl FnMut(&Outcome) -> Result<()>,
) -> Result<Done> {
    if !plan::valid_id(id) {
        bail!("bad plan id {id:?}");
    }
    let roots: HashMap<&str, &Path> = config
        .volumes
        .iter()
        .map(|v| (v.id.as_str(), v.path.as_path()))
        .collect();
    match op {
        Op::Apply => {
            let mut done = Done::default();
            let mut ex = Executor {
                roots: &roots,
                id,
                known,
                hashes: HashMap::new(),
                learned: Learned::default(),
                journals: HashMap::new(),
            };
            let mut stopped = None;
            for item in items {
                let outcome = match ex.apply(item) {
                    Ok(()) => Outcome {
                        seq: item.seq,
                        moved: true,
                        reason: None,
                    },
                    Err(reason) => Outcome {
                        seq: item.seq,
                        moved: false,
                        reason: Some(reason),
                    },
                };
                if outcome.moved {
                    done.moved += 1;
                } else {
                    done.skipped += 1;
                }
                if let Err(e) = report(&outcome) {
                    stopped = Some(e);
                    break;
                }
            }
            for (path, f) in &ex.journals {
                if let Err(e) = f.sync_data() {
                    eprintln!("warning: saving {}: {e}", path.display());
                }
            }
            // Handed back even when the run stops early: a resumed apply never sends the
            // files whose outcomes were already recorded, so this is their only chance.
            *learned = ex.learned;
            match stopped {
                Some(e) => Err(e),
                None => Ok(done),
            }
        }
        Op::Undo => Ok(undo(&roots, volumes, id, learned)),
        Op::Empty => Ok(empty(&roots, volumes, id)),
    }
}

struct Executor<'a> {
    roots: &'a HashMap<&'a str, &'a Path>,
    id: &'a str,
    known: &'a Known,
    /// Full hashes of kept copies, by (volume id, path): many removed files can share one.
    hashes: HashMap<(String, String), String>,
    /// Full hashes read of files that stay where they are, and files moved, for the catalog.
    learned: Learned,
    /// Each trash folder's open journal. A move's line is synced before the file moves, since
    /// undo finds moves only through it; removals are synced once at the end, since a removal's
    /// line lost to a crash only makes emptying keep that file (undo finds it in the trash).
    journals: HashMap<PathBuf, File>,
}

/// Why a file was skipped.
type Skip = String;

impl<'a> Executor<'a> {
    fn apply(&mut self, item: &Item) -> std::result::Result<(), Skip> {
        match (item.action, &item.keep, &item.dest) {
            (Action::Remove, Some(keep), None) => self.remove(item, keep),
            (Action::Move, None, Some(dest)) => {
                let read = self.relocate(item, dest)?;
                let (fingerprint, hash) = self
                    .known
                    .get(&(item.volume_id.clone(), item.path.clone()))
                    .cloned()
                    .unwrap_or_default();
                self.learned.carried.push(store::Carried {
                    from_volume_id: item.volume_id.clone(),
                    from_path: item.path.clone(),
                    to_volume_id: dest.volume_id.clone(),
                    to_path: dest.path.clone(),
                    size: item.size,
                    mtime: item.mtime,
                    fingerprint,
                    hash: read.or(hash),
                });
                Ok(())
            }
            _ => Err("the hub sent an entry whose fields don't match its action".into()),
        }
    }

    /// Moves a file to the trash after checking the copy that stays.
    fn remove(&mut self, item: &Item, kept: &Kept) -> std::result::Result<(), Skip> {
        let root = self.root(&item.volume_id)?;
        let keep_root = self.root(&kept.volume_id)?;
        let src = checked_path(root, &item.path)?;
        let keep = checked_path(keep_root, &kept.path)?;
        let trash = trash_dir(root, self.id).map_err(|e| format!("trash folder: {e}"))?;
        let dest = trash.join("files").join(&item.path);

        let meta = match fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Moved by an earlier, interrupted run of this plan?
                return match fs::symlink_metadata(&dest) {
                    Ok(m) if m.is_file() && m.len() as i64 == item.size => Ok(()),
                    _ => Err("gone since the scan".into()),
                };
            }
            Err(e) => return Err(format!("can't read it: {e}")),
        };
        if !unchanged(&meta, item.size, item.mtime) {
            return Err("changed since the scan".into());
        }
        let keep_meta =
            fs::symlink_metadata(&keep).map_err(|_| "the kept copy is gone".to_string())?;
        if !unchanged(&keep_meta, kept.size, kept.mtime) {
            return Err("the kept copy changed since the scan".into());
        }
        if platform::same_file(&meta, &keep_meta) {
            return Err("it and the kept copy are the same file".into());
        }

        // A vouched removal's content may differ from the kept copy's by design, so there's
        // nothing to compare; the trash and undo are its safety net.
        let hash = if item.vouched {
            String::new()
        } else {
            let keep_hash = match self
                .hashes
                .get(&(kept.volume_id.clone(), kept.path.clone()))
            {
                Some(h) => h.clone(),
                None => {
                    let h = full_hash(&keep, kept.size, kept.mtime)
                        .map_err(|e| format!("the kept copy: {e}"))?;
                    self.hashes
                        .insert((kept.volume_id.clone(), kept.path.clone()), h.clone());
                    self.learned.hashes.push(store::Hashed {
                        volume_id: kept.volume_id.clone(),
                        path: kept.path.clone(),
                        size: kept.size,
                        mtime: kept.mtime,
                        hash: h.clone(),
                    });
                    h
                }
            };
            let hash = full_hash(&src, item.size, item.mtime)?;
            if hash != keep_hash {
                // Remembered, so the catalog stops calling the two duplicates.
                self.learned.hashes.push(store::Hashed {
                    volume_id: item.volume_id.clone(),
                    path: item.path.clone(),
                    size: item.size,
                    mtime: item.mtime,
                    hash,
                });
                return Err("its content differs from the kept copy".into());
            }
            hash
        };
        // Last look at the kept copy before the move.
        match fs::symlink_metadata(&keep) {
            Ok(m) if unchanged(&m, kept.size, kept.mtime) => {}
            _ => return Err("the kept copy changed while checking".into()),
        }

        to_trash(&src, &dest)?;
        let entry = JournalEntry {
            path: item.path.clone(),
            size: item.size,
            mtime: item.mtime,
            hash,
            keep_volume_id: kept.volume_id.clone(),
            keep_path: kept.path.clone(),
            keep_size: kept.size,
            keep_mtime: kept.mtime,
            ..JournalEntry::new(Action::Remove, item)
        };
        // The move happened even if the journal can't be written; emptying keeps any file
        // the journal doesn't vouch for.
        if let Err(e) = self.append_journal(&trash, &entry) {
            eprintln!("warning: writing the trash journal: {e}");
        }
        Ok(())
    }

    /// Moves a file to its destination on this machine: a rename that never replaces anything,
    /// or, across filesystems, a verified copy and the original to the trash. Returns the full
    /// hash it read when it copied.
    fn relocate(&mut self, item: &Item, dest: &Dest) -> std::result::Result<Option<String>, Skip> {
        let root = self.root(&item.volume_id)?;
        let dest_root = self.root(&dest.volume_id)?;
        let src = checked_path(root, &item.path)?;
        let to =
            checked_path(dest_root, &dest.path).map_err(|e| format!("its destination: {e}"))?;
        if crate::scan::ignored_by_default(&dest.path) {
            return Err("its destination is somewhere scans skip".into());
        }
        let trash = trash_dir(root, self.id).map_err(|e| format!("trash folder: {e}"))?;
        let in_trash = trash.join("files").join(&item.path);

        let meta = match fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Moved by an earlier, interrupted run of this plan?
                return match fs::symlink_metadata(&to) {
                    Ok(m) if unchanged(&m, item.size, item.mtime) => {
                        self.journal_once(&trash, item, in_trash.exists());
                        Ok(None)
                    }
                    _ => Err("gone since the scan".into()),
                };
            }
            Err(e) => return Err(format!("can't read it: {e}")),
        };
        if !unchanged(&meta, item.size, item.mtime) {
            return Err("changed since the scan".into());
        }
        if fs::symlink_metadata(&to).is_ok() {
            return Err("something is already at its destination".into());
        }
        let created =
            make_parents(dest_root, &dest.path).map_err(|e| format!("its destination: {e}"))?;
        let undo_folders = |why: String| {
            remove_folders(dest_root, &created);
            why
        };
        // The journal is a move's only record for undo, so its line is on disk before the
        // file moves. A line for a move that then fails is harmless: undo finds the file still
        // at its old path.
        let entry = JournalEntry {
            created: created.clone(),
            ..JournalEntry::new(Action::Move, item)
        };
        self.append_journal_synced(&trash, &entry)
            .map_err(|e| undo_folders(format!("writing the trash journal: {e}")))?;

        let renamed = if force_copy() {
            Err(io::ErrorKind::CrossesDevices.into())
        } else {
            platform::rename_no_replace(&src, &to)
        };
        match renamed {
            Ok(()) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                let hash = copy_verified(&src, &to, item, self.id).map_err(&undo_folders)?;
                after_copy(&src);
                // Record the copy and its hash before the original goes to the trash.
                let entry = JournalEntry {
                    hash: hash.clone(),
                    copied: true,
                    ..entry
                };
                if let Err(why) = self
                    .append_journal_synced(&trash, &entry)
                    .map_err(|e| format!("writing the trash journal: {e}"))
                    .and_then(|()| fs::symlink_metadata(&src).map_err(|e| e.to_string()))
                    .and_then(|m| {
                        if unchanged(&m, item.size, item.mtime) {
                            Ok(())
                        } else {
                            Err("it changed while copying".to_string())
                        }
                    })
                    .and_then(|()| to_trash(&src, &in_trash))
                {
                    // Take back the copy (only ours, unchanged), so the file stays in one place.
                    if fs::symlink_metadata(&to).is_ok_and(|m| unchanged(&m, item.size, item.mtime))
                    {
                        let _ = fs::remove_file(&to);
                    }
                    return Err(undo_folders(format!("after copying it: {why}")));
                }
                Ok(Some(hash))
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(undo_folders(
                "something is already at its destination".into(),
            )),
            Err(e) => Err(undo_folders(format!("moving it: {e}"))),
        }
    }

    /// Records a move an earlier, interrupted run made, unless its journal already has it.
    fn journal_once(&mut self, trash: &Path, item: &Item, copied: bool) {
        if read_journal(trash).contains_key(&item.path) {
            return;
        }
        // Its hash and created folders are unknown: emptying then keeps the original, and undo
        // leaves the folders.
        let entry = JournalEntry {
            copied,
            ..JournalEntry::new(Action::Move, item)
        };
        if let Err(e) = self.append_journal(trash, &entry) {
            eprintln!("warning: writing the trash journal: {e}");
        }
    }

    fn append_journal(&mut self, trash: &Path, entry: &JournalEntry) -> io::Result<()> {
        self.journal_line(trash, entry).map(|_| ())
    }

    /// Appends a line and syncs it to disk before returning, for a move about to happen.
    fn append_journal_synced(&mut self, trash: &Path, entry: &JournalEntry) -> io::Result<()> {
        self.journal_line(trash, entry)?.sync_data()
    }

    fn journal_line(&mut self, trash: &Path, entry: &JournalEntry) -> io::Result<&File> {
        let path = trash.join("journal.jsonl");
        let f = match self.journals.entry(path) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .append(true)
                    .open(v.key())?;
                // A crash can leave a last line without its newline; start on a fresh line so
                // the next one isn't glued to it.
                if ends_mid_line(&mut f)? {
                    f.write_all(b"\n")?;
                }
                v.insert(f)
            }
        };
        // One write per line, so a line is never split by a later failure.
        f.write_all(format!("{}\n", serde_json::to_string(entry)?).as_bytes())?;
        Ok(f)
    }

    fn root(&self, volume_id: &str) -> std::result::Result<&'a Path, Skip> {
        volume_root(self.roots, volume_id)
    }
}

/// A volume's root on this machine, refusing one that isn't configured here or whose folder
/// is missing (an unmounted disk): the executor never creates a volume root.
fn volume_root<'a>(
    roots: &HashMap<&str, &'a Path>,
    volume_id: &str,
) -> std::result::Result<&'a Path, Skip> {
    let root = roots
        .get(volume_id)
        .copied()
        .ok_or_else(|| "its volume isn't configured on this machine".to_string())?;
    if !root.is_dir() {
        return Err(format!(
            "its volume folder {} is missing (not mounted?)",
            root.display()
        ));
    }
    Ok(root)
}

fn unchanged(meta: &fs::Metadata, size: i64, mtime: i64) -> bool {
    meta.is_file()
        && meta.len() as i64 == size
        && crate::scan::unix_secs(meta.modified().ok()) == mtime
}

fn full_hash(path: &Path, size: i64, mtime: i64) -> std::result::Result<String, Skip> {
    match fingerprint::full(path, size, mtime) {
        Ok(Some(h)) => Ok(h),
        Ok(None) => Err("changed while reading it".into()),
        Err(e) => Err(format!("can't read it: {e}")),
    }
}

/// `root` joined with a catalog path, after checking the path stays inside the volume, isn't
/// in the trash, and passes through no symlink.
fn checked_path(root: &Path, rel: &str) -> std::result::Result<PathBuf, Skip> {
    store::check_rel_path(rel).map_err(|e| e.to_string())?;
    if rel.split('/').next() == Some(TRASH) {
        return Err("it's in the trash".into());
    }
    let mut path = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        if i + 1 < parts.len() {
            match fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(format!("{} isn't a real folder", path.display())),
                // A missing folder: the file is gone too, which the caller reports.
                Err(_) => {}
            }
        }
    }
    Ok(path)
}

/// The plan's trash folder on a volume, created if needed, one level at a time below an
/// existing root. Refuses a level that is a symlink before creating anything inside it.
fn trash_dir(root: &Path, id: &str) -> io::Result<PathBuf> {
    let top = root.join(TRASH);
    let dir = top.join(id);
    for d in [&top, &dir, &dir.join("files")] {
        match fs::create_dir(d) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        if !fs::symlink_metadata(d)?.is_dir() {
            return Err(io::Error::other(format!(
                "{} isn't a real folder",
                d.display()
            )));
        }
    }
    Ok(dir)
}

/// One file the plan moved, as recorded in the plan's `journal.jsonl` in the trash of the
/// file's volume. Fields added after the first version have defaults, so older journals read
/// as removals.
#[derive(Debug, Default, Serialize, Deserialize)]
struct JournalEntry {
    #[serde(default)]
    action: Action,
    path: String,
    size: i64,
    /// The file's mtime, which the move keeps: emptying deletes it only while it's unchanged.
    mtime: i64,
    /// The file's full hash, when apply read it (not for a vouched removal or a rename).
    #[serde(default)]
    hash: String,
    /// A removal's copy that stays.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    keep_volume_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    keep_path: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    keep_size: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    keep_mtime: i64,
    /// A move's destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dest_volume_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dest_path: Option<String>,
    /// The move copied the file across filesystems and put the original in the trash.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    copied: bool,
    /// Folders the move created for its destination (relative to the destination's volume),
    /// outermost first, for undo to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    created: Vec<String>,
    at: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl JournalEntry {
    fn new(action: Action, item: &Item) -> JournalEntry {
        JournalEntry {
            action,
            path: item.path.clone(),
            size: item.size,
            mtime: item.mtime,
            dest_volume_id: item.dest.as_ref().map(|d| d.volume_id.clone()),
            dest_path: item.dest.as_ref().map(|d| d.path.clone()),
            at: crate::scan::unix_secs(Some(std::time::SystemTime::now())),
            ..JournalEntry::default()
        }
    }
}

/// Moves `src` to `dest` in the trash, creating its folders there.
fn to_trash(src: &Path, dest: &Path) -> std::result::Result<(), Skip> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("creating its trash folder: {e}"))?;
    }
    platform::rename_no_replace(src, dest).map_err(|e| match e.kind() {
        io::ErrorKind::CrossesDevices => "the trash is on another filesystem".to_string(),
        io::ErrorKind::AlreadyExists => "a file with its name is already in the trash".to_string(),
        _ => format!("moving it to the trash: {e}"),
    })
}

/// Creates the missing folders above `rel` (a file's path) under an existing `root`, one level
/// at a time, refusing a level that is a symlink or a file. Returns the folders it created,
/// outermost first; on failure it removes them again.
fn make_parents(root: &Path, rel: &str) -> std::result::Result<Vec<String>, Skip> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut created = Vec::new();
    let mut path = root.to_path_buf();
    for i in 0..parts.len().saturating_sub(1) {
        path.push(parts[i]);
        let fail = |created: &[String], why: String| {
            remove_folders(root, created);
            why
        };
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => continue,
            Ok(_) => {
                return Err(fail(
                    &created,
                    format!("{} isn't a real folder", path.display()),
                ));
            }
            Err(_) => {}
        }
        if let Err(e) = fs::create_dir(&path) {
            return Err(fail(&created, format!("creating {}: {e}", path.display())));
        }
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            return Err(fail(
                &created,
                format!("{} isn't a real folder", path.display()),
            ));
        }
        created.push(parts[..=i].join("/"));
    }
    Ok(created)
}

/// Removes folders `make_parents` created, deepest first, as long as they're empty.
fn remove_folders(root: &Path, created: &[String]) {
    for rel in created.iter().rev() {
        let _ = fs::remove_dir(root.join(rel));
    }
}

/// Copies `src` (a file on another filesystem) to `to`, which must not exist: into a hidden
/// partial file next to it, read back from the disk and compared in full with the source,
/// then renamed into place without replacing anything. Returns the content's full hash. On
/// any doubt it removes its own copy.
fn copy_verified(
    src: &Path,
    to: &Path,
    item: &Item,
    id: &str,
) -> std::result::Result<String, Skip> {
    let dir = to
        .parent()
        .ok_or_else(|| "its destination has no folder".to_string())?;
    let partial = dir.join(format!(".gfiles-partial-{id}-{}", item.seq));
    // Left by an earlier, interrupted run of this plan.
    match fs::symlink_metadata(&partial) {
        Ok(m) if m.is_file() => fs::remove_file(&partial)
            .map_err(|e| format!("removing an earlier partial copy: {e}"))?,
        Ok(_) => return Err(format!("{} isn't a file gfiles made", partial.display())),
        Err(_) => {}
    }
    let checked = write_copy(src, &partial, item).and_then(|()| {
        let source = full_hash(src, item.size, item.mtime)?;
        let copy =
            full_hash(&partial, item.size, item.mtime).map_err(|e| format!("its copy: {e}"))?;
        if source != copy {
            return Err("its copy doesn't match it (a disk or copy error)".to_string());
        }
        Ok(source)
    });
    let hash = match checked {
        Ok(h) => h,
        Err(why) => {
            let _ = fs::remove_file(&partial);
            return Err(why);
        }
    };
    if let Err(e) = platform::rename_no_replace(&partial, to) {
        let _ = fs::remove_file(&partial);
        return Err(match e.kind() {
            io::ErrorKind::AlreadyExists => "something is already at its destination".into(),
            _ => format!("placing its copy: {e}"),
        });
    }
    // The original goes to the trash next; make sure the copy's name survives a power cut.
    if let Err(e) = platform::sync_dir(dir) {
        let _ = fs::remove_file(to);
        return Err(format!("saving its copy: {e}"));
    }
    Ok(hash)
}

/// Writes a copy of `src` to the new file `partial`, with its permissions and mtime, synced to
/// disk and dropped from the cache so it's read back from the disk.
fn write_copy(src: &Path, partial: &Path, item: &Item) -> std::result::Result<(), Skip> {
    let mut from = platform::open_no_follow(src).map_err(|e| format!("can't read it: {e}"))?;
    let meta = from.metadata().map_err(|e| format!("can't read it: {e}"))?;
    if !unchanged(&meta, item.size, item.mtime) {
        return Err("changed since the scan".into());
    }
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial)
        .map_err(|e| format!("creating its copy: {e}"))?;
    let mut write = |out: &mut File| -> io::Result<u64> {
        let n = io::copy(&mut from, out)?;
        damage_copy(out)?;
        out.set_permissions(meta.permissions())?;
        out.set_modified(meta.modified()?)?;
        out.sync_all()?;
        Ok(n)
    };
    match write(&mut out) {
        Ok(n) if n == item.size as u64 => {}
        Ok(_) => return Err("it changed while copying".into()),
        Err(e) => return Err(format!("copying it: {e}")),
    }
    platform::drop_cache(&out);
    Ok(())
}

/// Whether `f` (open for reading) is non-empty and doesn't end with a newline.
fn ends_mid_line(f: &mut File) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    if f.metadata()?.len() == 0 {
        return Ok(false);
    }
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8];
    f.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Lets a test change a file between its copy and the check that it didn't change meanwhile.
#[cfg(not(test))]
fn after_copy(_: &Path) {}

#[cfg(test)]
fn after_copy(src: &Path) {
    if tests::CHANGE_AFTER_COPY.with(|c| c.get()) {
        fs::write(src, "changed while it was copied").unwrap();
    }
}

/// Whether moves always copy, as across filesystems: tests can't make a second filesystem.
#[cfg(not(test))]
fn force_copy() -> bool {
    false
}

#[cfg(test)]
fn force_copy() -> bool {
    tests::FORCE_COPY.with(|c| c.get())
}

/// Lets a test damage a copy before it's checked, as a disk error would.
#[cfg(not(test))]
fn damage_copy(_: &mut File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn damage_copy(out: &mut File) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    if tests::DAMAGE_COPY.with(|c| c.get()) {
        out.seek(SeekFrom::Start(0))?;
        out.write_all(b"X")?;
    }
    Ok(())
}

fn read_journal(trash: &Path) -> HashMap<String, JournalEntry> {
    let Ok(f) = File::open(trash.join("journal.jsonl")) else {
        return HashMap::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(std::result::Result::ok)
        .filter_map(|l| serde_json::from_str::<JournalEntry>(&l).ok())
        .map(|e| (e.path.clone(), e))
        .collect()
}

/// Every file in a plan's trash on one volume, as (path in the trash, path relative to root),
/// and how many entries the walk couldn't list as such (unreadable folders, names that aren't
/// UTF-8, anything that isn't a regular file).
fn trashed_files(files: &Path) -> (Vec<(PathBuf, String)>, usize) {
    let mut found = Vec::new();
    let mut unlisted = 0;
    for e in walkdir::WalkDir::new(files).follow_links(false) {
        let Ok(e) = e else {
            unlisted += 1;
            continue;
        };
        if e.file_type().is_dir() {
            continue;
        }
        let rel = e.path().strip_prefix(files).ok().and_then(|rel| {
            rel.components()
                .map(|c| c.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
        });
        match rel {
            Some(rel) if e.file_type().is_file() => {
                found.push((e.path().to_path_buf(), rel.join("/")));
            }
            _ => unlisted += 1,
        }
    }
    (found, unlisted)
}

/// The trash folder of a plan on each of `volumes` that has one, with the volume's id and
/// root. A volume that isn't configured here or isn't mounted is reported in `left`, so the
/// plan doesn't count as undone or emptied while its files may still be in that volume's
/// trash.
fn trash_folders<'a>(
    roots: &HashMap<&str, &'a Path>,
    volumes: &[String],
    id: &str,
    left: &mut Vec<String>,
) -> Vec<(String, &'a Path, PathBuf)> {
    let mut found = Vec::new();
    for volume_id in volumes {
        let root = match volume_root(roots, volume_id) {
            Ok(r) => r,
            Err(why) => {
                left.push(format!(
                    "volume {volume_id}: {}; any of the plan's files there stay in the trash",
                    why.trim_start_matches("its ")
                ));
                continue;
            }
        };
        let dir = root.join(TRASH).join(id);
        if fs::symlink_metadata(dir.join("files")).is_ok_and(|m| m.is_dir()) {
            found.push((volume_id.clone(), root, dir));
        }
    }
    found
}

fn report_unlisted(files: &Path, unlisted: usize, left: &mut Vec<String>) {
    if unlisted > 0 {
        left.push(format!(
            "{}: {unlisted} entries gfiles couldn't list stay there",
            files.display()
        ));
    }
}

/// Puts a plan's files back where they were: files in the trash, and moved files from their
/// destination. A file whose original path is taken again stays where it is. Folders a move
/// created are removed afterwards, once every move is undone, if they're empty.
fn undo(roots: &HashMap<&str, &Path>, volumes: &[String], id: &str, learned: &mut Learned) -> Done {
    let mut done = Done::default();
    let mut created: Vec<(&Path, String)> = Vec::new();
    let carry = |from_volume_id: &str, from_path: &str, volume_id: &str, entry: &JournalEntry| {
        store::Carried {
            from_volume_id: from_volume_id.to_string(),
            from_path: from_path.to_string(),
            to_volume_id: volume_id.to_string(),
            to_path: entry.path.clone(),
            size: entry.size,
            mtime: entry.mtime,
            fingerprint: None,
            hash: Some(entry.hash.clone()).filter(|h| !h.is_empty()),
        }
    };
    for (volume_id, root, dir) in trash_folders(roots, volumes, id, &mut done.left_in_trash) {
        let files = dir.join("files");
        let journal = read_journal(&dir);
        for entry in journal.values().filter(|e| e.action == Action::Move) {
            let restored = done.restored;
            if let Some(dest_root) = undo_move(roots, root, &files, entry, &mut done) {
                created.extend(entry.created.iter().map(|c| (dest_root, c.clone())));
            }
            if done.restored > restored
                && let (Some(dest_volume), Some(dest_path)) =
                    (&entry.dest_volume_id, &entry.dest_path)
            {
                learned
                    .carried
                    .push(carry(dest_volume, dest_path, &volume_id, entry));
            }
        }
        let (found, unlisted) = trashed_files(&files);
        report_unlisted(&files, unlisted, &mut done.left_in_trash);
        for (in_trash, rel) in found {
            // A copied move's original: undo_move dealt with it.
            if journal.get(&rel).is_some_and(|e| e.action == Action::Move) {
                continue;
            }
            let original = root.join(&rel);
            match restore(root, &rel, &in_trash) {
                Ok(()) => {
                    done.restored += 1;
                    if let Some(entry) = journal.get(&rel) {
                        learned
                            .carried
                            .push(carry(&volume_id, &rel, &volume_id, entry));
                    }
                }
                Err(why) => done
                    .left_in_trash
                    .push(format!("{}: {why}", original.display())),
            }
        }
    }
    // Deepest first, so a folder shared by several moves goes once all of them are undone.
    created.sort_by_key(|(_, rel)| std::cmp::Reverse(rel.matches('/').count()));
    for (root, rel) in created {
        let _ = fs::remove_dir(root.join(rel));
    }
    done
}

/// Moves `from` back to `rel` under `root`, recreating its folders, unless something is at
/// that path now.
fn restore(root: &Path, rel: &str, from: &Path) -> std::result::Result<(), Skip> {
    let original = root.join(rel);
    if fs::symlink_metadata(&original).is_ok() {
        return Err("something else is at its path now".into());
    }
    checked_path(root, rel)?;
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("recreating its folder: {e}"))?;
    }
    platform::rename_no_replace(from, &original).map_err(|e| format!("moving it back: {e}"))
}

/// Undoes one move. A renamed file goes back to its old path. A copied move's original comes
/// back from the trash, when its copy is gone or unchanged, and then the unchanged copy is
/// removed; an edited copy keeps both where they are. Returns the destination's volume root
/// when the move was undone, for removing the folders it created.
fn undo_move<'a>(
    roots: &HashMap<&str, &'a Path>,
    root: &Path,
    files: &Path,
    entry: &JournalEntry,
    done: &mut Done,
) -> Option<&'a Path> {
    let original = root.join(&entry.path);
    let mut fail = |why: String| {
        done.left_in_trash
            .push(format!("{}: {why}", original.display()));
        None
    };
    let (Some(dest_volume), Some(dest_path)) = (&entry.dest_volume_id, &entry.dest_path) else {
        return fail("its journal entry has no destination".into());
    };
    let dest_root = match volume_root(roots, dest_volume) {
        Ok(r) => r,
        Err(why) => {
            return fail(format!(
                "its destination: {}",
                why.trim_start_matches("its ")
            ));
        }
    };
    let at = match checked_path(dest_root, dest_path) {
        Ok(p) => p,
        Err(why) => return fail(format!("its destination: {why}")),
    };
    let copy = fs::symlink_metadata(&at);
    if !entry.copied {
        if !copy.as_ref().is_ok_and(|m| m.is_file()) {
            // Gone from its destination: put back already, or removed since.
            if fs::symlink_metadata(&original).is_ok() {
                return Some(dest_root);
            }
            return fail(format!("it's no longer at {}", at.display()));
        }
        return match restore(root, &entry.path, &at) {
            Ok(()) => {
                done.restored += 1;
                Some(dest_root)
            }
            Err(why) => fail(format!("{why}; it stays at {}", at.display())),
        };
    }
    // The copy is ours to remove only while it still holds what was copied: same size and
    // mtime, and the full hash the move recorded (an edit can keep both size and mtime).
    let holds_copy = |m: &fs::Metadata| {
        unchanged(m, entry.size, entry.mtime)
            && !entry.hash.is_empty()
            && full_hash(&at, entry.size, entry.mtime).is_ok_and(|h| h == entry.hash)
    };
    let copy_kept = |at: &Path| {
        if entry.hash.is_empty() {
            format!(
                "the journal has no hash to check its copy at {} against, so both stay",
                at.display()
            )
        } else {
            format!(
                "its copy at {} changed since the move, so both stay",
                at.display()
            )
        }
    };
    let in_trash = files.join(&entry.path);
    if fs::symlink_metadata(&in_trash).is_err() {
        if fs::symlink_metadata(&original).is_err() {
            return fail(format!(
                "it's no longer in the trash; its copy stays at {}",
                at.display()
            ));
        }
        // Restored by an earlier undo, or the move stopped before the original left: remove
        // the copy if it's still there and still ours.
        return match &copy {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(dest_root),
            Ok(m) if holds_copy(m) => match fs::remove_file(&at) {
                Ok(()) => Some(dest_root),
                Err(e) => fail(format!("removing its copy at {}: {e}", at.display())),
            },
            _ => fail(copy_kept(&at)),
        };
    }
    let copy_unchanged = match &copy {
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Ok(m) if holds_copy(m) => true,
        _ => return fail(format!("{} (the original in the trash)", copy_kept(&at))),
    };
    if let Err(why) = restore(root, &entry.path, &in_trash) {
        return fail(format!(
            "{why}; it stays in the trash, its copy at {}",
            at.display()
        ));
    }
    done.restored += 1;
    if copy_unchanged && let Err(e) = fs::remove_file(&at) {
        return fail(format!("removing its copy at {}: {e}", at.display()));
    }
    Some(dest_root)
}

/// Deletes a plan's files from the trash, except any that changed since the move, whose kept
/// copy is gone or changed, or that the journal doesn't vouch for: those stay, since they may
/// be the last copy. A copied move's original is deleted only when its copy reads back with
/// the original's full hash.
fn empty(roots: &HashMap<&str, &Path>, volumes: &[String], id: &str) -> Done {
    let mut done = Done::default();
    for (_, _, dir) in trash_folders(roots, volumes, id, &mut done.left_in_trash) {
        let files = dir.join("files");
        let journal = read_journal(&dir);
        let (found, unlisted) = trashed_files(&files);
        report_unlisted(&files, unlisted, &mut done.left_in_trash);
        for (in_trash, rel) in found {
            let keep = |why: &str| format!("{}: {why}", in_trash.display());
            let Some(entry) = journal.get(&rel) else {
                done.left_in_trash.push(keep(
                    "not in the plan's journal, so its kept copy is unknown",
                ));
                continue;
            };
            if !fs::symlink_metadata(&in_trash)
                .is_ok_and(|m| unchanged(&m, entry.size, entry.mtime))
            {
                done.left_in_trash
                    .push(keep("it changed after it was moved to the trash"));
                continue;
            }
            if let Err(why) = other_copy_holds(roots, entry) {
                done.left_in_trash.push(keep(&why));
                continue;
            }
            match fs::remove_file(&in_trash) {
                Ok(()) => {
                    done.deleted += 1;
                    done.deleted_bytes += entry.size as u64;
                }
                Err(e) => done.left_in_trash.push(keep(&format!("deleting it: {e}"))),
            }
        }
        if done.left_in_trash.is_empty() {
            if let Err(e) = remove_emptied(&dir) {
                done.left_in_trash.push(format!(
                    "{}: removing the plan's trash folder: {e}",
                    dir.display()
                ));
            } else if let Some(top) = dir.parent() {
                let _ = fs::remove_dir(top);
            }
        }
    }
    done
}

/// Whether the copy that makes a trashed file safe to delete is still there: a removal's kept
/// copy, unchanged since the scan, or a copied move's copy, reading back with the original's
/// full hash.
fn other_copy_holds(
    roots: &HashMap<&str, &Path>,
    entry: &JournalEntry,
) -> std::result::Result<(), Skip> {
    let at = |volume_id: &str, path: &str| {
        roots
            .get(volume_id)
            .and_then(|r| checked_path(r, path).ok())
    };
    match entry.action {
        Action::Remove => {
            let ok = at(&entry.keep_volume_id, &entry.keep_path).is_some_and(|p| {
                fs::symlink_metadata(p)
                    .is_ok_and(|m| unchanged(&m, entry.keep_size, entry.keep_mtime))
            });
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "its kept copy {} is gone or changed",
                    entry.keep_path
                ))
            }
        }
        Action::Move => {
            let dest = entry.dest_path.as_deref().unwrap_or_default();
            let copy = entry
                .dest_volume_id
                .as_deref()
                .and_then(|v| at(v, dest))
                .ok_or_else(|| format!("its copy {dest} isn't reachable"))?;
            let hash = full_hash(&copy, entry.size, entry.mtime)
                .map_err(|why| format!("its copy {dest}: {why}"))?;
            if entry.hash.is_empty() || hash != entry.hash {
                return Err(format!(
                    "its copy {dest} doesn't match it, or the journal has no hash to check"
                ));
            }
            Ok(())
        }
    }
}

/// Removes an emptied plan's trash folder: its journal, then each folder, deepest first.
/// Anything else still inside makes this fail instead of being deleted.
fn remove_emptied(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join("journal.jsonl")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    for e in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .contents_first(true)
    {
        let e = e?;
        if e.file_type().is_dir() {
            fs::remove_dir(e.path())?;
        }
    }
    Ok(())
}

/// `gfiles plan exec`: the machine side of the exchange, on stdin/stdout. `catalog` is this
/// machine's, read before files move (`known_before`). What the run learned goes into
/// `learned`, also when it stopped early (the hub gone), for the caller to store in this
/// machine's catalog (see `record_learned`).
pub fn serve(
    config: &Config,
    lock_path: &Path,
    catalog: Option<&Connection>,
    input: impl BufRead,
    output: impl Write,
    learned: &mut Learned,
) -> Result<()> {
    let mut out = BufWriter::new(output);
    let hello = Hello {
        gfiles_actions: PROTOCOL,
        machine: config.machine_name(),
    };
    writeln!(out, "{}", serde_json::to_string(&hello)?)?;
    out.flush()?;
    let refuse = |out: &mut BufWriter<_>, msg: String| -> Result<()> {
        writeln!(out, "{}", serde_json::to_string(&Reply::Error(msg))?)?;
        out.flush()?;
        Ok(())
    };
    // Read everything before working, so the hub can finish sending first.
    let lines: Vec<String> = input.lines().collect::<io::Result<_>>()?;
    let Some(first) = lines.first() else {
        return refuse(&mut out, "no request".into());
    };
    let req: Request = match serde_json::from_str(first) {
        Ok(r) => r,
        Err(e) => return refuse(&mut out, format!("bad request: {e}")),
    };
    if req.gfiles_actions != PROTOCOL {
        return refuse(
            &mut out,
            format!(
                "this machine's gfiles speaks plan protocol {PROTOCOL}, the hub's {}; deploy \
                 the same gfiles to both machines",
                req.gfiles_actions
            ),
        );
    }
    if lines.len() - 1 != req.items {
        return refuse(
            &mut out,
            format!(
                "the request was cut off ({} of {} files arrived); nothing was done",
                lines.len() - 1,
                req.items
            ),
        );
    }
    let mut items = Vec::new();
    for line in &lines[1..] {
        match serde_json::from_str::<Item>(line) {
            Ok(i) => items.push(i),
            Err(e) => return refuse(&mut out, format!("bad item: {e}")),
        }
    }
    let _lock = match Lock::take(lock_path) {
        Ok(l) => l,
        Err(e) => return refuse(&mut out, e.to_string()),
    };
    let known = match catalog {
        Some(conn) if req.op == Op::Apply => known_before(conn, &items),
        _ => Known::new(),
    };
    let total = items.len();
    let mut n = 0;
    let result = run(
        config,
        req.op,
        &req.plan,
        &items,
        &req.volumes,
        &known,
        learned,
        |o| {
            n += 1;
            if n % 1000 == 0 {
                eprintln!("{n} of {total} files checked");
            }
            writeln!(out, "{}", serde_json::to_string(&Reply::Item(o.clone()))?)?;
            // Sent now, so a hub that has gone away stops the run at the next file.
            out.flush()?;
            Ok(())
        },
    );
    match result {
        Ok(done) => writeln!(out, "{}", serde_json::to_string(&Reply::Done(done))?)?,
        Err(e) => writeln!(
            out,
            "{}",
            serde_json::to_string(&Reply::Error(format!("{e:#}")))?
        )?,
    }
    out.flush()?;
    Ok(())
}

/// Stores what a plan run learned in this machine's catalog: the full hashes it read, so the
/// files apply found different stop matching as duplicates (and kept copies count as
/// confirmed), and what moved files carry to their new paths, so the next scan doesn't lose
/// their fingerprints and hashes. The hub gets both at its next pull. Never fails the run: a
/// busy or missing catalog only means `gfiles hash` would have to read them again.
pub fn record_learned(conn: &Connection, learned: &Learned) {
    if learned.is_empty() {
        return;
    }
    // A scan holds the catalog for its whole walk; don't keep the hub waiting that long.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(30));
    if let Err(e) = store::record_hashes(conn, &learned.hashes) {
        eprintln!(
            "warning: couldn't store {} full hashes in the catalog ({e:#}); the files are \
             fine, `gfiles hash` can compute them again",
            learned.hashes.len()
        );
    }
    if let Err(e) = store::record_carried(conn, &learned.carried) {
        eprintln!(
            "warning: couldn't record {} moved files in the catalog ({e:#}); the files are \
             fine, but their next scan reads them as new",
            learned.carried.len()
        );
    }
}

/// Where `plan apply/undo/empty` runs a plan: in this process, or over SSH on its machine.
pub enum Route<'a> {
    Local {
        config: &'a Config,
        lock_path: &'a Path,
    },
    Ssh {
        host: String,
        gfiles: String,
    },
}

impl<'a> Route<'a> {
    /// The route for plans on `machine`, from the hub's config.
    pub fn for_machine(
        config: &'a Config,
        machine: &str,
        lock_path: &'a Path,
    ) -> Result<Route<'a>> {
        if machine == config.machine_name() {
            return Ok(Route::Local { config, lock_path });
        }
        let Some(remote) = config.remotes.iter().find(|r| r.name == machine) else {
            bail!("plan is for {machine}, which is neither this machine nor a [[remote]]");
        };
        Ok(Route::Ssh {
            host: remote.apply_host().to_string(),
            gfiles: remote.gfiles.clone(),
        })
    }
}

/// Runs `op` for a saved plan through `route`, recording outcomes and state in the catalog as
/// they arrive. The caller has confirmed with Darren.
pub fn execute(conn: &mut Connection, route: &Route, op: Op, id: &str) -> Result<Done> {
    let summary = plan::summary(conn, id)?;
    let allowed = match op {
        Op::Apply => ["draft", "applying"].as_slice(),
        Op::Undo => ["applying", "applied"].as_slice(),
        Op::Empty => ["applied"].as_slice(),
    };
    if !allowed.contains(&summary.state.as_str()) {
        bail!(
            "plan {id} is {}; {} needs it to be {}",
            summary.state,
            op_name(op),
            allowed.join(" or ")
        );
    }
    // Resuming an apply sends only what has no outcome yet.
    let items: Vec<Item> = if op == Op::Apply {
        plan::exec_items(conn, id)?
    } else {
        Vec::new()
    };
    let volumes = plan::trash_volumes(conn, id)?;
    let now = || crate::scan::unix_secs(Some(std::time::SystemTime::now()));
    if op == Op::Apply {
        plan::set_state(conn, id, "applying", now())?;
    }
    let mut pending: Vec<Outcome> = Vec::new();
    let done = {
        let mut record = |o: &Outcome, conn: &mut Connection| -> Result<()> {
            pending.push(o.clone());
            if pending.len() >= 500 {
                plan::record(conn, id, &pending)?;
                pending.clear();
            }
            Ok(())
        };
        match route {
            Route::Local { config, lock_path } => Lock::take(lock_path).and_then(|_lock| {
                let known = if op == Op::Apply {
                    known_before(conn, &items)
                } else {
                    Known::new()
                };
                let mut learned = Learned::default();
                let done = run(
                    config,
                    op,
                    id,
                    &items,
                    &volumes,
                    &known,
                    &mut learned,
                    |o| record(o, conn),
                );
                record_learned(conn, &learned);
                done
            }),
            Route::Ssh { host, gfiles } => over_ssh(
                host,
                gfiles,
                &summary.machine,
                op,
                id,
                &items,
                &volumes,
                |o| record(o, conn),
            ),
        }
    };
    plan::record(conn, id, &pending)?;
    if done.is_err() && op == Op::Apply && summary.state == "draft" {
        // Nothing was decided (the other machine wasn't reached, or the lock was held): the
        // plan is still a draft, which can be dropped.
        let recorded: i64 = conn.query_row(
            "SELECT count(*) FROM plan_items WHERE plan_id = ?1 AND outcome IS NOT NULL",
            [id],
            |r| r.get(0),
        )?;
        if recorded == 0 {
            plan::set_state(conn, id, "draft", summary.updated_at)?;
        }
    }
    let done = done?;
    let new_state = match op {
        Op::Apply => {
            let unrecorded: i64 = conn.query_row(
                "SELECT count(*) FROM plan_items WHERE plan_id = ?1 AND outcome IS NULL",
                [id],
                |r| r.get(0),
            )?;
            if unrecorded == 0 {
                "applied"
            } else {
                "applying"
            }
        }
        Op::Undo if done.left_in_trash.is_empty() => "undone",
        Op::Empty if done.left_in_trash.is_empty() => "emptied",
        _ => summary.state.as_str(),
    };
    plan::set_state(conn, id, new_state, now())?;
    Ok(done)
}

pub fn op_name(op: Op) -> &'static str {
    match op {
        Op::Apply => "apply",
        Op::Undo => "undo",
        Op::Empty => "empty",
    }
}

/// Runs `op` on another machine through `ssh <host> <gfiles> plan exec`.
#[allow(clippy::too_many_arguments)]
fn over_ssh(
    host: &str,
    gfiles: &str,
    machine: &str,
    op: Op,
    id: &str,
    items: &[Item],
    volumes: &[String],
    mut report: impl FnMut(&Outcome) -> Result<()>,
) -> Result<Done> {
    // No BatchMode: Darren is at the terminal and may need to unlock a key. Progress from the
    // other machine comes through on stderr.
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "ConnectTimeout=30",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=4",
            host,
        ])
        .arg(format!("{gfiles} plan exec"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let result = exchange(
        host,
        machine,
        op,
        id,
        items,
        volumes,
        stdin,
        BufReader::new(stdout),
        &mut report,
    );
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let done = result?;
    if !status.success() {
        bail!("ssh {host} exited with {status}");
    }
    Ok(done)
}

/// The hub's half of the exchange: check the other side's hello, send the request, read
/// outcomes.
#[allow(clippy::too_many_arguments)]
fn exchange(
    host: &str,
    machine: &str,
    op: Op,
    id: &str,
    items: &[Item],
    volumes: &[String],
    input: impl Write,
    mut output: impl BufRead,
    report: &mut impl FnMut(&Outcome) -> Result<()>,
) -> Result<Done> {
    let mut line = String::new();
    output.read_line(&mut line)?;
    let hello: Hello = match serde_json::from_str(&line) {
        Ok(h) => h,
        Err(_) if line.starts_with("{\"t\":\"hello\"") => bail!(
            "ssh {host} ran `gfiles export` instead of `plan exec`: that's the pull-only key. \
             Set apply_host in [[remote]] to an ssh destination with a full login (see \
             docs/setup.md); nothing was sent"
        ),
        Err(_) if line.is_empty() => bail!("ssh {host} closed without answering"),
        Err(_) => bail!(
            "ssh {host} answered {:?}, not gfiles plan exec; nothing was sent",
            line.trim()
        ),
    };
    if hello.gfiles_actions != PROTOCOL {
        bail!(
            "{host} speaks plan protocol {}, this gfiles {PROTOCOL}; deploy the same gfiles to \
             both machines (bin/deploy). Nothing was sent",
            hello.gfiles_actions
        );
    }
    if hello.machine != machine {
        bail!(
            "ssh {host} reached machine {:?}, but the plan is for {machine}; nothing was sent",
            hello.machine
        );
    }
    {
        let mut input = BufWriter::new(input);
        let req = Request {
            gfiles_actions: PROTOCOL,
            op,
            plan: id.to_string(),
            volumes: volumes.to_vec(),
            items: items.len(),
        };
        writeln!(input, "{}", serde_json::to_string(&req)?)?;
        for i in items {
            writeln!(input, "{}", serde_json::to_string(i)?)?;
        }
        input.flush()?;
        // Dropping closes stdin, which tells the other side the request is complete.
    }
    loop {
        line.clear();
        if output.read_line(&mut line)? == 0 {
            bail!(
                "the connection to {host} ended before the plan finished; run it again to resume"
            );
        }
        match serde_json::from_str::<Reply>(&line)
            .with_context(|| format!("bad reply from {host}: {:?}", line.trim()))?
        {
            Reply::Item(o) => report(&o)?,
            Reply::Done(d) => return Ok(d),
            Reply::Error(e) => bail!("{machine}: {e}"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Makes this thread's moves copy, as across filesystems.
        pub(super) static FORCE_COPY: Cell<bool> = const { Cell::new(false) };
        /// Damages this thread's copies before they're checked.
        pub(super) static DAMAGE_COPY: Cell<bool> = const { Cell::new(false) };
        /// Changes the source of this thread's copies once they're placed.
        pub(super) static CHANGE_AFTER_COPY: Cell<bool> = const { Cell::new(false) };
    }

    const ID: &str = "20261116-120000-abcd";

    struct Vol {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        config: Config,
    }

    fn volume() -> Vol {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("darren");
        fs::create_dir_all(&root).unwrap();
        let config: Config = toml::from_str(&format!(
            "machine = \"elrond\"\n[[volume]]\nid = \"v1\"\nname = \"darren\"\npath = {:?}\n",
            root.display().to_string()
        ))
        .unwrap();
        Vol {
            _tmp: tmp,
            root,
            config,
        }
    }

    impl Vol {
        fn write(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }

        fn stat(&self, rel: &str) -> (i64, i64) {
            let m = fs::symlink_metadata(self.root.join(rel)).unwrap();
            (m.len() as i64, crate::scan::unix_secs(m.modified().ok()))
        }

        /// An item removing `rel` and keeping `keep`, as of their current state.
        fn item(&self, seq: i64, rel: &str, keep: &str) -> Item {
            let (size, mtime) = self.stat(rel);
            let (keep_size, keep_mtime) = self.stat(keep);
            Item {
                seq,
                action: Action::Remove,
                volume_id: "v1".into(),
                path: rel.into(),
                size,
                mtime,
                vouched: false,
                keep: Some(Kept {
                    volume_id: "v1".into(),
                    path: keep.into(),
                    size: keep_size,
                    mtime: keep_mtime,
                }),
                dest: None,
            }
        }

        /// An item moving `rel` to `to` (on the same volume), as of its current state.
        fn mv(&self, seq: i64, rel: &str, to: &str) -> Item {
            let (size, mtime) = self.stat(rel);
            Item {
                seq,
                action: Action::Move,
                volume_id: "v1".into(),
                path: rel.into(),
                size,
                mtime,
                vouched: false,
                keep: None,
                dest: Some(Dest {
                    volume_id: "v1".into(),
                    path: to.into(),
                }),
            }
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).unwrap()
        }

        fn journal(&self) -> String {
            fs::read_to_string(self.root.join(TRASH).join(ID).join("journal.jsonl"))
                .unwrap_or_default()
        }

        fn run(&self, op: Op, items: &[Item]) -> (Done, Vec<Outcome>) {
            let (done, _, outcomes) = self.run_learning(op, items);
            (done, outcomes)
        }

        /// Runs `op`, also returning what it learned for the catalog.
        fn run_learning(&self, op: Op, items: &[Item]) -> (Done, Learned, Vec<Outcome>) {
            let mut outcomes = Vec::new();
            let mut learned = Learned::default();
            let done = run(
                &self.config,
                op,
                ID,
                items,
                &["v1".into()],
                &Known::new(),
                &mut learned,
                |o| {
                    outcomes.push(o.clone());
                    Ok(())
                },
            )
            .unwrap();
            (done, learned, outcomes)
        }

        fn exists(&self, rel: &str) -> bool {
            fs::symlink_metadata(self.root.join(rel)).is_ok()
        }

        fn trashed(&self, rel: &str) -> String {
            format!("{TRASH}/{ID}/files/{rel}")
        }
    }

    fn reasons(outcomes: &[Outcome]) -> Vec<String> {
        outcomes
            .iter()
            .map(|o| o.reason.clone().unwrap_or_else(|| "moved".into()))
            .collect()
    }

    #[test]
    fn moves_a_duplicate_to_the_trash_and_resumes() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        let items = [v.item(0, "todo/a.jpg", "Photos/a.jpg")];
        let (done, outcomes) = v.run(Op::Apply, &items);
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        assert_eq!(done.moved, 1);
        assert!(!v.exists("todo/a.jpg"));
        assert!(v.exists(&v.trashed("todo/a.jpg")));
        assert!(v.exists("Photos/a.jpg"));
        let journal =
            fs::read_to_string(v.root.join(TRASH).join(ID).join("journal.jsonl")).unwrap();
        assert!(
            journal.contains("\"keep_path\":\"Photos/a.jpg\""),
            "{journal}"
        );
        // Running it again (an interrupted apply resuming) counts it as moved.
        let (_, outcomes) = v.run(Op::Apply, &items);
        assert_eq!(reasons(&outcomes), vec!["moved"]);
    }

    #[test]
    fn apply_keeps_the_full_hashes_of_files_that_stay() {
        let v = volume();
        v.write("k/a.bmp", "same");
        v.write("t/a.bmp", "same");
        v.write("t/a copy.bmp", "same");
        v.write("t/b.bmp", "diff");
        let items = [
            v.item(0, "t/a.bmp", "k/a.bmp"),
            v.item(1, "t/a copy.bmp", "k/a.bmp"),
            v.item(2, "t/b.bmp", "k/a.bmp"),
        ];
        let (_, learned, outcomes) = v.run_learning(Op::Apply, &items);
        assert_eq!(
            reasons(&outcomes),
            vec!["moved", "moved", "its content differs from the kept copy"]
        );
        // The kept copy once, and the file that differs; not the files moved away.
        let hashed: Vec<(&str, bool)> = learned
            .hashes
            .iter()
            .map(|h| (h.path.as_str(), h.hash.starts_with("b3:")))
            .collect();
        assert_eq!(hashed, vec![("k/a.bmp", true), ("t/b.bmp", true)]);
        assert_ne!(learned.hashes[0].hash, learned.hashes[1].hash);
        assert_eq!(
            (learned.hashes[1].size, learned.hashes[1].mtime),
            v.stat("t/b.bmp")
        );
    }

    #[test]
    fn any_doubt_skips_the_file() {
        let v = volume();
        v.write("a/differs.jpg", "aaaa");
        v.write("k/differs.jpg", "bbbb");
        v.write("a/changed.jpg", "same");
        v.write("k/changed.jpg", "same");
        v.write("a/gone-keep.jpg", "same");
        v.write("k/gone-keep.jpg", "same");
        v.write("k/linked.jpg", "same");
        v.write("k/x.jpg", "same");
        v.write("real/s.jpg", "same");
        fs::hard_link(v.root.join("k/linked.jpg"), v.root.join("a/linked.jpg")).unwrap();
        std::os::unix::fs::symlink(v.root.join("real"), v.root.join("sym")).unwrap();
        v.write(".gfiles-trash/old/x.jpg", "same");

        let mut changed = v.item(1, "a/changed.jpg", "k/changed.jpg");
        changed.mtime -= 10;
        let gone_keep = v.item(2, "a/gone-keep.jpg", "k/gone-keep.jpg");
        fs::remove_file(v.root.join("k/gone-keep.jpg")).unwrap();
        let mut sym = v.item(4, "real/s.jpg", "k/x.jpg");
        sym.path = "sym/s.jpg".into();
        let mut trash = v.item(5, ".gfiles-trash/old/x.jpg", "k/x.jpg");
        trash.path = ".gfiles-trash/old/x.jpg".into();
        v.write("a/keep-changed.jpg", "same");
        v.write("k/keep-changed.jpg", "same");
        let mut keep_changed = v.item(8, "a/keep-changed.jpg", "k/keep-changed.jpg");
        keep_changed.keep.as_mut().unwrap().mtime -= 10;
        let mut other_volume = v.item(6, "k/x.jpg", "a/differs.jpg");
        other_volume.volume_id = "elsewhere".into();
        let mut escape = v.item(7, "k/x.jpg", "a/differs.jpg");
        escape.path = "../outside.jpg".into();
        let items = [
            v.item(0, "a/differs.jpg", "k/differs.jpg"),
            changed,
            gone_keep,
            v.item(3, "a/linked.jpg", "k/linked.jpg"),
            sym,
            trash,
            other_volume,
            escape,
            keep_changed,
        ];
        let (done, outcomes) = v.run(Op::Apply, &items);
        assert_eq!(done.moved, 0);
        let r = reasons(&outcomes);
        assert_eq!(r[0], "its content differs from the kept copy");
        assert_eq!(r[1], "changed since the scan");
        assert_eq!(r[2], "the kept copy is gone");
        assert_eq!(r[3], "it and the kept copy are the same file");
        assert!(r[4].contains("isn't a real folder"), "{}", r[4]);
        assert_eq!(r[5], "it's in the trash");
        assert_eq!(r[6], "its volume isn't configured on this machine");
        assert!(r[7].contains("'..'"), "{}", r[7]);
        assert_eq!(r[8], "the kept copy changed since the scan");
        for f in [
            "a/keep-changed.jpg",
            "a/differs.jpg",
            "a/changed.jpg",
            "a/gone-keep.jpg",
            "a/linked.jpg",
            "real/s.jpg",
        ] {
            assert!(v.exists(f), "{f} was moved");
        }
    }

    #[test]
    fn never_replaces_a_file_in_the_trash() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.write(&v.trashed("todo/a.jpg"), "something else");
        let (_, outcomes) = v.run(Op::Apply, &[v.item(0, "todo/a.jpg", "Photos/a.jpg")]);
        assert!(!outcomes[0].moved);
        assert!(v.exists("todo/a.jpg"));
        assert_eq!(
            fs::read_to_string(v.root.join(v.trashed("todo/a.jpg"))).unwrap(),
            "something else"
        );
    }

    #[test]
    fn a_file_named_like_the_journal_stays_apart_from_it() {
        let v = volume();
        v.write("journal.jsonl", "mine");
        v.write("copy/journal.jsonl", "mine");
        let (done, _) = v.run(
            Op::Apply,
            &[v.item(0, "journal.jsonl", "copy/journal.jsonl")],
        );
        assert_eq!(done.moved, 1);
        assert_eq!(
            fs::read_to_string(v.root.join(v.trashed("journal.jsonl"))).unwrap(),
            "mine"
        );
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 1);
        assert_eq!(
            fs::read_to_string(v.root.join("journal.jsonl")).unwrap(),
            "mine"
        );
    }

    #[test]
    fn undo_puts_files_back_unless_their_path_is_taken() {
        let v = volume();
        v.write("todo/deep/a.jpg", "same");
        v.write("todo/b.jpg", "bbb");
        v.write("Photos/a.jpg", "same");
        v.write("Photos/b.jpg", "bbb");
        let items = [
            v.item(0, "todo/deep/a.jpg", "Photos/a.jpg"),
            v.item(1, "todo/b.jpg", "Photos/b.jpg"),
        ];
        v.run(Op::Apply, &items);
        fs::remove_dir_all(v.root.join("todo")).unwrap();
        v.write("todo/b.jpg", "new");
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 1);
        assert_eq!(
            fs::read_to_string(v.root.join("todo/deep/a.jpg")).unwrap(),
            "same"
        );
        assert_eq!(
            fs::read_to_string(v.root.join("todo/b.jpg")).unwrap(),
            "new"
        );
        assert!(done.left_in_trash[0].contains("something else is at its path"));
        assert!(v.exists(&v.trashed("todo/b.jpg")));
    }

    #[test]
    fn empty_keeps_files_whose_kept_copy_is_gone() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("todo/b.jpg", "bbb");
        v.write("Photos/a.jpg", "same");
        v.write("Photos/b.jpg", "bbb");
        v.run(
            Op::Apply,
            &[
                v.item(0, "todo/a.jpg", "Photos/a.jpg"),
                v.item(1, "todo/b.jpg", "Photos/b.jpg"),
            ],
        );
        fs::remove_file(v.root.join("Photos/b.jpg")).unwrap();
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!((done.deleted, done.deleted_bytes), (1, 4));
        assert!(!v.exists(&v.trashed("todo/a.jpg")));
        assert!(v.exists(&v.trashed("todo/b.jpg")));
        assert!(done.left_in_trash[0].contains("Photos/b.jpg is gone or changed"));
        // Once nothing is held back, the plan's trash folder goes.
        v.write("Photos/b.jpg", "bbb");
        fs::remove_file(v.root.join(v.trashed("todo/b.jpg"))).unwrap();
        let (done, _) = v.run(Op::Empty, &[]);
        assert!(done.left_in_trash.is_empty());
        assert!(!v.exists(TRASH));
    }

    #[test]
    fn a_symlinked_trash_folder_is_refused_before_anything_is_written() {
        for link in [TRASH.to_string(), format!("{TRASH}/{ID}")] {
            let v = volume();
            let outside = v._tmp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            v.write("todo/a.jpg", "same");
            v.write("Photos/a.jpg", "same");
            let link = v.root.join(link);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let (_, outcomes) = v.run(Op::Apply, &[v.item(0, "todo/a.jpg", "Photos/a.jpg")]);
            assert!(!outcomes[0].moved);
            let reason = outcomes[0].reason.as_deref().unwrap();
            assert!(reason.starts_with("trash folder"), "{reason}");
            assert!(v.exists("todo/a.jpg"));
            assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_missing_volume_root_is_never_created() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        let item = v.item(0, "todo/a.jpg", "Photos/a.jpg");
        let away = v._tmp.path().join("unmounted");
        fs::rename(&v.root, &away).unwrap();
        let (_, outcomes) = v.run(Op::Apply, &[item]);
        assert!(!outcomes[0].moved);
        assert!(
            outcomes[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("not mounted")
        );
        assert!(!v.root.exists());
    }

    #[test]
    fn undo_and_empty_hold_back_while_a_volume_is_missing() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.run(Op::Apply, &[v.item(0, "todo/a.jpg", "Photos/a.jpg")]);
        let away = v._tmp.path().join("unmounted");
        fs::rename(&v.root, &away).unwrap();
        for op in [Op::Undo, Op::Empty] {
            let (done, _) = v.run(op, &[]);
            assert!(done.left_in_trash[0].contains("not mounted"), "{done:?}");
            assert!(!v.root.exists());
        }
        // A volume this machine doesn't have holds the plan back too.
        let done = run(
            &v.config,
            Op::Undo,
            ID,
            &[],
            &["gone".into()],
            &Known::new(),
            &mut Learned::default(),
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            done.left_in_trash[0].contains("isn't configured"),
            "{done:?}"
        );
        fs::rename(&away, &v.root).unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 1);
        assert!(done.left_in_trash.is_empty());
    }

    #[test]
    fn undo_never_follows_a_symlink_put_in_place_of_a_folder() {
        let v = volume();
        let outside = v._tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        v.write("todo/deep/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.run(Op::Apply, &[v.item(0, "todo/deep/a.jpg", "Photos/a.jpg")]);
        fs::remove_dir_all(v.root.join("todo")).unwrap();
        std::os::unix::fs::symlink(&outside, v.root.join("todo")).unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 0);
        assert!(
            done.left_in_trash[0].contains("isn't a real folder"),
            "{done:?}"
        );
        assert!(v.exists(&v.trashed("todo/deep/a.jpg")));
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn empty_keeps_what_it_cant_vouch_for() {
        let v = volume();
        for f in ["a", "b", "c"] {
            v.write(&format!("todo/{f}.jpg"), f);
            v.write(&format!("Photos/{f}.jpg"), f);
        }
        v.run(
            Op::Apply,
            &[
                v.item(0, "todo/a.jpg", "Photos/a.jpg"),
                v.item(1, "todo/b.jpg", "Photos/b.jpg"),
                v.item(2, "todo/c.jpg", "Photos/c.jpg"),
            ],
        );
        // The kept copy of a.jpg was rewritten, b.jpg changed in the trash after the move,
        // a stray file has no journal line, and a symlink isn't a file gfiles moved.
        v.write("Photos/a.jpg", "edited");
        v.write(&v.trashed("todo/b.jpg"), "written after the move");
        v.write(&v.trashed("stray.jpg"), "stray");
        std::os::unix::fs::symlink(v.root.join("Photos"), v.root.join(v.trashed("link"))).unwrap();
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!(done.deleted, 1);
        assert!(!v.exists(&v.trashed("todo/c.jpg")));
        let left = done.left_in_trash.join("\n");
        assert!(left.contains("Photos/a.jpg is gone or changed"), "{left}");
        assert!(left.contains("changed after it was moved"), "{left}");
        assert!(left.contains("not in the plan's journal"), "{left}");
        assert!(left.contains("couldn't list"), "{left}");
        for f in ["todo/a.jpg", "todo/b.jpg", "stray.jpg", "link"] {
            assert!(v.exists(&v.trashed(f)), "{f} was deleted");
        }
        assert!(v.exists("Photos/b.jpg"));

        // Without the journal, nothing is vouched for.
        fs::remove_file(v.root.join(TRASH).join(ID).join("journal.jsonl")).unwrap();
        v.write("Photos/a.jpg", "a");
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!(done.deleted, 0);
        assert!(v.exists(&v.trashed("todo/a.jpg")));
    }

    #[test]
    fn a_failed_start_leaves_the_plan_a_draft() {
        use crate::plan::tests::{A, put};
        let v = volume();
        let mut conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "elrond",
            "darren",
            &[("todo/a.jpg", 10, Some(A)), ("Photos/a.jpg", 10, Some(A))],
        );
        let folder = crate::mirror::parse_folder("elrond:darren/todo").unwrap();
        let built = plan::build(&conn, plan::Spec::Drain(vec![folder]), &[]).unwrap();
        let id = plan::save(&conn, &built, 0).unwrap();
        let lock = v._tmp.path().join("actions.lock");
        let _held = Lock::take(&lock).unwrap();
        let route = Route::Local {
            config: &v.config,
            lock_path: &lock,
        };
        assert!(execute(&mut conn, &route, Op::Apply, &id).is_err());
        assert_eq!(plan::summary(&conn, &id).unwrap().state, "draft");
        plan::drop(&conn, &id).unwrap();
    }

    #[test]
    fn one_run_at_a_time() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("actions.lock");
        let held = Lock::take(&path).unwrap();
        assert!(Lock::take(&path).is_err());
        drop(held);
        Lock::take(&path).unwrap();
    }

    /// Runs `serve` on `request`, then the hub's `exchange` on what it answered.
    fn round_trip(v: &Vol, request: &str, items: &[Item]) -> Result<(Done, Vec<Outcome>)> {
        let lock = v._tmp.path().join("actions.lock");
        let mut answer = Vec::new();
        serve(
            &v.config,
            &lock,
            None,
            request.as_bytes(),
            &mut answer,
            &mut Learned::default(),
        )
        .unwrap();
        let mut outcomes = Vec::new();
        let mut sent = Vec::new();
        let done = exchange(
            "elrond",
            "elrond",
            Op::Apply,
            ID,
            items,
            &[],
            &mut sent,
            answer.as_slice(),
            &mut |o| {
                outcomes.push(o.clone());
                Ok(())
            },
        )?;
        Ok((done, outcomes))
    }

    /// The request the hub sends for `items`.
    fn request(items: &[Item]) -> String {
        let mut request = format!(
            "{}\n",
            serde_json::to_string(&Request {
                gfiles_actions: PROTOCOL,
                op: Op::Apply,
                plan: ID.into(),
                volumes: vec!["v1".into()],
                items: items.len(),
            })
            .unwrap()
        );
        for i in items {
            request.push_str(&serde_json::to_string(i).unwrap());
            request.push('\n');
        }
        request
    }

    #[test]
    fn the_exchange_carries_items_and_outcomes() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.write("todo/b.jpg", "bbb");
        v.write("Photos/b.jpg", "bbb");
        let items = [v.item(0, "todo/a.jpg", "Photos/a.jpg")];
        let request = request(&items);

        // A request cut off while the hub was sending it is refused whole.
        let both = [
            v.item(0, "todo/a.jpg", "Photos/a.jpg"),
            v.item(1, "todo/b.jpg", "Photos/b.jpg"),
        ];
        let full = self::request(&both);
        let cut = &full[..full.trim_end().rfind('\n').unwrap() + 1];
        let err = round_trip(&v, cut, &[]).unwrap_err().to_string();
        assert!(err.contains("cut off"), "{err}");
        assert!(v.exists("todo/a.jpg"));

        let (done, outcomes) = round_trip(&v, &request, &items).unwrap();
        assert_eq!(done.moved, 1);
        assert!(outcomes[0].moved);

        let old = request.replacen(
            &format!("\"gfiles_actions\":{PROTOCOL}"),
            "\"gfiles_actions\":0",
            1,
        );
        let err = round_trip(&v, &old, &[]).unwrap_err().to_string();
        assert!(err.contains("deploy the same gfiles"), "{err}");
    }

    #[test]
    fn the_machine_stops_once_the_hub_is_gone() {
        /// Takes the hello, then fails like a closed pipe.
        struct Closed(bool);
        impl Write for Closed {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    Err(io::ErrorKind::BrokenPipe.into())
                } else {
                    Ok(buf.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.write("todo/b.jpg", "bbb");
        v.write("Photos/b.jpg", "bbb");
        let items = [
            v.item(0, "todo/a.jpg", "Photos/a.jpg"),
            v.item(1, "todo/b.jpg", "Photos/b.jpg"),
        ];
        let lock = v._tmp.path().join("actions.lock");
        let mut learned = Learned::default();
        let result = serve(
            &v.config,
            &lock,
            None,
            request(&items).as_bytes(),
            Closed(false),
            &mut learned,
        );
        assert!(result.is_err());
        assert!(!v.exists("todo/a.jpg"), "the first file moves");
        assert!(v.exists("todo/b.jpg"), "but the run stops there");
        // What it learned before stopping is still handed back for this machine's catalog.
        assert_eq!(learned.hashes.len(), 1, "{learned:?}");
    }

    #[test]
    fn the_hub_refuses_the_wrong_answer_before_sending() {
        let check = |answer: &str| {
            let mut sent = Vec::new();
            let err = exchange(
                "h",
                "elrond",
                Op::Apply,
                ID,
                &[],
                &[],
                &mut sent,
                answer.as_bytes(),
                &mut |_| Ok(()),
            )
            .unwrap_err()
            .to_string();
            assert!(sent.is_empty(), "sent {sent:?}");
            err
        };
        let pull_key = "{\"t\":\"hello\",\"protocol\":2,\"machine\":\"elrond\",\"volumes\":[]}\n";
        assert!(check(pull_key).contains("pull-only key"));
        assert!(check("{\"gfiles_actions\":99,\"machine\":\"elrond\"}\n").contains("deploy"));
        assert!(
            check(&format!(
                "{{\"gfiles_actions\":{PROTOCOL},\"machine\":\"bilbo\"}}\n"
            ))
            .contains("bilbo")
        );
        // A machine still on the first protocol, before lists, is told to update.
        assert!(check("{\"gfiles_actions\":1,\"machine\":\"elrond\"}\n").contains("deploy"));
        assert!(check("").contains("closed"));
    }

    /// Runs `f` with this thread's moves copying, as across filesystems.
    fn copying<T>(damage: bool, f: impl FnOnce() -> T) -> T {
        FORCE_COPY.with(|c| c.set(true));
        DAMAGE_COPY.with(|c| c.set(damage));
        let out = f();
        FORCE_COPY.with(|c| c.set(false));
        DAMAGE_COPY.with(|c| c.set(false));
        out
    }

    fn vouched(mut item: Item) -> Item {
        item.vouched = true;
        item
    }

    #[test]
    fn a_vouched_removal_goes_to_the_trash_without_comparing_content() {
        let v = volume();
        v.write("todo/a.NEF", "original");
        v.write("photo/a.NEF", "original + xmp tags");
        let (_, learned, outcomes) = v.run_learning(
            Op::Apply,
            &[vouched(v.item(0, "todo/a.NEF", "photo/a.NEF"))],
        );
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        assert!(v.exists(&v.trashed("todo/a.NEF")));
        assert!(learned.hashes.is_empty(), "{:?}", learned.hashes);
        assert!(v.journal().contains("\"keep_path\":\"photo/a.NEF\""));
        // Unvouched, the same pair is skipped.
        v.write("todo/b.NEF", "original");
        let (_, outcomes) = v.run(Op::Apply, &[v.item(1, "todo/b.NEF", "photo/a.NEF")]);
        assert_eq!(
            reasons(&outcomes),
            vec!["its content differs from the kept copy"]
        );
    }

    #[test]
    fn a_vouched_removal_still_needs_its_kept_copy_unchanged_and_apart() {
        let v = volume();
        v.write("todo/a.NEF", "a");
        v.write("todo/b.NEF", "b");
        v.write("todo/c.NEF", "c");
        v.write("photo/a.NEF", "A");
        v.write("photo/b.NEF", "B");
        let gone = vouched(v.item(0, "todo/a.NEF", "photo/a.NEF"));
        let changed = vouched(v.item(1, "todo/b.NEF", "photo/b.NEF"));
        fs::remove_file(v.root.join("photo/a.NEF")).unwrap();
        v.write("photo/b.NEF", "B, edited");
        fs::hard_link(v.root.join("todo/c.NEF"), v.root.join("photo/c.NEF")).unwrap();
        let same = vouched(v.item(2, "todo/c.NEF", "photo/c.NEF"));
        let (_, outcomes) = v.run(Op::Apply, &[gone, changed, same]);
        assert_eq!(
            reasons(&outcomes),
            vec![
                "the kept copy is gone",
                "the kept copy changed since the scan",
                "it and the kept copy are the same file"
            ]
        );
    }

    #[test]
    fn an_item_whose_fields_dont_match_its_action_is_skipped() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("Photos/a.jpg", "a");
        let mut bad = v.item(0, "todo/a.jpg", "Photos/a.jpg");
        bad.action = Action::Move;
        let mut no_keep = v.item(1, "todo/a.jpg", "Photos/a.jpg");
        no_keep.keep = None;
        let (_, outcomes) = v.run(Op::Apply, &[bad, no_keep]);
        assert!(
            reasons(&outcomes)
                .iter()
                .all(|r| r.contains("don't match its action"))
        );
        assert!(v.exists("todo/a.jpg"));
    }

    #[test]
    fn a_move_renames_and_creates_its_folders() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        fs::create_dir(v.root.join("photo")).unwrap();
        let item = v.mv(0, "todo/a.jpg", "photo/2013/08/13/a.jpg");
        let (done, outcomes) = v.run(Op::Apply, std::slice::from_ref(&item));
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        assert_eq!(done.moved, 1);
        assert!(!v.exists("todo/a.jpg"));
        assert_eq!(v.read("photo/2013/08/13/a.jpg"), "a");
        let journal = v.journal();
        assert!(
            journal.contains("\"created\":[\"photo/2013\",\"photo/2013/08\",\"photo/2013/08/13\"]"),
            "{journal}"
        );
        assert!(!journal.contains("\"copied\""), "{journal}");
        // A rerun counts it as moved and doesn't journal it twice.
        let (_, outcomes) = v.run(Op::Apply, &[item]);
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        assert_eq!(v.journal().lines().count(), 1);
    }

    #[test]
    fn a_move_never_replaces_or_follows_anything() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        v.write("todo/c.jpg", "c");
        v.write("photo/a.jpg", "already here");
        let taken = v.mv(0, "todo/a.jpg", "photo/a.jpg");
        let changed = v.mv(1, "todo/b.jpg", "photo/b.jpg");
        v.write("todo/b.jpg", "b, edited");
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), v.root.join("linked")).unwrap();
        let through_link = v.mv(2, "todo/c.jpg", "linked/c.jpg");
        let (_, outcomes) = v.run(Op::Apply, &[taken, changed, through_link]);
        let r = reasons(&outcomes);
        assert_eq!(r[0], "something is already at its destination");
        assert_eq!(r[1], "changed since the scan");
        assert!(r[2].contains("isn't a real folder"), "{r:?}");
        assert_eq!(v.read("photo/a.jpg"), "already here");
        assert!(v.exists("todo/a.jpg") && v.exists("todo/c.jpg"));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_move_across_filesystems_copies_checks_and_trashes_the_original() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        let mtime = v.stat("todo/a.jpg").1;
        let (_, outcomes) = copying(false, || {
            v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/2013/a.jpg")])
        });
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        assert_eq!(v.read("photo/2013/a.jpg"), "photo bytes");
        assert_eq!(v.stat("photo/2013/a.jpg").1, mtime);
        assert!(!v.exists("todo/a.jpg"));
        assert_eq!(v.read(&v.trashed("todo/a.jpg")), "photo bytes");
        let partials: Vec<_> = fs::read_dir(v.root.join("photo/2013"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(partials, vec![std::ffi::OsString::from("a.jpg")]);
        let journal = v.journal();
        assert!(journal.contains("\"copied\":true"), "{journal}");
        assert!(journal.contains("\"hash\":\"b3:"), "{journal}");
    }

    #[test]
    fn a_copy_that_reads_back_wrong_is_removed_and_the_original_stays() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        let (_, outcomes) = copying(true, || {
            v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")])
        });
        assert_eq!(
            reasons(&outcomes),
            vec!["its copy doesn't match it (a disk or copy error)"]
        );
        assert_eq!(v.read("todo/a.jpg"), "photo bytes");
        // Its copy, and the folder made for it, are gone.
        assert!(!v.exists("photo"));
    }

    #[test]
    fn a_copy_whose_original_cant_go_to_the_trash_is_taken_back() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        v.write(&v.trashed("todo/a.jpg"), "in the way");
        let (_, outcomes) = copying(false, || {
            v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")])
        });
        assert_eq!(
            reasons(&outcomes),
            vec!["after copying it: a file with its name is already in the trash"]
        );
        assert_eq!(v.read("todo/a.jpg"), "photo bytes");
        assert!(!v.exists("photo"));
    }

    #[test]
    fn undo_of_moves_puts_files_back_and_removes_the_folders_they_made() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        v.write("todo/c.jpg", "c");
        fs::create_dir(v.root.join("photo")).unwrap();
        let items = [
            v.mv(0, "todo/a.jpg", "photo/2013/08/13/a.jpg"),
            v.mv(1, "todo/b.jpg", "photo/2013/08/14/b.jpg"),
            v.mv(2, "todo/c.jpg", "photo/2014/c.jpg"),
        ];
        v.run(Op::Apply, &items);
        v.write("photo/2014/other.txt", "not the plan's");
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 3, "{:?}", done.left_in_trash);
        assert!(done.left_in_trash.is_empty(), "{:?}", done.left_in_trash);
        assert_eq!(v.read("todo/a.jpg"), "a");
        assert_eq!(v.read("todo/b.jpg"), "b");
        assert!(!v.exists("photo/2013"));
        // A folder that existed before stays, and so does one that holds something else now.
        assert!(v.exists("photo/2014/other.txt"));
    }

    fn carried(learned: &Learned) -> Vec<(String, String, bool)> {
        learned
            .carried
            .iter()
            .map(|c| (c.from_path.clone(), c.to_path.clone(), c.hash.is_some()))
            .collect()
    }

    #[test]
    fn moves_report_what_they_carry_to_the_new_path() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        v.write("todo/c.jpg", "c");
        v.write("photo/taken.jpg", "already here");
        let renamed = v.mv(0, "todo/a.jpg", "photo/a.jpg");
        let changed = v.mv(1, "todo/b.jpg", "photo/b.jpg");
        v.write("todo/b.jpg", "b, edited");
        let taken = v.mv(2, "todo/c.jpg", "photo/taken.jpg");
        let (_, learned, _) = v.run_learning(Op::Apply, &[renamed.clone(), changed, taken]);
        // Only the move that happened; a rename reads no hash.
        assert_eq!(
            carried(&learned),
            vec![("todo/a.jpg".into(), "photo/a.jpg".into(), false)]
        );
        let c = &learned.carried[0];
        assert_eq!((c.size, c.mtime), (renamed.size, renamed.mtime));
        let (_, learned, _) = v.run_learning(Op::Apply, &[renamed]);
        assert_eq!(learned.carried.len(), 1);

        // A copy across filesystems carries the hash it checked the copy with.
        v.write("todo/d.jpg", "d");
        let (_, learned, _) = copying(false, || {
            v.run_learning(Op::Apply, &[v.mv(3, "todo/d.jpg", "photo/d.jpg")])
        });
        assert_eq!(
            carried(&learned),
            vec![("todo/d.jpg".into(), "photo/d.jpg".into(), true)]
        );
        assert!(
            v.journal()
                .contains(learned.carried[0].hash.as_deref().unwrap())
        );
    }

    #[test]
    fn moves_carry_what_the_catalog_knew_before_the_run() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        let item = v.mv(0, "todo/a.jpg", "photo/a.jpg");
        let fp = "p1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let known = Known::from([(
            ("v1".to_string(), "todo/a.jpg".to_string()),
            (Some(fp.to_string()), Some("b3:known".to_string())),
        )]);
        let mut learned = Learned::default();
        run(
            &v.config,
            Op::Apply,
            ID,
            &[item],
            &["v1".into()],
            &known,
            &mut learned,
            |_| Ok(()),
        )
        .unwrap();
        let c = &learned.carried[0];
        assert_eq!(c.fingerprint.as_deref(), Some(fp));
        assert_eq!(c.hash.as_deref(), Some("b3:known"));
    }

    #[test]
    fn undo_reports_the_files_it_puts_back() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        v.write("todo/c.jpg", "same");
        v.write("Photos/c.jpg", "same");
        v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")]);
        copying(false, || {
            v.run(Op::Apply, &[v.mv(1, "todo/b.jpg", "photo/b.jpg")])
        });
        v.run(Op::Apply, &[v.item(2, "todo/c.jpg", "Photos/c.jpg")]);
        let (done, learned, _) = v.run_learning(Op::Undo, &[]);
        assert_eq!(done.restored, 3, "{:?}", done.left_in_trash);
        let mut got = carried(&learned);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("photo/a.jpg".into(), "todo/a.jpg".into(), false),
                ("photo/b.jpg".into(), "todo/b.jpg".into(), true),
                // A restored removal stays at its own path, with the hash apply compared.
                ("todo/c.jpg".into(), "todo/c.jpg".into(), true),
            ]
        );
    }

    #[test]
    fn a_run_that_stops_early_still_hands_back_its_moves() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        let items = [
            v.mv(0, "todo/a.jpg", "photo/a.jpg"),
            v.mv(1, "todo/b.jpg", "photo/b.jpg"),
        ];
        let mut learned = Learned::default();
        let result = run(
            &v.config,
            Op::Apply,
            ID,
            &items,
            &["v1".into()],
            &Known::new(),
            &mut learned,
            |_| bail!("the hub went away"),
        );
        assert!(result.is_err());
        assert_eq!(
            carried(&learned),
            vec![("todo/a.jpg".into(), "photo/a.jpg".into(), false)]
        );
    }

    #[test]
    fn moved_files_keep_their_hashes_through_the_next_scan() {
        const HASH: &str = "b3:ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
        let v = volume();
        v.write("todo/a.png", "shot");
        let mut conn = crate::db::open_in_memory().unwrap();
        let vol = &v.config.volumes[0];
        crate::scan::scan_volume(&mut conn, vol, "elrond").unwrap();
        conn.execute("UPDATE entries SET content_hash = ?1", [HASH])
            .unwrap();
        let known = |conn: &Connection, path: &str| -> (Option<String>, Option<String>) {
            conn.query_row(
                "SELECT fingerprint, content_hash FROM entries WHERE path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let before = known(&conn, "todo/a.png");
        assert!(before.0.is_some());

        let list = r#"{"move": "elrond:darren/todo/a.png", "to": "elrond:darren/Documents/a.png"}"#;
        let built = crate::listplan::build(&conn, list).unwrap();
        let id = plan::save(&conn, &built, 0).unwrap();
        let lock = v._tmp.path().join("actions.lock");
        let route = Route::Local {
            config: &v.config,
            lock_path: &lock,
        };
        execute(&mut conn, &route, Op::Apply, &id).unwrap();
        crate::scan::scan_volume(&mut conn, vol, "elrond").unwrap();
        assert_eq!(known(&conn, "Documents/a.png"), before);

        execute(&mut conn, &route, Op::Undo, &id).unwrap();
        crate::scan::scan_volume(&mut conn, vol, "elrond").unwrap();
        assert_eq!(known(&conn, "todo/a.png"), before);
    }

    #[test]
    fn undo_of_a_rename_leaves_a_file_whose_old_path_is_taken() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")]);
        v.write("todo/a.jpg", "new");
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 0);
        assert!(
            done.left_in_trash[0].contains("something else is at its path now"),
            "{:?}",
            done.left_in_trash
        );
        assert_eq!(v.read("photo/a.jpg"), "a");
        assert_eq!(v.read("todo/a.jpg"), "new");
    }

    #[test]
    fn undo_of_a_copied_move_never_loses_an_edited_copy() {
        let v = volume();
        for f in ["a", "b", "c"] {
            v.write(&format!("todo/{f}.jpg"), f);
        }
        copying(false, || {
            v.run(
                Op::Apply,
                &[
                    v.mv(0, "todo/a.jpg", "photo/a.jpg"),
                    v.mv(1, "todo/b.jpg", "photo/b.jpg"),
                    v.mv(2, "todo/c.jpg", "photo/c.jpg"),
                ],
            )
        });
        v.write("photo/a.jpg", "a, edited after the move");
        fs::remove_file(v.root.join("photo/c.jpg")).unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        // a: both stay; b: restored and its copy removed; c: copy gone, restored.
        assert_eq!(done.restored, 2);
        assert_eq!(done.left_in_trash.len(), 1, "{:?}", done.left_in_trash);
        assert!(done.left_in_trash[0].contains("changed since the move, so both stay"));
        assert_eq!(v.read("photo/a.jpg"), "a, edited after the move");
        assert!(v.exists(&v.trashed("todo/a.jpg")) && !v.exists("todo/a.jpg"));
        assert_eq!(v.read("todo/b.jpg"), "b");
        assert!(!v.exists("photo/b.jpg"));
        assert_eq!(v.read("todo/c.jpg"), "c");
    }

    #[test]
    fn empty_deletes_a_copied_original_only_while_its_copy_holds() {
        let v = volume();
        for f in ["a", "b", "c"] {
            v.write(&format!("todo/{f}.jpg"), &format!("{f}{f}{f}"));
        }
        copying(false, || {
            v.run(
                Op::Apply,
                &[
                    v.mv(0, "todo/a.jpg", "photo/a.jpg"),
                    v.mv(1, "todo/b.jpg", "photo/b.jpg"),
                    v.mv(2, "todo/c.jpg", "photo/c.jpg"),
                ],
            )
        });
        fs::remove_file(v.root.join("photo/a.jpg")).unwrap();
        // Same size and mtime, different bytes: only reading it shows the damage.
        let (_, mtime) = v.stat("photo/b.jpg");
        v.write("photo/b.jpg", "BBB");
        let f = fs::File::options()
            .write(true)
            .open(v.root.join("photo/b.jpg"))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime as u64))
            .unwrap();
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!(done.deleted, 1);
        assert!(!v.exists(&v.trashed("todo/c.jpg")));
        assert!(v.exists(&v.trashed("todo/a.jpg")));
        assert!(v.exists(&v.trashed("todo/b.jpg")));
        assert_eq!(done.left_in_trash.len(), 2, "{:?}", done.left_in_trash);
    }

    #[test]
    fn empty_keeps_a_vouched_file_whose_kept_copy_changed() {
        let v = volume();
        v.write("todo/a.NEF", "a");
        v.write("photo/a.NEF", "a + xmp");
        v.run(
            Op::Apply,
            &[vouched(v.item(0, "todo/a.NEF", "photo/a.NEF"))],
        );
        v.write("photo/a.NEF", "a + other xmp");
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!(done.deleted, 0);
        assert!(done.left_in_trash[0].contains("is gone or changed"));
    }

    #[test]
    fn empty_and_undo_read_a_journal_from_before_moves() {
        let v = volume();
        v.write("todo/a.jpg", "same");
        v.write("Photos/a.jpg", "same");
        v.run(Op::Apply, &[v.item(0, "todo/a.jpg", "Photos/a.jpg")]);
        // Rewrite the journal as the first version wrote it: no action field.
        let journal = v.root.join(TRASH).join(ID).join("journal.jsonl");
        let (size, mtime) = v.stat(&v.trashed("todo/a.jpg"));
        let (keep_size, keep_mtime) = v.stat("Photos/a.jpg");
        fs::write(
            &journal,
            format!(
                "{{\"path\":\"todo/a.jpg\",\"size\":{size},\"mtime\":{mtime},\"hash\":\"b3:x\",\
                 \"keep_volume_id\":\"v1\",\"keep_path\":\"Photos/a.jpg\",\
                 \"keep_size\":{keep_size},\"keep_mtime\":{keep_mtime},\"at\":1}}\n"
            ),
        )
        .unwrap();
        let (done, _) = v.run(Op::Empty, &[]);
        assert_eq!(done.deleted, 1, "{:?}", done.left_in_trash);
    }

    #[test]
    fn undo_holds_back_while_a_destination_volume_is_missing() {
        let v = volume();
        let mut done = Done::default();
        let roots: HashMap<&str, &Path> = [("v1", v.root.as_path())].into();
        let found = trash_folders(
            &roots,
            &["v1".into(), "v2".into()],
            ID,
            &mut done.left_in_trash,
        );
        assert!(found.is_empty());
        assert!(
            done.left_in_trash[0].contains("volume v2"),
            "{:?}",
            done.left_in_trash
        );
    }

    fn trash_journal(v: &Vol) -> PathBuf {
        v.root.join(TRASH).join(ID).join("journal.jsonl")
    }

    #[test]
    fn a_move_refuses_destinations_outside_plain_places() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        let items = [
            v.mv(0, "todo/a.jpg", "../a.jpg"),
            v.mv(1, "todo/a.jpg", ".gfiles-trash/a.jpg"),
            v.mv(2, "todo/a.jpg", ".ssh/a.jpg"),
            v.mv(3, "todo/a.jpg", "photo/#recycle/a.jpg"),
            v.mv(4, "todo/a.jpg", "photo/@eaDir/a.jpg"),
        ];
        let (_, outcomes) = v.run(Op::Apply, &items);
        let r = reasons(&outcomes);
        assert!(
            r[..2].iter().all(|r| r.starts_with("its destination: ")),
            "{r:?}"
        );
        assert!(r[2..].iter().all(|r| r.contains("scans skip")), "{r:?}");
        assert_eq!(v.read("todo/a.jpg"), "a");
        assert!(!v.exists(".ssh") && !v.exists("photo"));
        assert!(!v.root.parent().unwrap().join("a.jpg").exists());
    }

    #[test]
    fn a_copy_whose_source_changes_while_copying_is_taken_back() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        let item = v.mv(0, "todo/a.jpg", "photo/2013/a.jpg");
        CHANGE_AFTER_COPY.with(|c| c.set(true));
        let (_, outcomes) = copying(false, || v.run(Op::Apply, &[item]));
        CHANGE_AFTER_COPY.with(|c| c.set(false));
        assert_eq!(
            reasons(&outcomes),
            vec!["after copying it: it changed while copying"]
        );
        assert_eq!(v.read("todo/a.jpg"), "changed while it was copied");
        assert!(!v.exists("photo"));
        assert!(!v.exists(&v.trashed("todo/a.jpg")));
    }

    #[test]
    fn a_copy_keeps_the_mode_and_mtime_of_the_original() {
        use std::os::unix::fs::PermissionsExt;
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        let src = v.root.join("todo/a.jpg");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let item = v.mv(0, "todo/a.jpg", "photo/a.jpg");
        let (_, outcomes) = copying(false, || v.run(Op::Apply, &[item]));
        assert_eq!(reasons(&outcomes), vec!["moved"]);
        let m = fs::metadata(v.root.join("photo/a.jpg")).unwrap();
        assert_eq!(m.modified().unwrap(), old);
        assert_eq!(m.permissions().mode() & 0o777, 0o640);
    }

    #[test]
    fn a_rerun_journals_a_move_whose_journal_line_was_lost() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.write("todo/b.jpg", "b");
        let renamed = v.mv(0, "todo/a.jpg", "photo/a.jpg");
        let copied = v.mv(1, "todo/b.jpg", "photo/b.jpg");
        v.run(Op::Apply, std::slice::from_ref(&renamed));
        copying(false, || v.run(Op::Apply, std::slice::from_ref(&copied)));
        fs::remove_file(trash_journal(&v)).unwrap();
        let (_, outcomes) = v.run(Op::Apply, &[renamed, copied]);
        assert_eq!(reasons(&outcomes), vec!["moved", "moved"]);
        let journal = v.journal();
        assert_eq!(journal.lines().count(), 2, "{journal}");
        assert!(journal.contains("\"path\":\"todo/a.jpg\""), "{journal}");
        assert!(
            journal
                .lines()
                .any(|l| l.contains("todo/b.jpg") && l.contains("\"copied\":true")),
            "{journal}"
        );
        // Undo puts the renamed file back. The copied one's journal line has no hash, so its
        // copy can't be told unchanged: both stay.
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(v.read("todo/a.jpg"), "a");
        assert_eq!(done.restored, 1, "{:?}", done.left_in_trash);
        assert!(
            done.left_in_trash[0].contains("no hash"),
            "{:?}",
            done.left_in_trash
        );
        assert!(v.exists("photo/b.jpg") && v.exists(&v.trashed("todo/b.jpg")));
    }

    #[test]
    fn a_move_whose_journal_cant_be_written_is_skipped_before_it_moves() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        fs::create_dir_all(trash_journal(&v)).unwrap();
        let (_, outcomes) = v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/2013/a.jpg")]);
        let r = reasons(&outcomes);
        assert!(r[0].starts_with("writing the trash journal"), "{r:?}");
        assert_eq!(v.read("todo/a.jpg"), "a");
        assert!(!v.exists("photo"));
    }

    #[test]
    fn undo_of_a_move_that_never_happened_changes_nothing() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/2013/a.jpg")]);
        // As if the run stopped after its journal line, before the rename.
        fs::rename(v.root.join("photo/2013/a.jpg"), v.root.join("todo/a.jpg")).unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        assert!(done.left_in_trash.is_empty(), "{:?}", done.left_in_trash);
        assert_eq!(v.read("todo/a.jpg"), "a");
        assert!(!v.exists("photo"));
    }

    #[test]
    fn undo_keeps_a_copy_edited_without_changing_its_size_or_mtime() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        copying(false, || {
            v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")])
        });
        let (_, mtime) = v.stat("photo/a.jpg");
        v.write("photo/a.jpg", "PHOTO BYTES");
        fs::File::options()
            .write(true)
            .open(v.root.join("photo/a.jpg"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime as u64))
            .unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 0);
        assert!(
            done.left_in_trash[0].contains("changed since the move, so both stay"),
            "{:?}",
            done.left_in_trash
        );
        assert_eq!(v.read("photo/a.jpg"), "PHOTO BYTES");
        assert!(v.exists(&v.trashed("todo/a.jpg")));
    }

    #[test]
    fn undo_removes_a_copy_whose_original_never_left() {
        let v = volume();
        v.write("todo/a.jpg", "photo bytes");
        copying(false, || {
            v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/2013/a.jpg")])
        });
        // As if the run stopped after recording the copy, before the original went to the
        // trash.
        fs::rename(
            v.root.join(v.trashed("todo/a.jpg")),
            v.root.join("todo/a.jpg"),
        )
        .unwrap();
        let (done, _) = v.run(Op::Undo, &[]);
        assert!(done.left_in_trash.is_empty(), "{:?}", done.left_in_trash);
        assert_eq!(v.read("todo/a.jpg"), "photo bytes");
        assert!(!v.exists("photo"));
    }

    #[test]
    fn a_torn_last_journal_line_doesnt_swallow_the_next() {
        let v = volume();
        v.write("todo/a.jpg", "a");
        fs::create_dir_all(trash_journal(&v).parent().unwrap()).unwrap();
        fs::write(trash_journal(&v), "{\"path\":\"todo/x").unwrap();
        v.run(Op::Apply, &[v.mv(0, "todo/a.jpg", "photo/a.jpg")]);
        let (done, _) = v.run(Op::Undo, &[]);
        assert_eq!(done.restored, 1, "{:?}", done.left_in_trash);
        assert_eq!(v.read("todo/a.jpg"), "a");
    }
}
