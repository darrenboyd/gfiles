# Setting up gfiles across machines

`gaffer` (the laptop) is the hub: it holds the catalog of everything. The NAS (`elrond`) scans
its own shares into a local catalog, and the hub pulls that catalog over SSH. The NAS only
answers; nothing connects to the laptop.

```text
elrond (NAS)                                  gaffer (laptop, the hub)
  nightly: gfiles scan  ◀── ssh elrond gfiles export ──  hourly: gfiles sync
  (its own catalog)                                      (scan local + pull: TUI, MCP, search)
```

## 1. Install the binary

From a checkout on gaffer:

```bash
sudo apt install musl-tools               # once: a C compiler for the static build
bin/deploy elrond                         # static build, copied to ~/.local/bin/gfiles on the NAS
cargo install --path . --root ~/.local    # on gaffer itself: ~/.local/bin/gfiles
```

Re-run both after pulling new code: the installed binaries don't update with the checkout.

The static build runs on Synology DSM's old kernel and libc. Both machines must run the same
gfiles version: a pull refuses a different protocol.

## 2. The NAS

On `elrond`, name the machine and add the shares to catalog:

```bash
~/.local/bin/gfiles volume add /volume1/Media
~/.local/bin/gfiles volume add /volume1/photo
```

and in `~/.config/gfiles/config.toml`:

```toml
machine = "elrond"
```

Scan on a schedule with DSM's Task Scheduler: Control Panel → Task Scheduler → Create →
Scheduled Task → User-defined script. Run it as your user (not root, so it reads the shares
with your permissions and writes your catalog), daily at night (say 03:00), with the script:

```sh
export HOME=/var/services/homes/darren
"$HOME/.local/bin/gfiles" scan
```

Scheduled tasks don't always get a login environment, so setting `HOME` makes sure gfiles finds
`~/.config/gfiles/config.toml` and its catalog. Under Task Settings, ticking *Send run details
by email* (only when the script terminates abnormally) is the only way you'll hear about a
failed scan. Test it with **Run**; with Task Scheduler → Settings → *Save output results* turned
on, Action → View Result shows what the scan printed.

The first scan of a large share takes a while: besides walking it, gfiles reads a sample
(three 64 KiB pieces) of every file to fingerprint it, for finding duplicates. Later scans
only read new and changed files. The same happens once on the first scan after upgrading from
a version without fingerprints. gfiles only ever opens files to read them, but on a
filesystem that records access times, reading a file updates its atime (`relatime`, the usual
default, does this at most once a day).

Fingerprints match files by sample, so duplicates are "probable" until full hashes agree. To
confirm, hash a volume (or one folder of it) on the machine that holds it, then pull:

```sh
~/.local/bin/gfiles hash Media Photos/2019   # reads every byte: slow on big folders
```

## 3. The hub

On `gaffer`, add local folders:

```bash
gfiles volume add ~/Documents
gfiles volume add ~/Downloads --ignore 'go/pkg/'
```

and the NAS as a remote, in `~/.config/gfiles/config.toml`:

```toml
machine = "gaffer"

[[remote]]
name = "elrond"            # must match `machine` in the NAS's config
host = "elrond-gfiles"     # ssh destination (the alias below); defaults to name
```

### A pull-only key

The hourly timer runs `ssh -o BatchMode=yes`, so the hub needs a key the NAS accepts *without a
prompt*, which means a key without a passphrase. Give it its own key and lock that key on the
NAS to the one command a pull needs, so whoever gets hold of it can only read the NAS's
catalog: no shell, and no way to change anything.

On `gaffer`:

```bash
ssh-keygen -t ed25519 -N '' -C gfiles-pull@gaffer -f ~/.ssh/gfiles_pull
cat ~/.ssh/gfiles_pull.pub
```

Add a host alias that uses only that key, in `~/.ssh/config`:

```text
Host elrond-gfiles
    HostName elrond          # copy HostName and Port from your existing entry for the NAS
    IdentityFile ~/.ssh/gfiles_pull
    IdentitiesOnly yes
```

On `elrond`, add the public key to `~/.ssh/authorized_keys` with a forced command:

```text
restrict,command="/var/services/homes/darren/.local/bin/gfiles export" ssh-ed25519 AAAA... gfiles-pull@gaffer
```

`restrict` turns off port forwarding, agent forwarding and a terminal, and `command=` runs
`gfiles export` whatever the client asked for. DSM's sshd only reads `authorized_keys` when the
home folder isn't group-writable: `chmod 755 ~`, `chmod 700 ~/.ssh`,
`chmod 600 ~/.ssh/authorized_keys`.

