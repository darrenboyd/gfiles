//! Walks a volume and brings its catalog rows up to date, through `store::VolumeWriter`. The
//! walk is one transaction, so an interrupted walk leaves the previous catalog untouched.
//!
//! After the walk commits, a second pass fingerprints the files that need it (new or changed
//! ones) and re-reads each repository's HEAD, committing in batches: reading contents is slow
//! on a large disk, so an interrupted pass keeps what it did and the next scan carries on.
//!
//! A scan deletes the entries it didn't see, so it must not mistake "couldn't read" for "gone":
//! an unreadable root fails the scan, and entries under an unreadable folder or file are kept.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::Connection;
use walkdir::WalkDir;

use crate::classify;
use crate::config::VolumeConfig;
use crate::fingerprint;
use crate::store::{Entry, Volume, VolumeWriter};

/// Patterns every volume ignores, in gitignore syntax. A volume's own `ignore` patterns are
/// applied after these, so `!.something` can bring back a hidden file or folder.
const DEFAULT_IGNORES: &[&str] = &[
    ".*", // dotfiles and dot-folders: app state, caches, .DS_Store
    "node_modules/",
    "__pycache__/",
    "target/",        // Rust build output
    "_build/",        // Elixir (mix) build output
    "*.o",            // C object files
    "**/go/pkg/mod/", // Go's module cache
    "@eaDir/",        // Synology thumbnails and metadata
    "\\#recycle/",    // Synology recycle bin
    "\\#snapshot/",
    "$RECYCLE.BIN/",
    "System Volume Information/",
    "lost+found/",
    "Thumbs.db",
    "desktop.ini",
];

#[derive(Debug, Default, PartialEq)]
pub struct ScanStats {
    pub files: u64,
    pub repos: u64,
    pub removed: u64,
    /// Not seen this time, but kept because they're under something that couldn't be read.
    pub kept: u64,
    /// Names that aren't valid UTF-8; they're skipped because the catalog stores text paths.
    pub non_utf8: u64,
    pub errors: u64,
    /// Files fingerprinted this time (new or changed since the last scan).
    pub fingerprinted: u64,
    /// Files still without a fingerprint: unreadable, or changed while being read.
    pub unfingerprinted: u64,
}

