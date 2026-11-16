//! Moving catalogs between machines. The hub (gaffer) runs `pull`, which runs `gfiles export`
//! on each remote over SSH and applies the stream with `ingest`. Remotes only answer; nothing
//! connects to the hub.
//!
//! The stream is newline-delimited JSON, one message per line:
//!
//! ```text
//! {"t":"hello","protocol":1,"machine":"elrond","volumes":["<id>", ...]}   every configured volume
//! {"t":"volume","id":..,"name":..,"machine":..,"root":..,"last_scan_at":..}
//! {"t":"entry","path":..,"name":..,"ext":..,"kind":..,"size":..,"mtime":..}   × N
//! {"t":"end","entries":N}
//! ... more volumes ...
//! {"t":"done"}
//! ```
//!
//! Every export sends each scanned volume in full; ingest applies it with the same
//! `VolumeWriter` a local scan uses, one transaction per volume, so a cut-off stream never
//! leaves a volume half written. After `done`, ingest forgets the remote's volumes that are no
//! longer in `hello`.
//!
//! The stream is untrusted: the remote's machine name comes from the hub's config, not from
//! the stream (ingest refuses a `hello` naming any other machine), and store refuses entry
//! paths that leave their volume. On the remote, the hub's key should be locked to
//! `gfiles export` with a forced command in `authorized_keys` (see docs/setup.md).

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::Remote;
use crate::scan::unix_secs;
use crate::store::{self, Entry, Volume, VolumeWriter};

/// Bump when the stream format changes incompatibly. Ingest refuses other versions, so the
/// binaries on both ends must be updated together.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Message {
    Hello {
        protocol: u32,
        machine: String,
        volumes: Vec<String>,
    },
    Volume(Volume),
    Entry(Entry),
    End {
        entries: u64,
    },
    Done,
}

/// What ingest did, one item per volume written or forgotten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Report {
    Volume {
        name: String,
        entries: u64,
        removed: u64,
    },
    Forgot {
        name: String,
    },
}

/// Writes the stream for `machine`'s volumes in the local catalog. `volume_ids` is every
/// volume in this machine's config; those never scanned are listed in `hello` but not sent.
pub fn export(
    conn: &Connection,
    machine: &str,
    volume_ids: &[String],
    out: impl Write,
) -> Result<()> {
    let mut out = BufWriter::new(out);
    let send = |out: &mut BufWriter<_>, m: &Message| -> Result<()> {
        serde_json::to_writer(&mut *out, m)?;
        out.write_all(b"\n")?;
        Ok(())
    };
    send(
        &mut out,
        &Message::Hello {
            protocol: PROTOCOL,
            machine: machine.to_string(),
            volumes: volume_ids.to_vec(),
        },
    )?;
    for id in volume_ids {
        let Some(volume) = store::volume(conn, id)? else {
            continue; // configured but not scanned yet
        };
        if volume.machine != machine {
            bail!(
                "volume {:?} was scanned as machine {:?}, not {machine:?}; rescan it",
                volume.name,
                volume.machine
            );
        }
        send(&mut out, &Message::Volume(volume))?;
        let mut entries = 0;
        store::for_each_entry(conn, id, |e| {
            entries += 1;
            send(&mut out, &Message::Entry(e))
        })?;
        send(&mut out, &Message::End { entries })?;
    }
    send(&mut out, &Message::Done)?;
    out.flush()?;
    Ok(())
}

