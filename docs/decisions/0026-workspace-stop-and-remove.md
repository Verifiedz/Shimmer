# 0026. Workspaces: stopping one, and removing one with an undo

Status: accepted (M3, #96) · Raised by Dev B (while testing ADR 0025's `web-project`) · Signed off:
Dev A (a new transition in CLAUDE.md §10.3's state machine; three new ops in `workspaces`,
docs/protocol.md) · Dev C notified · No `core` or `proto` change

## Context

Testing `web-project` on a real project showed two gaps:

- **Nothing stops a workspace.** Activating one starts a dev server that keeps running after
  the launch (a detached step, CLAUDE.md §10.2). Today the only way to stop it is by hand
  (`pkill -f "next dev"`), because a workspace's cleanup script only runs to recover a `dirty`
  one (CLAUDE.md §10.3). A port left taken is exactly what makes the next launch fail.
- **Nothing removes a workspace.** It is a folder, so `rm -r` works, but that leaves its dev
  server running and can't be undone.

What a workspace starts falls into two groups, and only one can be stopped safely:

- **Processes only the workspace uses**: a dev server, services it started (`docker compose
  up -d`). A detached step is its own process group (ADR 0010 §3), so a script that saved its id
  can stop it and everything it started.
- **Windows of apps people use for other things**: the editor, browser tabs, a terminal. When
  such an app is already running, opening a folder or a link hands it to the existing process,
  which the workspace didn't start and doesn't own. Stopping it would close everything else the
  person had open in it, unsaved work included, and no app offers a reliable way to close one
  particular window from a script on both Linux and macOS.

## Decision

### 1. `stop`: run the cleanup script on an active workspace

| Op | Execution | Params | Result |
|---|---|---|---|
| `workspaces.stop` | queued (`workspaces`) | `{"id"}` | The task ends `{"id","state":"ready"}` |

- **Only `active` can be stopped.** `ready` is `invalid_params` ("isn't running"); `dirty` is
  `invalid_params` pointing at `workspaces.cleanup`; `launching` is `conflict`.
- **It runs the workspace's cleanup script**, supervised, with the same variables and its own
  session id, exactly as `workspaces.cleanup` does (CLAUDE.md §10.3). So one script answers
  "undo what this workspace started", whether the launch failed or succeeded.
- **Success: `active → ready`**, and `workspaces.session.stopped` with `{workspace, log}`.
- **No cleanup script: `active → ready` at once**, nothing run, and the event says
  `"ran_cleanup": false`. Shimmer keeps no list of processes (§10.3: no liveness claims), so
  without a script there is nothing it could stop.
- **Failure leaves it `active`** and fails the task with the log, as a failed cleanup leaves a
  workspace `dirty`. A stop that half-worked is not a half-configured launch, so it doesn't
  block activating again; the next launch's check step will notice a port still taken.
- **Windows: only those the workspace owns are closed, and stop says which** (amended below).

### 1a. Amendment: closing the windows a workspace owns, and saying what stays

Testing showed "stop processes, never windows" left too much behind. Windows split by who owns
them, and that decides what's closable:

| Opened by the workspace | Closed by `stop`? | Why |
|---|---|---|
| Dev server, services, the VM | ✅ always (as before) | started by the workspace as its own processes |
| A kitty / foot / alacritty / wezterm / ghostty / konsole / xterm terminal | ✅ with `CLOSE_ON_STOP = yes` (the default) | each window is its own program, started in the step's process group; started with each terminal's "new process" flag (`--always-new-process`, `--gtk-single-instance=false`, `--nofork`) |
| Links in a browser window of the workspace's own (`BROWSER_WINDOW = separate`) | ✅ with `CLOSE_ON_STOP = yes` | a separate copy of the browser with a per-workspace profile (`~/.local/state/shimmer/browser-profiles/<id>`, kept so logins stick); a second launch hands its tabs to that window |
| The editor (Cursor, VS Code, …) | ❌ never | one program owns all its windows, and they may hold unsaved work |
| GNOME Terminal, macOS Terminal / iTerm windows | ❌ never | one program owns all their windows |
| Tabs in your normal browser (`BROWSER_WINDOW = shared`, the default) | ❌ never | they share the browser with your other tabs |

- Each opening step records what it opened in the workspace's temp folder: a closable window as
  its process group, anything else with the reason it can't be closed.
- The cleanup script closes the closable ones (`CLOSE_ON_STOP = no` keeps them) and prints one
  line per item: `closed: …`, `already closed: …`, `left open, close it yourself: …`.
- `workspaces.stop` and `workspaces.cleanup` now end `{"id","state":"ready","log"}`, so the CLI
  prints that list after `--wait` instead of a bare "stopped".
- Process-group checks use `kill -s 0 -- -PGID`: Debian's `sh` (dash) reads `kill -0 -- -PGID`
  as a process named `--` and fails, which first made every window look "already closed".

CLAUDE.md §10.3 gains one transition, `active ──stop (cleanup script)──> ready`, and its
"Recovery is by cleanup script" bullet says the script is also what `stop` runs.

### 2. `remove` and `restore`: a removed workspace can be brought back

| Op | Execution | Params | Result |
|---|---|---|---|
| `workspaces.remove` | inline | `{"id"}` | `{"removed":true}` |
| `workspaces.restore` | inline | `{"id"}` | the workspace, as `workspaces.status` shows it |

- **Only a workspace that isn't running can be removed**: `ready`, or one whose files can't be
  read (removing a broken workspace is a reasonable thing to want). `active` and `launching` are
  `conflict` ("stop it first"), and `dirty` is `conflict` ("clean it up or reset it first"),
  so removing never leaves a dev server running with nothing left to stop it.
- **Removing moves the folder** to `data/workspaces/.removed/<id>/` in one transaction and
  emits `workspaces.workspace.removed` with `{workspace}`. The latest removal of an id replaces
  an older one, as the records trash does (ADR 0020). Folders whose name starts with `.` are
  not workspaces: listing skips them.
- **Restoring moves it back**, `conflict` if a workspace with that id exists again, `not_found`
  if there's nothing to restore. It comes back exactly as removed, `state.toml` included, and
  emits `workspaces.workspace.restored`.
- There is no "delete for good" op yet. The removed folder is small, and deleting
  `data/workspaces/.removed/` by hand is safe.

### 2a. `rename`: a new id for a workspace that isn't running

`shimmer workspaces rename NAME NEW [--move-schedules]` (`workspaces.rename {id, to}`) moves every
file of `data/workspaces/<id>/` to `data/workspaces/<to>/` in one transaction (state,
`.template` and all) and emits `workspaces.workspace.renamed {from, to}`.

- **Only while `ready`**, as for `reconfigure` (ADR 0025 §9): a running or dirty workspace's stop
  and cleanup find what its launch started by its id (its temp folder, its recorded windows), so
  renaming it then would leave those behind. `to` must be a valid id, different, and free.
  Unlike `remove`, an **unreadable state file refuses** the rename (fail closed): it may hide a
  running or dirty workspace, and a rename, unlike a removal, isn't undone by `restore`.
- **What the daemon can't move, it names.** A template may keep things outside the Shimmer
  folder under the id: the result's `notes` say so with the command that moves them (today: a
  separate browser window's profile, where the person logged in to sites).
- **Schedules are the person's call.** Triggers that run a `workspaces.*` op on the old id would
  fail from now on. Neither the module nor the scheduler can know that (no module calls another,
  and the scheduler never knows a module's names, CLAUDE.md §12 rule 7), so the CLI lists the
  ones you added, unfinished, that name the old id, and moves them only on a yes (or
  `--move-schedules`): each added again with the new id (paused if it was), then the old one
  removed; one that can't be added stays as it was. Without a terminal and without the flag it
  only lists them, with how to undo the rename.
- The event log and old step logs keep the old id: they're history.

### 2b. `copy`: a new workspace like an existing one

`shimmer workspaces copy NAME NEW [--set QUESTION=ANSWER]… [--label TEXT]` (`workspaces.copy
{id, to, values?, label?}`) makes `<to>` from every file of `<id>` (scripts with their hand edits,
`.template`) **except its state**: the copy starts `ready`, with no last session. In one
transaction; emits `workspaces.workspace.copied`.

- **The original can be in any state**, even running: it is only read.
- **Different answers in the same step**: `values` are checked exactly as `reconfigure` checks
  them (ADR 0025 §9, shared code), so a second site is `copy site blog --set PROJECT_DIR=…`.
  At a terminal with no `--set`, the CLI asks whether to change any, then asks the questions with
  the original's answers as defaults. A workspace made by hand copies as it is, with no questions.
- **Sharing is said out loud.** With no answer changed, the CLI warns that running both at once
  means the same folders and ports, and how to change them. Everything a template keeps per
  workspace (its temp folder, recorded windows, `vm.started`, a browser profile) is keyed by the
  id, so the two never stop each other's processes; the end-to-end tests run a copy beside its
  original and stop each alone.
- Not copied: the state, schedules (they keep naming the original), and anything outside the
  Shimmer folder (a separate browser window's profile: the copy gets its own).

### 3. `web-project`'s cleanup stops services too

The template (ADR 0025 §6) gains one optional question, `SERVICES_STOP_COMMAND` (e.g.
`docker compose down`), which its cleanup runs after stopping the dev server. So `stop` undoes
what `SERVICES_COMMAND` started. Empty by default, like `SERVICES_COMMAND`.

### 4. CLI

```
$ shimmer workspaces stop my-site --wait
✓ my-site is stopped (ready)

$ shimmer workspaces remove my-site
removed my-site (undo: shimmer workspaces restore my-site)
```

- `stop NAME [--wait]`, queued like `activate` and `cleanup`.
- `remove NAME` and `restore NAME`. No question first: removing can be undone (as records'
  `remove`, ADR 0020).
- Removing an active workspace says how to stop it first, rather than stopping it silently.

## Consequences

- `crates/modules/workspaces`: the two transitions in `state.rs` (pure, tested without a
  runtime, §12 rule 10); `stop` sharing `cleanup`'s script run in `launch.rs`; `remove` and
  `restore`; listing skips dot-folders. Tests against the in-memory `Ctx` and `FakeLauncher`.
- CLAUDE.md §10.3: the new transition and the cleanup bullet (this needs Dev A's agreement,
  CLAUDE.md being the source of truth).
- `crates/cli`: `workspaces stop`, `remove`, `restore`.
- `docs/protocol.md`: the three ops and three topics; `crates/mockd/fixtures/core.json` lists
  them. Dev C told.

## Not done here

- Closing editor, browser or terminal windows (Context).
- Deleting a removed workspace for good, and listing removed ones.
- `stop` for every active workspace at once (e.g. on logout).
