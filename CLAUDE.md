# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

`gfiles` catalogs Darren's own files (not system files) across all his machines, so that he, and
AI tools working for him, can find, organize and manage them. The question it must answer well is
"where are all my books?", across every machine at once.

Machines in scope:

| Machine  | What it is                                   | OS / arch                 |
| -------- | -------------------------------------------- | ------------------------- |
| `gaffer` | Darren's laptop: the hub, holds the catalog; also the development machine | Pop!_OS, x86_64 |
| `elrond` | Synology DS1019+ NAS; a remote that gaffer pulls from | DSM 7 (Linux), x86_64 |

`bill`, the home-lab server, is out of scope: its files belong to the apps running there (and
are backed up where needed), not to Darren.

Both are x86_64 Linux, so one static `x86_64-unknown-linux-musl` build runs everywhere.
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
- **Topology:** gaffer is the hub and holds the catalog. Every machine scans its own disks
  into a local catalog; the hub pulls each remote's catalog over SSH (`ssh elrond gfiles
  export`). Remotes only answer, so no daemon, no open port and no sshd on the laptop, and the
  key lives on the hub. A hub that's away catches up the next time it can reach the remote.
- **Interfaces:** a TUI, CLI commands with `--json` output, and an MCP
  server mode (`gfiles mcp`, stdio) with typed tools for AI clients.
- **Volumes:** a volume is a configured root folder on a named machine, with a stable UUID. Paths
  are stored relative to the volume root, so moving a mount point doesn't orphan the catalog.
- **What counts as "my files":** built-in ignores (`node_modules`, `.cache`, `target/`,
  `__pycache__`, `_build/`, `*.o`, Go's module cache, Synology `@eaDir` and `#recycle`,
  `Thumbs.db`, `.DS_Store`, ...) plus per-volume gitignore-style patterns. A git repository is
  cataloged as one item, not file by file.
- **Scale target:** 1-10 TB and around 1M files. Every scan walks the whole volume and stamps
  every row, but only new or changed files (by size and mtime) are read: unchanged files keep
  their fingerprint. Fingerprints sample the start, middle and end of a file; a full content
  hash is computed only on request.
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
3. **Multi-machine:** the hub (gaffer) pulls the NAS's catalog over SSH. Code done (`export`,
   `pull`, `sync`); deployment in progress (see `docs/setup.md`).
4. **Duplicates and backup coverage:** identical files across machines; files that exist in only
   one place; mirrors (synced folders) and their health. Code done; deploy both machines
   together (sync `PROTOCOL` 2).
5. **Duplicate cleanup:** folder overlaps on one machine, drain previews, and cleanup plans
   (drain, keep-over) applied through a trash with undo. Code done; deploy both machines
   together before the first NAS plan.
6. **Classify and tag:** metadata extraction (EXIF, ID3, EPUB title and author) and Darren's own
   tags.
7. **More actions:** moves and vouched removals from a supplied list (`plan import`,
   `draft_plan_from_list`): code done; deploy both machines together (`apply::PROTOCOL` 3).
   Rename and copy through plan, apply and undo are still to come.

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
gfiles dupes [-k KIND] [--volume V] [--machine M] [--min-size 100M] [-n 20 | --all] [--json]
gfiles coverage [same filters] [--folder machine:volume/dir | mirror:name/dir] [--json]
gfiles mirrors [-n 20] [--json]         # [[mirror]] health, plus suggested mirrors
gfiles overlaps [--machine M] [-n 30 | --all] [--json]   # folder pairs sharing files, and what only each holds
gfiles drain <machine:volume/dir> [--except <dir>]... [--json]              # what draining it would free (saves nothing)
gfiles plan drain <folder>... [--except <dir>]... [--json]  # draft plan: remove files with a copy elsewhere
gfiles plan keep <keeper> --over <folder> [--except <dir>]... [--json]      # draft plan: remove what keeper holds
gfiles plan import <list.jsonl | -> [--json]  # draft plan from another tool's list of moves and removals
gfiles plan list | show <id> [--under <folder>] [-n | --all] [--json]
gfiles plan show <id> --folders [--depth N | --all] [--under <folder>]  # tree: what leaves, arrives, stays per folder
gfiles plan apply | undo | empty <id>   # Darren only: asks for the id on the terminal
gfiles plan drop <id>                   # delete a draft
gfiles hash <volume> [folder]           # full BLAKE3 hashes, to confirm duplicates (slow)
gfiles mcp                              # MCP server on stdin/stdout
gfiles pull                             # fetch each [[remote]]'s catalog over ssh
gfiles sync                             # scan everything, then pull (what timers run)
gfiles export                           # this machine's catalog on stdout (run by pull over ssh)

