//! Walks a volume and brings its catalog rows up to date, through `store::VolumeWriter`. The
//! whole scan is one transaction, so an interrupted scan leaves the previous catalog untouched.
//!
//! A scan deletes the entries it didn't see, so it must not mistake "couldn't read" for "gone":
//! an unreadable root fails the scan, and entries under an unreadable folder or file are kept.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::Connection;
use walkdir::WalkDir;

use crate::classify;
use crate::config::VolumeConfig;
use crate::store::{Entry, Volume, VolumeWriter};

/// Patterns every volume ignores, in gitignore syntax. A volume's own `ignore` patterns are
/// applied after these, so `!.something` can bring back a hidden file or folder.
const DEFAULT_IGNORES: &[&str] = &[
    ".*", // dotfiles and dot-folders: app state, caches, .DS_Store
    "node_modules/",
    "__pycache__/",
    "target/",
    "@eaDir/",     // Synology thumbnails and metadata
    "\\#recycle/", // Synology recycle bin
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
        if ignores.matched(entry.path(), is_dir).is_ignore() {
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
        let ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase);
        let kind = classify::kind_for_ext(ext.as_deref()).to_string();
        writer.put(&Entry {
            path: rel,
            name,
            ext,
            kind,
            size: meta.len() as i64,
            mtime: unix_secs(meta.modified().ok()),
        })?;
        stats.files += 1;
    }

    let scanned_at = unix_secs(Some(SystemTime::now()));
    let finished = match &unreadable {
        Some(list) => writer.finish(scanned_at, list)?,
        // An error with no path: keep everything not seen rather than guess.
        None => writer.finish(scanned_at, &[String::new()])?,
    };
    stats.removed = finished.removed;
    stats.kept = finished.kept;
    tx.commit()?;
    Ok(stats)
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

    #[test]
    fn refuses_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut conn = db::open_in_memory().unwrap();
        let vol = volume(&tmp.path().join("unmounted"), vec![]);
        assert!(scan_volume(&mut conn, &vol, "gaffer").is_err());
    }
}
