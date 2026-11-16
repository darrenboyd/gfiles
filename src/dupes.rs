//! Duplicates and backup coverage, across every machine in the catalog. Shared by the CLI, the
//! TUI and the MCP server.
//!
//! Two entries hold the same content when their size and fingerprint match (see
//! `fingerprint`). That's "probable" until full hashes of every copy agree ("confirmed");
//! repositories match by HEAD commit, which is exact. Mirror replicas (see `mirror`) are one
//! logical file: never duplicates of each other, and one place for coverage.
//!
//! A file is covered when copies of its content are in at least two places and on at least two
//! machines. A place is a machine, or a mirror as a whole, since a sync tool copies deletes and
//! damage to every replica. Each distinct content is judged separately, so a mirror file whose
//! replicas differ can be partly covered.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::mirror::{JOIN, KEY, MIRROR_PATH, PLACE};
use crate::search::{self, Query};

#[derive(Debug, Default, Clone)]
pub struct Filters {
    pub kind: Option<String>,
    pub volume: Option<String>,
    pub machine: Option<String>,
    /// Bytes; files smaller than this are left out.
    pub min_size: i64,
    pub limit: Option<usize>,
}

impl Filters {
    /// Refuses a volume or machine that isn't in the catalog, as search does.
    pub fn check(&self, conn: &Connection) -> Result<()> {
        search::check_filters(
            conn,
            &Query {
                volume: self.volume.clone(),
                machine: self.machine.clone(),
                ..Default::default()
            },
        )
    }
}

/// One place a file is stored.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Location {
    pub machine: String,
    pub volume: String,
    pub path: String,
    pub abs_path: String,
}

/// One logical copy: a single file, or a mirror file with its replicas.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Copy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// Path inside the mirror, for a mirror file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror_path: Option<String>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub size: i64,
    pub kind: String,
    /// Every copy has the same full hash (or they're repos at the same commit).
    pub confirmed: bool,
    /// Bytes that removing all but one copy would free.
    pub wasted: i64,
    pub copies: Vec<Copy>,
}

#[derive(Debug, Serialize)]
pub struct Duplicates {
    pub total_groups: i64,
    pub groups: Vec<Group>,
    /// Mirror folders left out because their volume isn't in the catalog yet: their replicas
    /// count as separate copies until it is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Entries that can match others: fingerprinted, and not empty (an empty file matches every
/// other empty file) unless it's a repository, which matches by commit.
const ELIGIBLE: &str = "e.fingerprint IS NOT NULL AND (e.size > 0 OR e.fingerprint LIKE 'git:%')";

/// The entry columns `read_row` reads, with the mirror join. `conditions` adds to the WHERE.
fn rows_sql(conditions: &str) -> String {
    format!(
        "SELECT e.size, e.fingerprint, e.content_hash, e.kind, v.machine, v.name, v.root, e.path,
                mf.mirror, {MIRROR_PATH}, {KEY}
         FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         WHERE {ELIGIBLE}{conditions}"
    )
}

struct Row {
    size: i64,
    fingerprint: String,
    hash: Option<String>,
    kind: String,
    location: Location,
    mirror: Option<String>,
    mirror_path: Option<String>,
    key: String,
}

fn read_row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    let root: String = r.get(6)?;
    let path: String = r.get(7)?;
    Ok(Row {
        size: r.get(0)?,
        fingerprint: r.get(1)?,
        hash: r.get(2)?,
        kind: r.get(3)?,
        location: Location {
            machine: r.get(4)?,
            volume: r.get(5)?,
            abs_path: search::join_root(&root, &path),
            path,
        },
        mirror: r.get(8)?,
        mirror_path: r.get(9)?,
        key: r.get(10)?,
    })
}