bin/deploy elrond                       # static musl build → ~/.local/bin/gfiles on each host
cargo install --path . --root ~/.local  # install on gaffer (where the systemd unit expects it)

claude mcp add gfiles -- gfiles mcp     # register with Claude Code (on gaffer)
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
- `src/store.rs`: `VolumeWriter`, the one way a volume's entries get written (scans and pulls
  both use it), plus volume and entry reads.
- `src/sync.rs`: the export/pull stream between machines (format in its module doc).
- `src/classify.rs`: extension to kind.
- `src/fingerprint.rs`: sampled fingerprints (`p1:`), full hashes (`b3:`), and repo HEAD
  commits (`git:`). Every read goes through the guards there (no-follow open, same file
  before and after).
- `src/dupes.rs`: duplicate groups and backup coverage across machines.
- `src/mirror.rs`: `[[mirror]]` config (folders that a sync tool keeps the same on several
  machines), the SQL fragments that treat replicas as one logical file, mirror health and
  mirror suggestions.
- `src/platform.rs`: the OS-specific bits (no-follow open, network filesystem detection).
- `src/search.rs`: queries shared by every interface (CLI, TUI and MCP). Interfaces
  stay thin and call into it (and `dupes`, `mirror`), so they can't drift apart. A mirror
  file is one hit, listing its other replicas; each hit says how many copies of its content
  exist (`copies`) and in how many places (`places`, as backup coverage counts them).
- `src/overlap.rs`: folder pairs that share content on one machine, and drain previews.
- `src/plan.rs`: building cleanup plans (which copy stays, what's left and why), the
  synced-folder refusal, and plan storage (`plans`, `plan_items`, `plan_left`,
  `plan_rejected`).
- `src/listplan.rs`: plans from a supplied list (JSON lines of removals and moves): parsing
  and checking each entry against the catalog, and rejecting the ones that fail.
- `src/plantree.rs`: a plan's folder tree (`plan show --folders`, and the MCP tool with
  `folders: true`): what leaves, arrives and stays in each folder it changes.
- `src/apply.rs`: the only code that writes under a volume root: moves to the trash and to
  a list's destinations, undo, empty, the per-machine lock, and the `plan exec` exchange the hub runs over SSH.
- `src/fmt.rs`: sizes and dates for display (dates are UTC, computed without a date crate).
- `src/tui/`: `app.rs` holds state and key handling, `ui.rs` draws it, `mod.rs` runs the loop.
  Test behavior through `App::handle_key` and layout through ratatui's `TestBackend`, not a
  real terminal.
- `src/mcp.rs`: the MCP server. The JSON-RPC layer is hand-written (newline-delimited, no async
  runtime) because a tools-only server needs just `initialize`, `ping`, `tools/list` and
  `tools/call`. Tools: `catalog_overview`, `file_locations` (matches grouped by folder, for
  "where are my X?"), `search_files`, `find_duplicates`, `backup_coverage`, `mirror_health`,
  `folder_overlaps`, `drain_preview`, `draft_cleanup_plan`, `draft_plan_from_list`,
  `list_cleanup_plans`, `show_cleanup_plan`. All read-only, except that the two draft tools
  save a draft plan in the catalog; none touches files.

## Implementation notes

