//! Content fingerprints, computed by the machine that holds the file.
//!
//! - A file's fingerprint (`p1:<hex>`) is BLAKE3 over its size and three 64 KiB samples (start,
//!   middle, end); a file of up to three samples is hashed whole. Equal fingerprints mean
//!   "probably the same content": cheap to compute on a spinning disk, but not proof.
//! - A full hash (`b3:<hex>`) is BLAKE3 over the whole file, computed on request. Equal full
//!   hashes confirm a duplicate.
//! - A repository's fingerprint (`git:<sha>`) is its HEAD commit, read from `.git` without a
//!   git library.
//!
//! These only read. A file is hashed only if it is still the regular file the catalog saw:
//! same size and mtime, not a symlink (checked by path and again on the open file), and
//! unchanged by the time the read ends. Anything else returns `Ok(None)`, and the entry waits
//! for the next scan.

use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::platform;
use crate::scan::unix_secs;

/// Bytes read at each sample point.
pub const SAMPLE: u64 = 64 * 1024;

/// The sampled fingerprint of the file at `path`, if it still has `size` and `mtime`.
pub fn file(path: &Path, size: i64, mtime: i64) -> io::Result<Option<String>> {
    let Some(mut f) = open_unchanged(path, size, mtime)? else {
        return Ok(None);
    };
    let len = size as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());
    if len <= 3 * SAMPLE {
        if !hash_exactly(&mut f, &mut hasher, len)? {
            return Ok(None);
        }
    } else {
        for offset in [0, len / 2 - SAMPLE / 2, len - SAMPLE] {
            f.seek(SeekFrom::Start(offset))?;
            if !hash_exactly(&mut f, &mut hasher, SAMPLE)? {
                return Ok(None);
            }
        }
    }
    if !still_unchanged(&f, size, mtime)? {
        return Ok(None);
    }
    Ok(Some(format!("p1:{}", hasher.finalize().to_hex())))
}

/// The full content hash of the file at `path`, if it still has `size` and `mtime`.
pub fn full(path: &Path, size: i64, mtime: i64) -> io::Result<Option<String>> {
    let Some(mut f) = open_unchanged(path, size, mtime)? else {
        return Ok(None);
    };
    let mut hasher = blake3::Hasher::new();
    if !hash_exactly(&mut f, &mut hasher, size as u64)? || !still_unchanged(&f, size, mtime)? {
        return Ok(None);
    }
    Ok(Some(format!("b3:{}", hasher.finalize().to_hex())))
}

/// The HEAD commit of the repository at `dir` as `git:<sha>`, or `None` when it has no commits
/// yet or its `.git` can't be understood.
pub fn repo(dir: &Path) -> Option<String> {
    let git_dir = git_dir(dir)?;
    let head = read_small(&git_dir.join("HEAD"), SMALL)?;
    let head = head.trim();
    let sha = match head.strip_prefix("ref:") {
        None => head.to_string(),
        Some(name) => {
            let name = name.trim();
            if name.split('/').any(|p| p.is_empty() || p == "..") {
                return None;
            }
            // Refs live in the common git dir for a linked worktree.
            let common = read_small(&git_dir.join("commondir"), SMALL)
                .map(|c| git_dir.join(c.trim()))
                .unwrap_or_else(|| git_dir.clone());
            [&git_dir, &common]
                .iter()
                .find_map(|d| read_small(&d.join(name), SMALL))
                .map(|s| s.trim().to_string())
                .or_else(|| packed_ref(&common, name))?
        }
    };
    is_sha(&sha).then(|| format!("git:{sha}"))
}

fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if meta.is_file() {
        // A worktree or submodule: `.git` is a file saying `gitdir: <path>`.
        let text = read_small(&dot_git, SMALL)?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        return Some(dir.join(target));
    }
    None
}

fn packed_ref(git_dir: &Path, name: &str) -> Option<String> {
    let text = read_small(&git_dir.join("packed-refs"), PACKED_REFS_MAX)?;
    text.lines().find_map(|line| {
        let (sha, r) = line.split_once(' ')?;
        (r == name).then(|| sha.to_string())
    })
}

