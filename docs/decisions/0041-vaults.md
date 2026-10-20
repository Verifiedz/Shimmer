# 0041. Vaults: named Shimmer folders you can switch between

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (`crates/app/src/main.rs` applies the vault
before the runtime starts; the CLAUDE.md §7 and §15 wording below) · No `core`, `proto` or daemon
change · Depends on #293 (ADR 0039: one socket per Shimmer folder)

## Context

ADR 0039 (#293) makes several Shimmer folders *work* side by side: each `$SHIMMER_HOME` gets its own
daemon and socket, so a test folder can no longer silently write into the real one. Using one is still
awkward: you type `SHIMMER_HOME=~/play shimmer …` on every command, and nothing reminds you which
folder you're in. Forget it once and a test runs on your real data.

Who needs more than one folder:

- **Template and kit authors** (workspace, automation and fetchers templates, `shimmer setup`, #79;
  later the marketplace, #155). A template must work on a fresh install with nothing seeded (ADR 0023),
  and a buggy template's scripts must not touch real data. That needs a clean, throwaway folder.
- **Dogfooding.** A safe copy of your real data to try a purge, a rule or an import on.
- **Long-lived splits**: work and personal.

## Decision

### 1. A vault is a named Shimmer folder

A **vault** is a `$SHIMMER_HOME` with a name. **`main`** is the one you have today (the platform's
default folder, CLAUDE.md §7): always there, never renamed, removed or purged. Every other vault has a
name (`shimmer_core::ids::is_valid_id`; `main` is reserved) and a path.

Each vault is a complete Shimmer folder with its own daemon (ADR 0039). Nothing in the daemon, the
protocol or any module knows vaults exist: the CLI picks the folder, and everything else sees one
`$SHIMMER_HOME`, as today.

**Why "vaults".** `folders` is accurate but plain; `profiles` is kept free for a future, optional
identity or sync module (CLAUDE.md §1.5), where it's the word people expect; `spaces` would be confused
with workspaces, `envs` with environment variables and `[env]`, `zones` with time zones. "Vault" says
what it is, a separate place your data lives. CLAUDE.md §15's "boring on purpose" is about themed
words like `mite` that don't say what a command does; `vaults` does, so it stays the canonical name.

### 2. Commands

The words are ADR 0038 §6's, so they mean what they mean everywhere else. The shape is `packs`'s, the
other client-only command.

```
shimmer vaults                              same as list
shimmer vaults list [--removed]             every vault: NAME, PATH, whether it's the one in use, its daemon
shimmer vaults show VAULT                   one vault in full: path, socket, daemon, size, made by Shimmer or adopted
shimmer vaults status                       which vault you're in and why (--vault, SHIMMER_HOME, use, or main), and which daemons run
shimmer vaults new NAME [--path DIR]        a new, empty vault
shimmer vaults use VAULT                    make it the default; `vaults use main` goes back
shimmer vaults rename VAULT NEW             its name only; the folder doesn't move
shimmer vaults copy VAULT NEW [--path DIR] [--live]   a new vault with VAULT's data, everything in it off (§5)
shimmer vaults remove VAULT                 forget it; its files are kept; `restore` brings it back
shimmer vaults restore REMOVED              remember it again
shimmer vaults purge [REMOVED] [--yes]      delete removed vaults' files for good (asks first, with a count)
shimmer --vault NAME <command>              run one command in another vault
```

Like every command: each takes `--json`; errors are the usual `not_found`, `conflict` and
`invalid_params`, with exit codes 1 (failed) and 2 (bad usage); anything that can't be undone asks first
and takes `--yes`.

### 3. Which vault a command uses

In this order, first match wins:

1. **`--vault NAME`** on the line.
2. **`SHIMMER_HOME`** in the environment (a workspace's scripts, tests, a person's own alias).
3. The vault chosen with **`vaults use`**.
4. **`main`**.

What's typed beats what's inherited, so a script reaches another vault only by naming it. A script that
doesn't type `--vault` behaves exactly as today: the script ABI (CLAUDE.md §10.1) is unchanged, and a
workspace's scripts get their own vault's path as `SHIMMER_HOME`.

`--vault` is a front flag, like `--pack`: it counts before the command word, so a records field called
`vault` still works after it. `--socket` still wins for the socket itself.

**Applied once, first thing.** `crates/app/src/main.rs` resolves the vault before the tokio runtime and
any thread exist, then sets `SHIMMER_HOME` and `SHIMMER_SOCKET` (ADR 0039's `default_socket` for that
folder) for this process. Everything that already reads `proto::paths::shimmer_home()` and
`socket_path()` then just works: auto-start, user packs in `$SHIMMER_HOME/packs` and their linked
commands, `shimmer startup`, `doctor`, Tab completion, workspaces' step logs. `main` becomes a plain
function that builds the runtime itself, instead of `#[tokio::main]`. Setting the environment before
any thread exists is the one moment it's safe; nothing changes it afterwards. `shimmer daemon` is
unaffected: auto-start already pins its environment.

### 4. Where vaults are kept

In the CLI's own `~/.config/shimmer/cli.toml` (ADR 0013), next to the active pack and the login
service, never in `$SHIMMER_HOME`, which belongs to the daemon:

```toml
vault = "play"                    # the one `vaults use` chose; absent: main

[vaults.play]
path = "/home/me/.local/share/shimmer-vaults/play"
made = true                       # made by `vaults new`/`copy`: the only kind `purge` deletes

[removed_vaults.old-test]
path = "/home/me/.local/share/shimmer-vaults/old-test"
made = true
```

Written all or nothing, like the rest of `cli.toml`; an unknown key is a warning, so an older and a
newer Shimmer can share the file.

### 5. Rules for each command

- **`new`**: the default path is `<data dir>/shimmer-vaults/NAME` (`~/.local/share/shimmer-vaults/` on
  Linux, `~/Library/Application Support/shimmer-vaults/` on macOS): beside the main folder, never inside
  it, since the main folder is the one you sync with git. `--path DIR` must not exist or be empty
  (`made = true`), or already be a Shimmer folder, one with the store's `.gitignore` and `data/`
  (**adopted**, `made = false`). Anything else is refused. The folder is filled in by its daemon the
  first time a command runs there.
- **`use`**: refuses a vault whose folder is gone, saying where it was expected.
- **`rename`**: the name only. If it's the vault in use, `use` follows it.
- **`remove`**: refuses `main`, the vault in use (`use` another first), and the vault `shimmer startup`
  starts at login (`startup off` first). It stops that vault's daemon (`core.shutdown` on its socket),
  since nothing can name it any more, and keeps every file.
- **`restore`**: `conflict` if a vault of that name exists again; `not_found` if nothing's removed.
- **`purge`**: confirmed with `{"vaults": N}`, the same shape as every purge (ADR 0038 §4). It deletes a
  folder only when it was made by Shimmer (`made = true`), it still looks like a Shimmer folder, and it
  isn't a symlink. Its daemon is stopped first, and the folder goes once the socket has. An adopted vault
  is only forgotten: Shimmer never deletes a folder it didn't make.
- **`copy`** needs no daemon change:
  1. It stops VAULT's daemon, as `shimmer shutdown` does, and **holds VAULT's `.daemon.lock` itself**,
     so no daemon can start there mid-copy. There is still exactly one writer (CLAUDE.md §2): for those
     moments, none.
  2. It copies everything except what's derived or tied to the machine: `index.sqlite*`, `cache/`,
     `logs/`, the socket and the lock. `.staging/` *is* copied, so the copy's own recovery finishes any
     save that was committed (ADR 0040).
  3. It releases the lock and registers NEW (`made = true`).
  4. **The copy starts with everything off**, so it never sends a duplicate reminder or email. The CLI
     starts the copy's daemon and switches things off **through their own ops**, never by editing the
     copy's files: `scheduler.disable` for every enabled schedule, `automations.disable` for every
     enabled rule, `fetchers.disable` for every enabled source once #277 adds it (skipped while the op
     isn't in the manifest), and `notify.pause {"all": true}`, held until `notify resume`. The `✓` line
     says what was switched off and how to turn it back on. `--live` skips this step.

  The next command in VAULT starts its daemon again, as auto-start always does.
- **`shimmer startup on`** pins the vault in effect (`--vault`, or the one in use): one login service,
  as today. `startup status` names the vault.
- **Seeing where you are**: `vaults status`, and `shimmer doctor`'s daemon line names the vault.
  Ordinary commands print nothing extra, so scripts see no noise.

### 6. The same as every other feature

- **Help**: `shimmer vaults --help`, laid out like every other page; a line in `shimmer --help`, and
  `--vault NAME` with the global flags.
- **Tab completion**: from the help page, as for every feature (subcommands and flags); vault names for
  `VAULT`, `REMOVED` and `--vault`; and `--vault` on the line counts, as `--pack` and `--socket` do.
- **Colours**: the usual roles: names, table headings, `✓` lines, `!` warnings, hints; plain under
  `--no-color` and `--json`.
- **Output**: tables and `--json` through the CLI's usual renderers.
- **Reserved word**: `vaults` joins the command words no pack alias may take (ADR 0013).
- **Pack aliases**: none, like `packs`, `startup` and `doctor`. Letting packs alias the client's own
  commands is a separate change to ADR 0029.

## Proposed changes to CLAUDE.md

- §7, the paragraph on client settings: "…the CLI keeps its own (the active command pack, §15.1, **and
  named vaults, ADR 0041**) in `~/.config/shimmer/cli.toml`…".
- §15, after "Boring on purpose": "A plain word that says what a thing is (`vaults`, ADR 0041) is
  canonical; a themed word that doesn't (`mite`) is an alias."

## Not done here

- **Moving pieces between vaults.** Records already move with `records export` / `import`, an
  automation with `automations show` / `new --file`, and a template author keeps the template, not the
  test data. Merging two vaults' histories is a hard problem nobody has yet.
- **A login service per vault.** One, as today.
- **Pack aliases for `vaults`** (§6).

## Consequences

- Testing a template, a purge or a rule needs no environment variables and can't touch the real data.
- The vault is chosen once, before anything else runs, so no other code needs to know vaults exist.
- `purge` can delete a whole folder; it's limited to folders Shimmer made, and always asks.
- Depends on #293: without one socket per folder, two vaults would share a daemon.

## Tests (the implementation PR)

- Which vault wins: `--vault` > `SHIMMER_HOME` > `use` > `main`, as a plain function with no
  environment changed.
- `cli.toml` round trip; name rules; `new` refusing a non-empty folder that isn't a Shimmer folder;
  `purge` refusing `main`, the vault in use, an adopted one and the login one.
- End to end with the real binary: a record added with `--vault play` lands in play's folder, not
  main's; `vaults use play` makes plain commands go there; `SHIMMER_HOME` still wins over `use`, and
  `--vault` over `SHIMMER_HOME`; two vaults' daemons run at once; a copy is independent of its source,
  nothing starts in the source mid-copy, the copy's schedules and rules are off and its notify paused
  (and `--live` leaves them on); remove, `list --removed`, restore; purge asks, stops the daemon and
  deletes only a vault Shimmer made; Tab completion offers vault names; every command takes `--json`.
