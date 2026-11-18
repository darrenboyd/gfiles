//! Carrying out cleanup plans (see `plan`) on the machine that holds the files: moving files to
//! the trash, undoing that, and emptying the trash. This is the only code in gfiles that
//! writes under a volume root, and it only:
//!
//! - creates and writes inside the volume's `.gfiles-trash` folder,
//! - renames a file into it, or back out of it on undo,
//! - recreates a restored file's missing parent folders on undo,
//! - deletes a plan's files from the trash when emptying it.
//!
//! A plan's trash is `<volume root>/.gfiles-trash/<plan id>/`: `files/` holds each moved file
//! at its path relative to the volume root, and `journal.jsonl` records each move (with the
//! copy that stays). Moves are single renames that never replace anything, so a file is never
//! copied and nothing is overwritten.
//!
//! Before moving a file, the executor checks that it and the copy that stays are unchanged
//! since the scan (size and mtime), aren't the same file, and have the same full content. Any
//! doubt skips the file. Re-running a plan resumes it: a file already in the trash counts as
//! moved.
//!
//! The hub talks to another machine's executor through `gfiles plan exec` over SSH, a
//! newline-delimited JSON exchange:
//!
//! ```text
//! machine → hub   {"gfiles_actions":1,"machine":"elrond"}       before reading anything
//! hub → machine   {"gfiles_actions":1,"op":"apply","plan":"<id>","volumes":[..],"items":N}
//! hub → machine   {"seq":0,"volume_id":..,"path":..,...}         × N (apply only), then EOF
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
pub const PROTOCOL: u32 = 1;

/// The trash folder at the top of each volume. Scans always skip it.
pub const TRASH: &str = ".gfiles-trash";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Apply,
    Undo,
    Empty,
}

/// A file to move to the trash, and the copy that must stay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub seq: i64,
    pub volume_id: String,
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub keep_volume_id: String,
    pub keep_path: String,
    pub keep_size: i64,
    pub keep_mtime: i64,
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
/// `report` receives each item's outcome as it's decided. The caller holds the lock.
pub fn run(
    config: &Config,
    op: Op,
    id: &str,
    items: &[Item],
    volumes: &[String],
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
                hashes: HashMap::new(),
                journals: HashMap::new(),
            };
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
                report(&outcome)?;
            }
            for (path, f) in &ex.journals {
                if let Err(e) = f.sync_data() {
                    eprintln!("warning: saving {}: {e}", path.display());
                }
            }
            Ok(done)
        }
        Op::Undo => Ok(undo(&roots, volumes, id)),
        Op::Empty => Ok(empty(&roots, volumes, id)),
    }
}

struct Executor<'a> {
    roots: &'a HashMap<&'a str, &'a Path>,
    id: &'a str,
    /// Full hashes of kept copies, by (volume id, path): many removed files can share one.
    hashes: HashMap<(String, String), String>,
    /// Each trash folder's open journal. Synced once at the end: a line lost to a crash only
    /// makes emptying keep that file.
    journals: HashMap<PathBuf, File>,
}

/// Why a file was skipped.
type Skip = String;