/// Applies a stream to this catalog. `local_machine` is this machine's own name: a stream
/// claiming it is refused, since it would replace this machine's own volumes. `remote` is the
/// machine this stream is allowed to write for (from the config); a stream claiming any other
/// is refused.
pub fn ingest(
    conn: &mut Connection,
    local_machine: &str,
    remote: &str,
    input: impl BufRead,
    mut on_report: impl FnMut(&Report) -> Result<()>,
) -> Result<()> {
    let mut lines = input.lines();
    let mut next = move || -> Result<Message> {
        let line = lines
            .next()
            .context("the stream ended early (connection lost?)")??;
        serde_json::from_str(&line).with_context(|| format!("bad message: {line:.200}"))
    };

    let Message::Hello {
        protocol,
        machine,
        volumes: listed,
    } = next()?
    else {
        bail!("the stream must start with hello");
    };
    if protocol != PROTOCOL {
        bail!(
            "the remote gfiles speaks protocol {protocol}, this one speaks {PROTOCOL}; \
             install the same gfiles version on both machines"
        );
    }
    if machine == local_machine {
        bail!(
            "refusing a catalog from machine {machine:?}, which is this machine's own name; \
             give the remote its own name with `machine = \"...\"` in its config"
        );
    }
    if machine != remote {
        bail!(
            "remote {remote:?} sent a catalog for machine {machine:?}; \
             set `machine = \"{remote}\"` in its config, or rename the remote here"
        );
    }

    loop {
        match next()? {
            Message::Volume(volume) => {
                if volume.machine != machine {
                    bail!(
                        "volume {:?} claims machine {:?} in a catalog from {machine:?}",
                        volume.name,
                        volume.machine
                    );
                }
                if let Some(existing) = store::volume(conn, &volume.id)?
                    && existing.machine != machine
                {
                    bail!(
                        "volume id {} belongs to machine {:?}, not {machine:?}",
                        volume.id,
                        existing.machine
                    );
                }
                let tx = conn.transaction()?;
                let mut writer = VolumeWriter::begin(&tx, &volume)?;
                let mut entries = 0;
                loop {
                    match next()? {
                        Message::Entry(e) => {
                            writer.put(&e)?;
                            entries += 1;
                        }
                        Message::End { entries: sent } if sent == entries => break,
                        Message::End { entries: sent } => {
                            bail!(
                                "volume {:?}: sent {sent} entries, got {entries}",
                                volume.name
                            )
                        }
                        other => bail!("expected an entry or end, got {other:?}"),
                    }
                }
                let scanned_at = volume
                    .last_scan_at
                    .unwrap_or_else(|| unix_secs(Some(std::time::SystemTime::now())));
                let removed = writer.finish(scanned_at, &[])?.removed;
                tx.commit()?;
                on_report(&Report::Volume {
                    name: volume.name,
                    entries,
                    removed,
                })?;
            }
            Message::Done => break,
            other => bail!("expected a volume or done, got {other:?}"),
        }
    }

    // Only after a complete stream: forget volumes the machine no longer has in its config.
    let mut stmt = conn.prepare("SELECT id, name FROM volumes WHERE machine = ?1")?;
    let gone: Vec<(String, String)> = stmt
        .query_map([&machine], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(id, _)| !listed.contains(id))
        .collect();
    for (id, name) in gone {
        store::forget_volume(conn, &id)?;
        on_report(&Report::Forgot { name })?;
    }
    Ok(())
}