- **Scans:** the walk is one transaction. It bumps the volume's `scan_gen`, stamps every entry it
  sees, then deletes entries with an older stamp. A fingerprint pass follows, reading only rows
  without a fingerprint (and every repo's HEAD), committing in batches; each update is guarded
  by the row's size and mtime, so a file that changed in between isn't stamped. A volume whose root is missing (an unmounted
  disk or share) or unreadable is refused instead of being treated as empty, and entries under
  a folder or file the walk couldn't read are kept, not deleted ("couldn't read" isn't "gone").
- **Search:** FTS5 (external-content table kept in sync by triggers) over name and path. Every
  word is a quoted prefix match, so user input can't inject FTS syntax.
- **Skipped:** symlinks (not followed or recorded), and names that aren't valid UTF-8 (counted and
  reported, since the catalog stores text paths).
- **Hidden files** (`.*`) are ignored by default; a volume can bring one back with `!.name/`.
- **PDFs** are documents, not books, until metadata extraction can tell them apart.
- **Repos** are one `repo` row with size 0. Their size isn't computed yet. They match each
  other by HEAD commit.
- **Duplicates and coverage:** same size + sampled fingerprint is a "probable" match; equal
  full hashes on every copy (`gfiles hash`) make it confirmed. Full hashes that differ always
  win: when files sharing a size and fingerprint have different full hashes, they match only
  by full hash, and one without a hash matches only others without one (`dupes::content`,
  used by duplicates, coverage, search, overlaps and plans). Zero-byte files never match.
  A file is covered when its content is in at least two places and on at least two machines,
  where a place is a machine, or a whole mirror (the sync tool replicates deletes, so a
  mirror's replicas aren't backups of each other). Mirrors live in the hub's config and
  are loaded into a per-connection temp table when a query command opens the catalog (the MCP
  server reloads them before each tool call). A mirror folder whose volume isn't cataloged yet
  is left out with a warning, carried in the results; the rest of the mirror still loads, so
  its replicas never count as backups of each other.
- **Mirror health** ignores Google-native stand-ins (`.gddoc`, `.gmap`, ...), reports sync
  conflict copies, and pairs one-sided files across replicas by content or extension
  (Insync and Cloud Sync name some files differently). A file missing from, or different in,
  a replica scanned before the file last changed is listed as `not_yet_seen`, not as drift:
  the catalog hasn't seen that side since.
- **The TUI draws to stderr**, so `$(gfiles)` captures only the path chosen with Enter (like
  fzf). Keep stdout clean while it runs. It loads at most 1000 rows; titles show the full count.
- To check the TUI by hand, run it in a detached tmux session (`tmux new-session -d -x 110 -y 26`),
  drive it with `tmux send-keys` and read it with `tmux capture-pane -p`.
- **Network folders:** `volume add` warns when the path is on CIFS, NFS, sshfs or 9p: those
  files belong to another machine, which should scan them itself.
- **Export/pull:** each export sends every scanned volume in full, fingerprints and hashes
  included, since content can only be read on the machine holding it (no deltas yet; fine at LAN
  speeds for ~1M files). Ingest (the receiving half of `pull`) writes one transaction per
  volume, verifies the entry count, and only after a complete stream forgets the remote's
  volumes that left its config. It refuses a stream claiming a machine other than the
  `[[remote]]` name it was pulled for, the hub's own name, a volume id owned by another
  machine, and a different `sync::PROTOCOL`. Bump `PROTOCOL` on any incompatible stream change.
  `store` validates every entry (relative path, no `.`/`..`/empty parts, name matches, known
  kind) and volume root (absolute), so a pulled path can't point outside its volume. Every
  `pull`/`sync` forgets the volumes of machines that are neither this one nor a configured
  remote.
- **Deployment:** `docs/setup.md` has the per-machine setup. Binaries are static musl builds so
  they run on Synology DSM (old kernel and libc). The NAS is `elrond` (DS1019+, DSM 7, home
  `/var/services/homes/darren`, shares under `/volume1`).
- **MCP:** stdout carries protocol messages only; never print anything else there in `mcp` mode.
  Bad tool arguments come back as tool results with `isError: true` (so the model can correct
  itself); only unknown methods or tools are JSON-RPC errors. Results carry both a JSON text
  block and `structuredContent`. To check it with a real client, point `claude -p` at a scratch
  catalog with `--strict-mcp-config --mcp-config <file>` and ask it a question.