/// Groups of files with the same content, largest wasted space first. A group matches the
/// filters when any of its copies does.
pub fn duplicates(conn: &Connection, f: &Filters) -> Result<Duplicates> {
    let groups_sql = format!(
        "SELECT size, fp, count(DISTINCT key) AS n FROM (
             SELECT e.size AS size, e.fingerprint AS fp, {KEY} AS key, e.kind AS kind,
                    v.machine AS machine, v.name AS volume
             FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
             WHERE e.fingerprint IS NOT NULL AND (e.size > 0 OR e.fingerprint LIKE 'git:%'))
         GROUP BY size, fp
         HAVING n >= 2 AND size >= ?1
            AND (?2 IS NULL OR max(kind = ?2)) AND (?3 IS NULL OR max(machine = ?3))
            AND (?4 IS NULL OR max(volume = ?4))"
    );
    let args = params![f.min_size, f.kind, f.machine, f.volume];
    let total_groups: i64 =
        conn.query_row(&format!("SELECT count(*) FROM ({groups_sql})"), args, |r| {
            r.get(0)
        })?;
    let mut sql = format!("{groups_sql} ORDER BY size * (n - 1) DESC, size DESC, fp");
    if let Some(limit) = f.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    let page: Vec<(i64, String)> = conn
        .prepare(&sql)?
        .query_map(args, |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut members = conn.prepare(&rows_sql(
        " AND e.size = ?1 AND e.fingerprint = ?2 ORDER BY v.machine, v.name, e.path",
    ))?;
    let mut groups = Vec::new();
    for (size, fp) in page {
        let rows: Vec<Row> = members
            .query_map(params![size, fp], read_row)?
            .collect::<rusqlite::Result<_>>()?;
        let copies = collapse(&rows);
        groups.push(Group {
            size,
            kind: rows[0].kind.clone(),
            confirmed: confirmed(&rows),
            wasted: size * (copies.len() as i64 - 1),
            copies,
        });
    }
    Ok(Duplicates {
        total_groups,
        groups,
        warnings: crate::mirror::warnings(conn)?,
    })
}

/// Whether rows of one content are certainly the same: full hashes all present and equal, or
/// repositories at the same commit.
fn confirmed(rows: &[Row]) -> bool {
    agree(&rows[0].fingerprint, rows.iter().map(|r| &r.hash))
}

fn agree<'a>(fingerprint: &str, mut hashes: impl Iterator<Item = &'a Option<String>>) -> bool {
    if fingerprint.starts_with("git:") {
        return true;
    }
    let Some(Some(first)) = hashes.next() else {
        return false;
    };
    hashes.all(|h| h.as_ref() == Some(first))
}

/// Rows grouped into logical copies, in first-seen order.
fn collapse(rows: &[Row]) -> Vec<Copy> {
    let mut copies: Vec<(String, Copy)> = Vec::new();
    for r in rows {
        match copies.iter_mut().find(|(k, _)| *k == r.key) {
            Some((_, c)) => c.locations.push(r.location.clone()),
            None => copies.push((
                r.key.clone(),
                Copy {
                    mirror: r.mirror.clone(),
                    mirror_path: r.mirror_path.clone(),
                    locations: vec![r.location.clone()],
                },
            )),
        }
    }
    copies.into_iter().map(|(_, c)| c).collect()
}

/// A count of files and their bytes.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq)]
pub struct Tally {
    pub files: i64,
    pub bytes: i64,
}

