//! Queries over the catalog, shared by the CLI and TUI (and later the MCP server).

use anyhow::Result;
use rusqlite::{Connection, types::Value};
use serde::Serialize;

#[derive(Debug, Default)]
pub struct Query {
    /// Words that must all appear in the file's name or path (prefix match, any case).
    pub terms: Vec<String>,
    pub kind: Option<String>,
    pub volume: Option<String>,
    pub machine: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub machine: String,
    pub volume: String,
    pub path: String,
    pub abs_path: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
}

/// The FROM and WHERE clauses for a query, with their bound arguments.
struct Filter {
    sql: String,
    args: Vec<Value>,
    fts: bool,
}

impl Filter {
    fn new(q: &Query, with_kind: bool) -> Filter {
        let mut sql = String::from(" FROM entries e JOIN volumes v ON v.id = e.volume_id");
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
        }
    }
}

pub fn search(conn: &Connection, q: &Query) -> Result<Vec<Hit>> {
    let filter = Filter::new(q, true);
    let mut sql = format!(
        "SELECT v.machine, v.name, v.root, e.path, e.kind, e.size, e.mtime{}",
        filter.sql
    );
    sql.push_str(if filter.fts {
        " ORDER BY f.rank"
    } else {
        " ORDER BY v.machine, v.name, e.path"
    });
    if let Some(limit) = q.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let hits = stmt
        .query_map(rusqlite::params_from_iter(filter.args), |r| {
            let root: String = r.get(2)?;
            let path: String = r.get(3)?;
            Ok(Hit {
                machine: r.get(0)?,
                volume: r.get(1)?,
                abs_path: join_root(&root, &path),
                path,
                kind: r.get(4)?,
                size: r.get(5)?,
                mtime: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(hits)
}

/// How many entries match the query for each kind, ignoring the query's own kind filter.
pub fn count_by_kind(conn: &Connection, q: &Query) -> Result<Vec<(String, i64)>> {
    let filter = Filter::new(q, false);
    let sql = format!("SELECT e.kind, count(*){} GROUP BY e.kind", filter.sql);
    let mut stmt = conn.prepare(&sql)?;
    let counts = stmt
        .query_map(rusqlite::params_from_iter(filter.args), |r| {
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

fn join_root(root: &str, path: &str) -> String {
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
}
