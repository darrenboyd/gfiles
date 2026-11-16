//! The few things that differ by operating system. Everything Unix-only lives here, so the rest
//! of gfiles stays portable (Windows isn't supported yet, but nothing should rule it out).

use std::fs::File;
use std::io;
use std::path::Path;

/// Opens `path` for reading without following a symlink at its last component: a symlink fails
/// to open on Unix instead of opening whatever it points to. The open doesn't block either, so
/// a FIFO swapped in for a file can't hang the caller (reads of regular files are unaffected).
/// Callers still check what they opened, since other platforms only get a plain open.
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

/// Filesystem types that are another machine's disk seen over the network.
const NETWORK_FS: &[&str] = &["cifs", "smb3", "smbfs", "nfs", "nfs4", "fuse.sshfs", "9p"];

/// The network filesystem type `path` is on (like `"cifs"`), if it's on one. Linux only; elsewhere
/// always `None`. `path` should be absolute and canonical.
pub fn network_fs(path: &Path) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        mount_fs_type(&mountinfo, path).filter(|fs| NETWORK_FS.contains(&fs.as_str()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// The filesystem type of the mount holding `path`, from `/proc/self/mountinfo` text: the
/// longest mount point containing it, the last one listed when mounts stack (autofs then cifs).
fn mount_fs_type(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let (Some(mount_point), Some(sep)) = (fields.get(4), fields.iter().position(|f| *f == "-"))
        else {
            continue;
        };
        let Some(fs_type) = fields.get(sep + 1) else {
            continue;
        };
        let mount_point = unescape_octal(mount_point);
        if !path.starts_with(&mount_point) {
            continue;
        }
        let depth = Path::new(&mount_point).components().count();
        if best.as_ref().is_none_or(|(d, _)| depth >= *d) {
            best = Some((depth, fs_type.to_string()));
        }
    }
    best.map(|(_, fs)| fs)
}

/// Mountinfo writes space, tab, newline and backslash as `\040`-style octal escapes.
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let n = (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(n);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
40 22 0:38 / /mnt/darren rw,relatime shared:20 - autofs systemd-1 rw,fd=50
41 40 0:52 / /mnt/darren rw,relatime shared:21 - cifs //elrond/darren rw,vers=3.1.1
42 22 0:53 / /media/my\\040disk rw - nfs4 elrond:/volume1/Media rw
43 22 0:54 / /home/darren/remote rw - fuse.sshfs darren@elrond: rw
";

    #[test]
    fn finds_the_mount_holding_a_path() {
        let fs = |p: &str| mount_fs_type(MOUNTINFO, Path::new(p));
        assert_eq!(fs("/home/darren/Books").as_deref(), Some("ext4"));
        assert_eq!(
            fs("/mnt/darren/Drive").as_deref(),
            Some("cifs"),
            "stacked: last wins"
        );
        assert_eq!(fs("/media/my disk/Photos").as_deref(), Some("nfs4"));
        assert_eq!(fs("/home/darren/remote").as_deref(), Some("fuse.sshfs"));
        assert_eq!(
            fs("/home/darren/remotely").as_deref(),
            Some("ext4"),
            "whole components"
        );
    }

    #[test]
    fn unescapes_mount_points() {
        assert_eq!(unescape_octal("/a\\040b\\134c"), "/a b\\c");
        assert_eq!(unescape_octal("/trailing\\04"), "/trailing\\04");
    }

    #[cfg(unix)]
    #[test]
    fn does_not_open_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, "x").unwrap();
        let link = tmp.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_no_follow(&target).is_ok());
        assert!(open_no_follow(&link).is_err());
    }
}
