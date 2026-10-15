# 0012. `workspace.toml`: the step list, its fields, and how it is checked

Status: accepted (M3, #41; implemented in #55) · Raised by Dev B (issue #31) · Signed off: Dev A
(defines the script ABI he consumes through `Launcher`, CLAUDE.md §10.1) · No `core` or `proto`
change

## Context

ADR 0010 §2a split a workspace's launch into numbered steps,
`data/workspaces/<id>/steps/<NN>-<name>.{sh,ps1}`, each run as its own `LaunchStep` with its own
`SpawnMode`, plus a single `cleanup.{sh,ps1}`. It left `workspace.toml`'s format to Dev B:
`core` only needs an index, a count and a name per step (`Step::Launch { index, count, name }`)
and a `SpawnMode`.

Users write this file by hand, for their own machines, and CLAUDE.md §10.1 treats the script ABI
as permanent: once people have workspaces, changing these rules breaks their setups. So the
format is decided here, before any code parses it. The only parser that exists today, on the
`workspaces-state-machine` branch, predates §2a (one `mode` for the whole workspace, no step
list) and is superseded by this ADR.

## Decision

### 1. A full example

```
$SHIMMER_HOME/data/workspaces/deep-work/
  workspace.toml
  steps/01-setup.sh
  steps/02-editor.sh
  steps/03-terminal.sh
  cleanup.sh
```

```toml
# data/workspaces/deep-work/workspace.toml

[workspace]
label = "Deep Work"
description = "Shimmer development on Hyprland workspace 3"

[[step]]
name = "setup"                    # steps/01-setup.sh
description = "Pull the latest code and start the database"
mode = "supervised"
timeout_s = 60

[[step]]
name = "editor"                   # steps/02-editor.sh
mode = "detached"

[[step]]
name = "terminal"                 # steps/03-terminal.sh
mode = "detached"

[cleanup]                         # required because cleanup.sh exists
timeout_s = 30

[env]
PROJECT_DIR = "/home/me/code/shimmer"
```

```sh
# steps/01-setup.sh: supervised, so a non-zero exit marks the workspace dirty
set -e
hyprctl dispatch workspace 3
git -C "$PROJECT_DIR" pull --ff-only
docker start shimmer-db
```

```sh
# steps/02-editor.sh: detached, outlives the daemon
code "$PROJECT_DIR"
```

```sh
# steps/03-terminal.sh: detached
exec kitty --directory "$PROJECT_DIR"
```

Any step can also call back into Shimmer, because `SHIMMER_SOCKET` is set: a setup step could
run `shimmer records complete leetcode/two-sum` (CLAUDE.md §10.1).

Activating `deep-work` runs `setup` and waits for it. Only if it succeeds does it start
`editor`, then `terminal`. If `setup` fails or times out, the launch stops, `editor` and
`terminal` never start, and the workspace goes `dirty` (CLAUDE.md §10.3).

### 2. Order: the `[[step]]` list decides, the files must match

Steps run in the order they appear as `[[step]]` tables. Step *N* (counting from 1) must be the
file `steps/<N as two digits>-<name>.sh` (or `.ps1` on Windows): the second step named `editor`
is `steps/02-editor.sh`. The list is the single place the order is written down; the file
numbers must agree with it, and any disagreement is an error (§6), never a guess.

Rejected: letting the file names decide the order, with `workspace.toml` only saying how to
run each name. A stray script dropped into `steps/` would silently become part of the launch,
and renaming a file would silently reorder it.

### 3. Fields

**`[workspace]`**

| Field | Required | Meaning |
|---|---|---|
| `label` | yes | Display name, shown in lists. |
| `description` | no | One line about the workspace. Display only. |

There is no `id` field. **The workspace's id is its folder name**, the same way a record's id
is its file name (ADR 0008), so the file and the folder can never disagree about it.

**`[[step]]`**

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Matches the script's file name: `steps/<NN>-<name>.sh`. |
| `mode` | yes | `"supervised"` (wait for it, check its exit code, kill it on timeout) or `"detached"` (start it and walk away; it outlives the daemon). CLAUDE.md §10.2. |
| `timeout_s` | supervised only | Seconds before the step's whole process group is killed and the step counts as failed. |
| `description` | no | One line about the step. Display only; shown next to the step in status and in `workspace_dirty` messages. |

**`mode` has no default**, for the same reason `catch_up` has none on scheduler triggers
(CLAUDE.md §6.3): the wrong choice causes real damage either way. A detached setup step means its
failure is never noticed; a supervised editor gets killed when its timeout fires.

**`[cleanup]`**

| Field | Required | Meaning |
|---|---|---|
| `timeout_s` | yes | Seconds before `cleanup.sh` is killed. Cleanup always runs supervised (CLAUDE.md §10.3), so the timeout is the only thing to configure. |

The `[cleanup]` table is **required when `cleanup.sh` (or `cleanup.ps1`) exists, and rejected
when it doesn't**: a `[cleanup]` table with no script is almost always a misspelled file name.

**`[env]`**

Optional. Extra environment variables passed to every step and to cleanup, on top of the ones
Shimmer injects (CLAUDE.md §10.1). Values are strings.

### 4. One step list for every platform

There is one list. The launcher picks `.sh` on Linux and macOS and `.ps1` on Windows (ADR 0010
§2, §7). A workspace only needs scripts for the platforms it is used on: a step whose script is
missing *for the current platform* is an error when the workspace is activated, naming the
missing file, not when the file is loaded.

### 5. How steps run

These follow from ADR 0010 and CLAUDE.md §10; they are stated here because they are what a
user writing a workspace needs to know.

- **One at a time, in order.** A step starts only after the previous one has finished
  (supervised) or been started (detached).
- **The first failure stops the launch.** A supervised step fails when it exits non-zero or
  hits its timeout; any step fails if its script can't be started at all. The remaining steps
  don't run, and the workspace goes `dirty` with that step as `failed_step` (e.g.
  `"1/3 setup"`).