impl Tally {
    fn add(&mut self, bytes: i64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

#[derive(Debug, Serialize)]
pub struct CoverageFolder {
    /// `machine:volume/folder`, or `mirror:name/folder` for a folder inside a mirror. Pass it
    /// back as the folder to list its files.
    pub folder: String,
    /// Where it is on disk (the first replica, for a mirror).
    pub abs_folder: String,
    /// Copies in only one place.
    pub uncovered: Tally,
    /// Copies in two places, but matched only by sampled fingerprint, not full hash.
    pub probably_covered: Tally,
}

#[derive(Debug, Serialize)]
pub struct CoverageFile {
    pub copy: Copy,
    pub size: i64,
    pub kind: String,
    /// "uncovered" or "probably covered".
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    pub covered: Tally,
    pub probably_covered: Tally,
    pub uncovered: Tally,
    /// Files with no fingerprint yet (new, or unreadable when last scanned): unknown.
    pub not_yet_fingerprinted: Tally,
    pub total_folders: i64,
    pub folders: Vec<CoverageFolder>,
    /// The files in the requested folder that need a backup, when one was given, up to the
    /// limit, biggest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<CoverageFile>>,
    /// How many files the requested folder has that need a backup (the list may be cut off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<i64>,
    /// Mirror folders left out because their volume isn't in the catalog yet: their replicas
    /// count as separate places until it is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Which files have no backup, grouped by folder, most uncovered bytes first. With `folder`
/// (a `folder` value from an earlier result), also lists that folder's files that need one,
/// up to the limit.
pub fn coverage(conn: &Connection, f: &Filters, folder: Option<&str>) -> Result<Coverage> {
    // Where each content is (places, machines, whether full hashes agree) is aggregated in
    // SQL over the whole catalog, since a copy outside the filters still counts as a backup.
    // Only the filtered rows come back, one at a time, each with its status.
    let sql = format!(
        "WITH spread AS (
             SELECT e.size, e.fingerprint,
                    count(DISTINCT {PLACE}) >= 2 AND count(DISTINCT v.machine) >= 2 AS covered,
                    e.fingerprint LIKE 'git:%'
                        OR (count(e.content_hash) = count(*)
                            AND min(e.content_hash) = max(e.content_hash)) AS sure
             FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
             WHERE {ELIGIBLE}
             GROUP BY e.size, e.fingerprint)
         SELECT e.size, e.fingerprint, e.content_hash, e.kind, v.machine, v.name, v.root, e.path,
                mf.mirror, {MIRROR_PATH}, {KEY}, s.covered, s.sure
         FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
         JOIN spread s ON s.size = e.size AND s.fingerprint = e.fingerprint
         WHERE {ELIGIBLE} AND e.size >= ?1 AND (?2 IS NULL OR e.kind = ?2)
           AND (?3 IS NULL OR v.machine = ?3) AND (?4 IS NULL OR v.name = ?4)"
    );
    let mut out = Coverage {
        covered: Tally::default(),
        probably_covered: Tally::default(),
        uncovered: Tally::default(),
        not_yet_fingerprinted: not_fingerprinted(conn, f)?,
        total_folders: 0,
        folders: Vec::new(),
        files: None,
        total_files: None,
        warnings: crate::mirror::warnings(conn)?,
    };
    // Each logical file counts once per status, however many replicas it has. Only mirror
    // keys repeat; replicas that diverged can have different content, so different statuses.
    let mut seen: HashSet<(Status, String)> = HashSet::new();
    let mut folders: BTreeMap<String, CoverageFolder> = BTreeMap::new();
    // The requested folder's files, and every replica of each mirror file listed.
    let mut files: BTreeMap<(Status, String), CoverageFile> = BTreeMap::new();
    let mut replicas: HashMap<String, Vec<Location>> = HashMap::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![f.min_size, f.kind, f.machine, f.volume])?;
    while let Some(row) = rows.next()? {
        let r = read_row(row)?;
        let status = match (row.get::<_, bool>(11)?, row.get::<_, bool>(12)?) {
            (true, true) => Status::Covered,
            (true, false) => Status::Probable,
            (false, _) => Status::Uncovered,
        };
        if r.mirror.is_some() {
            if folder.is_some() {
                replicas
                    .entry(r.key.clone())
                    .or_default()
                    .push(r.location.clone());
            }
            if !seen.insert((status, r.key.clone())) {
                continue;
            }
        }
        let tally = match status {
            Status::Covered => {
                out.covered.add(r.size);
                continue;
            }
            Status::Probable => &mut out.probably_covered,
            Status::Uncovered => &mut out.uncovered,
        };
        tally.add(r.size);
        let (id, abs) = folder_of(&r);
        let entry = folders.entry(id.clone()).or_insert_with(|| CoverageFolder {
            folder: id.clone(),
            abs_folder: abs,
            uncovered: Tally::default(),
            probably_covered: Tally::default(),
        });
        match status {
            Status::Probable => entry.probably_covered.add(r.size),
            _ => entry.uncovered.add(r.size),
        }
        if Some(id.as_str()) == folder {
            files.insert(
                (status, r.key),
                CoverageFile {
                    copy: Copy {
                        mirror: r.mirror,
                        mirror_path: r.mirror_path,
                        locations: vec![r.location],
                    },
                    size: r.size,
                    kind: r.kind,
                    status: status.label(),
                },
            );
        }
    }
    let mut folders: Vec<CoverageFolder> = folders.into_values().collect();
    folders.sort_by(|a, b| {
        (b.uncovered.bytes, b.probably_covered.bytes, &a.folder).cmp(&(
            a.uncovered.bytes,
            a.probably_covered.bytes,
            &b.folder,
        ))
    });
    if let Some(wanted) = folder {
        if !folders.iter().any(|c| c.folder == wanted) {
            anyhow::bail!(
                "no files needing a backup in folder {wanted:?}; pass a folder value from the coverage report"
            );
        }
        let mut list: Vec<CoverageFile> = files
            .into_iter()
            .map(|((_, key), mut file)| {
                if let Some(all) = replicas.remove(&key) {
                    file.copy.locations = all;
                }
                file.copy
                    .locations
                    .sort_by(|a, b| (&a.machine, &a.path).cmp(&(&b.machine, &b.path)));
                file
            })
            .collect();
        list.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.copy.locations[0].path.cmp(&b.copy.locations[0].path))
        });
        out.total_files = Some(list.len() as i64);
        if let Some(limit) = f.limit {
            list.truncate(limit);
        }
        out.files = Some(list);
    }
    out.total_folders = folders.len() as i64;
    if let Some(limit) = f.limit {
        folders.truncate(limit);
    }
    out.folders = folders;
    Ok(out)
}