pub fn scan_volume(conn: &mut Connection, vol: &VolumeConfig, machine: &str) -> Result<ScanStats> {
    let root = &vol.path;
    // A missing root usually means an unmounted disk or share. Scanning it would look like
    // every file had been deleted, so refuse instead.
    if !root.is_dir() {
        bail!(
            "volume {:?}: {} is not a directory (not mounted?)",
            vol.name,
            root.display()
        );
    }
    let ignores = build_ignores(root, &vol.ignore)?;
    let root_str = root.to_str().context("volume path is not valid UTF-8")?;

    let tx = conn.transaction()?;
    let mut writer = VolumeWriter::begin(
        &tx,
        &Volume {
            id: vol.id.clone(),
            name: vol.name.clone(),
            machine: machine.to_string(),
            root: root_str.to_string(),
            last_scan_at: None,
        },
    )?;
    let mut stats = ScanStats::default();
    // Relative paths that couldn't be read; `None` once an error can't be pinned to a path.
    let mut unreadable: Option<Vec<String>> = Some(Vec::new());

    let mut walk = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(next) = walk.next() {
        let entry = match next {
            Ok(e) => e,
            Err(e) => {
                let at = e.path().map(|p| relative_path(root, p));
                if e.depth() == 0 || matches!(at, Some(Some(ref rel)) if rel.is_empty()) {
                    // Nothing below the root can be listed, so this scan knows nothing.
                    bail!("volume {:?}: can't read {}: {e}", vol.name, root.display());
                }
                eprintln!("warning: {e}");
                stats.errors += 1;
                match (at, &mut unreadable) {
                    (Some(Some(rel)), Some(list)) => list.push(rel),
                    _ => unreadable = None,
                }
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let is_dir = file_type.is_dir();
        // Cleanup plans' trash: always skipped, even when the volume un-ignores hidden folders.
        let in_trash = entry.depth() == 1 && entry.file_name() == crate::apply::TRASH;
        if in_trash || ignores.matched(entry.path(), is_dir).is_ignore() {
            if is_dir {
                walk.skip_current_dir();
            }
            continue;
        }
        let Some(rel) = relative_path(root, entry.path()) else {
            eprintln!(
                "warning: skipping non-UTF-8 name {}",
                entry.path().display()
            );
            stats.non_utf8 += 1;
            if is_dir {
                walk.skip_current_dir();
            }
            continue;
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();

        if is_dir {
            // A git repository is one item; its files aren't cataloged individually.
            if entry.path().join(".git").exists() {
                walk.skip_current_dir();
                let mtime = entry
                    .metadata()
                    .ok()
                    .map_or(0, |m| unix_secs(m.modified().ok()));
                writer.put(&Entry {
                    path: rel,
                    name,
                    ext: None,
                    kind: classify::REPO.to_string(),
                    size: 0,
                    mtime,
                    fingerprint: None,
                    content_hash: None,
                })?;
                stats.repos += 1;
            }
            continue;
        }
        if !file_type.is_file() {
            continue; // sockets, fifos, devices
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("warning: {e}");
                stats.errors += 1;
                if let Some(list) = &mut unreadable {
                    list.push(rel);
                }
                continue;
            }
        };
        let ext = crate::store::ext_of(&name);
        let kind = classify::kind_for_ext(ext.as_deref()).to_string();
        writer.put(&Entry {
            path: rel,
            name,
            ext,
            kind,
            size: meta.len() as i64,
            mtime: unix_secs(meta.modified().ok()),
            fingerprint: None,
            content_hash: None,
        })?;
        stats.files += 1;
    }

    let scanned_at = unix_secs(Some(SystemTime::now()));
    // An error with no path: keep everything not seen rather than guess.
    let keep = unreadable.unwrap_or_else(|| vec![String::new()]);
    let finished = writer.finish(scanned_at, &keep)?;
    stats.removed = finished.removed;
    stats.kept = finished.kept;
    crate::store::take_carried(&tx, &vol.id, &keep)?;
    tx.commit()?;

    fingerprint_pass(conn, vol, &mut stats)?;
    Ok(stats)
}

/// Rows written per transaction by the fingerprint pass.
const BATCH: usize = 500;

/// Fingerprints this volume's files that lack one and refreshes every repository's HEAD.
/// Each fingerprint is written only if the row still has the size and mtime that were hashed.
fn fingerprint_pass(
    conn: &mut Connection,
    vol: &VolumeConfig,
    stats: &mut ScanStats,
) -> Result<()> {
    let todo: Vec<(i64, String, String, i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT id, path, kind, size, mtime FROM entries
             WHERE volume_id = ?1
               AND (kind = ?2 OR (fingerprint IS NULL AND size > 0))",
        )?;
        stmt.query_map(rusqlite::params![vol.id, classify::REPO], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let files = todo.iter().filter(|t| t.2 != classify::REPO).count();
    let started = Instant::now();
    let mut last_report = started;
    let mut done = 0;
    for batch in todo.chunks(BATCH) {
        let tx = conn.transaction()?;
        {
            let mut update = tx.prepare(
                "UPDATE entries SET fingerprint = ?2 WHERE id = ?1 AND size = ?3 AND mtime = ?4",
            )?;
            for (id, rel, kind, size, mtime) in batch {
                let path = vol.path.join(rel);
                let fp = if kind == classify::REPO {
                    fingerprint::repo(&path)
                } else {
                    done += 1;
                    match fingerprint::file(&path, *size, *mtime) {
                        Ok(fp) => fp,
                        Err(e) => {
                            eprintln!("warning: {}: {e}", path.display());
                            None
                        }
                    }
                };
                if kind != classify::REPO {
                    if fp.is_some() {
                        stats.fingerprinted += 1;
                    } else {
                        stats.unfingerprinted += 1;
                    }
                }
                update.execute(rusqlite::params![id, fp, size, mtime])?;
            }
        }
        tx.commit()?;
        if last_report.elapsed() > Duration::from_secs(30) {
            last_report = Instant::now();
            eprintln!(
                "{}: fingerprinted {done} of {files} files ({:.0}s)",
                vol.name,
                started.elapsed().as_secs_f64()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct HashStats {
    pub hashed: u64,
    /// Already had a full hash.
    pub already: u64,
    /// Unreadable, or changed since the last scan: run `gfiles scan`, then hash again.
    pub skipped: u64,
}

/// Computes full content hashes for this machine's files in `vol`, or only those under
/// `folder` (relative to the volume root). Only reads, with the same checks as fingerprinting.
pub fn hash_volume(
    conn: &mut Connection,
    vol: &VolumeConfig,
    folder: Option<&str>,
) -> Result<HashStats> {
    let folder = folder
        .map(|f| f.trim_matches('/'))
        .filter(|f| !f.is_empty());
    if let Some(f) = folder
        && f.split('/').any(|p| p.is_empty() || p == "." || p == "..")
    {
        bail!("folder {f:?} must be a path inside the volume, like Photos/2024");
    }
    let mut stats = HashStats::default();
    let todo: Vec<(i64, String, i64, i64, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT id, path, size, mtime, content_hash IS NOT NULL FROM entries
             WHERE volume_id = ?1 AND kind != ?2 AND size > 0
               AND (?3 IS NULL OR path = ?3 OR substr(path, 1, length(?3) + 1) = ?3 || '/')",
        )?;
        stmt.query_map(rusqlite::params![vol.id, classify::REPO, folder], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    if todo.is_empty()
        && let Some(f) = folder
    {
        bail!("no cataloged files under {f:?} in volume {:?}", vol.name);
    }
    for batch in todo.chunks(BATCH) {
        let tx = conn.transaction()?;
        {
            let mut update = tx.prepare(
                "UPDATE entries SET content_hash = ?2 WHERE id = ?1 AND size = ?3 AND mtime = ?4",
            )?;
            for (id, rel, size, mtime, has_hash) in batch {
                if *has_hash {
                    stats.already += 1;
                    continue;
                }
                let path = vol.path.join(rel);
                match fingerprint::full(&path, *size, *mtime) {
                    Ok(Some(hash)) => {
                        update.execute(rusqlite::params![id, hash, size, mtime])?;
                        stats.hashed += 1;
                    }
                    Ok(None) => stats.skipped += 1,
                    Err(e) => {
                        eprintln!("warning: {}: {e}", path.display());
                        stats.skipped += 1;
                    }
                }
            }
        }
        tx.commit()?;
    }
    Ok(stats)
}

/// Whether a scan of any volume would skip `rel` (a relative, `/`-separated path) or a folder
/// above it by the built-in ignores: a hidden part, `#recycle`, `@eaDir`, and so on. A file
/// moved there would drop out of the catalog (and from `#recycle`, be deleted by the NAS), so
/// moves refuse such destinations. A volume's own patterns can bring back a hidden folder; moves
/// don't follow them, which only refuses more.
pub fn ignored_by_default(rel: &str) -> bool {
    static IGNORES: std::sync::OnceLock<Gitignore> = std::sync::OnceLock::new();
    let root = Path::new("/");
    let ignores = IGNORES
        .get_or_init(|| build_ignores(root, &[]).expect("the built-in ignore patterns are valid"));
    ignores
        .matched_path_or_any_parents(root.join(rel), false)
        .is_ignore()
}

fn build_ignores(root: &Path, extra: &[String]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in DEFAULT_IGNORES
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
    {
        builder
            .add_line(None, pattern)
            .with_context(|| format!("bad ignore pattern {pattern:?}"))?;
    }
    Ok(builder.build()?)
}

/// `path` relative to `root`, joined with `/` on every platform. `None` if any part of it
/// isn't valid UTF-8.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

pub(crate) fn unix_secs(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_the_built_in_ignores_skip() {
        for rel in [
            ".ssh/x",
            "a/.hidden/b.jpg",
            "photo/#recycle/a.jpg",
            "photo/@eaDir/a.jpg",
            "code/node_modules/x/y.js",
            "x.o",
        ] {
            assert!(ignored_by_default(rel), "{rel}");
        }
        for rel in ["photo/2013/08/a.jpg", "a.jpg", "notes/recycle/a.txt"] {
            assert!(!ignored_by_default(rel), "{rel}");
        }
    }
    use crate::db;
    use std::fs;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, rel).unwrap();
    }

    fn paths(conn: &Connection) -> Vec<(String, String)> {
        let mut stmt = conn
            .prepare("SELECT path, kind FROM entries ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn volume(root: &Path, ignore: Vec<String>) -> VolumeConfig {
        VolumeConfig {
            id: "vol-1".into(),
            name: "test".into(),
            path: root.into(),
            ignore,
        }
    }

    #[test]
    fn scans_classifies_and_ignores() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "Books/Tolkien/The Hobbit.EPUB");
        touch(root, "Photos/beach.jpg");
        touch(root, "Photos/@eaDir/beach.jpg/SYNOPHOTO_THUMB.jpg");
        touch(root, ".hidden/secret.txt");
        touch(root, ".keep/wanted.txt");
        touch(root, "notes.part");
        touch(root, "code/proj/.git/HEAD");
        touch(root, "code/proj/src/main.rs");
        touch(root, "code/proj/node_modules/x/index.js");
        touch(root, "code/app/_build/dev/app.beam");
        touch(root, "src/tool/main.o");
        touch(root, "code/go/pkg/mod/github.com/x/y@v1/y.go");

        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec!["*.part".into(), "!.keep/".into()]);
        let stats = scan_volume(&mut conn, &vol, "gaffer").unwrap();

        assert_eq!(stats.files, 3);
        assert_eq!(stats.repos, 1);
        assert_eq!(
            paths(&conn),
            vec![
                (".keep/wanted.txt".into(), "document".into()),
                ("Books/Tolkien/The Hobbit.EPUB".into(), "book".into()),
                ("Photos/beach.jpg".into(), "photo".into()),
                ("code/proj".into(), "repo".into()),
            ]
        );
    }

    #[test]
    fn the_cleanup_trash_is_never_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, ".gfiles-trash/20261116-120000-abcd/files/a.jpg");
        touch(root, "sub/.gfiles-trash/b.jpg");
        let mut conn = db::open_in_memory().unwrap();
        // Even a volume that brings back every hidden folder skips the trash at its root.
        let vol = volume(root, vec!["!.*".into()]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!(
            paths(&conn),
            vec![("sub/.gfiles-trash/b.jpg".into(), "photo".into())]
        );
    }

    #[test]
    fn rescan_removes_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "a.txt");
        touch(root, "b.txt");
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();
        touch(root, "c.txt");
        let stats = scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!(stats.removed, 1);
        let names: Vec<String> = paths(&conn).into_iter().map(|(p, _)| p).collect();
        assert_eq!(names, vec!["a.txt", "c.txt"]);

        let fts_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM entries_fts WHERE entries_fts MATCH 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_rows, 0);
    }

    /// Makes `dir` unreadable for the rest of the test, unless permissions aren't enforced
    /// (running as root), in which case it returns false and the test has nothing to check.
    #[cfg(unix)]
    fn lock(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        fs::read_dir(dir).is_err()
    }

    #[cfg(unix)]
    fn unlock(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_keeps_its_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for rel in [
            "locked/f1.txt",
            "locked/sub/f2.txt",
            "lockedness.txt",
            "b.txt",
            "c.txt",
        ] {
            touch(root, rel);
        }
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();

        fs::remove_file(root.join("c.txt")).unwrap();
        if !lock(&root.join("locked")) {
            return;
        }
        let stats = scan_volume(&mut conn, &vol, "gaffer");
        unlock(&root.join("locked"));
        let stats = stats.unwrap();
        assert_eq!((stats.errors, stats.kept, stats.removed), (1, 2, 1));
        let names: Vec<String> = paths(&conn).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            names,
            vec![
                "b.txt",
                "locked/f1.txt",
                "locked/sub/f2.txt",
                "lockedness.txt"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_root_fails_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vol");
        touch(&root, "a.txt");
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(&root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();

        if !lock(&root) {
            return;
        }
        let result = scan_volume(&mut conn, &vol, "gaffer");
        unlock(&root);
        assert!(result.unwrap_err().to_string().contains("can't read"));
        assert_eq!(paths(&conn), vec![("a.txt".into(), "document".into())]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_or_recorded() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        touch(&outside, "secret.txt");
        let root = tmp.path().join("vol");
        touch(&root, "real.txt");
        symlink(&outside, root.join("dir-link")).unwrap();
        symlink(outside.join("secret.txt"), root.join("file-link.txt")).unwrap();

        let mut conn = db::open_in_memory().unwrap();
        let stats = scan_volume(&mut conn, &volume(&root, vec![]), "gaffer").unwrap();
        assert_eq!(stats.files, 1);
        assert_eq!(paths(&conn), vec![("real.txt".into(), "document".into())]);
    }

    fn fingerprints(conn: &Connection) -> Vec<(String, Option<String>)> {
        let mut stmt = conn
            .prepare("SELECT path, fingerprint FROM entries ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn fingerprints_new_files_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a.txt"), "same").unwrap();
        fs::write(root.join("b.txt"), "same").unwrap();
        fs::write(root.join("empty.txt"), "").unwrap();
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);

        let stats = scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!((stats.fingerprinted, stats.unfingerprinted), (2, 0));
        let fps = fingerprints(&conn);
        assert!(fps[0].1.is_some());
        assert_eq!(fps[0].1, fps[1].1, "same content, same fingerprint");
        assert_eq!(fps[2], ("empty.txt".into(), None), "empty files have none");

        // Nothing changed, so nothing is read again.
        let stats = scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!(stats.fingerprinted, 0);
        assert_eq!(fingerprints(&conn), fps);
    }

    fn set_mtime(path: &Path, secs: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn a_moved_file_gets_what_its_move_carried() {
        const HASH: &str = "b3:ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for name in ["a.png", "b.png"] {
            touch(root, &format!("todo/{name}"));
            set_mtime(&root.join("todo").join(name), 1_000);
        }
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();
        let before = fingerprints(&conn);
        crate::store::record_hashes(
            &conn,
            &[crate::store::Hashed {
                volume_id: vol.id.clone(),
                path: "todo/a.png".into(),
                size: 10,
                mtime: 1_000,
                hash: HASH.into(),
            }],
        )
        .unwrap();

        // Both move (a rename keeps the mtime); b is then edited before the next scan.
        fs::create_dir(root.join("docs")).unwrap();
        let moves: Vec<crate::store::Carried> = ["a.png", "b.png"]
            .iter()
            .map(|name| crate::store::Carried {
                from_volume_id: vol.id.clone(),
                from_path: format!("todo/{name}"),
                to_volume_id: vol.id.clone(),
                to_path: format!("docs/{name}"),
                size: 10,
                mtime: 1_000,
                fingerprint: None,
                hash: None,
            })
            .collect();
        for name in ["a.png", "b.png"] {
            fs::rename(root.join("todo").join(name), root.join("docs").join(name)).unwrap();
        }
        crate::store::record_carried(&conn, &moves).unwrap();
        set_mtime(&root.join("docs/b.png"), 2_000);

        let stats = scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!(stats.fingerprinted, 1);
        let (fp, hash): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT fingerprint, content_hash FROM entries WHERE path = 'docs/a.png'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((fp, hash), (before[0].1.clone(), Some(HASH.into())));
        let hash_b: Option<String> = conn
            .query_row(
                "SELECT content_hash FROM entries WHERE path = 'docs/b.png'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash_b, None);
        let records: i64 = conn
            .query_row("SELECT count(*) FROM carried", [], |r| r.get(0))
            .unwrap();
        assert_eq!(records, 0, "the scan forgets every record it was given");
    }

    #[test]
    fn a_new_commit_updates_a_repo_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repo = root.join("code/proj");
        touch(root, "code/proj/src/main.rs");
        let head = repo.join(".git/refs/heads/main");
        fs::create_dir_all(head.parent().unwrap()).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(&head, "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();
        let before = fingerprints(&conn);
        assert_eq!(
            before[0].1.as_deref(),
            Some("git:0123456789abcdef0123456789abcdef01234567")
        );

        // A commit writes only inside .git, so the repo folder's mtime doesn't change.
        let dir_mtime = fs::metadata(&repo).unwrap().modified().unwrap();
        fs::write(&head, "89abcdef0123456789abcdef0123456789abcdef\n").unwrap();
        assert_eq!(fs::metadata(&repo).unwrap().modified().unwrap(), dir_mtime);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();
        assert_eq!(
            fingerprints(&conn)[0].1.as_deref(),
            Some("git:89abcdef0123456789abcdef0123456789abcdef")
        );
    }

    /// Every file's bytes and mtime under `root`, to show nothing was written.
    fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>, SystemTime)> {
        let mut all: Vec<_> = WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let m = e.metadata().unwrap();
                (
                    e.path().to_owned(),
                    fs::read(e.path()).unwrap(),
                    m.modified().unwrap(),
                )
            })
            .collect();
        all.sort();
        all
    }

    #[test]
    fn scanning_and_hashing_change_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "Photos/a.jpg");
        touch(root, "Photos/2024/b.jpg");
        touch(root, "notes.txt");
        let before = snapshot(root);
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(root, vec![]);
        scan_volume(&mut conn, &vol, "gaffer").unwrap();

        let stats = hash_volume(&mut conn, &vol, Some("Photos/")).unwrap();
        assert_eq!((stats.hashed, stats.already, stats.skipped), (2, 0, 0));
        let hashed: Vec<String> = conn
            .prepare("SELECT path FROM entries WHERE content_hash IS NOT NULL ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(hashed, vec!["Photos/2024/b.jpg", "Photos/a.jpg"]);
        assert_eq!(hash_volume(&mut conn, &vol, None).unwrap().already, 2);
        assert!(hash_volume(&mut conn, &vol, Some("../etc")).is_err());
        assert!(hash_volume(&mut conn, &vol, Some("Nope")).is_err());

        assert_eq!(snapshot(root), before);
    }

    #[test]
    fn refuses_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(&tmp.path().join("unmounted"), vec![]);
        assert!(scan_volume(&mut conn, &vol, "gaffer").is_err());
    }
}
