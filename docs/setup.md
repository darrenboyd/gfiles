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
Scheduled Task → User-defined script. Run it as your user (not root), daily at night, with the
script:

```sh
/var/services/homes/darren/.local/bin/gfiles scan
```

The first scan of a large share takes a while; later ones are faster on a warm cache.

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
