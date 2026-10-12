# 0013. Command packs: opt-in aliases, the built-in packs, and how a pack is chosen and checked

Status: accepted (M5, #70, #75), amended by ADR 0029 (every command can be aliased) · Raised by
Dev B (issue #35) · Signed off: Dev A (changes CLAUDE.md
§15 and §15.1, reads the "one writer" rule for client settings, and needs a small change in
`crates/app`, §10 below) · No `core` or `proto` change · Dev C: FYI (§3, a client setting the TUI
may also want to read)

## Context

CLAUDE.md §15.1 describes command packs: themed aliases such as `bankai` for
`workspaces activate`, which live only in the client and never reach the daemon. Nothing of it is
built yet, and building it raised questions §15.1 does not answer, plus three places where it
can't work as written:

- **Every fresh install would be themed.** §15.1 embeds a default pack (`arise`, `bankai`, …)
  and turns it on. Not everyone wants anime names in their terminal, and the default pack's
  names come from other people's franchises (below).
- **The active pack can't be changed from the CLI.** §15.1 stores it in `config.toml`, inside
  `$SHIMMER_HOME`. Only the daemon may write there (§1.6, §2), and the daemon must never know
  pack names (§12 rule 16), so neither the CLI nor a daemon op can save a choice there. People
  would have to edit a file by hand to switch packs.
- **Aliases only work after `shimmer`.** `shimmer bankai deep-work` is not what anyone wants to
  type. They want `bankai deep-work`.

Three smaller gaps: §15's table maps `arise` to `daemon start`, a command that doesn't exist;
the example pack maps `kido` to `scheduler.add`, which has no CLI command, so the pack would fail
its own load check; and the example packs (Bleach, Solo Leveling) are other companies' names,
which a project shouldn't build into its binary.

## Decision

### 1. A pack, in full

A pack is a folder holding one `pack.toml`, plus optional animation files:

```
$SHIMMER_HOME/packs/my-pack/
  pack.toml
  anim/activate.txt        optional
```

```toml
# $SHIMMER_HOME/packs/my-pack/pack.toml

[pack]
id = "my-pack"             # must match the folder name
label = "My Pack"          # shown in `shimmer packs list`

[alias]                    # alias = the canonical op it runs
"go-time"  = "workspaces.activate"
"finished" = "records.complete"

[anim]                     # optional; checked now, played in a later PR (§12)
"workspaces.activate" = "anim/activate.txt"
```

Packs are data, never code: TOML plus text files, no scripts (§12 rule 8). A pack can only give
a new name to a command that exists; it can never add one.

### 2. No aliases by default

A fresh install has **no active pack**: every command is the plain `shimmer …` it is today, and
`shimmer --help` is unchanged. Packs are opt-in. Canonical names always work, whichever pack is
active.

### 3. Choosing a pack: `cli.toml`, `--pack`, and `shimmer packs use`

The active pack is a **client setting**, like a terminal theme, so it lives in the client's own
file, outside `$SHIMMER_HOME`:

| Platform | File |
|---|---|
| Linux, macOS | `$XDG_CONFIG_HOME/shimmer/cli.toml`, else `~/.config/shimmer/cli.toml` |
| Windows | Not decided yet (see "Not done here") |

```toml
# ~/.config/shimmer/cli.toml (written by `shimmer packs use`, `link` and `unlink`)
pack = "anime-tropes"                  # the active pack; absent = none
link_dir = "/home/me/.local/bin"       # optional; where bare commands go (§10)
linked = ["ohayo", "owari", "ikuzo"]   # the links Shimmer made; the only ones it ever removes
```

- **`shimmer packs use <name>`** checks the pack (§7), refuses with every problem listed if it
  fails, and otherwise saves it as active **and links its aliases as bare commands** (§10), so
  choosing a pack is the only step: after `packs use anime-tropes`, `ikuzo deep-work` works.
  The previous pack's links are removed in the same step.
- **`shimmer packs use <name> --no-link`** switches without adding bare commands (aliases then
  work as `shimmer <alias>`), and removes any links from the previous pack so none go stale.
- **`shimmer packs use none`** turns packs off and removes every link Shimmer made.
- If linking fails (the folder isn't writable, say), the switch still happens: `packs use` says
  which links couldn't be made and why. A pack switch never fails because of a link.
- **`shimmer --pack <name> …`** uses a pack for one command without saving anything.
  `--pack none` forces plain names.
- Order: `--pack`, then `cli.toml`, then none.
- The CLI writes `cli.toml` the same way the daemon writes its files (§7.1): to a temp file in
  the same folder, `fsync`, then `rename`. A crash leaves the old file or the new one, never half
  of one.
- `cli.toml` is also fine to edit by hand. An unknown key gives a warning rather than an error,
  so a file written by a newer Shimmer never breaks an older one.

**Why not `config.toml`.** Everything in `$SHIMMER_HOME` belongs to the daemon (§1.6 "one
writer"), and the daemon must never see a pack name (§12 rule 16), so no one could write the
setting there except the user by hand. `cli.toml` is outside `$SHIMMER_HOME`, holds no platform
state, and nothing but the CLI reads it. This is the same reading of "one writer" that already
lets workspace launches spawn processes (§2): the rule protects `$SHIMMER_HOME`, and this file
isn't in it.

The cost: copying `$SHIMMER_HOME` to another machine doesn't bring your pack choice. It's a
per-machine preference, so that's acceptable; `shimmer packs use <name>` there is one command.

### 4. Where packs come from

| Source | Where | Changed by |
|---|---|---|
| **Built-in** | Compiled into the binary (§5) | A Shimmer release |
| **Yours** | `$SHIMMER_HOME/packs/<id>/` | You, by adding a folder |

To find pack `<name>`: the folder `$SHIMMER_HOME/packs/<name>/` if it exists, otherwise the
built-in of that name. A folder with a built-in's name **replaces** that built-in, and
`shimmer packs list` says so. The CLI only ever reads `packs/` (§15.1); it never writes there.

`none` is reserved and can't be a pack id.

### 5. The built-in packs

Four, all **unbranded**: no names from games, shows, films, teams or companies. A built-in pack
ships in the binary of a public project, so it must never use someone else's trademark. Packs
for a franchise are welcome as user packs or in a separate community collection; they don't go
in the binary.

| Command | `anime-tropes` | `ship-it` | `starship` | `short` |
|---|---|---|---|---|
| ping | `ohayo` | `on-call` | `comms` | `up` |
| shutdown | `owari` | `ooo` | `cryo` | `down` |
| manifest | `nakama` | `readme` | `blueprints` | `ops` |
| records collections | `iroiro` | `boards` | `fleets` | `rcol` |
| records add | `kohai` | `ticket` | `waypoint` | `radd` |
| records list | `mite` | `triage` | `logbook` | `rlist` |
| records get | `naruhodo` | `blame` | `scan` | `rget` |
| records update | `senpai` | `amend` | `retrofit` | `rset` |
| records complete | `yatta` | `lgtm` | `landed` | `rdone` |
| records remove | `sayonara` | `wontfix` | `airlock` | `rrm` |
| workspaces list | `minna` | `envs` | `sectors` | `wlist` |
| workspaces status | `nani` | `standup` | `diagnostics` | `wst` |
| workspaces activate | `ikuzo` | `deploy` | `engage` | `wgo` |
| workspaces cleanup | `daijoubu` | `postmortem` | `damage-control` | `wclean` |
| workspaces force-relaunch | `yatte-yaru` | `force-push` | `override` | `wforce` |
| workspaces reset | `tadaima` | `rollback` | `cold-start` | `wreset` |

Each built-in passes every check in §7, and none of their aliases is a common developer tool
(§10). A test enforces both, so a later built-in can't slip through.

### 6. What an alias can point at

An alias's value is a canonical op. The CLI keeps its own table from op to command words, and
an alias can only point at an op in that table:

| Op | Runs |
|---|---|
| `core.ping` | `shimmer ping` (starts the daemon if needed) |
| `core.shutdown` | `shimmer shutdown` |
| `core.manifest` | `shimmer manifest` |
| `records.collections`, `.add`, `.list`, `.get`, `.update`, `.complete`, `.remove` | `shimmer records <verb>` |
| `workspaces.list`, `.status`, `.activate`, `.cleanup`, `.force_relaunch`, `.reset` | `shimmer workspaces <verb>` (`force_relaunch` → `force-relaunch`) |

`daemon`, `mockd`, `call` and `packs` can never be aliased: they're plumbing, and a broken pack
must never be able to get in the way of fixing it. Several aliases may point at the same op.
An alias can't point at another alias, so aliases can't loop.

When `shimmer queue …` and `shimmer scheduler …` exist, their ops join this table and packs can
alias them; nothing else changes.

### 7. Checks

Every pack, built-in or yours, is checked when it's loaded, by `shimmer packs check`, and by
`shimmer packs use`. A check reports **every** problem in a pack, not just the first, each with
the file it's in.

**`pack.toml`:**

- Valid TOML, at most 64 KiB. Unknown sections or keys are errors (a typo must not be silently
  ignored, as with `workspace.toml`, ADR 0012).
- `[pack] id` is required, matches the folder name (for a folder pack), uses the name charset
  (`shimmer_core::ids::is_valid_name`: `[a-z][a-z0-9_-]*`, so it starts with a letter), is at
  most 32 characters, and is not `none`.
- `[pack] label` is required: one line, 1–40 characters, no control characters.
- `[alias]` has at least one entry and at most 64.

**Each alias:**

- Uses the same name charset and is at most 32 characters.
- Is not one of Shimmer's own command words: `ping`, `manifest`, `shutdown`, `call`, `daemon`,
  `mockd`, `records`, `workspaces`, `packs`, `help`. Also not one of the words held for commands
  we expect (`queue`, `scheduler`, `fetchers`, `notify`, `calendar`, `tui`, `version`), nor
  `shimmer` itself. A pack that's valid today can't break when those commands arrive.
- Points at an op in the §6 table.
- Appears once (TOML already rejects a repeated key).

**`[anim]`**, for each entry:

- The key is an op in the §6 table.
- The value is a relative path inside the pack's folder: no `..`, no absolute path, and no
  symlink that leads outside the folder (the path is resolved before it's checked).
- The file exists, is a regular file, is valid UTF-8, and is at most 256 KiB.

**`cli.toml`:** at most 16 KiB, valid TOML; `pack` and `link_dir` are strings, `linked` is a list
of strings that each pass the alias charset.

Whether an alias is also a program on your machine is **not** a pack error. `shimmer <alias>`
always works; only bare commands could clash, and `packs link` handles that (§10).

### 8. When something is wrong

A broken pack must never break a command. If `cli.toml` or the active pack fails to load or fails
a check, the CLI prints **one** warning line to stderr and runs the command with plain names:

```
shimmer: pack 'my-pack' not loaded (2 problems; see 'shimmer packs check my-pack'); using no aliases
```

Canonical commands then work exactly as before. `--json` output is unaffected: warnings only ever
go to stderr. The one case that must fail is running an alias directly (§10) when its pack can't
load, since there's nothing to fall back to; that exits 2 with the same message.

### 9. How an alias runs

Before the CLI parses anything, it looks at the first command word, skipping global flags
(`--socket`, `--json`, `--pack`, `-h`). If that word is an alias in the active pack, it's
replaced by the command words of its op, and the rest of the line stays exactly as typed:

```
shimmer ikuzo deep-work --wait   →   shimmer workspaces activate deep-work --wait
```

From then on it's the normal CLI. The daemon only ever sees `workspaces.activate`, and every
error and help text uses canonical names (§12 rule 16). Only the first word is ever rewritten,
so an alias can never change an argument.

### 10. Bare commands: linked by `shimmer packs use`

An alias replaces `shimmer` **and** the whole command: `ikuzo deep-work` runs
`shimmer workspaces activate deep-work`. To make that work, `shimmer packs use` (§3) puts a
symlink in the link folder for each alias in the pack, pointing at the `shimmer` program.
`shimmer packs link` does the same on its own (to repair or refresh links), and
`shimmer packs unlink` removes them all. When the
program starts under one of those names, it runs that alias, as if it were
`shimmer <alias> …`. This is how `git-*` helpers and BusyBox work, and it works in every shell
and in scripts, with no shell setup.

- **Link folder:** `link_dir` in `cli.toml`, else `~/.local/bin`. It's created if missing, and
  `packs use` / `packs link` warn if it isn't on your `PATH`.
- **Never overwrites anything.** An alias is skipped, and named in the output, if its name already
  exists in the link folder (other than a link `packs link` made), if it's a program anywhere
  else on your `PATH`, or if it's on the CLI's list of common developer tools (`git`, `go`, `rg`,
  `fd`, `bat`, `gh`, `jq`, `rls`, `node`, `npm`, `cargo`, …), which catches tools you might
  install later.
- **Only removes what it made.** The names it links are saved in `cli.toml` `linked`.
  `packs use`, `link` and `unlink` only remove a name that is in that list *and* is still a
  symlink; a real file is never deleted.
- **Switching packs** with `packs use` swaps the links in one step: the old pack's go, the new
  pack's arrive.
- **A stale link** (an alias from a pack that's no longer active) exits 2 with:
  `'ikuzo' isn't an alias in your active pack (starship). Run 'shimmer packs link'.`
- **`crates/app` change (Dev A).** `main` currently picks `daemon` / `mockd` / CLI from the first
  argument. It will first ask the CLI whether the program's own name (`argv[0]`, minus any
  extension) is an alias in any known pack. If it is, everything goes to the CLI as that alias;
  otherwise behaviour is exactly as today. So `ikuzo daemon` activates a workspace named
  `daemon` rather than starting the daemon, and a renamed binary (`shimmer-dev`) still works.

### 11. The commands

| Command | What it does |
|---|---|
| `shimmer packs list` | Every pack: id, label, alias count, built-in or folder (and "replaces built-in"), which is active. A broken folder pack is listed as invalid with its first problem, never hidden. |
| `shimmer packs show <name>` | A pack's aliases and the command each runs |
| `shimmer packs use <name>` | Check and switch the active pack, and link its aliases as bare commands (§3, §10) |
| `shimmer packs use <name> --no-link` | Switch without bare commands (`shimmer <alias>` only) |
| `shimmer packs use none` | Turn packs off and remove every link Shimmer made |
| `shimmer packs link` / `unlink` | Refresh or remove the bare commands for the active pack (§10) |
| `shimmer packs check <folder>` | Run every §7 check on a folder and list all problems; exit 1 if any |
| `shimmer --pack <name> …` | Use a pack for one command (§3) |
| `shimmer --help` | The normal help, plus an "aliases (<pack>)" section when a pack is active |
| `shimmer --help --canonical` | The normal help only |

### 12. Animations

The `[anim]` format and its checks are decided now (§1, §7), so pack files written today won't
need to change. Playing animations, and §15.1's rules for them (never delay the request, off when
stdout isn't a terminal, `--no-anim`), arrive in a later PR. The built-in packs ship without
animations.

## What changes in CLAUDE.md

- §15: commands have a canonical name and, when a pack is active, an alias.
- §15.1: the example pack and the rules are rewritten to match this ADR: no default pack, the
  four unbranded built-ins, the active pack in `cli.toml` set by `shimmer packs use`, which also
  links bare commands, and the checks and fallback. The `arise` / `bankai` table is
  replaced by the built-in packs.
- §7: one line saying client settings live outside `$SHIMMER_HOME`, in `cli.toml`.

`docs/protocol.md` already says aliases never reach the wire, which stays true; it isn't changed.

## Alternatives considered

- **Keep a themed default pack** (§15.1 as written). Rejected: not everyone wants it, and the
  names it used belong to other companies.
- **Store the choice in `config.toml` and have the daemon write it.** Rejected: the daemon would
  have to handle pack names, which §12 rule 16 forbids, and it would need a new op (a `proto`
  change, §4) for a purely client-side preference.
- **Let the CLI write `config.toml`.** Rejected: it breaks "one writer" (§1.6) inside the
  daemon's own folder.
- **Shell aliases** (`eval "$(shimmer packs shell)"` in `.bashrc`). Rejected: different for every
  shell, needs a manual edit, and doesn't work in scripts. Symlinks have none of these problems.
- **Copy the binary under each alias name.** Rejected: dozens of copies of a large binary, all
  needing updates on every upgrade.
- **Built-in franchise packs.** Rejected for the binary (§5). They can live as user packs or in a
  community collection.

## Not done here

- Playing animations and `--no-anim` (§12). That PR closes #35.
- `shimmer queue …` and `shimmer scheduler …` commands, so packs can alias them (§6).
- Bare commands on Windows (`.cmd` shims instead of symlinks), and `cli.toml`'s Windows location.
- A community collection for franchise packs, and where it lives.