impl Executor<'_> {
    fn apply(&mut self, item: &Item) -> std::result::Result<(), Skip> {
        let root = self.root(&item.volume_id)?;
        let keep_root = self.root(&item.keep_volume_id)?;
        let src = checked_path(root, &item.path)?;
        let keep = checked_path(keep_root, &item.keep_path)?;
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
        if !unchanged(&keep_meta, item.keep_size, item.keep_mtime) {
            return Err("the kept copy changed since the scan".into());
        }
        if platform::same_file(&meta, &keep_meta) {
            return Err("it and the kept copy are the same file".into());
        }

        let keep_hash = match self
            .hashes
            .get(&(item.keep_volume_id.clone(), item.keep_path.clone()))
        {
            Some(h) => h.clone(),
            None => {
                let h = full_hash(&keep, item.keep_size, item.keep_mtime)
                    .map_err(|e| format!("the kept copy: {e}"))?;
                self.hashes.insert(
                    (item.keep_volume_id.clone(), item.keep_path.clone()),
                    h.clone(),
                );
                h
            }
        };
        let hash = full_hash(&src, item.size, item.mtime)?;
        if hash != keep_hash {
            return Err("its content differs from the kept copy".into());
        }
        // Last look at the kept copy before the move.
        match fs::symlink_metadata(&keep) {
            Ok(m) if unchanged(&m, item.keep_size, item.keep_mtime) => {}
            _ => return Err("the kept copy changed while checking".into()),
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("creating its trash folder: {e}"))?;
        }
        platform::rename_no_replace(&src, &dest).map_err(|e| match e.kind() {
            io::ErrorKind::CrossesDevices => "the trash is on another filesystem".to_string(),
            io::ErrorKind::AlreadyExists => {
                "a file with its name is already in the trash".to_string()
            }
            _ => format!("moving it to the trash: {e}"),
        })?;
        let entry = JournalEntry {
            path: item.path.clone(),
            size: item.size,
            mtime: item.mtime,
            hash,
            keep_volume_id: item.keep_volume_id.clone(),
            keep_path: item.keep_path.clone(),
            keep_size: item.keep_size,
            keep_mtime: item.keep_mtime,
            at: crate::scan::unix_secs(Some(std::time::SystemTime::now())),
        };
        // The move happened even if the journal can't be written; emptying keeps any file
        // the journal doesn't vouch for.
        if let Err(e) = self.append_journal(&trash, &entry) {
            eprintln!("warning: writing the trash journal: {e}");
        }
        Ok(())
    }

    fn append_journal(&mut self, trash: &Path, entry: &JournalEntry) -> io::Result<()> {
        let path = trash.join("journal.jsonl");
        let f = match self.journals.entry(path) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(v.key())?;
                v.insert(f)
            }
        };
        // One write per line, so a line is never split by a later failure.
        f.write_all(format!("{}\n", serde_json::to_string(entry)?).as_bytes())
    }

    fn root(&self, volume_id: &str) -> std::result::Result<&Path, Skip> {
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

/// One move, as recorded in the plan's `journal.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
struct JournalEntry {
    path: String,
    size: i64,
    /// The file's mtime, which the move keeps: emptying deletes it only while it's unchanged.
    mtime: i64,
    hash: String,
    keep_volume_id: String,
    keep_path: String,
    keep_size: i64,
    keep_mtime: i64,
    at: i64,
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

/// The `files` folder of a plan's trash on each of `volumes` that has one. A volume that isn't
/// configured here or isn't mounted is reported in `left`, so the plan doesn't count as undone
/// or emptied while its files may still be in that volume's trash.
fn trash_folders<'a>(
    roots: &HashMap<&str, &'a Path>,
    volumes: &[String],
    id: &str,
    left: &mut Vec<String>,
) -> Vec<(&'a Path, PathBuf)> {
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
            found.push((root, dir));
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

/// Puts a plan's files back where they were. A file whose original path is taken again stays
/// in the trash.
fn undo(roots: &HashMap<&str, &Path>, volumes: &[String], id: &str) -> Done {
    let mut done = Done::default();
    for (root, dir) in trash_folders(roots, volumes, id, &mut done.left_in_trash) {
        let files = dir.join("files");
        let (found, unlisted) = trashed_files(&files);
        report_unlisted(&files, unlisted, &mut done.left_in_trash);
        for (in_trash, rel) in found {
            let original = root.join(&rel);
            let fail = |why: String| format!("{}: {why}", original.display());
            if fs::symlink_metadata(&original).is_ok() {
                done.left_in_trash
                    .push(fail("something else is at its path now".into()));
                continue;
            }
            if let Err(e) = checked_path(root, &rel) {
                done.left_in_trash.push(fail(e));
                continue;
            }
            if let Some(parent) = original.parent()
                && let Err(e) = fs::create_dir_all(parent)
            {
                done.left_in_trash
                    .push(fail(format!("recreating its folder: {e}")));
                continue;
            }
            match platform::rename_no_replace(&in_trash, &original) {
                Ok(()) => done.restored += 1,
                Err(e) => done
                    .left_in_trash
                    .push(fail(format!("moving it back: {e}"))),
            }
        }
    }
    done
}

/// Deletes a plan's files from the trash, except any that changed since the move, whose kept
/// copy is gone or changed, or that the journal doesn't vouch for: those stay, since they may
/// be the last copy.
fn empty(roots: &HashMap<&str, &Path>, volumes: &[String], id: &str) -> Done {
    let mut done = Done::default();
    for (_, dir) in trash_folders(roots, volumes, id, &mut done.left_in_trash) {
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
            let kept_ok = roots.get(entry.keep_volume_id.as_str()).is_some_and(|r| {
                checked_path(r, &entry.keep_path).is_ok_and(|p| {
                    fs::symlink_metadata(p)
                        .is_ok_and(|m| unchanged(&m, entry.keep_size, entry.keep_mtime))
                })
            });
            if !kept_ok {
                done.left_in_trash.push(keep(&format!(
                    "its kept copy {} is gone or changed",
                    entry.keep_path
                )));
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

/// `gfiles plan exec`: the machine side of the exchange, on stdin/stdout.
pub fn serve(
    config: &Config,
    lock_path: &Path,
    input: impl BufRead,
    output: impl Write,
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
    let total = items.len();
    let mut n = 0;
    let result = run(config, req.op, &req.plan, &items, &req.volumes, |o| {
        n += 1;
        if n % 1000 == 0 {
            eprintln!("{n} of {total} files checked");
        }
        writeln!(out, "{}", serde_json::to_string(&Reply::Item(o.clone()))?)?;
        // Sent now, so a hub that has gone away stops the run at the next file.
        out.flush()?;
        Ok(())
    });
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
            Route::Local { config, lock_path } => Lock::take(lock_path)
                .and_then(|_lock| run(config, op, id, &items, &volumes, |o| record(o, conn))),
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
                volume_id: "v1".into(),
                path: rel.into(),
                size,
                mtime,
                keep_volume_id: "v1".into(),
                keep_path: keep.into(),
                keep_size,
                keep_mtime,
            }
        }

        fn run(&self, op: Op, items: &[Item]) -> (Done, Vec<Outcome>) {
            let mut outcomes = Vec::new();
            let done = run(&self.config, op, ID, items, &["v1".into()], |o| {
                outcomes.push(o.clone());
                Ok(())
            })
            .unwrap();
            (done, outcomes)
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
        keep_changed.keep_mtime -= 10;
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
        let done = run(&v.config, Op::Undo, ID, &[], &["gone".into()], |_| Ok(())).unwrap();
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
        serve(&v.config, &lock, request.as_bytes(), &mut answer).unwrap();
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
        let result = serve(&v.config, &lock, request(&items).as_bytes(), Closed(false));
        assert!(result.is_err());
        assert!(!v.exists("todo/a.jpg"), "the first file moves");
        assert!(v.exists("todo/b.jpg"), "but the run stops there");
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
        assert!(check("{\"gfiles_actions\":1,\"machine\":\"bilbo\"}\n").contains("bilbo"));
        assert!(check("").contains("closed"));
    }
}
