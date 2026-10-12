# 0025. Workspace templates: ready-made workspaces you fill in with your own answers

Status: accepted (M3, #96) · Raised by Dev B (issue #68) · Signed off: Dev A (two new ops in
`workspaces`, docs/protocol.md; the scripts run through his `Launcher`) · Dev C notified · No `core`
or `proto` change

## Context

A workspace today means writing a `workspace.toml` and a numbered shell script per step by hand
(ADRs 0010, 0012). That is the right format, but an empty folder is a poor start: most people
want "open my project the way a web developer does", not a lesson in process groups. It also
makes testing workspaces on real machines (#34) slow, because every test starts with writing
scripts from scratch.

Record collections solved the same problem with templates (ADR 0022). This ADR does the same
for workspaces, following ADR 0022 §5's convention so people learn it once:
`shimmer workspaces templates` lists them, `shimmer workspaces new ID --from TEMPLATE`
creates one, built-ins are embedded in the binary, creating never overwrites, and the result
is ordinary data that forgets where it came from.

What's different from records: a workspace runs scripts on someone's machine, so a template
can't be "just a file to copy". It needs the person's own paths, editor and links, and its
scripts must work on Linux and macOS without being rewritten for each person.

## Decision

### 1. A template is a workspace folder plus a list of questions

```
crates/modules/workspaces/templates/web-project/
  template.toml          the questions (this ADR)
  workspace.toml         an ordinary workspace.toml (ADR 0012), with no [env]
  steps/01-check.sh      ordinary step scripts
  steps/…
  cleanup.sh
  lib/shimmer-open.sh    shared helper (§5), copied with the rest
```

```toml
# template.toml
[template]
label = "Web project"
description = "Editor, dev server, localhost and your links, opened in one go"
category = "code"
needs = ["Your editor (VS Code, Cursor, Zed, WebStorm, IntelliJ, Sublime or Neovim)", "…"]
good_to_know = ["Pulls the latest code each time it opens, only when … never merges", "…"]
on_stop = "Stops the dev server (freeing its port) and the services it started, …"

[[question]]
name = "PROJECT_DIR"
prompt = "Project folder"
kind = "folder"
required = true

[[question]]
name = "CODE_EDITOR"
prompt = "Editor"
kind = "choice"
choices = ["vscode", "cursor", "zed", "webstorm", "intellij", "sublime", "neovim", "none"]
default = "vscode"

[[question]]
name = "LINKS"
prompt = "Other links to open (live site, hosting dashboard, analytics…)"
kind = "urls"
help = "Separate them with spaces. Leave empty for none."
```

The template's id is its folder name, like a workspace's (ADR 0012 §3).

**Question fields**

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | The `[env]` variable the answer becomes (§2). |
| `prompt` | yes | What to ask, one line. |
| `kind` | yes | `text`, `folder`, `file`, `url`, `urls`, `choice` or `command` (§3). |
| `required` | no | Default `false`. A required answer can't be empty. |
| `default` | no | Used when the answer is left empty. |
| `choices` | `choice` only | The allowed answers. |
| `help` | no | One more line shown under the prompt. |

`template.toml` is checked like every other file Shimmer reads: unknown keys rejected, wrong
types rejected, every problem naming the file and the question.

**What someone must know before using one.** `good_to_know` (required, at least one line)
says what it changes, deletes, asks for and never does; `needs` what must be installed or set
up; `on_stop` what `workspaces stop` does. With the steps, read from the template's own
`workspace.toml` (name, `supervised` with its time limit or `detached`, description) so they can't
drift from what runs, they are what `shimmer workspaces peek TEMPLATE` shows before anything is
created.

**Consistency.** Questions several templates share are asked the same way: `CODE_EDITOR`,
`TERMINAL_APP` (`auto` first, `none` last when offered), `OPEN_ON` and `CLOSE_ON_STOP` have the
same prompt and help everywhere, and the window questions come last, after the ones about the
project. A `description` fits the list on one line (72 characters); the detail is in `peek`.
Every `workspace.toml` starts with the same commands (activate, stop, reconfigure), and every
hint a script prints is the exact `shimmer workspaces reconfigure ID --set …` that fixes it.

**Categories.** Every template names one `category`, so the list is grouped rather than one
long alphabet. The set is fixed in the module (`template::CATEGORIES`), in this order, and
`workspaces.templates` sends it with each heading, so a client groups them without knowing any
template:

| Id | Heading | Templates |
|---|---|---|
| `code` | Coding | `web-project`, `monorepo`, `scratch`, `vm` |
| `daily` | Every day | `github-inbox` |
| `upkeep` | Machine upkeep | `update-everything`, `free-disk`, `health` |
| `travel` | On the go | `offline-prep` |
| `testing` | Testing Shimmer | `smoke-test` |

An unknown category is an error naming the known ones, as with any other mistake in a template.
A new category is one line in `CATEGORIES` and this table; user templates in `$SHIMMER_HOME`
(not done here) may need their own, which would be decided then.

### 2. Answers go into `[env]`; scripts are never rewritten

Creating a workspace writes every answer into its `workspace.toml` `[env]`, in question order,
**including empty ones** (`LINKS = ""`), so the person can see and fill in every setting later
by editing one table. The scripts are copied byte for byte. There is no text substitution
inside scripts: they read `"$PROJECT_DIR"` like any hand-written workspace does (ADR 0012 §1).
That keeps them readable, safe to inspect, and identical for everyone who uses the template.

Question names follow `[env]`'s rules (ADR 0012 §6: `[A-Za-z_][A-Za-z0-9_]*`, never
`SHIMMER_…`), and also may not be a variable other programs rely on, since `[env]` is passed
to everything the scripts start: `PATH`, `HOME`, `USER`, `SHELL`, `PWD`, `TMPDIR`, `LANG`,
`TERM`, `TERMINAL`, `EDITOR`, `VISUAL` and `BROWSER` are rejected. That is why the editor
question is `CODE_EDITOR`, not `EDITOR`.

