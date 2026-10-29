# 0041. Vaults: named Shimmer folders you can switch between

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (`crates/app/src/main.rs` re-runs itself
in the chosen vault, §3; a quiet start for the daemon, §5a; the CLAUDE.md §2, §7, §10.3 and §15
wording below) · No `core` or `proto` change · Depends on #293 (ADR 0039: one socket per Shimmer folder),
merged; the quiet start (§5a) builds on #308's scheduler recovery, so the implementation follows #322
(data safety) into master · Revised after Wasay's two reviews on #312, and checked against #308, #309,
#310, #320 and #323 once they merged

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

**Applied once, first thing, by re-running in the vault.** When a command needs a vault other than
the one its environment already names (`--vault` given, or a vault chosen with `use` and no
`SHIMMER_HOME` set), `crates/app/src/main.rs` resolves it and **re-runs the same binary** with
`SHIMMER_HOME` and `SHIMMER_SOCKET` (ADR 0039's `default_socket` for that folder) set through
`Command::env`, and `--vault` taken off the line: on Unix with `CommandExt::exec`, which replaces the
process and keeps its pid; on Windows by spawning it, waiting, and passing on its exit code. It
re-runs only when the vault's resolved folder (`proto::paths::resolve`) differs from the environment's
`SHIMMER_HOME`, so the re-run itself, which has `SHIMMER_HOME` set to that folder and no `--vault`,
never re-runs again: no marker variable, so nothing new reaches the daemon or workspace scripts (a
`SHIMMER_*` variable reaching scripts is a §10.1 ABI addition, and a stale marker in a terminal a
script opened would make `--vault` act on the wrong vault; Wasay on #312). Everything that already reads
`proto::paths::shimmer_home()` and `socket_path()` then just works, unchanged: auto-start, user packs
and their linked commands, `shimmer startup`, `doctor`, Tab completion, workspaces' step logs.

Why not set the environment in-process: `std::env::set_var` is `unsafe` from Rust edition 2024, the
workspace forbids `unsafe_code`, and `crates/daemon/src/secrets.rs` already declines to call it for
that reason (CLAUDE.md §12 rule 8, Wasay on #312). Passing a resolved `Paths { home, socket }` down
explicitly instead would work too, but touches every call site above for the same result. The cost of
re-running is one extra exec, only when a vault other than the environment's is in use. `shimmer
daemon` never re-runs: auto-start already pins its environment. Nor does `shimmer vaults …`: it only
reads and writes `cli.toml`, never a vault's folder, and `status` and `list` say which vault is in use
*and why* from the line, the environment and `cli.toml` as they were given. After a re-run only the
folder is known, not whether `--vault`, `use` or `SHIMMER_HOME` chose it.

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
  (**adopted**, `made = false`). Anything else is refused, and so is a `DIR` inside the main folder:
  its files would be synced with main's, and main's daemon would sit around another vault's. The folder
  is filled in by its daemon the first time a command runs there.
- **`use`**: refuses a vault whose folder is gone, saying where it was expected. When `SHIMMER_HOME` is
  set in its own environment (a shell rc, say), it warns that plain commands still go there, since
  `SHIMMER_HOME` comes before `use` (§3).
- **`rename`**: the name only. If it's the vault in use, `use` follows it.
- **`remove`**: refuses `main`, the vault in use (`use` another first), and the vault `shimmer startup`
  starts at login (`startup off` first). It stops that vault's daemon (`core.shutdown` on its socket),
  since nothing can name it any more, and keeps every file. It also refuses while a removed vault
  already has that name (`purge` or `restore` it first). That differs from ADR 0038's "the latest
  removal replaces an older one" on purpose: replacing it would forget a folder Shimmer made, which
  `purge` could then never find to delete.
- **`restore`**: `conflict` if a vault of that name exists again; `not_found` if nothing's removed.
- **`purge`**: confirmed with `{"vaults": N}`, the same shape as every purge (ADR 0038 §4). It deletes a
  folder only when it was made by Shimmer (`made = true`), it still looks like a Shimmer folder, and it
  isn't a symlink. Its daemon is stopped first, and the folder goes once the socket has. An adopted vault
  is only forgotten: Shimmer never deletes a folder it didn't make.
- **`copy`**:
  1. It stops VAULT's daemon, as `shimmer shutdown` does, and **holds VAULT's `.daemon.lock` itself**,
     so no daemon can start there mid-copy. There is still exactly one writer (CLAUDE.md §2): for those
     moments, none.
  2. It copies into a temporary folder beside the target (`<target>.copying`), everything except what's
     derived or tied to the machine: `index.sqlite*`, `cache/`, `logs/`, the socket and the lock.
     `.staging/` *is* copied, so the copy's own recovery finishes any save that was committed (ADR 0040),
     including one still waiting to be applied; `.staging/broken/` comes too, so a damaged save kept
     aside in VAULT is kept aside in the copy, and `doctor` reports it in both.
     With `--path DIR`, the temporary folder sits beside DIR. Then it releases VAULT's lock.
  3. **The copy is switched off while it's still the temporary folder** (§5a), so it never sends a
     duplicate reminder or email, and a vault that's registered is always a quiet one (Wasay on #312:
     a copy interrupted after registering but before switching off would otherwise start live).
     `<target>.copying` gets its own daemon (ADR 0039: its own socket, by its own path), started quiet,
     and the CLI switches things off **through their own ops**, in this order: `fetchers.disable` for
     every enabled source (#320; it removes the source's own next-run schedule, so the source and its
     schedule agree), `automations.disable` for every enabled rule, then `scheduler.disable` for every
     schedule still enabled, which includes the one-time schedules modules added for themselves
     (`ctx.schedules`: an automation's delayed action, ADR 0035 §5.6), `notify.pause {"all": true}`
     (held until `notify resume`), and `workspaces.reset` for any workspace whose copied state isn't
     `ready` (it can't have launched anything in the copy; see below). Then it stops that daemon.
     `--live` skips this step.
  4. Only then it renames `<target>.copying` to the target and registers NEW (`made = true`). An
     interrupted copy, at any step, leaves no registered vault, only a `.copying` folder, removed by the
     next `copy` to that name. The `✓` line says what was switched off and how to turn it back on.

  **Resetting a copied workspace.** `workspaces.reset` is `dirty → ready` today; `stop` runs
  `cleanup.sh`, which in the copy would close the *source* vault's real windows and processes. So
  `reset` grows to `active` too: it forgets what the workspace thinks is running and runs nothing,
  asking first as it already does. Not to `launching`: the workspaces module already turns a launch
  a stop cut off into `dirty` when it starts, so a copied `launching` workspace is `dirty` by the time
  the CLI looks, and one that's `launching` while the daemon runs is a launch running now, which a
  reset would pull the state out from under (found building part 2; `conflict`). That's a §10.3
  state-machine amendment (below), owned by workspaces, so the daemon never edits a module's files.

  The next command in VAULT starts its daemon again, as auto-start always does.
- **Packs** stay global (the active pack is in `cli.toml`), and a user pack is read from the vault in
  use and then from `main`'s `packs/`, so switching to a vault without that pack folder doesn't turn
  every command into a broken-pack warning.
- **`shimmer startup on`** pins the vault in effect (`--vault`, or the one in use): one login service,
  as today. `startup status` names the vault.
- **Seeing where you are**: `vaults status`, and `shimmer doctor`'s daemon line names the vault.
  Ordinary commands print nothing extra, so scripts see no noise.

### 5a. A quiet start

Starting the copy's daemon, then switching things off, isn't enough on its own (Wasay on #312): a
daemon does work while it starts, before any client can reach it. The scheduler's recovery (ADR 0004
amendment, #308) queues again every firing copied in as `unfinished`, its first tick catches up what
came due while the source was stopped, and automations and notify react from `init`. So the daemon
gets a quiet start: `shimmer daemon --quiet`, which `vaults copy` asks auto-start for once, for the
copy's first start. During a quiet start:

- **The scheduler** forgets every `unfinished` firing instead of settling it (they were the source
  vault's run, not this one's), through `forget()`, so it's **written to the trigger files**:
  otherwise the next ordinary start's recovery would queue them again, since recovery doesn't check
  whether a trigger is enabled (Wasay on #312). It counts every schedule from now instead of catching
  up (as `enable` does), also written, and doesn't tick.
- **The queue refuses** a task a module asks for (`Origin::Module`: an automation's action, a
  fallback) with `unavailable` ("the daemon started quiet"), so it's never enqueued: no
  `queue.task.enqueued` is logged, `queue list` never shows it, and nothing is left for a later
  recovery to mistake for an unfinished firing. Tasks a person asks for run as usual, which is how the
  CLI switches things off.

**Copied events don't need anything.** The copy's event log is VAULT's history, but a module only
reacts to events emitted after its daemon started: catching up after a lag reads the log from that
start onward, never before it (#310), and every module has finished `init` before any reacts (#323).
So a rule never fires again on an event copied in; what's left to stop is what the daemon does by
itself, above.

The **mode** isn't written down, so it lasts for that one run; **what it settles is** (the forgotten
firings, the recounted schedules, and whatever the CLI switches off). Once the CLI has switched
everything off and stopped it, every later start is ordinary, with nothing to run.

**How it's asked for.** `shimmer daemon --quiet` for a daemon started by hand, and an argument to
auto-start's spawn (`start_and_connect(…, quiet: true)`) that only `vaults copy` passes, so `doctor`,
`startup` and ordinary auto-start can't start a quiet daemon by accident. It's a small daemon change,
in the scheduler and the queue, and needs Dev A's sign-off.

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

- §10.3, the `reset` bullet: "`workspaces.reset` also takes an `active` workspace: it forgets what
  the launch started and runs nothing, for a vault copy whose `active` was another folder's launch
  (ADR 0041). Never a `launching` one: that launch is running now."

- §2, after "The daemon is the only process that opens the data directory for writing": "One
  exception: `shimmer vaults copy` and `purge` copy and delete a whole vault's folder while holding its
  `.daemon.lock`, so there is still at most one writer (ADR 0041)."

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
- The vault is chosen once, by re-running in it before anything else runs, so no other code needs to
  know vaults exist, and nothing sets the environment in-process.
- `purge` can delete a whole folder; it's limited to folders Shimmer made, and always asks.
- The daemon gains a quiet start, used only for a copy's first run; nothing else changes in it.
- Depends on #293 (merged): without one socket per folder, two vaults would share a daemon.

## Tests (the implementation PR)

- A copy of a vault with a due `run_once` schedule, an `unfinished` firing and a rule that sends a
  message: nothing runs in the copy, during its quiet start or at its next ordinary start (Wasay on
  #312). `--live` leaves them on.
- A copy interrupted before it's switched off (between steps 2 and 4) leaves no registered vault,
  and the next `copy` to that name removes the leftover `.copying` folder.
- A copied `active` workspace is `ready` in the copy and its `cleanup.sh` never ran; a copied
  `launching` one is `dirty` at the copy's start, then `ready`; `reset` on a launch running now is
  `conflict`.
- `--vault` re-runs once (the resolved-folder check stops a loop), keeps the exit code, and takes
  `--vault` off the line; with no vault to switch to, nothing re-runs; no `SHIMMER_*` variable is
  added to the daemon's or a script's environment.
- `vaults use` warns when `SHIMMER_HOME` is set; the active pack found from `main` in another vault.
- `vaults status` after `--vault`, `use` and `SHIMMER_HOME` each names the right reason; `new --path`
  inside main is refused; `remove` refuses while a removed vault has that name.
- A copy has a fetchers source and an automation's delayed action switched off; a rule matching an
  event in the copied log doesn't fire at the copy's first start; a save waiting in VAULT's
  `.staging/` is finished in the copy, and one in `.staging/broken/` stays there.

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
