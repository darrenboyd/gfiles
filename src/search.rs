//! Queries over the catalog, shared by the CLI, the TUI and the MCP server.

use anyhow::{Result, bail};
use rusqlite::{Connection, params, types::Value};
use serde::Serialize;

use crate::mirror::{JOIN, KEY, MIRROR_PATH, PLACE};

#[derive(Debug, Default)]
pub struct Query {
    /// Words that must all appear in the file's name or path (prefix match, any case).
    pub terms: Vec<String>,
    pub kind: Option<String>,
    pub volume: Option<String>,
    pub machine: Option<String>,
    pub limit: Option<usize>,
}

/// One file, or one mirror file (its first matching replica, with the others in `replicas`).
#[derive(Debug, Serialize)]
pub struct Hit {
    pub machine: String,
    pub volume: String,
    pub path: String,
    pub abs_path: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
    /// The mirror this file is in, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// The mirror file's other replicas.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replicas: Vec<Replica>,
    /// Places in the catalog holding this content, this one included, where a place is a
    /// machine or a whole mirror (as `dupes::coverage` counts them); absent until the file is
    /// fingerprinted. 1 means no copy on another machine or outside its mirror.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copies: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Replica {
    pub machine: String,
    pub volume: String,
    pub path: String,
    pub abs_path: String,
}

/// Refuses a volume or machine filter that matches nothing in the catalog, listing the known
/// names, so a mistyped filter (or a model's guess) isn't mistaken for "no such files".
pub fn check_filters(conn: &Connection, q: &Query) -> Result<()> {
    for (what, column, value) in [
        ("volume", "name", &q.volume),
        ("machine", "machine", &q.machine),
    ] {
        let Some(value) = value else { continue };
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT {column} FROM volumes ORDER BY {column}"
        ))?;
        let known: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !known.contains(value) {
            bail!("unknown {what} {value:?}; use one of: {}", known.join(", "));
        }
    }
    Ok(())
}

/// The FROM and WHERE clauses for a query, with their bound arguments.
struct Filter {
    sql: String,
    args: Vec<Value>,
    fts: bool,
    has_where: bool,
}

impl Filter {
    fn new(q: &Query, with_kind: bool) -> Filter {
        let mut sql = format!(" FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}");
        let mut wheres = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        let fts = fts_query(&q.terms);
        if let Some(fts) = &fts {
            sql.push_str(" JOIN entries_fts f ON f.rowid = e.id");
            wheres.push("entries_fts MATCH ?");
            args.push(fts.clone().into());
        }
        let kind = if with_kind { &q.kind } else { &None };
        for (clause, value) in [
            ("e.kind = ?", kind),
            ("v.name = ?", &q.volume),
            ("v.machine = ?", &q.machine),
        ] {
            if let Some(value) = value {
                wheres.push(clause);
                args.push(value.clone().into());
            }
        }
        if !wheres.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&wheres.join(" AND "));
        }
        Filter {
            sql,
            args,
            fts: fts.is_some(),
            has_where: !wheres.is_empty(),
        }
    }

    /// The FROM and WHERE clauses limited to entries inside a mirror (`true`) or outside one.
    /// Queries split this way so only mirror rows pay for collapsing replicas.
    fn split(&self, in_mirror: bool) -> String {
        let cond = if in_mirror {
            "mf.mirror IS NOT NULL"
        } else {
            "mf.mirror IS NULL"
        };
        let glue = if self.has_where { " AND " } else { " WHERE " };
        format!("{}{glue}{cond}", self.sql)
    }

    /// The arguments for a query using both halves of `split`.
    fn args_twice(&self) -> Vec<Value> {
        self.args.iter().chain(&self.args).cloned().collect()
    }
}