### 3. Kinds, and what is checked when

| Kind | Checked by the daemon on create | Notes |
|---|---|---|
| `text` | one line | |
| `command` | one line | Run with `sh -c` by the script. `auto` is allowed where the template says so (§6). |
| `folder`, `file` | an absolute path, one line | The CLI turns `~/code/site` and `./site` into absolute paths before sending (§8). |
| `url` | starts with `http://` or `https://`, no spaces | |
| `urls` | each space-separated item is a `url` | URLs can't contain spaces, so a plain `for u in $LINKS` works in `sh`. |
| `choice` | one of `choices` | |

**The daemon does not check that a folder or file exists, or that an app is installed.**
Modules can't read outside their own namespace (CLAUDE.md §5), and the answer may be right
for the machine that runs the workspace even if it's wrong where it was created (a synced
`$SHIMMER_HOME`). Every template's first step checks those things instead (§4), every time it
launches, which is also when they matter.

`auto` is an ordinary answer, not a feature of the format: a template's question may default
to `auto`, and its scripts decide what that means when they run (§6). Working things out at
launch rather than at creation means a workspace keeps working when the project changes, say
from `npm` to `pnpm`.

### 4. Every template starts with a check step

Step 1 of every built-in template is `check`, supervised, with a short timeout. It confirms
what the rest needs (the folder exists, the editor is installed, the port is free) and exits
non-zero with **one clear sentence** if not: `Cursor isn't installed (looked for the 'cursor'
command and Cursor.app)`. The workspace goes `dirty` with that step and its log (CLAUDE.md
§10.3) before anything opens, instead of failing halfway with a half-opened desktop.

Steps whose answer is empty **do nothing and exit 0**, so one template covers a static site, a
backend with no browser tabs and a full web app.

Steps never fail because the network is down. Shimmer is local-first (CLAUDE.md §1.5): a
`git pull` that can't reach the server says so in the step's log and lets the launch continue.

### 5. Linux and macOS differences live in one helper

Opening an app, a folder, a URL or a terminal differs between platforms (`xdg-open` vs `open`,
`code` vs `open -a "Visual Studio Code"`). A template doesn't repeat that in every step:
`lib/shimmer-open.sh` provides small functions, and steps load it with
`. "$SHIMMER_WORKSPACE_DIR/lib/shimmer-open.sh"`.

- `shimmer_open_url URL…`: the default browser.
- `shimmer_open_editor EDITOR PATH`: the editor on a folder or file. On macOS it uses
  `open -a`, which works even when the editor's shell command isn't installed.
- `shimmer_open_terminal TERMINAL DIR [COMMAND]`: a terminal window in `DIR`, optionally
  running `COMMAND` (e.g. `claude`).
- `shimmer_has_editor EDITOR`: for the check step.

**Where windows open (`OPEN_ON`).** A step inherits the daemon's environment, which comes from
whatever terminal last started the daemon. A Cursor or VS Code terminal connected from another
computer carries that editor's helpers (`BROWSER`, `VSCODE_IPC_HOOK_CLI`, its `remote-cli` on
`PATH`), so windows go to *that* computer; the machine's own desktop session keeps them local.
Found in practice: reinstalling Shimmer from a Cursor terminal silently moved every window from
the VM's screen to the host. So `web-project` and `vm` ask `OPEN_ON`, and the helper applies it
when a step loads it:

| `OPEN_ON` | Windows open | How |
|---|---|---|
| `auto` (default) | wherever the daemon was started | nothing changed |
| `this-machine` | this computer's own screen | drops the editors' remote helpers from the environment and `PATH`, and finds the desktop session (`$XDG_RUNTIME_DIR/wayland-*`, `/tmp/.X11-unix/X0`, the session bus) |
| `connected-computer` | the computer you're connected from | finds the newest Cursor or VS Code server's `remote-cli`, `helpers/browser.sh` and live `vscode-ipc-*.sock` |

The check step fails with one sentence when the chosen place isn't available ("no desktop session
open", "no Cursor or VS Code window connected").

It branches on `$SHIMMER_PLATFORM` (CLAUDE.md §10.1), is POSIX `sh` (ADR 0012 §5: Debian's `sh`
is `dash`), and is copied into each created workspace like any other file, so the workspace
keeps working even if a later Shimmer changes the helper. Files outside `steps/` are already
allowed in a workspace folder (ADR 0012 §6 only checks `steps/`).

### 6. The built-ins

**`web-project`**: develop a website or web app. Nothing in it is specific to one person or one
host: your setup is a set of answers.

| Question | Kind | Default | Used by |
|---|---|---|---|
| `PROJECT_DIR` | folder, required | | every step |
| `CODE_EDITOR` | choice | `vscode` | check, editor |
| `OPEN_PATH` | text | empty = the project folder | editor: a file, or a `.code-workspace`, inside the project |
| `GIT_ON_OPEN` | choice `pull`, `none` | `pull` | git |
| `INSTALL_COMMAND` | command | `auto` | install |
| `SERVICES_COMMAND` | command | empty | services, e.g. `docker compose up -d` |
| `DEV_COMMAND` | command | `auto` | dev server |
| `LOCAL_URL` | url or `auto` | `auto` | wait, browser |
| `LIVE_URL` | url or `auto` | `auto` | browser: the deployed site |
| `REPO_PAGE` | choice `home`, `pulls`, `issues`, `actions`, `none` | `home` | browser |
| `LINKS` | urls | empty | browser: live site, Vercel/Netlify/… dashboard, analytics |
| `IDE_TERMINAL_COMMAND` | command | empty | editor: a terminal *inside* VS Code / Cursor running it |
| `OPEN_ON` | choice `auto`, `this-machine`, `connected-computer` | `auto` | every opening step (§5) |
| `TERMINAL_APP` | choice `auto`, `kitty`, `foot`, `alacritty`, `wezterm`, `ghostty`, `gnome-terminal`, `konsole`, `terminal`, `iterm`, `none` | `auto` | terminal |
| `TERMINAL_COMMAND` | command | empty | terminal, e.g. `claude` |

