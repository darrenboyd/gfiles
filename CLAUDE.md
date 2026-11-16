# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

`gfiles` catalogs Darren's own files (not system files) across all his machines, so that he, and
AI tools working for him, can find, organize and manage them. The question it must answer well is
"where are all my books?", across every machine at once.

Machines in scope:

| Machine  | What it is                                   | OS / arch                 |
| -------- | -------------------------------------------- | ------------------------- |
| `gaffer` | Darren's laptop; also the development machine | Pop!_OS, x86_64           |
| `bill`   | Home-lab server; holds the central catalog   | Ubuntu + Docker, x86_64 (see `~/code/homelab`) |
| NAS      | Synology DS1019+                             | DSM (Linux), x86_64       |

All three are x86_64 Linux, so one static `x86_64-unknown-linux-musl` build runs everywhere.
A Windows machine is out of scope for now, but no decision may rule it out (see Windows below).

The repo is public on GitHub, MIT licensed. Changes are committed and pushed straight to `main`
(no branches or PRs), when Darren asks.

## Review docs

- `CODING_STANDARDS.md`: the rules code here follows. Read it before writing or reviewing code;
  update it when a change establishes or retires a rule.
- `REVIEW.md`: how review behaves here (what ranks highest). Read it before reviewing a change.

## Decisions

- **Language:** Rust, built as a single binary. TUI with `ratatui`.
- **Storage:** one embedded SQLite database (`rusqlite` with the `bundled` feature, FTS5 for name
  and path search). No external services.
- **Topology:** one central catalog on `bill`. The same binary runs as an agent on each machine,
  scans local disks there, and streams the results to `bill` over SSH (`ssh bill gfiles ingest --machine <name>`).
  No daemon and no open port. A laptop that's away syncs the next time it can reach `bill`.
- **Interfaces:** a TUI (usable over SSH on `bill`), CLI commands with `--json` output, and an MCP
  server mode (`gfiles mcp`, stdio, so it also works through SSH) with typed tools for AI clients.
- **Volumes:** a volume is a configured root folder on a named machine, with a stable UUID. Paths
  are stored relative to the volume root, so moving a mount point doesn't orphan the catalog.
- **What counts as "my files":** built-in ignores (`node_modules`, `.cache`, `target/`,
  `__pycache__`, Synology `@eaDir` and `#recycle`, `Thumbs.db`, `.DS_Store`, ...) plus per-volume
  gitignore-style patterns. A git repository is cataloged as one item, not file by file.
- **Scale target:** 1-10 TB and around 1M files. Rescans are incremental (unchanged size and mtime
  means skip). Hashing is lazy: size plus a partial hash first, and a full content hash only when
  sizes collide or on request.
- **Acting on files:** plan, then apply. An action builds a reviewable plan, Darren confirms it,
  and every applied change is written to an undo journal. Deletes go to a trash folder, never
  straight to unlink.

## Windows

Windows is out of scope for now, but don't make it impossible:

- Avoid Unix-only crates and APIs outside a small, isolated platform module.
- Store relative paths with `/` separators; convert at the filesystem boundary.
- Find config and data directories through the `directories` crate (XDG on Linux, AppData on
  Windows), not hard-coded `~/.config`.
- SSH transport relies on the `ssh` client, which Windows ships with (OpenSSH).

## Roadmap

1. **Scan + search (v1):** done. configure volumes, scan them incrementally into SQLite, classify files
   by kind (book, photo, music, video, document, code, ...), and search from the CLI (JSON) and a
   basic TUI. Single machine.
2. **MCP server** over the same queries. Done.
3. **Multi-machine:** agent mode and `ingest` over SSH; agents on the laptop, `bill` and the NAS.
   Code done (`push`, `sync`, `ingest`); deployment in progress (see `docs/setup.md`).
4. **Duplicates and backup coverage:** identical files across machines; files that exist in only
   one place.
5. **Classify and tag:** metadata extraction (EXIF, ID3, EPUB title and author) and Darren's own
   tags.
6. **Actions:** move, rename, delete and copy through plan, apply and undo.

## Commands

```bash
cargo test                              # unit tests (in each module) + tests/cli.rs end-to-end
cargo clippy --all-targets && cargo fmt # keep both clean
cargo build --release

gfiles                                  # interactive TUI; Enter prints the chosen path
gfiles volume add <path> [--name N] [--ignore PATTERN]...
gfiles volume list [--json]
gfiles volume remove <name>             # from the config and the catalog
gfiles scan [volume...]                 # all volumes if none named
gfiles search [words...] [-k KIND] [--volume V] [--machine M] [-n 50 | --all] [--json]
gfiles stats [--json]
gfiles mcp                              # MCP server on stdin/stdout
gfiles push                             # send this machine's volumes to [central]
gfiles sync                             # scan everything, then push (what timers run)
gfiles ingest --machine M               # receive M's push on stdin (run by push over ssh)

bin/deploy bill elrond                  # static musl build → ~/.local/bin/gfiles on each host

claude mcp add gfiles -- gfiles mcp               # register locally with Claude Code
claude mcp add gfiles -- ssh bill gfiles mcp      # or against the central catalog on bill
```