/// Matching files, a mirror file once however many of its replicas match (shown as its first
/// fingerprinted replica, so the hit can say how many copies exist).
pub fn search(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    let filter = Filter::new(q, true);
    let rank = if filter.fts { "f.rank" } else { "0" };
    // Files outside a mirror are their own logical file; only mirror rows need the window
    // that keeps one replica of each.
    let content = crate::dupes::content("e");
    let mut sql = format!(
        "SELECT v.machine, v.name, v.root, e.path, e.kind, e.size, e.mtime, NULL AS mirror,
                NULL AS mpath, e.fingerprint, {content} AS content, {rank} AS rank
         {outside}
         UNION ALL
         SELECT machine, name, root, path, kind, size, mtime, mirror, mpath, fingerprint,
                content, rank
         FROM (
             SELECT v.machine, v.name, v.root, e.path, e.kind, e.size, e.mtime, mf.mirror,
                    {MIRROR_PATH} AS mpath, e.fingerprint, {content} AS content,
                    {rank} AS rank,
                    row_number() OVER (PARTITION BY {KEY}
                        ORDER BY e.fingerprint IS NULL, v.machine, v.name, e.path) AS rn
             {inside})
         WHERE rn = 1
         ORDER BY rank, 1, 2, 4",
        outside = filter.split(false),
        inside = filter.split(true),
    );
    if let Some(limit) = q.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let mut hits = Vec::new();
    let mut rows = stmt.query(rusqlite::params_from_iter(filter.args_twice()))?;
    let mut details = Details::new(conn)?;
    while let Some(r) = rows.next()? {
        let root: String = r.get(2)?;
        let path: String = r.get(3)?;
        let size: i64 = r.get(5)?;
        let mirror: Option<String> = r.get(7)?;
        let mirror_path: Option<String> = r.get(8)?;
        let fingerprint: Option<String> = r.get(9)?;
        let content: Option<String> = r.get(10)?;
        let replicas = match (&mirror, &mirror_path) {
            (Some(m), Some(mp)) => {
                details.replicas(m, mp, &r.get::<_, String>(0)?, &r.get::<_, String>(1)?)?
            }
            _ => Vec::new(),
        };
        let copies = match (fingerprint, content) {
            (Some(fp), Some(content)) if size > 0 || fp.starts_with("git:") => {
                Some(details.copies(size, &fp, &content)?)
            }
            _ => None,
        };
        hits.push(Hit {
            machine: r.get(0)?,
            volume: r.get(1)?,
            abs_path: join_root(&root, &path),
            path,
            kind: r.get(4)?,
            size,
            mtime: r.get(6)?,
            mirror,
            replicas,
            copies,
        });
    }
    Ok(hits)
}

/// Per-hit lookups, prepared once per search.
struct Details<'c> {
    copies: rusqlite::Statement<'c>,
    folders: rusqlite::Statement<'c>,
    entry: rusqlite::Statement<'c>,
}