| # | Step | Mode | Does |
|---|---|---|---|
| 1 | `check` | supervised, 15 s | Folder exists; editor installed; tools the commands need exist; the local port isn't taken by something other than this workspace's own dev server. |
| 2 | `git` | supervised, 60 s | With `pull`: `git pull --ff-only` only when the working copy is clean and has an upstream. Offline or diverged: logged, never fatal. |
| 3 | `install` | supervised, 600 s | `auto`: the package manager's install, only when `node_modules` is missing or older than the lockfile. |
| 4 | `services` | supervised, 120 s | `SERVICES_COMMAND`, if any. |
| 5 | `editor` | detached | The editor on `OPEN_PATH` or the folder. |
| 6 | `dev-server` | detached | `DEV_COMMAND`, unless this workspace's dev server is already running (activating an `active` workspace again doesn't start a second one). |
| 7 | `wait` | supervised, 120 s | Waits until `LOCAL_URL` answers, so the browser never shows "can't connect". |
| 8 | `browser` | detached | `LOCAL_URL`, the repo page and `LINKS`. |
| 9 | `terminal` | detached | A terminal in the project, running `TERMINAL_COMMAND` if given. |
| | `cleanup.sh` | supervised, 30 s | Stops the dev server it started and frees the port. |

What `auto` means, decided when the step runs:

- **Package manager**, from the lockfile: `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn,
  `bun.lockb`/`bun.lock` → bun, otherwise npm. No `package.json`: no install, no dev command.
- **`DEV_COMMAND`**: `<pm> run dev` if `package.json` has a `dev` script, else `<pm> start` if it
  has `start`, else nothing.
- **`LOCAL_URL`**: from the framework in `package.json`: Next.js, Nuxt, Create React App →
  `http://localhost:3000`; Vite, SvelteKit → `:5173`; Astro → `:4321`; Gatsby → `:8000`.
  Unknown: no local tab, and `wait` does nothing.
- **`LIVE_URL`**: the `homepage` field of `package.json` (an `http(s)` URL), the standard place a
  project records where it's deployed. None: no live-site tab. (The host's own dashboard, e.g.
  Vercel, can't be asked without the person's login, which a workspace never holds.)
- **Repo page**: from `git remote get-url origin`, with `git@github.com:you/site.git` turned
  into `https://github.com/you/site`. `pulls`/`issues`/`actions` are GitHub's paths; on other
  hosts only `home` is opened. No remote: nothing.
- **Node version**: when the project has `.nvmrc` or `.node-version` and `fnm`, `nvm` or
  `volta` is installed, the install and dev-server steps use that version.

**A terminal inside the editor** (`IDE_TERMINAL_COMMAND`, VS Code and Cursor only). Neither editor
has a command-line flag for it, but both run a task marked `"runOn": "folderOpen"` in the
folder's `.vscode/tasks.json` in their own terminal. The editor step writes that file before
opening the folder, with a first line saying Shimmer created it; it rewrites only a file it
created, prints the task to add when the project already has a `tasks.json` of its own, deletes
its file when the answer is cleared, and hides it from git through `.git/info/exclude` (local,
never committed). The editors may ask once to allow automatic tasks. It needs the editor to open
the folder, not a single `OPEN_PATH` file.

**The dev server's output and process id** go in the system temp folder,
`${TMPDIR:-/tmp}/shimmer-<workspace id>/` (`dev-server.log`, `dev-server.pid`), never inside
`$SHIMMER_HOME`: only the daemon writes there (CLAUDE.md §2), and `logs/` is the launcher's
(ADR 0010 §3). The temp folder is the right lifetime too: a reboot clears it, and a reboot also
ends the server. A detached step is its own process group (ADR 0010 §3), so cleanup stops the
server and everything it started with one `kill` of that group.

**Secrets never go in answers.** `workspace.toml` is meant to be committable with the rest of
`$SHIMMER_HOME` (CLAUDE.md §7). API keys stay in the project's own `.env.local`. The template's
header comment says so, as the personal-data record templates do.

**`smoke-test`**: for testing workspaces themselves (#34). It opens no apps, so it behaves the
same on every machine and needs nothing installed.

| Question | Kind | Default |
|---|---|---|
| `FAIL_AT` | choice `none`, `check`, `cleanup` | `none` |

| # | Step | Mode | Does |
|---|---|---|---|
| 1 | `check` | supervised, 15 s | Prints every `SHIMMER_*` variable (checking the script ABI, CLAUDE.md §10.1) and runs `shimmer ping` through `SHIMMER_SOCKET` (checking the callback). Fails if `FAIL_AT = check`. |
| 2 | `wait` | supervised, 15 s | `sleep 2`: a supervised step that takes time. |
| 3 | `background` | detached | `sleep 300` in its own process group: a detached step that outlives the launch. |
| | `cleanup.sh` | supervised, 15 s | Stops `background`; fails if `FAIL_AT = cleanup`. |

So one template walks through `ready → launching → active`, `dirty` (with `FAIL_AT = check`),
cleanup back to `ready`, and a failed cleanup that stays `dirty`.

**`free-disk`**: free space by removing only what comes back by itself. Its helper is
`lib/shimmer-free.sh`.

- **Build folders of projects untouched for `UNTOUCHED_DAYS`** (default 30) under `CODE_DIRS`
  (searched 6 levels deep, never inside hidden folders): a Rust `target/` only when Cargo's own
  `CACHEDIR.TAG` marker is inside, and a `node_modules/` only next to a `package.json` and not a
  symlink. "Untouched" means none of the project's own files (not its build folders or `.git`)
  changed for that long, so the project you're working on is never cleaned. They are removed,
  not moved to the trash, which would free nothing; `cargo build` or `npm install` makes them
  again.
- **Download caches** through each installed tool's own command: npm, pnpm, yarn, bun, pip, uv,
  go, cargo (`cargo-cache --autoclean`, when installed), brew, docker (`docker system prune -f`:
  never volumes), and the thumbnail cache. Each is measured by its folder before and after, or
  by the disk's free space when it has none.
- **`SYSTEM_CLEAN`** (`skip` by default): the package manager's downloaded packages (`paccache`,
  `pacman -Sc`, `apt-get clean`, `dnf`/`zypper clean`) and journal logs over 2 weeks old, with
  sudo, the same three ways as `update-everything`'s `SYSTEM_UPDATES`. The terminal-and-wait
  code both use is `shimmer_terminal_and_wait` in `lib/shimmer-open.sh`.