- **A detached step succeeds as soon as it starts.** Shimmer does not know if the editor
  crashes a second later; `active` means "the launch succeeded", nothing more (CLAUDE.md §10.3).
  Put anything whose success matters in a supervised step before the detached ones.
- **Failed steps are never retried automatically** (CLAUDE.md §11.2). Re-running a
  half-finished setup script on top of itself can make things worse; the workspace waits in
  `dirty` for the user to run cleanup, reset, or force a relaunch.
- **Scripts run with `sh`, not `bash`** (`sh steps/01-setup.sh`, CLAUDE.md §10.1). On
  Debian and Ubuntu, `sh` is `dash`, which rejects bash-only syntax such as `[[ ]]` or
  arrays. Write POSIX `sh`, or make the first line of the script
  `[ -n "$BASH_VERSION" ] || exec bash "$0" "$@"` to re-run it under bash.

### 6. Checks

Every problem is `invalid_params`. The message names the workspace, the file, and the step
(by number and name) or field, e.g.

    deep-work/workspace.toml: step 2 ("editor"): timeout_s is only allowed when mode = "supervised"

**When they run.** When a workspace is listed or shown, so a broken workspace is visible as
invalid with its reason before anyone tries to start it; and again on activate, since files can
change in between. **One invalid workspace never affects another**: listing still returns
every other workspace, and only requests about the broken one fail. (`records` got this wrong
for collection files: issue #29. This ADR designs it out from the start.)

**The file**

- Must be valid TOML.
- **Unknown keys are rejected everywhere** (`deny_unknown_fields`, as for collections in ADR
  0008), so `tiemout_s = 30` is "unknown field `tiemout_s`", never a silently ignored setting.
- **Wrong types are rejected, never converted.** `timeout_s = "30"` is an error, not 30.
- `workspace.toml` itself must exist; a folder under `data/workspaces/` without one is reported
  as invalid, not skipped.

**`[workspace]`**

- `label` is required and must not be empty or only whitespace.
- The folder name (the id) must match `[a-z0-9][a-z0-9_-]*`, at most 64 characters: the same
  rule as step names. The charset is the one record ids use (ADR 0008), but the length cap is
  not; record ids allow 128. A folder named `Deep Work` is reported as invalid,
  not ignored.

**`[[step]]`**

- At least 1 step and at most 99 (file numbers are two digits).
- `name` is required, matches `[a-z0-9][a-z0-9_-]*`, at most 64 characters, and is unique
  within the file. This is the charset ADR 0010 §2a requires before a name is ever used in a
  path.
- `mode` is required and must be exactly `"supervised"` or `"detached"`, lowercase. The
  error lists both.
- `timeout_s` is required when `mode = "supervised"` and rejected when `mode = "detached"`. It
  is a whole number from 1 to 3600. The one-hour cap is there to catch typos (`300000`); a
  setup step that genuinely needs longer should be split, or do its slow work detached.

**`steps/` against the list**

- Only `.sh` and `.ps1` files count as scripts. Hidden files (`.DS_Store`, which macOS creates
  on its own) and anything else (a `README.md`, notes) are ignored and never cause errors.
- Every script must belong to a listed step, at exactly its position. Each kind of mismatch
  has its own message:
  - wrong padding: `2-editor.sh` → "use two digits: `02-editor.sh`"
  - wrong number: `05-editor.sh` when `editor` is step 2 → "`editor` is step 2: expected `02-editor.sh`"
  - not listed: `04-music.sh` → "`04-music.sh` is not listed in `workspace.toml`"
  - not in the form `<NN>-<name>`: `setup.sh` → "scripts in `steps/` must be named `<NN>-<name>.sh`"
- A listed step with no script for the current platform is an error on activate (§4), naming
  the file it expected.

**`[cleanup]`**

- Required if `cleanup.sh` or `cleanup.ps1` exists; rejected if neither does.
- `timeout_s` follows the same rules as a step's: required, whole number, 1 to 3600.

**`[env]`**

- Names must match `[A-Za-z_][A-Za-z0-9_]*`.
- Values must be strings. `PORT = 8080` is an error asking for `PORT = "8080"`.
- **No name may start with `SHIMMER_`**, not only the six variables injected today. The whole
  prefix is reserved, so a variable Shimmer adds later can never collide with one a user has
  already set. (ADR 0010 §2 already makes the launcher ignore such an override; rejecting it
  here tells the user, instead of their setting silently not working.)

### 7. No format version field

Collection files have none either. Every field added later will be optional (§8), so files
written today stay valid without a version number. If an incompatible change is ever needed,
a `format` field can be introduced then, with a file that lacks it meaning version 1.

### 8. Fields considered and left out

**The rule for this file:** adding an *optional* field later breaks nobody, because files
without it keep working. Removing a field later breaks everyone who used it. So the format
starts with only what is clearly needed now.

| Field | Would do | Decision |
|---|---|---|
| `description` | One line about a step or the workspace. | **Added** (§3). Display only and harmless, and it makes `dirty` messages readable. |
| `retry` | Re-run a failed step automatically. | **Never.** CLAUDE.md §11.2: "a workspace launch should not auto-retry at all, because re-running a half-finished setup script is actively harmful." A failed launch goes `dirty` and waits for the user. |
| `optional` / `continue_on_failure` | A step whose failure doesn't stop the launch (e.g. start music). | **Later, if needed.** It changes what `dirty` means, which deserves its own decision. Today: end that script with `exit 0`. |
| `enabled = false` | Switch a step off without deleting it. | **Later.** It complicates the files-must-match check (§6). Today: remove the `[[step]]` and move the script out of `steps/`. |
| `delay_s` | Wait after starting a step, e.g. for a window to open. | **Not needed.** End the script with `sleep 2`. |
| `cwd` | The folder a step runs in. | **Not needed.** The script can `cd`. |
| Per-step `[env]` | Variables for one step only. | **Later.** The workspace-wide `[env]` covers it for now. |
| `platforms` | Run a step only on some operating systems. | **Later**, see below. |

**What "later" means for `platforms`.** It is left out of the first version on purpose, not
ruled out. The case it serves: a Shimmer folder synced between a Mac and a Linux machine, with
one step that should only run on the Mac. Most people set up workspaces per machine, and the
case already has a workaround with no new rules, because every script receives
`SHIMMER_PLATFORM`:

```sh
[ "$SHIMMER_PLATFORM" = "macos" ] || exit 0
open -a Spotify
```

If that pattern turns up often, add an optional `platforms = ["macos"]` then; existing files
stay valid.

## Consequences

- `crates/modules/workspaces` (issue #32) parses this format as plain functions over the file's
  text and the list of files in `steps/` (§12 rule 10: no `Ctx`, testable without a runtime).
  It turns each `[[step]]` into a `Step::Launch { index, count, name }` and a `SpawnMode`;
  nothing in `core` or `proto` changes.
- `workspaces-state-machine`'s `manifest.rs` is superseded; whether its `state.rs` is reused
  is decided separately (issue #30).
- `workspaces.list` / `workspaces.status` need a way to show an invalid workspace and its
  reason. The exact response shape is #32's to define, in `docs/protocol.md`, with Dev C
  notified.
- CLAUDE.md §10.1 now shows this example and points here.

## Not done here

- Any code. The parser and its tests are #32.
- `platforms`, `optional`, `enabled`, per-step `[env]`: left out on purpose (§8).
- Windows: the format already covers `.ps1`, but the Windows launch backend is still only
  sketched (ADR 0010 §7).