Check it from `gaffer`: `gfiles pull` lists the NAS's volumes, `ssh elrond-gfiles id` prints
the catalog stream instead of running `id`, and `ssh -t elrond-gfiles` gets no terminal.

## 4. Run it on a schedule

On `gaffer`, a systemd user timer runs `gfiles sync` hourly (scan local volumes, then pull):

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/gfiles-sync.* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now gfiles-sync.timer
journalctl --user -u gfiles-sync      # results
```

Away from home, the pull fails within 30 seconds and the local scan still runs; the catalog
keeps the NAS's last pulled state (with its scan time) until the next pull succeeds.

## Using the catalog

On `gaffer`:

```bash
gfiles                                 # the TUI, over everything
claude mcp add gfiles -- gfiles mcp    # AI tools, over everything
```

Taking a remote out of the config removes its volumes from the catalog on the next `pull` or
`sync`.

```bash
gfiles dupes --min-size 100M           # same content in more than one place
gfiles coverage                        # files with no copy on another machine, by folder
gfiles coverage --folder elrond:Media/Photos/2019   # one folder's files
gfiles mirrors                         # mirror health, and folders that look like mirrors
```

## Cleaning up duplicates

`gfiles overlaps` lists folder pairs on one machine that hold the same files (with what only
each side holds), and
`gfiles drain <machine:volume/dir>` shows what emptying one of them would free. A plan records
what to remove and which copy of each file stays:

```bash
gfiles plan drain elrond:darren/todo                         # remove what has a copy elsewhere
gfiles plan keep elrond:darren/Photos --over elrond:darren/todo   # remove what Photos also holds
gfiles plan show <id> --all                                  # review every file
gfiles plan show <id> --under elrond:darren/todo/Incoming    # what it leaves there, by folder
gfiles plan apply <id>                                       # asks you to type the id
gfiles plan undo <id>                                        # put everything back
gfiles plan empty <id>                                       # free the space for good
```

`apply` moves each file to `.gfiles-trash/<id>/files/` at the top of its volume, after checking
its full content against the copy that stays. The space comes back only with `empty`, which
keeps any file that changed in the trash or whose kept copy has since gone or changed. A
volume that isn't mounted is left alone: apply skips its files, and undo and empty leave the
plan as it was until the disk is back. Plans refuse mirrors, and folders
that look synced (ones `gfiles mirrors` suggests, or inside `Dropbox`, `OneDrive` or
`SynologyDrive`): add the folder to `[[mirror]]` if a tool syncs it, or list it in
`not_synced = ["machine:volume/dir"]` at the top of the hub's config if none does.

**Before the first plan on the NAS:**

- Deploy the same gfiles to both machines (`bin/deploy elrond`, then
  `cargo install --path . --root ~/.local`): the hub refuses a NAS on another plan protocol.
- A NAS plan runs as `ssh elrond gfiles plan exec`, with your normal login, not the pull-only
  key (that one only runs `export`, and the hub refuses its answer). Set `apply_host` in the
  `[[remote]]` if `elrond` isn't the right ssh destination. If an ssh agent holds the pull key,
  add `IdentitiesOnly yes` and your own `IdentityFile` to the `Host elrond` entry, so the agent
  doesn't offer the pull key first.
- Synology snapshots of the share keep the deleted files' space until the snapshots holding
  them expire.
- Files owned by another DSM user can't be moved and are skipped.

## Mirrors

A mirror is a folder that a sync tool keeps the same on several machines, like Google Drive
synced by Insync on gaffer and by Synology Cloud Sync on the NAS. Tell the hub about it, in
`~/.config/gfiles/config.toml` on `gaffer`:

```toml
[[mirror]]
name = "drive"
folders = ["gaffer:darren/Drive", "elrond:darren/Drive"]   # machine:volume/folder
ignore = [".sync/"]                                        # optional, gitignore-style
```

Each folder is `machine:volume/path`, with the volume names from `gfiles stats`. Search then
shows each mirror file once with its replicas. A mirror counts as one place for backup
coverage, because the sync tool copies deletes and damage to every replica: a file only in
the Drive mirror isn't backed up, however many machines sync it. `gfiles mirrors` reports
files only in some replicas, files that differ, sync-tool conflict copies, and files the two
tools named differently, and suggests folder pairs that look like mirrors with a snippet to
paste. Don't add a folder that's a backup copy rather than a sync: a backup should count as
its own place.