- **Never**: your files, git repos, virtualenvs, the trash, docker volumes.

`MODE = preview` removes nothing and lists what `clean` would free. `SKIP` names things never to
clean. Steps `check`, `projects`, `caches`, `system`, all supervised, then `summary`: biggest
first (at most 15 lines), anything skipped or failed, the total, and the free space before and
after, which `activate --wait` prints. A failure is reported, never the end of the run.

**`github-inbox`**: what GitHub is waiting on you for, through the `gh` CLI. Shimmer never holds
a GitHub token: `gh` does (`gh auth login`; `GH_HOST` for GitHub Enterprise). One GraphQL query
(three aliased searches) and `gh`'s own `--jq` produce the lists, so `jq` isn't needed. Its
helper is `lib/shimmer-github.sh`.

- **Waiting for your review**: open PRs requesting your review, with author and how long since
  opened; `!` marks those waiting `STALE_DAYS` (2) or more.
- **Your open PRs**: CI (the last commit's check rollup) and review decision, as "ready to merge"
  (checks pass or none, approved), "checks failing, changes requested", "draft", and so on.
- **Assigned to you**: open issues. Then the unread notification count (up to 50).
- `SCOPE` limits all of it to orgs or `owner/repo`s; archived repos are left out.

Steps `check` (gh installed and logged in), `fetch` (a failed query makes the workspace dirty
with gh's message, never an inbox that just looks empty), `open` (detached: with `OPEN`, the
review PRs, everything (at most 10), or GitHub's own inbox pages), and `summary`, which
`activate --wait` prints. `NOTIFY = when-reviews` sends a desktop notification when reviews are
waiting, for a scheduled run.

**`health`**: one read-only report on the computer: nothing is changed and nothing needs sudo.
Its helper is `lib/shimmer-health.sh`, one function per check, each printing `ok`, `warn` or
`info` lines (or nothing when it doesn't apply here, e.g. no battery):

| Check | Warns when |
|---|---|
| `disk` | a real disk (one line per device; no loop or macOS system volumes) is `DISK_WARN_PERCENT` full (90) |
| `memory` | memory is `MEMORY_WARN_PERCENT` used (90); swap shown |
| `load` | the 15-minute load is above the core count |
| `uptime` | a restart is needed (`/var/run/reboot-required`, or the running kernel's modules are gone after an update), or up `UPTIME_WARN_DAYS` (30) |
| `battery` | health (full ÷ design capacity; `Maximum Capacity` on macOS) is below `BATTERY_WARN_PERCENT` (80) |
| `temperature` | a sensor is at 85 °C (Linux only; macOS needs sudo for it) |
| `services` | a systemd unit, system or user, has failed |
| `workspaces` | a Shimmer workspace is `dirty` or `invalid`, through `shimmer workspaces list` |
| `updates`, `ports`, `docker`, `folders` | information only: updates waiting (`checkupdates`, `apt`, `brew`), what of yours listens on a port, what docker could free, the `BIGGEST_FOLDERS` in your home |

Steps `check`, `report`, then `summary`: needs attention (`!`) first, then fine (`✓`), then
information (`·`), which `activate --wait` prints. `NOTIFY = on-warning` sends a desktop
notification (`notify-send`, or `osascript` on macOS) only when something needs attention, for
a scheduled run. `SKIP` leaves checks out.

**`monorepo`**: several apps from one repo at once, with their logs side by side. Separate from
`web-project` so the single-app case stays simple. Its own helper, `lib/shimmer-monorepo.sh`,
recognises the tool and builds the commands:

| Found at the root | Tool | Dev servers, with `DEV_COMMAND = auto` |
|---|---|---|
| `turbo.json` | Turborepo | one process: `turbo run dev --filter=a --filter=b` (the tool's own labelled output) |
| `nx.json` | Nx | one process: `nx run-many -t serve -p a,b` |
| `pnpm-workspace.yaml` | pnpm | per app: `pnpm --filter a run dev` |
| `package.json` `"workspaces"` | npm / yarn / bun (by lockfile) | per app: `npm run dev -w a`, `yarn workspace a run dev`, `bun run --filter a dev` |
| `Cargo.toml` `[workspace]` | Cargo | per app: `cargo run -p a` |
| `go.work` | Go | per app: `go run ./a` |

A `DEV_COMMAND` of your own runs once per app when it contains `{app}`, else once; `DEV_TARGET`
names the task (`auto`: `serve` for Nx, `dev` otherwise). Every app's process writes
`dev-<app>.log` in the temp folder and runs in the `dev` step's process group, which stays alive
while they do (a second activate starts nothing new; `stop` stops them together). The `wait`
step waits for every `LOCAL_URLS` entry and fails as soon as any app's process exits, with that
app's log. A `logs` step opens one terminal following every app's log (`LOGS_TERMINAL`). Install
is one root install when the lockfile changed (Cargo and Go skip it). The rest (editor, `OPEN_ON`,
in-editor terminal, live site, links, browser window, `CLOSE_ON_STOP`) is `web-project`'s.

**`offline-prep`**: get projects ready to work with no network (a flight, bad Wi-Fi, a trip,
a capped hotspot, a network where GitHub or npm is blocked). `activate` is "going offline",
`stop` is "back online". Its helper is `lib/shimmer-offline.sh`, one function per ecosystem.

Projects are a list (`PROJECTS`, one per line in `workspace.toml` for paths with spaces) plus
`CLONE_REPOS` cloned into `CLONE_DIR` when missing; a list rather than every folder under a root,
so nothing is downloaded for projects you won't touch. Parts, each a supervised step and each
can be left out with `SKIP`:

| Part | Does |
|---|---|
| `machine` | Battery (warns under 80% unplugged), disk space (under 5 GB), logins that have expired while there's still network to refresh them (`gh`, `aws`, `gcloud`, `az`). |
| `repos` | Clone, fetch; with `UPDATE_REPOS = pull` (a required answer, or `fetch-only`) also fast-forward the current branch when clean (never a merge); submodules, Git LFS; warns on uncommitted or unpushed work. |
| `deps` | Each project's own tools, from the files at its top: cargo, npm/pnpm/yarn/bun (by lockfile), uv/poetry/pip (pip also downloads a wheelhouse to `OFFLINE_DIR/wheels`), go, maven/gradle (the project's wrapper first), bundler, composer, dotnet, mix, dart/flutter pub, swift, cabal/stack, zig, deno. For Rust projects it says when `rust-src` or `rust-analyzer` (editor support offline) is missing, and never adds it: toolchains are the person's. A missing tool is a warning, not a failure. |
| `docker` | Each compose file's images (`pull --ignore-buildable`, then `build`) and each Dockerfile's `FROM` images (not its own stages). Not with `DATA_SAVER`. |
| `build` | With `WARM_BUILD = yes`: compiles once, offline, with tests (compiled languages only; a web build often reaches the network). |
| `verify` | **Does it really work offline?** Each tool in its own offline mode (`cargo fetch --offline`, `uv sync --offline`, `GOPROXY=off go list`, `pip --dry-run --no-index`, `mvn -o`, `gradle --offline`, `bundle --local`, `pub get --offline`, `deno --cached-only`), or what's on disk (every `package.json` dependency in `node_modules`: `npm ls` fails on harmless peer warnings), and compose images present. A failure says why, in the tool's own last line. Also lists hosts in `.env` files that aren't this machine: those services need the network whatever is downloaded. |
| `docs` | `cargo doc` per Rust project (docs of the exact versions used), `rustup doc` when `rust-docs` is installed (else how to add it), `tldr --update`. |
| `pages` | `SAVE_PAGES` as single files: `monolith`, else `wget` with page requisites, else `curl`. |
| `github` | With `GITHUB_SNAPSHOT` (`projects`: their GitHub remotes; `everything`): issues assigned to you, your open PRs and PRs waiting for your review, each `gh … view --comments` as Markdown and PRs' diffs. |
| `ai` | `ollama pull AI_MODEL`, starting `ollama serve` for the download if needed. Not with `DATA_SAVER`. |

`summary` prints every warning, each project's verdict ("works offline" / "NOT ready offline
(why)"), the other parts counted, and services needing the network, then writes
`OFFLINE_DIR/index.html`: a start page linking each project's Rust docs, the saved pages, the
GitHub snapshot, what else works offline (`rustup doc`, `pydoc -b`, `go doc`, the AI model), and
the full report.

**Back online** (`cleanup.sh`, run by `stop`): fetch each repo and say what's behind, unpushed or
uncommitted. `PULL_ON_RETURN = yes` fast-forwards repos that are clean and have nothing of their
own; `PUSH_ON_RETURN = yes` pushes the current branch when it is ahead and not behind. Both are
off by default, never forced, never a merge. Nothing downloaded is removed.

Not done: browser-based offline docs (DevDocs, Zeal, Dash) need a click in their own app, and
download sizes aren't estimated (`DATA_SAVER` skips the big parts instead).

**`scratch`**: a fresh throwaway folder for trying something out. Each activate makes
`SCRATCH_DIR/<date>-<language>` (`-2`, `-3`… on the same day; `NEW_FOLDER = once-a-day` reopens
that day's instead) with a starter file for `LANGUAGE`, opens the folder in the editor with that
file, and a terminal in it showing the command that runs it. Its helper is
`lib/shimmer-scratch.sh`.

There is no end to languages, so `LANGUAGE` is text, not a choice. About 30 are known, each with a
starter that prints "hello from scratch" and its usual run command (python, rust, javascript,
typescript, go, c, cpp, java, kotlin, swift, csharp, ruby, php, lua, perl, r, julia, haskell,
ocaml, elixir, clojure, dart, scala, zig, nim, crystal, fortran, sql, html, shell; common other
spellings such as `c++`, `ts`, `golang` too), plus `none` for an empty folder. **Any other
language is a file name** (`main.odin`): that file is made empty and opened, and the folder is
named by its extension. `RUN_COMMAND` gives the run command for it, or replaces a known one.

| # | Step | Mode | Does |
|---|---|---|---|
| 1 | `check` | supervised, 30 s | The language, editor, terminal, `KEEP_DAYS`; a note (not a failure) when the language's program isn't installed. |
| 2 | `folder` | supervised, 60 s | Tidies old folders (below), makes the folder and its starter (never over an existing file), `git init` with `GIT_INIT = yes`. Writes the folder's path to the temp folder for the next steps. |
| 3 | `editor` | detached | The folder with the starter file open (VS Code, Cursor, Zed and Sublime take both at once). |
| 4 | `terminal` | detached | A terminal in the folder, printing the run command. |
| 5 | `summary` | supervised, 10 s | The folder and the run command, which `activate --wait` prints. |
| | `cleanup.sh` | supervised, 15 s | Closes the terminal (`CLOSE_ON_STOP`); the folder is always kept. |

**Nothing is deleted.** With `KEEP_DAYS` set, a folder is moved to the trash (`gio trash`,
`trash-put`, or macOS `trash`) only if this template made it (it holds a `.shimmer-scratch`
file) and nothing inside it changed for that many days. With no trash command it is kept and
listed. Scripts that delete people's files on a timer are how work is lost; the trash keeps a
way back.

**`update-everything`**: one command that updates every developer tool you have installed and
your system's packages, then says what updated and what failed. A one-off job rather than a
session: run it again any time (an `active` workspace may be activated again), or on a schedule.
Its helper is `lib/shimmer-update.sh`.

The list of things that could be updated has no end, so the template doesn't try to know them
all:

- **It detects, it doesn't ask.** A built-in list (rustup, `cargo install-update`, brew, mas, npm,
  pnpm, bun, deno, pipx, uv, flatpak `--user`, gh extensions, ghcup, tldr, claude) runs each
  updater only when it's installed. npm is skipped, with the reason, when its global folder
  belongs to root.
- **topgrade, when there is one** (`USE_TOPGRADE = auto`): it knows 100+ updaters, so it
  replaces the list, with its `system` step disabled (see below).
- **`SKIP`** names updaters never to run; **`EXTRA_COMMANDS`** adds your own, one per line, each
  run as its own updater named by its first word.
- **One failure never stops the rest.** Every updater runs with input from `/dev/null` (nothing
  waits for an answer nobody will type) and its result is recorded; the run stays successful,
  and the `summary` step lists `updated` / `skipped` / `failed` with the log path. Failing the
  run would make the workspace `dirty` and block the next scheduled run over one bad updater.

**System packages need sudo, and a password is never an answer** (the rule below).
`SYSTEM_UPDATES` (required: no default):

| Answer | What happens |
|---|---|
| `terminal` | A terminal opens running the update (`paru`/`yay`, `pacman`, `apt-get`, `dnf`, `zypper`, `apk`, or `softwareupdate` on macOS, plus `snap` and system `flatpak` when present); you type your password into `sudo` there. The `system` step waits for the window's exit code (written to the temp folder), notices a window closed early, and fails only if no window opens within 30 s. |
| `passwordless` | `sudo -n` and the tool's own "yes" flag, for a sudo rule of yours that needs no password, e.g. for a scheduled run. A "password is required" refusal is reported as such, with how to fix it. |
| `skip` | Tools only. |

| # | Step | Mode | Does |
|---|---|---|---|
| 1 | `check` | supervised, 30 s | Lists what this run will update; checks the terminal (and where it opens, `OPEN_ON`) for `terminal`, and topgrade for `USE_TOPGRADE = yes`. Clears the last run's results. |
| 2 | `system` | supervised, 3600 s | The system update, as above. |
| 3 | `tools` | supervised, 3600 s | Every installed tool (or topgrade), then `EXTRA_COMMANDS`. |
| 4 | `summary` | supervised, 30 s | What updated, was skipped and failed. |
| | `cleanup.sh` | supervised, 30 s | Closes the system update's terminal if it is still open. Updates are not undone. |

Because its last step is supervised, `workspaces.activate`'s result carries that step's log
(docs/protocol.md), and `shimmer workspaces activate <id> --wait` prints the summary.

**`vm`**: start a virtual machine, wait until it answers over SSH, then open an editor and a
terminal inside it. Shimmer runs on the computer that hosts the VM.

| Question | Kind | Default |
|---|---|---|
| `VM_SOFTWARE` | choice `utm`, `parallels`, `virtualbox`, `vmware`, `libvirt`, `multipass`, `custom` | required |
| `VM_NAME` | text (VMware: the `.vmx` path) | required |
| `START_COMMAND`, `STOP_COMMAND` | command, `custom` only | empty |
| `START_MODE` | choice `window`, `headless` | `window` |
| `VM_HOST` | text: hostname, IP, `~/.ssh/config` alias, or `auto` (UTM, Multipass) | empty: only start the VM |
| `SSH_USER`, `SSH_PORT` | text | empty, `22` |
| `SSH_PASSWORD_ITEM` | text: the **name** of a keychain entry | empty |
| `REMOTE_EDITOR`, `REMOTE_FOLDER` | choice `none`, `cursor`, `vscode`; text | `none`, empty |
| `TERMINAL_APP` | choice, as `web-project`'s, plus `none` | `none` |
| `ON_STOP` | choice `suspend`, `shutdown`, `leave-running`; only ever applied to a VM this workspace started | required |

| # | Step | Mode | Does |
|---|---|---|---|
| 1 | `check` | supervised, 30 s | The software's command-line tool exists (on `PATH` or in its macOS app bundle), the VM exists, the editor and terminal exist, and a named keychain entry can be read. |
| 2 | `start` | supervised, 180 s | Starts the VM unless it's already running, with its window or headless. |
| 3 | `wait` | supervised, 300 s | Waits until the SSH port answers (`nc`, else `ssh` itself), then logs whether an SSH key works and, if not, the one `ssh-copy-id` command that fixes it. |
| 4 | `editor` | detached | Cursor or VS Code on `REMOTE_FOLDER` inside the VM, over Remote-SSH. |
| 5 | `terminal` | detached | A terminal on the host running `ssh` into the VM. |
| | `cleanup.sh` | supervised, 180 s | Suspends, shuts down or leaves the VM, as `ON_STOP` says. |

Each VM program's commands live in a second shared helper, `lib/shimmer-vm.sh`, copied only into
workspaces made from `vm`.

**Passwords are never answers, and Shimmer never sees one.** In order of preference, the
template's header comment and its `wait` step steer people to:

1. **An SSH key**, set up once with `ssh-copy-id` (the password is typed into `ssh` one last
   time, never into Shimmer);
2. **a key with a passphrase** unlocked by the login keychain (`ssh-add --apple-use-keychain`);
3. **nothing**: with no key, `ssh` asks for the password itself, in the terminal window the
   workspace opened, and a Remote-SSH editor asks in its own window;
4. **`SSH_PASSWORD_ITEM`**: the name of an entry in macOS Keychain (`security`) or the Linux
   keyring (`secret-tool`). The terminal step writes a tiny `SSH_ASKPASS` program into the
   workspace's temp folder (mode `700`), which holds only the entry's name and reads the secret
   when `ssh` asks (`SSH_ASKPASS_REQUIRE=force`, OpenSSH 8.4+). The password is never in
   `workspace.toml`, a log, an environment variable, or a command line (so never in `ps`);
   `sshpass -p` is not used for that reason.

The rule generalises to every template: a question may ask for the **name** of a keychain entry,
never for the secret itself.

### 6a. Safe to run, from activate to remove

Every template follows these rules, and `crates/app/tests/templates_e2e.rs` checks them for
every built-in against the real daemon (below).

1. **The person decides anything with consequences.** A question whose answer deletes, updates,
   changes their code or shuts something down is `required`, with no default: `free-disk`
   `MODE` (`preview` / `clean`), `update-everything` `MODE` (`preview` / `update`) and
   `SYSTEM_UPDATES`, `vm` `ON_STOP`, `offline-prep` `UPDATE_REPOS` (`fetch-only` / `pull`).
   Without a terminal a missing one is an error naming the `--set` to add (§8). Safe,
   reversible choices keep a default (pull fast-forward-only on open, close windows on stop).
   A script reading an empty answer (an older workspace) takes the safe side: preview, skip,
   fetch-only, leave the VM running.
2. **Preview before doing.** The two templates that change the machine have `MODE = preview`,
   which runs nothing and lists exactly what the other mode would do.
3. **`peek` before making one** (§1): what it changes, needs, its steps and what stop does.
4. **Activating again starts nothing twice.** A dev server, services (`services.started`), a
   custom VM (`vm.started`), a background step, a terminal or a browser window that the last
   activate opened and is still running is reused, never duplicated, so nothing is left behind
   that stop doesn't know about. (Each closable window is recorded with the step that opened it,
   `pgid step label`; `shimmer_window_open` asks whether it's still up.)
5. **Stop stops only what the workspace started.** Its dev servers, services, windows it could
   close, and a VM only if this workspace started it; then it says what it left open and why
   (the editor, shared terminals, services with no stop command). It never touches the
   person's own windows or a VM they started themselves.
6. **No orphans.** After `stop`, after `cleanup` of a failed launch, and after `remove`, no
   process the workspace started is still running (a supervised step's leftovers are its own
   process group; detached ones are recorded and stopped). `remove` is refused while a
   workspace is active or dirty; `reset` (dirty to ready without cleanup) asks first and says
   that whatever the launch started keeps running.
7. **Secrets are never answers** (above), and nothing a template writes holds one.

**End-to-end tests.** `templates_e2e.rs` runs every built-in through the real binary and daemon
in a sandbox: create (after `peek`), activate, activate again, stop, a deliberate failure and its
cleanup, remove, plus each template's own behaviour (the dev server answers; free-disk keeps a
`target/` Cargo didn't make; preview runs nothing; back online pushes only when asked…). The
daemon gets a fake `HOME`, its own `TMPDIR`, and a `PATH` that is an allowlist of basic system
tools plus stand-ins for every program a template drives (editors, terminals and browsers that
stay open until closed, sudo, gh, npm, ollama, updaters), so a test can never update, delete,
prune or open anything real. Orphans are found through `/proc`: any live process with the
workspace's `SHIMMER_WORKSPACE_ID` and the sandbox's `SHIMMER_HOME` fails the test. Linux only.

### 7. Two ops

| Op | Execution | Params | Result |
|---|---|---|---|
| `workspaces.templates` | inline | `{}` | `{"templates":[{"id","label","description","questions":[{"name","prompt","kind","required","default"?,"choices"?,"help"?}]}]}`, by id |
| `workspaces.create` | inline | `{"id","template","values"?,"label"?}` | the new workspace, as `workspaces.status` shows it |

`workspaces.create`:

- `id` must be a valid workspace id (ADR 0012 §6), so a request can never name a path outside
  the namespace.
- `template` must exist, or `not_found` naming the ones that do.
- `values` maps question names to strings. An unknown name, a missing required answer, or an
  answer that doesn't fit its kind (§3) is `invalid_params` naming the question. Left out means
  the default, or empty.
- **Never overwrites.** If anything exists under `data/workspaces/<id>/`, it is `conflict`.
- **Checked before writing**: the generated `workspace.toml` and the copied `steps/` go
  through ADR 0012's checks. A template that would produce an invalid workspace is a bug and
  fails loudly instead of being written.
- **One transaction** writes every file and emits `workspaces.workspace.created` with
  `{workspace, template}` (CLAUDE.md §7.1).
- The result **forgets it came from a template**: no `state.toml` (so it starts `ready`), no
  link back, and later changes to the template never touch it.

### 8. CLI

```
$ shimmer workspaces templates
Coding (code)
  monorepo           Several apps from one repo at once: Turborepo, Nx…
  scratch            A fresh throwaway folder with a starter file in your language…
  …
Machine upkeep (upkeep)
  free-disk          Free space safely: build folders of projects you haven't…
  …

one in full, with its questions: shimmer workspaces templates TEMPLATE
make a workspace from one:       shimmer workspaces new NAME --from TEMPLATE

$ shimmer workspaces templates upkeep          # one category
$ shimmer workspaces peek free-disk            # everything before using one: description, good
                                               # to know, needs, each step and how it runs, what
                                               # stop does, and every question
$ shimmer workspaces peek free-disk --scripts  # …then each step's script, in order, and the
                                               # cleanup script stop runs; other files named
$ shimmer workspaces peek free-disk --file lib/shimmer-free.sh   # one file, as it is

$ shimmer workspaces new site --from web-project
Project folder: ~/code/site
Editor [vscode] (vscode, cursor, zed, webstorm, intellij, sublime, neovim, none): cursor
Other links to open (live site, hosting dashboard, analytics…) []: https://me.dev https://vercel.com/me/site/deployments
  Separate them with spaces. Leave empty for none.
…
✓ created workspace site from web-project
  start it: shimmer workspaces activate site
  change an answer: edit [env] in ~/.local/share/shimmer/data/workspaces/site/workspace.toml
```

- `shimmer workspaces new ID --from TEMPLATE [--label TEXT] [--set NAME=VALUE]…`.
- At a terminal it asks each question not given with `--set`, showing the default. Without a
  terminal it **never asks** (CLAUDE.md §15.1): unanswered questions take their defaults, and a
  missing required one is an error naming the `--set` to add.
- `folder` and `file` answers: the CLI expands `~` and makes relative paths absolute, since
  only the client knows the person's home and current folder.
- No pack aliases for these two, as for record templates (ADR 0022 §4).
- `shimmer workspaces edit NAME [FILE] [--path]` opens `workspace.toml` (or `FILE` in the same
  folder, e.g. `steps/01-check.sh`) in `$VISUAL`, then `$EDITOR`, then `nano` or `vi`, and asks
  `workspaces.status` afterwards, so a typo is reported as soon as the editor closes rather than at
  the next launch. Client-only: the folder is the one CLAUDE.md §7 documents, under
  `shimmer_proto::paths::shimmer_home()`, and the person's editor writes it, as any hand edit
  would. It refuses a `FILE` outside the folder, an unknown workspace (the daemon's `not_found`),
  and running without a terminal (it prints the path instead). `--path` only prints the path.

### 9. Reconfigure: answering again

`shimmer workspaces reconfigure NAME [--set QUESTION=ANSWER]…` asks the template's questions
again with the workspace's current answers as the defaults (Enter keeps one, `-` clears an
optional one), instead of hand-editing `[env]`. Two ops:

- `workspaces.answers {id}`: the template it was made from, the questions, and each current
  answer (null when `[env]` doesn't have it yet; that question shows the template's default).
- `workspaces.reconfigure {id, values}`: the new answers.

Rules:

- **Which template.** `create` now writes `.template` (the template's id) in the workspace's
  folder. A workspace made before that is known by the "Created from Shimmer's X template." line
  every template writes at the top of `workspace.toml`. One made by hand has no questions:
  `invalid_params`, pointing at `workspaces edit`.
- **Only `[env]` is rewritten**: the table (and our comment above it) is replaced, everything else
  in `workspace.toml` (comments, step edits) stays byte for byte. A table header inside a
  multi-line string isn't mistaken for the end of `[env]`. `[env]` values no question asks for
  (added by hand) are kept, after the answers.
- **Checked as `create` checks**, but only the answers that change: an unchanged answer stays
  exactly as it is, even one edited by hand into several lines. A required question can't be
  emptied. The new file is parsed as a manifest before it is written.
- **Only while `ready`.** Active, launching or dirty is `conflict`: stop and cleanup read the
  answers, and would undo what the *new* ones name (another project folder, another stop
  command) instead of what the old ones started.
- Nothing changed: nothing written, no event. Otherwise one transaction writes the file and emits
  `workspaces.workspace.reconfigured` with the names that changed (never the values).
- Scripts are not touched: bringing them up to a newer template is a separate feature (an
  `upgrade`, not done here).

## Consequences

- `crates/modules/workspaces`: a `templates/` folder embedded with `include_str!`, a pure
  `template.rs` (parse `template.toml`, check answers, build the `workspace.toml` text), the two
  ops, and tests against the in-memory `Ctx` (§12 rule 13): every built-in creates a workspace
  that passes ADR 0012, never overwrites, rejects bad answers, and launches against the
  `FakeLauncher`.
- The scripts are checked with `sh -n` in a test, and run for real in an end-to-end test of
  `smoke-test` (it needs nothing installed).
- `crates/cli`: `workspaces templates`, `workspaces new`.
- `docs/protocol.md`: both ops and the topic. `crates/mockd/fixtures/core.json` lists them.
  Dev C told.
- `web-project` is tested by hand on Linux and macOS before it ships: that is part of #34.

## Not done here

- More built-ins (a plain `project` template is next), user templates in `$SHIMMER_HOME`, and a
  community repository.
- Hyper-V and other Windows VM software: the Windows launch backend is still a sketch.
- Showing a project's tracker items on launch (`shimmer records list … --filter`), which needs
  the records query work (ADR 0019) on master first. A one-line step to add later.
- Placing windows on Hyprland workspaces or monitors, and `vercel env pull` (needs the Vercel
  CLI and a login): easy extra steps for anyone who wants them, not defaults.
- Windows (`.ps1` versions): the launch backend for Windows is still a sketch (ADR 0010 §7).
- Kits (#79), which create collections and a workspace together.