impl<'c> Details<'c> {
    fn new(conn: &'c Connection) -> Result<Details<'c>> {
        Ok(Details {
            copies: conn.prepare(&format!(
                "SELECT count(DISTINCT {PLACE})
                 FROM entries e JOIN volumes v ON v.id = e.volume_id{JOIN}
                 WHERE e.size = ?1 AND e.fingerprint = ?2 AND {content} = ?3",
                content = crate::dupes::content("e")
            ))?,
            folders: conn.prepare(
                "SELECT mf.volume_id, mf.prefix, v.machine, v.name, v.root
                 FROM temp.mirror_folders mf JOIN volumes v ON v.id = mf.volume_id
                 WHERE mf.mirror = ?1 ORDER BY v.machine, v.name",
            )?,
            entry: conn.prepare("SELECT 1 FROM entries WHERE volume_id = ?1 AND path = ?2")?,
        })
    }

    fn copies(&mut self, size: i64, fingerprint: &str, content: &str) -> Result<i64> {
        Ok(self
            .copies
            .query_row(params![size, fingerprint, content], |r| r.get(0))?)
    }

    /// The replicas of a mirror file other than the one on `machine`'s `volume`.
    fn replicas(
        &mut self,
        mirror: &str,
        mirror_path: &str,
        machine: &str,
        volume: &str,
    ) -> Result<Vec<Replica>> {
        let folders: Vec<(String, String, String, String, String)> = self
            .folders
            .query_map([mirror], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (volume_id, prefix, m, v, root) in folders {
            let path = if prefix.is_empty() {
                mirror_path.to_string()
            } else {
                format!("{prefix}/{mirror_path}")
            };
            if (m == machine && v == volume) || !self.entry.exists(params![volume_id, path])? {
                continue;
            }
            out.push(Replica {
                machine: m,
                volume: v,
                abs_path: join_root(&root, &path),
                path,
            });
        }
        Ok(out)
    }
}

/// How many files match the query (ignoring its limit), a mirror file once.
pub fn count(conn: &Connection, q: &Query) -> Result<i64> {
    let filter = Filter::new(q, true);
    let sql = format!(
        "SELECT (SELECT count(*){}) + (SELECT count(DISTINCT {KEY}){})",
        filter.split(false),
        filter.split(true)
    );
    Ok(
        conn.query_row(&sql, rusqlite::params_from_iter(filter.args_twice()), |r| {
            r.get(0)
        })?,
    )
}

#[derive(Debug, Serialize)]
pub struct Folder {
    pub machine: String,
    pub volume: String,
    /// Relative to the volume root, `/`-separated; empty for the root itself.
    pub folder: String,
    pub abs_folder: String,
    pub count: i64,
    pub size: i64,
    /// The mirror this folder is a replica of (part of), if any: the same files are also in
    /// the mirror's other folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
}

/// The folders holding the matching entries, with how many each holds, most first. Answers
/// "where are my books?" without listing every book.
pub fn folders(conn: &Connection, q: &Query) -> Result<Vec<Folder>> {
    let filter = Filter::new(q, true);
    // rtrim(path, <path without its slashes>) strips the last component, keeping the slash.
    let mut sql = format!(
        "SELECT v.machine, v.name, v.root, rtrim(e.path, replace(e.path, '/', '')) AS dir,
                count(*), sum(e.size), max(mf.mirror){}
         GROUP BY v.id, dir ORDER BY count(*) DESC, v.machine, v.name, dir",
        filter.sql
    );
    if let Some(limit) = q.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    let mut stmt = conn.prepare(&sql)?;
    let folders = stmt
        .query_map(rusqlite::params_from_iter(filter.args), |r| {
            let root: String = r.get(2)?;
            let dir: String = r.get(3)?;
            let folder = dir.trim_end_matches('/').to_string();
            Ok(Folder {
                machine: r.get(0)?,
                volume: r.get(1)?,
                abs_folder: join_root(&root, &folder),
                folder,
                count: r.get(4)?,
                size: r.get(5)?,
                mirror: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(folders)
}

/// How many entries match the query for each kind, ignoring the query's own kind filter.
pub fn count_by_kind(conn: &Connection, q: &Query) -> Result<Vec<(String, i64)>> {
    let filter = Filter::new(q, false);
    let sql = format!(
        "SELECT kind, sum(n) FROM (
             SELECT e.kind AS kind, count(*) AS n{} GROUP BY e.kind
             UNION ALL
             SELECT e.kind, count(DISTINCT {KEY}){} GROUP BY e.kind)
         GROUP BY kind",
        filter.split(false),
        filter.split(true)
    );
    let mut stmt = conn.prepare(&sql)?;
    let counts = stmt
        .query_map(rusqlite::params_from_iter(filter.args_twice()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(counts)
}

#[derive(Debug, Serialize)]
pub struct KindStats {
    pub kind: String,
    pub count: i64,
    pub size: i64,
}

#[derive(Debug, Serialize)]
pub struct VolumeStats {
    pub machine: String,
    pub volume: String,
    pub root: String,
    pub last_scan_at: Option<i64>,
    pub kinds: Vec<KindStats>,
}

/// Item counts and total sizes per kind, for each volume.
pub fn stats(conn: &Connection) -> Result<Vec<VolumeStats>> {
    let mut volumes: Vec<VolumeStats> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, machine, name, root, last_scan_at FROM volumes ORDER BY machine, name",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            ids.push(r.get(0)?);
            volumes.push(VolumeStats {
                machine: r.get(1)?,
                volume: r.get(2)?,
                root: r.get(3)?,
                last_scan_at: r.get(4)?,
                kinds: Vec::new(),
            });
        }
    }
    let mut stmt = conn.prepare(
        "SELECT kind, count(*), sum(size) FROM entries WHERE volume_id = ?1
         GROUP BY kind ORDER BY count(*) DESC",
    )?;
    for (id, vol) in ids.iter().zip(volumes.iter_mut()) {
        vol.kinds = stmt
            .query_map([id], |r| {
                Ok(KindStats {
                    kind: r.get(0)?,
                    count: r.get(1)?,
                    size: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(volumes)
}

/// Turns search words into an FTS5 query: every word must match as a prefix. Each word is
/// quoted, so punctuation and FTS operators in it are treated as plain text.
fn fts_query(terms: &[String]) -> Option<String> {
    let words: Vec<String> = terms
        .iter()
        .flat_map(|t| t.split_whitespace())
        .map(|w| format!("\"{}\"*", w.replace('"', "\"\"")))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

pub(crate) fn join_root(root: &str, path: &str) -> String {
    std::path::Path::new(root)
        .join(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::VolumeConfig, db, scan};
    use std::fs;

    fn catalog() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        for rel in [
            "Books/Tolkien/The Hobbit.epub",
            "Books/Tolkien/notes.txt",
            "Books/Le Guin/A Wizard of Earthsea.mobi",
            "Docs/tolkien-essay.pdf",
            "Photos/café.jpg",
        ] {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, rel).unwrap();
        }
        let mut conn = db::open_in_memory().unwrap();
        let vol = VolumeConfig {
            id: "v1".into(),
            name: "home".into(),
            path: tmp.path().into(),
            ignore: vec![],
        };
        scan::scan_volume(&mut conn, &vol, "gaffer").unwrap();
        (tmp, conn)
    }

    fn paths(hits: &[Hit]) -> Vec<&str> {
        let mut p: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        p.sort();
        p
    }

    #[test]
    fn a_mirror_file_is_one_hit_with_its_replicas() {
        let (tmp, mut conn) = catalog();
        // The same folder again, as elrond's: Books is mirrored, Docs is just a second copy.
        let nas = VolumeConfig {
            id: "v2".into(),
            name: "nas".into(),
            path: tmp.path().into(),
            ignore: vec![],
        };
        scan::scan_volume(&mut conn, &nas, "elrond").unwrap();
        let config: crate::config::Config = toml::from_str(
            "machine = \"gaffer\"\n[[mirror]]\nname = \"books\"\nfolders = [\"gaffer:home/Books\", \"elrond:nas/Books\"]\n",
        )
        .unwrap();
        crate::mirror::load(&conn, &config).unwrap();

        let q = |words: &str| Query {
            terms: vec![words.into()],
            ..Default::default()
        };
        let hits = search(&conn, &q("hobbit")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(count(&conn, &q("hobbit")).unwrap(), 1);
        let hit = &hits[0];
        assert_eq!(
            (hit.machine.as_str(), hit.mirror.as_deref()),
            ("elrond", Some("books"))
        );
        assert_eq!(hit.replicas.len(), 1);
        assert_eq!(hit.replicas[0].machine, "gaffer");
        assert_eq!(hit.copies, Some(1), "a mirror is one place");

        let hits = search(&conn, &q("essay")).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .all(|h| h.copies == Some(2) && h.replicas.is_empty())
        );

        let folders = folders(&conn, &q("hobbit")).unwrap();
        assert_eq!(folders.len(), 2, "each replica's folder, marked");
        assert!(folders.iter().all(|f| f.mirror.as_deref() == Some("books")));

        // Mixed results (mirror and plain files) come back sorted and counted together.
        let all = Query::default();
        let hits = search(&conn, &all).unwrap();
        assert_eq!(hits.len() as i64, count(&conn, &all).unwrap());
        assert_eq!(
            hits.len(),
            3 + 2 * 2,
            "3 Books files once, Docs and Photos twice"
        );
        assert!(
            hits.windows(2)
                .all(|w| (&w[0].machine, &w[0].volume, &w[0].path)
                    <= (&w[1].machine, &w[1].volume, &w[1].path))
        );
        let by_kind: i64 = count_by_kind(&conn, &all)
            .unwrap()
            .iter()
            .map(|k| k.1)
            .sum();
        assert_eq!(by_kind, 7);
    }

    #[test]
    fn copies_on_one_machine_are_one_place() {
        let (tmp, mut conn) = catalog();
        fs::write(
            tmp.path().join("Docs/tolkien-essay (1).pdf"),
            "Docs/tolkien-essay.pdf",
        )
        .unwrap();
        let vol = VolumeConfig {
            id: "v1".into(),
            name: "home".into(),
            path: tmp.path().into(),
            ignore: vec![],
        };
        scan::scan_volume(&mut conn, &vol, "gaffer").unwrap();
        let q = Query {
            terms: vec!["essay".into()],
            ..Default::default()
        };
        let hits = search(&conn, &q).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter().all(|h| h.copies == Some(1)),
            "no backup elsewhere"
        );
    }

    #[test]
    fn filters_by_kind() {
        let (_tmp, conn) = catalog();
        let hits = search(
            &conn,
            &Query {
                kind: Some("book".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            paths(&hits),
            vec![
                "Books/Le Guin/A Wizard of Earthsea.mobi",
                "Books/Tolkien/The Hobbit.epub"
            ]
        );
        assert!(hits[0].abs_path.ends_with(&hits[0].path));
    }

    #[test]
    fn matches_name_and_path_words_by_prefix() {
        let (_tmp, conn) = catalog();
        let q = |terms: &[&str]| Query {
            terms: terms.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(
            paths(&search(&conn, &q(&["tolk"])).unwrap()),
            vec![
                "Books/Tolkien/The Hobbit.epub",
                "Books/Tolkien/notes.txt",
                "Docs/tolkien-essay.pdf"
            ]
        );
        assert_eq!(
            paths(&search(&conn, &q(&["tolkien", "hob"])).unwrap()),
            vec!["Books/Tolkien/The Hobbit.epub"]
        );
        assert_eq!(
            paths(&search(&conn, &q(&["CAFE"])).unwrap()),
            vec!["Photos/café.jpg"]
        );
        assert!(search(&conn, &q(&["\"AND(", "*"])).unwrap().is_empty());
    }

    #[test]
    fn stats_per_kind() {
        let (_tmp, conn) = catalog();
        let s = stats(&conn).unwrap();
        assert_eq!(s.len(), 1);
        let books = s[0].kinds.iter().find(|k| k.kind == "book").unwrap();
        assert_eq!(books.count, 2);
    }

    #[test]
    fn counts_ignore_the_kind_filter() {
        let (_tmp, conn) = catalog();
        let q = Query {
            terms: vec!["tolkien".into()],
            kind: Some("book".into()),
            ..Default::default()
        };
        let mut counts = count_by_kind(&conn, &q).unwrap();
        counts.sort();
        assert_eq!(counts, vec![("book".into(), 1), ("document".into(), 2)]);
    }

    #[test]
    fn folders_group_matches_by_parent() {
        let (_tmp, conn) = catalog();
        let q = Query {
            terms: vec!["tolkien".into()],
            ..Default::default()
        };
        assert_eq!(count(&conn, &q).unwrap(), 3);
        let f = folders(&conn, &q).unwrap();
        let got: Vec<(&str, i64)> = f.iter().map(|f| (f.folder.as_str(), f.count)).collect();
        assert_eq!(got, vec![("Books/Tolkien", 2), ("Docs", 1)]);
        assert!(f[0].abs_folder.ends_with("Books/Tolkien"));

        let photos = Query {
            kind: Some("photo".into()),
            ..Default::default()
        };
        assert_eq!(folders(&conn, &photos).unwrap()[0].folder, "Photos");
    }
}