`--config` / `GFILES_CONFIG` and `--db` / `GFILES_DB` override the default paths
(`~/.config/gfiles/config.toml`, `~/.local/share/gfiles/catalog.db`). Use them, pointed at a
scratch directory, when trying the binary out, so experiments never touch the real catalog.

## Code layout

- `src/config.rs`: the per-machine TOML config. `volume add`/`remove` edit it with `toml_edit`
  so comments survive.
- `src/db.rs`: opens SQLite and runs migrations (`PRAGMA user_version`). Never edit a migration
  that has been committed; add a new one.
- `src/scan.rs`: walks a volume and writes its rows through `store`. Holds the default ignores.
- `src/store.rs`: `VolumeWriter`, the one way a volume's entries get written (scans and ingest
  both use it), plus volume and entry reads.
- `src/sync.rs`: the push/ingest stream between machines (format in its module doc).
- `src/classify.rs`: extension to kind.
- `src/search.rs`: queries shared by every interface (CLI, TUI and MCP). Interfaces
  stay thin and call into it, so they can't drift apart.
- `src/fmt.rs`: sizes and dates for display (dates are UTC, computed without a date crate).
- `src/tui/`: `app.rs` holds state and key handling, `ui.rs` draws it, `mod.rs` runs the loop.
  Test behavior through `App::handle_key` and layout through ratatui's `TestBackend`, not a
  real terminal.
- `src/mcp.rs`: the MCP server. The JSON-RPC layer is hand-written (newline-delimited, no async
  runtime) because a tools-only server needs just `initialize`, `ping`, `tools/list` and
  `tools/call`. Tools: `catalog_overview`, `file_locations` (matches grouped by folder, for
  "where are my X?"), `search_files`. All read-only.

## Implementation notes

- **Scans:** each scan is one transaction. It bumps the volume's `scan_gen`, stamps every entry it
  sees, then deletes entries with an older stamp. A volume whose root is missing (an unmounted
  disk or share) or unreadable is refused instead of being treated as empty, and entries under
  a folder or file the walk couldn't read are kept, not deleted ("couldn't read" isn't "gone").
- **Search:** FTS5 (external-content table kept in sync by triggers) over name and path. Every
  word is a quoted prefix match, so user input can't inject FTS syntax.
- **Skipped:** symlinks (not followed or recorded), and names that aren't valid UTF-8 (counted and
  reported, since the catalog stores text paths).
- **Hidden files** (`.*`) are ignored by default; a volume can bring one back with `!.name/`.
- **PDFs** are documents, not books, until metadata extraction can tell them apart.
- **Repos** are one `repo` row with size 0. Their size isn't computed yet.
- **The TUI draws to stderr**, so `$(gfiles)` captures only the path chosen with Enter (like
  fzf). Keep stdout clean while it runs. It loads at most 1000 rows; titles show the full count.
- To check the TUI by hand, run it in a detached tmux session (`tmux new-session -d -x 110 -y 26`),
  drive it with `tmux send-keys` and read it with `tmux capture-pane -p`.
- **Push/ingest:** each push sends every scanned volume in full (no deltas yet; fine at LAN
  speeds for ~1M files). Ingest writes one transaction per volume, verifies the entry count, and
  only after a complete stream forgets the pusher's volumes that left its config. It refuses a
  push claiming a machine other than `ingest --machine` (set by the forced command on the
  pusher's key in `authorized_keys`), the central machine's own name, a volume id owned by
  another machine, and a different `sync::PROTOCOL`. Bump `PROTOCOL` on any incompatible stream
  change. `store` validates every entry (relative path, no `.`/`..`/empty parts, name matches,
  known kind) and volume root (absolute), so a pushed path can't point outside its volume.
- **Deployment:** `docs/setup.md` has the per-machine setup. Binaries are static musl builds so
  they run on Synology DSM (old kernel and libc). The NAS is `elrond` (DS1019+, DSM 7, home
  `/var/services/homes/darren`, shares under `/volume1`).
- **MCP:** stdout carries protocol messages only; never print anything else there in `mcp` mode.
  Bad tool arguments come back as tool results with `isError: true` (so the model can correct
  itself); only unknown methods or tools are JSON-RPC errors. Results carry both a JSON text
  block and `structuredContent`. To check it with a real client, point `claude -p` at a scratch
  catalog with `--strict-mcp-config --mcp-config <file>` and ask it a question.
