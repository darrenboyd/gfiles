# Coding standards

The rules code in gfiles follows. Read this before writing or reviewing a change; every
review checks the diff against it. Commands, setup and anything `cargo clippy` or
`cargo fmt` already enforces live in `CLAUDE.md` or the tools, not here.

## Catalog and file safety

- **"Couldn't read" is never "gone".** Code that deletes catalog rows because their items
  weren't seen must leave out anything the walk failed to read, and must refuse outright when
  the volume root is missing or unreadable. A scan that deletes on partial information
  silently wipes the catalog, and the hub's next `pull` copies the loss into its catalog.
- **Only the apply executor touches the files in a volume.** No other code path writes,
  moves or deletes under a volume root, and the scanner never follows a symlink. The
  executor (`apply`) only creates and writes inside the volume's `.gfiles-trash` folder,
  renames files into it and back out of it, recreates a restored file's missing parent
  folders on undo, and deletes a plan's trash folder when emptying it. Its renames never
  replace an existing file and never fall back to copy-and-delete. Before moving a file it
  checks that the file and the copy that stays are unchanged since the scan, aren't the same
  file, and have equal full content; any doubt skips the file. It never creates a volume
  root, and creates each trash folder level only after checking the one above isn't a
  symlink. Emptying keeps any file that changed since the move, whose kept copy is gone or
  changed, or that the journal doesn't vouch for, and never deletes anything it didn't list
  and check. Undo and empty don't count a plan done while one of its volumes is missing. Each
  of these guarantees has a test that fails if it is removed, so a refactor can't drop one
  quietly.
- **Only Darren applies a plan.** `plan apply`, `undo` and `empty` ask for the plan id on
  the controlling terminal (not stdin), refuse without one, and have no flag to skip the
  question. `plan exec`, which carries out a confirmed plan for the hub, refuses unless it
  runs over SSH. MCP tools may draft plans in the catalog but never apply them.
- **Content reads are guarded.** Code that reads a file's content (fingerprints, hashes)
  opens it without following a symlink, checks it is still the regular file the catalog
  row describes (size and mtime) before and after reading, and writes the result only to a
  row that hasn't changed since. A file that changed or vanished is skipped, not recorded
  with a fingerprint that belongs to other content. Small metadata reads (a repo's `HEAD`,
  refs and `packed-refs`) go through the same no-follow open, never open a FIFO or other
  special file (it could block an unattended scan), and are capped in size.

## Trust boundaries

- **A pulled stream is untrusted input.** `ingest` takes the remote's identity from the hub's
  config (the `[[remote]]` it pulled from), never from the stream, and refuses anything
  claiming another machine. Every entry and volume root is validated in `store` before it is
  written, so a stored path is always relative and inside its volume, whoever sent it.
- **User input never becomes SQL or FTS syntax.** Values are bound as parameters, and search
  words are quoted FTS prefix terms. The same holds for MCP tool arguments, which are
  type-checked and allowlisted where a fixed set of values exists.
- **Unattended and network operations are bounded.** Anything run from a timer or over SSH
  has a timeout or keep-alive so a dead peer can't hang it, and anything installed over the
  network is checked (size, runs) before it replaces the working copy.

## Interfaces

- **Interfaces stay thin.** The CLI, TUI and MCP server call shared query modules
  (`search`, `dupes`, `mirror`, `overlap`, `plan`) rather than writing their own queries, so their answers
  can't drift apart.
- **stdout is reserved.** In `mcp` mode it carries protocol messages only; while the TUI runs
  it carries only the chosen path (the TUI draws to stderr). Diagnostics go to stderr.
- **Agent-facing results don't mislead.** A result list that can be cut off says so (a
  total alongside the items), and a filter value that matches nothing is an error listing
  the valid values, not an empty result. Bad MCP tool arguments come back as tool results
  with `isError: true` so the model can correct itself; only unknown methods or tools are
  JSON-RPC errors.

## Compatibility

- **Schema and protocol changes are additive.** Never edit a migration that has been
  committed; add a new one. Bump `sync::PROTOCOL` on any incompatible change to the export
  stream, since ingest refuses other versions, and `apply::PROTOCOL` on any incompatible
  change to the `plan exec` exchange.
- **Don't rule out Windows.** Unix-only crates and APIs stay inside a small, isolated
  platform module (unix-only tests behind `#[cfg(unix)]` are fine). Stored relative paths
  use `/` and are converted at the filesystem boundary. Config and data directories come
  from the `directories` crate, not hard-coded paths.
- **Config edits keep the user's file intact.** The TOML config is edited with `toml_edit`
  so comments and layout survive.

## Maintaining this file

A rule belongs here only when the problem showed up in a review finding that was fixed, a
fix commit, a cleanup across the codebase, or a solution record, and no lint or check
already catches it. State each rule as a condition that stays true without naming
volatile numbers or symbols. When a change establishes or retires a rule, update this file
in the same change.