/// Largest HEAD, ref, `commondir` or `.git` file read.
const SMALL: u64 = 4096;
/// Largest `packed-refs` read (a repository with many tags can have a big one).
const PACKED_REFS_MAX: u64 = 64 << 20;

/// A regular file's text, up to `max` bytes; `None` for a symlink, a FIFO or other special
/// file, a directory, or anything larger. The path is checked before opening, so a FIFO is
/// never opened (which could block), and the open file is checked again.
fn read_small(path: &Path, max: u64) -> Option<String> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let f = platform::open_no_follow(path).ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let mut s = String::new();
    f.take(max).read_to_string(&mut s).ok()?;
    Some(s)
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Opens `path` only if it is a regular file (not a symlink) with the expected size and mtime.
fn open_unchanged(path: &Path, size: i64, mtime: i64) -> io::Result<Option<File>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !matches_entry(&meta, size, mtime) {
        return Ok(None);
    }
    let f = match platform::open_no_follow(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        // A symlink swapped in since the check above (ELOOP on Linux).
        Err(_) if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    // The open file, not the path: whatever was opened must itself be the file we expect.
    if !still_unchanged(&f, size, mtime)? {
        return Ok(None);
    }
    Ok(Some(f))
}

fn still_unchanged(f: &File, size: i64, mtime: i64) -> io::Result<bool> {
    Ok(matches_entry(&f.metadata()?, size, mtime))
}

fn matches_entry(meta: &Metadata, size: i64, mtime: i64) -> bool {
    meta.is_file() && meta.len() as i64 == size && unix_secs(meta.modified().ok()) == mtime
}

/// Feeds exactly `len` bytes from `f` to `hasher`; false if the file ended early.
fn hash_exactly(f: &mut File, hasher: &mut blake3::Hasher, len: u64) -> io::Result<bool> {
    let copied = io::copy(&mut f.take(len), &mut HashWriter(hasher))?;
    Ok(copied == len)
}

struct HashWriter<'a>(&'a mut blake3::Hasher);