/// Pulls `remote`'s catalog over SSH into this one, returning what ingest did.
pub fn pull(conn: &mut Connection, local_machine: &str, remote: &Remote) -> Result<Vec<Report>> {
    // Pulls usually run from timers, with nobody to notice a hang. BatchMode fails instead of
    // waiting for a password; ConnectTimeout bounds an unreachable host (the laptop away from
    // home); ServerAlive* drops a connection that stops answering after about 2 minutes.
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=30",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ServerAliveCountMax=4",
            remote.host(),
        ])
        .arg(format!("{} export", remote.gfiles))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reports = Vec::new();
    // Ingest drops the pipe when it returns, so a remote still writing after a refusal stops.
    let ingested = ingest(
        conn,
        local_machine,
        &remote.name,
        BufReader::new(stdout),
        |r| {
            reports.push(r.clone());
            Ok(())
        },
    );
    let status = child.wait()?;
    match ingested {
        Ok(()) => Ok(reports),
        // ssh or the remote export failed; its stderr (passed straight through) says why.
        Err(e) if !status.success() => Err(e.context(format!(
            "pulling from {} failed (ssh exited with {status}; any remote error is above)",
            remote.host()
        ))),
        Err(e) => Err(e.context(format!("pulling from {}", remote.host()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VolumeConfig;
    use crate::search::{self, Query};
    use crate::{db, scan};
    use std::fs;
    use std::path::Path;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, rel).unwrap();
    }

    fn vol(id: &str, root: &Path) -> VolumeConfig {
        VolumeConfig {
            id: id.into(),
            name: format!("vol-{id}"),
            path: root.into(),
            ignore: vec![],
        }
    }

    /// Exports `machine`'s volumes from `remote_db` and ingests them into `hub` (whose own
    /// machine is "elrond"), as a pull from a remote named `machine` would.
    fn transfer(
        remote_db: &Connection,
        machine: &str,
        ids: &[&str],
        hub: &mut Connection,
    ) -> Result<Vec<Report>> {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let mut stream = Vec::new();
        export(remote_db, machine, &ids, &mut stream).unwrap();
        let mut reports = Vec::new();
        ingest(hub, "gaffer", machine, stream.as_slice(), |r| {
            reports.push(r.clone());
            Ok(())
        })?;
        Ok(reports)
    }

    fn hub_paths(hub: &Connection) -> Vec<String> {
        let mut hits = search::search(hub, &Query::default()).unwrap();
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        hits.into_iter()
            .map(|h| format!("{}:{}", h.machine, h.path))
            .collect()
    }

    #[test]
    fn pull_updates_and_forgets() {
        let tmp = tempfile::tempdir().unwrap();
        let (books, music) = (tmp.path().join("books"), tmp.path().join("music"));
        touch(&books, "Tolkien/The Hobbit.epub");
        touch(&books, "Dune.epub");
        touch(&music, "song.flac");

        let mut remote_db = db::open_in_memory().unwrap();
        let mut hub = db::open_in_memory().unwrap();
        // The hub machine's own volume must survive pulls from others.
        let own = tmp.path().join("own");
        touch(&own, "own.txt");
        scan::scan_volume(&mut hub, &vol("c", &own), "gaffer").unwrap();

        scan::scan_volume(&mut remote_db, &vol("b", &books), "elrond").unwrap();
        scan::scan_volume(&mut remote_db, &vol("m", &music), "elrond").unwrap();
        let reports =
            transfer(&remote_db, "elrond", &["b", "m", "never-scanned"], &mut hub).unwrap();
        assert_eq!(
            reports,
            vec![
                Report::Volume {
                    name: "vol-b".into(),
                    entries: 2,
                    removed: 0
                },
                Report::Volume {
                    name: "vol-m".into(),
                    entries: 1,
                    removed: 0
                },
            ]
        );
        assert_eq!(
            hub_paths(&hub),
            vec![
                "elrond:Dune.epub",
                "elrond:Tolkien/The Hobbit.epub",
                "gaffer:own.txt",
                "elrond:song.flac"
            ]
        );

        // A deleted file disappears on the next pull; a volume dropped from the config is forgotten.
        fs::remove_file(books.join("Dune.epub")).unwrap();
        scan::scan_volume(&mut remote_db, &vol("b", &books), "elrond").unwrap();
        let reports = transfer(&remote_db, "elrond", &["b"], &mut hub).unwrap();
        assert_eq!(
            reports,
            vec![
                Report::Volume {
                    name: "vol-b".into(),
                    entries: 1,
                    removed: 1
                },
                Report::Forgot {
                    name: "vol-m".into()
                },
            ]
        );
        assert_eq!(
            hub_paths(&hub),
            vec!["elrond:Tolkien/The Hobbit.epub", "gaffer:own.txt"]
        );
    }

    #[test]
    fn a_cut_off_stream_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        touch(tmp.path(), "b.txt");
        let mut remote_db = db::open_in_memory().unwrap();
        let mut hub = db::open_in_memory().unwrap();
        scan::scan_volume(&mut remote_db, &vol("v", tmp.path()), "elrond").unwrap();
        transfer(&remote_db, "elrond", &["v"], &mut hub).unwrap();

        fs::remove_file(tmp.path().join("b.txt")).unwrap();
        scan::scan_volume(&mut remote_db, &vol("v", tmp.path()), "elrond").unwrap();
        let mut stream = Vec::new();
        export(&remote_db, "elrond", &["v".into()], &mut stream).unwrap();
        let text = String::from_utf8(stream).unwrap();
        let cut: String = text.lines().take(3).map(|l| format!("{l}\n")).collect(); // no "end"
        let err = ingest(&mut hub, "gaffer", "elrond", cut.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("ended early"), "{err}");
        assert_eq!(hub_paths(&hub), vec!["elrond:a.txt", "elrond:b.txt"]);
    }

    #[test]
    fn refusals() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let mut remote_db = db::open_in_memory().unwrap();
        let mut hub = db::open_in_memory().unwrap();
        scan::scan_volume(&mut remote_db, &vol("v", tmp.path()), "elrond").unwrap();

        let err = transfer(&remote_db, "gaffer", &[], &mut hub).unwrap_err();
        assert!(err.to_string().contains("this machine's own name"), "{err}");

        let hello = r#"{"t":"hello","protocol":99,"machine":"elrond","volumes":[]}"#;
        let err = ingest(&mut hub, "gaffer", "elrond", hello.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("protocol 99"), "{err}");

        // The same volume id sent by a different machine.
        transfer(&remote_db, "elrond", &["v"], &mut hub).unwrap();
        let mut other = db::open_in_memory().unwrap();
        scan::scan_volume(&mut other, &vol("v", tmp.path()), "nas2").unwrap();
        let err = transfer(&other, "nas2", &["v"], &mut hub).unwrap_err();
        assert!(err.to_string().contains("belongs to machine"), "{err}");
    }

    /// Ingests a hand-written stream as if pulled from `remote`.
    fn ingest_lines(hub: &mut Connection, remote: &str, lines: &[&str]) -> Result<()> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        ingest(hub, "gaffer", remote, input.as_bytes(), |_| Ok(()))
    }

    #[test]
    fn a_remote_can_only_send_its_own_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let mut remote_db = db::open_in_memory().unwrap();
        let mut hub = db::open_in_memory().unwrap();
        scan::scan_volume(&mut remote_db, &vol("v", tmp.path()), "elrond").unwrap();
        transfer(&remote_db, "elrond", &["v"], &mut hub).unwrap();

        // nas2 claiming to be elrond, with no volumes: would forget all of elrond's.
        let err = ingest_lines(
            &mut hub,
            "nas2",
            &[
                r#"{"t":"hello","protocol":1,"machine":"elrond","volumes":[]}"#,
                r#"{"t":"done"}"#,
            ],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("sent a catalog for machine"),
            "{err}"
        );
        assert_eq!(hub_paths(&hub), vec!["elrond:a.txt"]);
    }

    #[test]
    fn entries_that_leave_their_volume_are_refused() {
        let mut hub = db::open_in_memory().unwrap();
        let hello = r#"{"t":"hello","protocol":1,"machine":"nas2","volumes":["v"]}"#;
        let volume = |root: &str| {
            format!(
                r#"{{"t":"volume","id":"v","name":"x","machine":"nas2","root":"{root}","last_scan_at":null}}"#
            )
        };
        let entry = |path: &str, name: &str| {
            format!(
                r#"{{"t":"entry","path":"{path}","name":"{name}","ext":null,"kind":"other","size":1,"mtime":1}}"#
            )
        };
        for (root, path, name) in [
            ("/volume1/Media", "/etc/shadow", "shadow"),
            ("/volume1/Media", "../../.ssh/id_ed25519", "id_ed25519"),
            ("/volume1/Media", "a/../../b", "b"),
            ("/volume1/Media", "a.txt", "b.txt"),
            ("Media", "a.txt", "a.txt"),
        ] {
            let lines = [
                hello.to_string(),
                volume(root),
                entry(path, name),
                r#"{"t":"end","entries":1}"#.into(),
                r#"{"t":"done"}"#.into(),
            ];
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            assert!(
                ingest_lines(&mut hub, "nas2", &lines).is_err(),
                "{root} {path} accepted"
            );
        }
        assert!(hub_paths(&hub).is_empty());
        let volumes: i64 = hub
            .query_row("SELECT count(*) FROM volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(volumes, 0, "a refused volume rolls back entirely");
    }
}
