# Setting up gfiles across machines

One machine holds the central catalog (here `bill`). Every other machine scans its own volumes
into a local catalog and pushes them to the central one over SSH with `gfiles sync`.

```text
gaffer (laptop) ──┐
                  ├─ ssh bill gfiles ingest --machine <name> ──▶ bill: central catalog
elrond (NAS)   ───┘                                              (TUI, MCP, search over everything)
```

## 1. Install the binary

From a checkout on the development machine:

```bash
sudo apt install musl-tools   # once: a C compiler for the static build
bin/deploy bill elrond        # static build, copied to ~/.local/bin/gfiles on each host
cargo install --path .        # on the development machine itself
```

The static build runs on any x86_64 Linux, including Synology DSM. The pushing and the
central machine must run the same gfiles version: `ingest` refuses a different protocol.

## 2. The central machine

Nothing to configure unless it has volumes of its own, in which case add them and scan with a
timer as below (`gfiles sync` without a `[central]` section just scans). Its catalog lives at
`~/.local/share/gfiles/catalog.db`.

## 3. Each other machine

```bash
gfiles volume add ~/Documents
gfiles volume add ~/Downloads --ignore 'go/pkg/'
```

Then add the central machine to `~/.config/gfiles/config.toml`:

```toml
machine = "gaffer"                 # optional; defaults to the hostname

[central]
host = "bill"                      # ssh destination
gfiles = "~/.local/bin/gfiles"     # non-interactive ssh doesn't put ~/.local/bin on PATH
```

Check it by hand: `gfiles sync`.

### A push-only key

Pushes run from a timer with `ssh -o BatchMode=yes`, so the machine needs a key that `bill`
accepts *without a prompt*, which means a key without a passphrase. Give each pushing machine
its own key and lock it on `bill` to the one command a push needs, so whoever gets hold of it
(say, on a compromised NAS) can only push that machine's catalog: no shell on `bill`, and no
writing as any other machine.

On the pushing machine (here `elrond`):

```bash
ssh-keygen -t ed25519 -N '' -C gfiles-push@elrond -f ~/.ssh/gfiles_push
cat ~/.ssh/gfiles_push.pub
```

Add a host alias that uses only that key, in `~/.ssh/config`:

```text
Host bill-gfiles
    HostName bill
    IdentityFile ~/.ssh/gfiles_push
    IdentitiesOnly yes
```

and use it as the `host` in the `[central]` section above (`host = "bill-gfiles"`).

On `bill`, add the public key to `~/.ssh/authorized_keys` with a forced command naming the
machine (it must match the pushing machine's `machine` name):

```text
restrict,command="~/.local/bin/gfiles ingest --machine elrond" ssh-ed25519 AAAA... gfiles-push@elrond
```

`restrict` turns off port forwarding, agent forwarding and a terminal, and `command=` runs
`gfiles ingest --machine elrond` whatever the client asked for, so ingest refuses a push that
claims any other machine. Check it with `gfiles sync` on `elrond`, and see that
`ssh bill-gfiles` gives no shell.

On a machine you log in from anyway (the laptop), a passphrase-protected key works for the
timer only if the timer can reach a running ssh-agent; the dedicated restricted key is simpler.

## 4. Run it on a schedule

**Linux (systemd user timer):**

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/gfiles-sync.* ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now gfiles-sync.timer
journalctl --user -u gfiles-sync      # results
```

On a server with nobody logged in, also run `sudo loginctl enable-linger $USER` so user timers
keep running.

**Synology DSM (Task Scheduler):** Control Panel → Task Scheduler → Create → Scheduled Task →
User-defined script. Run as your user (not root, so it uses your SSH key and config), daily or
hourly, with the script:

```sh
/var/services/homes/darren/.local/bin/gfiles sync
```

## Using the central catalog

```bash
ssh -t bill ~/.local/bin/gfiles                         # the TUI, over everything
claude mcp add gfiles -- ssh bill ~/.local/bin/gfiles mcp # AI tools, over everything
```