/// A file's backup status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Status {
    Uncovered,
    Probable,
    Covered,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Uncovered => "uncovered",
            Status::Probable => "probably covered",
            Status::Covered => "covered",
        }
    }
}

/// The folder a row is reported under, and where that is on disk.
fn folder_of(r: &Row) -> (String, String) {
    let parent = |p: &str| {
        p.rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string())
    };
    let abs = r
        .location
        .abs_path
        .rsplit_once(['/', '\\'])
        .map_or(String::new(), |(d, _)| d.to_string());
    match (&r.mirror, &r.mirror_path) {
        (Some(m), Some(mp)) => (join_folder(&format!("mirror:{m}"), &parent(mp)), abs),
        _ => (
            join_folder(
                &format!("{}:{}", r.location.machine, r.location.volume),
                &parent(&r.location.path),
            ),
            abs,
        ),
    }
}

fn join_folder(base: &str, dir: &str) -> String {
    if dir.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{dir}")
    }
}

/// Logical files (a mirror file once) none of whose matching copies has a fingerprint yet.
fn not_fingerprinted(conn: &Connection, f: &Filters) -> Result<Tally> {
    Ok(conn.query_row(
        &format!(
            "SELECT count(*), coalesce(sum(size), 0) FROM (
                 SELECT max(e.size) AS size
                 FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
                 WHERE (e.size > 0 OR e.kind = 'repo') AND e.size >= ?1
                   AND (?2 IS NULL OR e.kind = ?2) AND (?3 IS NULL OR v.machine = ?3)
                   AND (?4 IS NULL OR v.name = ?4)
                 GROUP BY {KEY}
                 HAVING max(e.fingerprint IS NOT NULL) = 0)"
        ),
        params![f.min_size, f.kind, f.machine, f.volume],
        |r| {
            Ok(Tally {
                files: r.get(0)?,
                bytes: r.get(1)?,
            })
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::{Entry, Volume, VolumeWriter};

    const A: &str = "p1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "p1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "p1:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const HASH: &str = "b3:1111111111111111111111111111111111111111111111111111111111111111";
    const GIT: &str = "git:0123456789abcdef0123456789abcdef01234567";

    /// (path, size, fingerprint, full hash)
    type Spec<'a> = (&'a str, i64, Option<&'a str>, Option<&'a str>);

    fn put(conn: &Connection, machine: &str, volume: &str, entries: &[Spec]) {
        let vol = Volume {
            id: format!("{machine}-{volume}"),
            name: volume.into(),
            machine: machine.into(),
            root: format!("/{machine}/{volume}"),
            last_scan_at: None,
        };
        let mut w = VolumeWriter::begin(conn, &vol).unwrap();
        for (path, size, fp, hash) in entries {
            let kind = if fp.is_some_and(|f| f.starts_with("git:")) {
                "repo"
            } else {
                "document"
            };
            w.put(&Entry {
                path: path.to_string(),
                name: path.rsplit('/').next().unwrap().into(),
                ext: None,
                kind: kind.into(),
                size: *size,
                mtime: 0,
                fingerprint: fp.map(Into::into),
                content_hash: hash.map(Into::into),
            })
            .unwrap();
        }
        w.finish(0, &[]).unwrap();
    }

    fn with_drive_mirror(conn: &Connection) {
        let config: Config = toml::from_str(
            "machine = \"gaffer\"\n[[mirror]]\nname = \"drive\"\nfolders = [\"gaffer:darren/Drive\", \"elrond:darren/Drive\"]\n",
        )
        .unwrap();
        crate::mirror::load(conn, &config).unwrap();
    }

    fn all() -> Filters {
        Filters::default()
    }

    fn paths(c: &Copy) -> Vec<String> {
        c.locations
            .iter()
            .map(|l| format!("{}:{}", l.machine, l.path))
            .collect()
    }

    #[test]
    fn same_content_on_two_machines_is_a_covered_duplicate() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Books/x.epub", 10, Some(A), None)],
        );
        put(
            &conn,
            "elrond",
            "Media",
            &[
                ("Books/x.epub", 10, Some(A), None),
                ("y.epub", 10, Some(B), None),
            ],
        );
        let d = duplicates(&conn, &all()).unwrap();
        assert_eq!(d.total_groups, 1);
        assert_eq!(d.groups[0].wasted, 10);
        assert!(!d.groups[0].confirmed);

        let c = coverage(&conn, &all(), None).unwrap();
        // Each copy is a file with a backup: gaffer's and elrond's.
        assert_eq!(c.probably_covered.files, 2);
        assert_eq!(
            c.uncovered,
            Tally {
                files: 1,
                bytes: 10
            }
        );
        assert_eq!(c.folders[0].folder, "elrond:Media");
    }

    #[test]
    fn full_hashes_confirm() {
        let conn = crate::db::open_in_memory().unwrap();
        put(&conn, "gaffer", "darren", &[("x", 10, Some(A), Some(HASH))]);
        put(&conn, "elrond", "Media", &[("x", 10, Some(A), Some(HASH))]);
        assert!(duplicates(&conn, &all()).unwrap().groups[0].confirmed);
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!((c.covered.files, c.probably_covered.files), (2, 0));
        assert!(c.folders.is_empty());
    }

    #[test]
    fn mirror_replicas_are_one_uncovered_file_not_duplicates() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Drive/Taxes/2024.pdf", 10, Some(A), None)],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[("Drive/Taxes/2024.pdf", 10, Some(A), None)],
        );
        with_drive_mirror(&conn);
        assert_eq!(duplicates(&conn, &all()).unwrap().total_groups, 0);
        let c = coverage(&conn, &all(), Some("mirror:drive/Taxes")).unwrap();
        assert_eq!(
            c.uncovered,
            Tally {
                files: 1,
                bytes: 10
            }
        );
        assert_eq!(c.folders[0].folder, "mirror:drive/Taxes");
        let files = c.files.unwrap();
        assert_eq!(
            paths(&files[0].copy),
            vec!["elrond:Drive/Taxes/2024.pdf", "gaffer:Drive/Taxes/2024.pdf"]
        );
    }

    #[test]
    fn a_copy_outside_the_mirror_is_a_backup() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Drive/Taxes/2024.pdf", 10, Some(A), None)],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/Taxes/2024.pdf", 10, Some(A), None),
                ("Archive/2024.pdf", 10, Some(A), None),
            ],
        );
        with_drive_mirror(&conn);
        let d = duplicates(&conn, &all()).unwrap();
        assert_eq!(d.total_groups, 1);
        let copies = &d.groups[0].copies;
        assert_eq!(copies.len(), 2);
        assert_eq!(paths(&copies[0]), vec!["elrond:Archive/2024.pdf"]);
        assert_eq!(copies[1].mirror.as_deref(), Some("drive"));
        assert_eq!(copies[1].locations.len(), 2);
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!(c.uncovered.files, 0);
        assert_eq!(
            c.probably_covered.files, 2,
            "the mirror file and the archive copy"
        );
    }

    #[test]
    fn diverged_replicas_are_judged_by_content() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Drive/a.pdf", 10, Some(A), None)],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/a.pdf", 10, Some(B), None),
                ("Archive/a.pdf", 10, Some(A), None),
            ],
        );
        with_drive_mirror(&conn);
        let c = coverage(&conn, &all(), Some("mirror:drive")).unwrap();
        // Content A is backed up in Archive; elrond's diverged content B is not.
        assert_eq!(
            c.uncovered,
            Tally {
                files: 1,
                bytes: 10
            }
        );
        assert_eq!(
            paths(&c.files.unwrap()[0].copy),
            vec!["elrond:Drive/a.pdf", "gaffer:Drive/a.pdf"]
        );
    }

    #[test]
    fn a_mirror_on_one_machine_is_not_a_backup() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Drive/a.pdf", 10, Some(A), None),
                ("Other/a.pdf", 10, Some(A), None),
            ],
        );
        put(
            &conn,
            "gaffer",
            "backup",
            &[("Drive/a.pdf", 10, Some(A), None)],
        );
        let config: Config = toml::from_str(
            "machine = \"gaffer\"\n[[mirror]]\nname = \"m\"\nfolders = [\"gaffer:darren/Drive\", \"gaffer:backup/Drive\"]\n",
        )
        .unwrap();
        crate::mirror::load(&conn, &config).unwrap();
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!(c.uncovered.files, 2, "two places, but one machine");
    }

    #[test]
    fn repos_match_by_commit() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("code/gfiles", 0, Some(GIT), None),
                ("empty.txt", 0, Some(C), None),
            ],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("code/gfiles", 0, Some(GIT), None),
                ("empty.txt", 0, Some(C), None),
            ],
        );
        let d = duplicates(&conn, &all()).unwrap();
        assert_eq!(d.total_groups, 1, "repos group; zero-byte files don't");
        assert!(d.groups[0].confirmed);
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!(c.covered.files, 2);
    }

    #[test]
    fn unfingerprinted_files_are_only_counted() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("new.mkv", 4000, None, None), ("x", 10, Some(A), None)],
        );
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!(
            c.not_yet_fingerprinted,
            Tally {
                files: 1,
                bytes: 4000
            }
        );
        assert_eq!(c.uncovered.files, 1);
    }

    #[test]
    fn an_unfingerprinted_mirror_file_counts_once() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Drive/a.pdf", 10, Some(A), None),
                ("Drive/b.pdf", 20, None, None),
            ],
        );
        // elrond's fingerprint pass hasn't run yet.
        put(
            &conn,
            "elrond",
            "darren",
            &[
                ("Drive/a.pdf", 10, None, None),
                ("Drive/b.pdf", 20, None, None),
            ],
        );
        with_drive_mirror(&conn);
        let c = coverage(&conn, &all(), None).unwrap();
        // b.pdf once (two replicas, neither read); a.pdf not at all (one replica was).
        assert_eq!(
            c.not_yet_fingerprinted,
            Tally {
                files: 1,
                bytes: 20
            }
        );
    }

    #[test]
    fn a_folder_listing_is_capped_with_a_total() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[
                ("Docs/a", 30, Some(A), None),
                ("Docs/b", 20, Some(B), None),
                ("Docs/c", 10, Some(C), None),
            ],
        );
        let f = Filters {
            limit: Some(2),
            ..all()
        };
        let c = coverage(&conn, &f, Some("gaffer:darren/Docs")).unwrap();
        assert_eq!(c.total_files, Some(3));
        let files = c.files.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            paths(&files[0].copy),
            vec!["gaffer:Docs/a"],
            "biggest first"
        );
    }

    #[test]
    fn a_partly_loaded_mirror_still_isnt_its_own_backup() {
        let conn = crate::db::open_in_memory().unwrap();
        put(
            &conn,
            "gaffer",
            "darren",
            &[("Drive/a.pdf", 10, Some(A), None)],
        );
        put(
            &conn,
            "elrond",
            "darren",
            &[("Drive/a.pdf", 10, Some(A), None)],
        );
        // The third replica's volume hasn't been pulled: the other two still form the mirror.
        let config: Config = toml::from_str(
            "machine = \"gaffer\"\n[[remote]]\nname = \"nas2\"\n[[mirror]]\nname = \"drive\"\n\
             folders = [\"gaffer:darren/Drive\", \"elrond:darren/Drive\", \"nas2:darren/Drive\"]\n",
        )
        .unwrap();
        crate::mirror::load(&conn, &config).unwrap();
        let c = coverage(&conn, &all(), None).unwrap();
        assert_eq!(c.covered.files, 0);
        assert_eq!(c.uncovered.files, 1);
        assert!(c.warnings[0].contains("nas2"), "{:?}", c.warnings);
    }

    #[test]
    fn filters_and_limits() {
        let conn = crate::db::open_in_memory().unwrap();
        let specs: Vec<(String, i64, &str)> = [A, B, C]
            .iter()
            .enumerate()
            .map(|(i, fp)| (format!("f{i}"), 10 * (i as i64 + 1), *fp))
            .collect();
        let entries: Vec<Spec> = specs
            .iter()
            .map(|(p, s, f)| (p.as_str(), *s, Some(*f), None))
            .collect();
        put(&conn, "gaffer", "darren", &entries);
        put(&conn, "elrond", "darren", &entries);
        let d = duplicates(
            &conn,
            &Filters {
                limit: Some(2),
                ..all()
            },
        )
        .unwrap();
        assert_eq!((d.total_groups, d.groups.len()), (3, 2));
        assert_eq!(d.groups[0].size, 30, "most wasted first");
        let d = duplicates(
            &conn,
            &Filters {
                min_size: 25,
                ..all()
            },
        )
        .unwrap();
        assert_eq!(d.total_groups, 1);
        let err = Filters {
            machine: Some("nosuch".into()),
            ..all()
        }
        .check(&conn)
        .unwrap_err();
        assert!(
            err.to_string().contains("use one of: elrond, gaffer"),
            "{err}"
        );
        assert!(coverage(&conn, &all(), Some("gaffer:darren/nope")).is_err());
    }
}