- **Cleanup plans:** only same-machine copies count; mirror files are never removed or kept,
  repos never removed. The copy that stays is outside the plan's folders when one exists, else
  the shortest path, then alphabetical; a keep-over plan only keeps copies in its keeper.
  `--except` folders (inside the folders files are removed from) are left out entirely, and
  count as outside, so a copy there can be the one that stays. A removal is confirmed when
  the file and its kept copy already have equal full hashes in the catalog, else probable;
  `plan show` counts both for a draft. Apply stores the full hashes it reads of files that
  stay (kept copies, and files it skipped as different) in that machine's own catalog,
  guarded by size and mtime, so the hub learns them at its next pull. Plans are stored on the hub; `apply` runs in-process for this machine and as
  `ssh <apply_host> gfiles plan exec` for a remote (format in `apply`'s module doc, versioned
  by `apply::PROTOCOL`). Files go to `<volume root>/.gfiles-trash/<plan id>/files/`, which scans
  always skip; the catalog forgets them at the next scan, and until then overlaps and new plans
  leave out files an applied plan moved (as they were when moved; a new file at the same path
  counts). Nor may a plan move or remove a file an applied plan's trash relies on (a removal's
  kept copy, a move's destination) until that plan is emptied or undone: drafts leave such files
  out (a list rejects them), and apply skips them, deciding afresh each time. A re-run apply resumes. The executor never creates a volume root: a file on an
  unmounted volume is skipped, and undo and empty leave the plan where it was while one of its
  volumes is missing. Undo leaves a file in the trash when its path is taken again. Empty
  keeps files that changed after the move, or that the journal doesn't list. A removed file
  whose kept copy is gone or changed stays too, unless another file on that machine holds its
  content: one the catalog says may match (same size and fingerprint, no differing full
  hash), outside synced folders as plan building defines them (the hub loads its mirrors and sends
  its mirror and suggested folders by volume name, and `not_synced`, which waives suggested
  folders but never a mirror; a name the machine doesn't have stops the search), that reads
  back with the journal's full hash. Empty removes the plan's trash folder only once nothing is left in it. Undo and
  empty measure what's left in the plan's trash; the hub stores it as the plan's `in_trash`
  (`in_trash_partial` when a volume couldn't be measured), which until then is the bytes of
  the removals apply moved.
- **Plans from a list:** another tool decides what should change (gfiles can't tell that a
  photo whose metadata a photo manager rewrote is the same shot, or where a new photo
  belongs) and hands `plan import` (or the MCP tool) JSON lines:
  `{"remove": "m:vol/path", "keep": "m:vol/path"}`, optionally `"vouched": true` with a
  `"reason"`, and `{"move": "m:vol/path", "to": "m:vol/path"}`. Valid entries make a draft
  `list` plan on one machine; failing ones are stored as rejects with their line and reason,
  and a list with nothing valid saves no plan. An unvouched removal must not differ by the
  catalog (size, fingerprint, full hash) and is compared in full at apply; a vouched one
  skips only that comparison. Moves rename, or across filesystems copy, verify by full hash,
  then trash the original; undo and empty treat a copied move's original like a removal's
  file, guarded by the copy's journaled hash. A move's journal line is synced before the file
  moves, and destinations a scan would skip (hidden, `#recycle`, `@eaDir`, ...) are refused on
  the hub and again by the executor. A moved file (and one undo puts back) keeps its
  fingerprint and full hash: the executor reads them from its machine's catalog before files
  move (so a scan running alongside can't lose them), records them there afterwards
  (`carried`), and the next scan of that volume gives them to the entry at the new path while
  its size and mtime match, then drops the volume's records (except under folders it couldn't
  read).
- **Agents never run `gfiles plan apply`, `undo`, `empty` or `exec`**, by any means (including
  tmux, a pseudo-terminal, or setting `SSH_CONNECTION`). `plan exec` refuses outside SSH, but
  anyone with an SSH login to a remote can still call it there without the confirmation. They may build and show plans; Darren applies them. To try the executor,
  use the tests or a scratch `--config`/`--db` over a scratch folder.
