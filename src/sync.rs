//! Moving catalogs between machines: `push` sends this machine's volumes to the central
//! catalog, where `ingest` receives them.
//!
//! The stream is newline-delimited JSON, one message per line:
//!
//! ```text
//! {"t":"hello","protocol":1,"machine":"gaffer","volumes":["<id>", ...]}   every configured volume
//! {"t":"volume","id":..,"name":..,"machine":..,"root":..,"last_scan_at":..}
//! {"t":"entry","path":..,"name":..,"ext":..,"kind":..,"size":..,"mtime":..}   × N
//! {"t":"end","entries":N}
//! ... more volumes ...
//! {"t":"done"}
//! ```
//!
//! Every push sends each scanned volume in full; ingest applies it with the same `VolumeWriter`
//! a local scan uses, one transaction per volume, so a cut-off stream never leaves a volume half
//! written. After `done`, ingest forgets this machine's volumes that are no longer in `hello`.
//!
//! Push runs `gfiles ingest --machine <its name>`. On the central machine each pusher's SSH key
//! should be locked to that command (`restrict,command="..."` in `authorized_keys`, see
//! docs/setup.md), so the name comes from the key, not from the stream: ingest refuses a `hello`
//! naming any other machine, and store refuses entry paths that leave their volume.

use std::io::{BufRead, BufWriter, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::Central;
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

/// What ingest did, sent back to the pushing machine as one JSON line per item.
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

/// Writes the push stream for `machine`'s volumes in the local catalog. `volume_ids` is every
/// volume in this machine's config; those never scanned are listed in `hello` but not sent.
pub fn write_push(
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

/// Applies a push stream to this catalog. `local_machine` is this machine's own name: a push
/// claiming it is refused, since it would replace this machine's own volumes. `pusher` is the
/// machine this push is allowed to write for (from `ingest --machine`); a stream claiming any
/// other is refused.
pub fn ingest(
    conn: &mut Connection,
    local_machine: &str,
    pusher: &str,
    input: impl BufRead,
    mut on_report: impl FnMut(&Report) -> Result<()>,
) -> Result<()> {
    let mut lines = input.lines();
    let mut next = move || -> Result<Message> {
        let line = lines
            .next()
            .context("the push stream ended early (connection lost?)")??;
        serde_json::from_str(&line).with_context(|| format!("bad message: {line:.200}"))
    };

    let Message::Hello {
        protocol,
        machine,
        volumes: listed,
    } = next()?
    else {
        bail!("the push stream must start with hello");
    };
    if protocol != PROTOCOL {
        bail!(
            "the pushing gfiles speaks protocol {protocol}, this one speaks {PROTOCOL}; \
             install the same gfiles version on both machines"
        );
    }
    if machine != pusher {
        bail!(
            "refusing a push claiming machine {machine:?}; this connection may only push for {pusher:?}"
        );
    }
    if machine == local_machine {
        bail!(
            "refusing a push from machine {machine:?}, which is this machine's own name; \
             give the pushing machine its own name with `machine = \"...\"` in its config"
        );
    }

    loop {
        match next()? {
            Message::Volume(volume) => {
                if volume.machine != machine {
                    bail!(
                        "volume {:?} claims machine {:?} in a push from {machine:?}",
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

/// Pushes this machine's volumes to the central catalog over SSH, returning ingest's reports.
pub fn push(
    conn: &Connection,
    central: &Central,
    machine: &str,
    volume_ids: &[String],
) -> Result<Vec<Report>> {
    // Pushes usually run from timers, with nobody to notice a hang. BatchMode fails instead of
    // waiting for a password; ConnectTimeout bounds an unreachable host; ServerAlive* drops a
    // connection that stops answering (a sleeping laptop, a dropped link) after about 2 minutes.
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
            &central.host,
        ])
        .arg(format!(
            "{} ingest --machine {}",
            central.gfiles,
            shell_quote(machine)
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let sent = write_push(conn, machine, volume_ids, stdin);
    // If the remote side failed, writing usually fails too (broken pipe); its exit status and
    // stderr (passed straight through) say why, so check that first.
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "ingest on {} failed ({}); its error is above",
            central.host,
            output.status
        );
    }
    sent?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).with_context(|| format!("bad report: {l}")))
        .collect()
}

/// Quotes `s` for the POSIX shell that runs the remote command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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

    /// Pushes `machine`'s volumes from `agent` into `central` (whose own machine is "bill").
    fn push_into(
        agent: &Connection,
        machine: &str,
        ids: &[&str],
        central: &mut Connection,
    ) -> Result<Vec<Report>> {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        let mut stream = Vec::new();
        write_push(agent, machine, &ids, &mut stream).unwrap();
        let mut reports = Vec::new();
        ingest(central, "bill", machine, stream.as_slice(), |r| {
            reports.push(r.clone());
            Ok(())
        })?;
        Ok(reports)
    }

    fn central_paths(central: &Connection) -> Vec<String> {
        let mut hits = search::search(central, &Query::default()).unwrap();
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        hits.into_iter()
            .map(|h| format!("{}:{}", h.machine, h.path))
            .collect()
    }

    #[test]
    fn push_updates_and_forgets() {
        let tmp = tempfile::tempdir().unwrap();
        let (books, music) = (tmp.path().join("books"), tmp.path().join("music"));
        touch(&books, "Tolkien/The Hobbit.epub");
        touch(&books, "Dune.epub");
        touch(&music, "song.flac");

        let mut agent = db::open_in_memory().unwrap();
        let mut central = db::open_in_memory().unwrap();
        // The central machine's own volume must survive pushes from others.
        let own = tmp.path().join("own");
        touch(&own, "bill.txt");
        scan::scan_volume(&mut central, &vol("c", &own), "bill").unwrap();

        scan::scan_volume(&mut agent, &vol("b", &books), "gaffer").unwrap();
        scan::scan_volume(&mut agent, &vol("m", &music), "gaffer").unwrap();
        let reports =
            push_into(&agent, "gaffer", &["b", "m", "never-scanned"], &mut central).unwrap();
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
            central_paths(&central),
            vec![
                "gaffer:Dune.epub",
                "gaffer:Tolkien/The Hobbit.epub",
                "bill:bill.txt",
                "gaffer:song.flac"
            ]
        );

        // A deleted file disappears on the next push; a volume dropped from the config is forgotten.
        fs::remove_file(books.join("Dune.epub")).unwrap();
        scan::scan_volume(&mut agent, &vol("b", &books), "gaffer").unwrap();
        let reports = push_into(&agent, "gaffer", &["b"], &mut central).unwrap();
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
            central_paths(&central),
            vec!["gaffer:Tolkien/The Hobbit.epub", "bill:bill.txt"]
        );
    }

    #[test]
    fn a_cut_off_stream_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        touch(tmp.path(), "b.txt");
        let mut agent = db::open_in_memory().unwrap();
        let mut central = db::open_in_memory().unwrap();
        scan::scan_volume(&mut agent, &vol("v", tmp.path()), "gaffer").unwrap();
        push_into(&agent, "gaffer", &["v"], &mut central).unwrap();

        fs::remove_file(tmp.path().join("b.txt")).unwrap();
        scan::scan_volume(&mut agent, &vol("v", tmp.path()), "gaffer").unwrap();
        let mut stream = Vec::new();
        write_push(&agent, "gaffer", &["v".into()], &mut stream).unwrap();
        let text = String::from_utf8(stream).unwrap();
        let cut: String = text.lines().take(3).map(|l| format!("{l}\n")).collect(); // no "end"
        let err = ingest(&mut central, "bill", "gaffer", cut.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("ended early"), "{err}");
        assert_eq!(
            central_paths(&central),
            vec!["gaffer:a.txt", "gaffer:b.txt"]
        );
    }

    #[test]
    fn refusals() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let mut agent = db::open_in_memory().unwrap();
        let mut central = db::open_in_memory().unwrap();
        scan::scan_volume(&mut agent, &vol("v", tmp.path()), "gaffer").unwrap();

        let err = push_into(&agent, "bill", &[], &mut central).unwrap_err();
        assert!(err.to_string().contains("this machine's own name"), "{err}");

        let hello = r#"{"t":"hello","protocol":99,"machine":"gaffer","volumes":[]}"#;
        let err = ingest(&mut central, "bill", "gaffer", hello.as_bytes(), |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("protocol 99"), "{err}");

        // The same volume id pushed by a different machine.
        push_into(&agent, "gaffer", &["v"], &mut central).unwrap();
        let mut other = db::open_in_memory().unwrap();
        scan::scan_volume(&mut other, &vol("v", tmp.path()), "elrond").unwrap();
        let err = push_into(&other, "elrond", &["v"], &mut central).unwrap_err();
        assert!(err.to_string().contains("belongs to machine"), "{err}");
    }

    /// Ingests a hand-written stream for `machine`, pushed by `pusher`.
    fn ingest_lines(central: &mut Connection, pusher: &str, lines: &[&str]) -> Result<()> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        ingest(central, "bill", pusher, input.as_bytes(), |_| Ok(()))
    }

    #[test]
    fn a_pusher_can_only_push_for_itself() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.txt");
        let mut agent = db::open_in_memory().unwrap();
        let mut central = db::open_in_memory().unwrap();
        scan::scan_volume(&mut agent, &vol("v", tmp.path()), "gaffer").unwrap();
        push_into(&agent, "gaffer", &["v"], &mut central).unwrap();

        // elrond's key claiming to be gaffer, with no volumes: would forget all of gaffer's.
        let err = ingest_lines(
            &mut central,
            "elrond",
            &[
                r#"{"t":"hello","protocol":1,"machine":"gaffer","volumes":[]}"#,
                r#"{"t":"done"}"#,
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("may only push for"), "{err}");
        assert_eq!(central_paths(&central), vec!["gaffer:a.txt"]);
    }

    #[test]
    fn entries_that_leave_their_volume_are_refused() {
        let mut central = db::open_in_memory().unwrap();
        let hello = r#"{"t":"hello","protocol":1,"machine":"elrond","volumes":["v"]}"#;
        let volume = |root: &str| {
            format!(
                r#"{{"t":"volume","id":"v","name":"x","machine":"elrond","root":"{root}","last_scan_at":null}}"#
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
                ingest_lines(&mut central, "elrond", &lines).is_err(),
                "{root} {path} accepted"
            );
        }
        assert!(central_paths(&central).is_empty());
        let volumes: i64 = central
            .query_row("SELECT count(*) FROM volumes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(volumes, 0, "a refused volume rolls back entirely");
    }
}