impl io::Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Size and mtime of `path`, as a scan would record them.
    fn stat(path: &Path) -> (i64, i64) {
        let m = fs::metadata(path).unwrap();
        (m.len() as i64, unix_secs(m.modified().ok()))
    }

    fn fp(path: &Path) -> Option<String> {
        let (size, mtime) = stat(path);
        file(path, size, mtime).unwrap()
    }

    fn big(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn same_content_same_fingerprint_whatever_the_name() {
        let tmp = tempfile::tempdir().unwrap();
        let data = big(1_000_000);
        fs::write(tmp.path().join("a.mkv"), &data).unwrap();
        fs::write(tmp.path().join("copy of a.mkv"), &data).unwrap();
        let a = fp(&tmp.path().join("a.mkv")).unwrap();
        assert!(a.starts_with("p1:") && a.len() == 67);
        assert_eq!(Some(a.clone()), fp(&tmp.path().join("copy of a.mkv")));

        // A change inside the middle sample changes it.
        let mut changed = data.clone();
        changed[500_000] ^= 1;
        fs::write(tmp.path().join("b.mkv"), &changed).unwrap();
        assert_ne!(Some(a.clone()), fp(&tmp.path().join("b.mkv")));

        // A change outside every sample doesn't: that is what "probable" means.
        let mut unsampled = data;
        unsampled[200_000] ^= 1;
        fs::write(tmp.path().join("c.mkv"), &unsampled).unwrap();
        assert_eq!(Some(a), fp(&tmp.path().join("c.mkv")));
    }

    #[test]
    fn a_small_file_is_hashed_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let data = big(150_000);
        let mut other = data.clone();
        other[100_000] ^= 1; // between where samples would be
        fs::write(tmp.path().join("a"), &data).unwrap();
        fs::write(tmp.path().join("b"), &other).unwrap();
        assert_ne!(fp(&tmp.path().join("a")), fp(&tmp.path().join("b")));
    }

    #[test]
    fn full_hash_covers_every_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let data = big(1_000_000);
        let mut unsampled = data.clone();
        unsampled[200_000] ^= 1;
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, &data).unwrap();
        fs::write(&b, &unsampled).unwrap();
        let (size, mtime) = stat(&a);
        let ha = full(&a, size, mtime).unwrap().unwrap();
        assert!(ha.starts_with("b3:") && ha.len() == 67);
        let (size, mtime) = stat(&b);
        assert_ne!(Some(ha), full(&b, size, mtime).unwrap());
    }

    #[test]
    fn a_changed_or_missing_file_is_not_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.txt");
        fs::write(&path, "hello").unwrap();
        let (size, mtime) = stat(&path);
        assert!(file(&path, size, mtime).unwrap().is_some());
        assert_eq!(file(&path, size + 1, mtime).unwrap(), None);
        assert_eq!(file(&path, size, mtime - 5).unwrap(), None);
        assert_eq!(full(&path, size, mtime - 5).unwrap(), None);
        assert_eq!(file(&tmp.path().join("gone"), size, mtime).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_read() {
        // The walk saw a regular file; by the time the pass runs it has been replaced by a
        // symlink to a file outside the volume with the same size and mtime.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        let (size, mtime) = stat(&outside);
        let inside = tmp.path().join("inside.txt");
        std::os::unix::fs::symlink(&outside, &inside).unwrap();
        assert_eq!(file(&inside, size, mtime).unwrap(), None);
        assert_eq!(full(&inside, size, mtime).unwrap(), None);
    }

    fn git(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(".git").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA2: &str = "89abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn repo_head_from_loose_packed_and_detached() {
        let tmp = tempfile::tempdir().unwrap();
        let loose = tmp.path().join("loose");
        git(&loose, "HEAD", "ref: refs/heads/main\n");
        git(&loose, "refs/heads/main", &format!("{SHA}\n"));
        assert_eq!(repo(&loose), Some(format!("git:{SHA}")));

        let packed = tmp.path().join("packed");
        git(&packed, "HEAD", "ref: refs/heads/main\n");
        git(
            &packed,
            "packed-refs",
            &format!("# pack-refs with: peeled\n{SHA2} refs/heads/other\n{SHA} refs/heads/main\n"),
        );
        assert_eq!(repo(&packed), Some(format!("git:{SHA}")));

        let detached = tmp.path().join("detached");
        git(&detached, "HEAD", &format!("{SHA2}\n"));
        assert_eq!(repo(&detached), Some(format!("git:{SHA2}")));

        let empty = tmp.path().join("empty");
        git(&empty, "HEAD", "ref: refs/heads/main\n");
        assert_eq!(repo(&empty), None);

        let escape = tmp.path().join("escape");
        git(&escape, "HEAD", "ref: ../../../etc/passwd\n");
        assert_eq!(repo(&escape), None);
    }

    /// Nothing inside `.git` is followed out through a symlink, and a FIFO there can't hang the
    /// scan: both just leave the repo without a fingerprint.
    #[cfg(unix)]
    #[test]
    fn repo_never_follows_a_symlink_or_opens_a_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::write(&outside, format!("{SHA} refs/heads/main\n")).unwrap();

        let linked = tmp.path().join("linked");
        git(&linked, "HEAD", "ref: refs/heads/main\n");
        std::os::unix::fs::symlink(&outside, linked.join(".git/packed-refs")).unwrap();
        assert_eq!(repo(&linked), None);

        let piped = tmp.path().join("piped");
        fs::create_dir_all(piped.join(".git")).unwrap();
        let head = std::ffi::CString::new(piped.join(".git/HEAD").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(head.as_ptr(), 0o600) }, 0);
        assert_eq!(
            repo(&piped),
            None,
            "returns instead of blocking on the FIFO"
        );
    }
}
