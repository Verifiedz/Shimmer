# Shimmer

## What it is

Shimmer is a local-first command center for software engineers. It keeps the things you track
(LeetCode problems, job applications, anything with fields) on your own machine, in plain files
you can read, edit, copy to another computer or commit to git. There is no server, no account
and no network requirement.

Today Shimmer is a background daemon plus a command-line client. You use it to define trackers
and add, list, filter, update and complete records in them, to launch workspaces (your own
scripts that set up a work session), and to schedule and watch background work.

## Overview

Shimmer is built as a few general mechanisms rather than one feature per tracker. A LeetCode
tracker is not code: it is a short TOML file describing a collection. The mechanisms in the
code today:

| Mechanism | What it does |
|---|---|
| **records** | Typed collections. Each collection is a TOML file that declares its fields (`string`, `enum`, `date`, …); each record is its own TOML file. Handles add, list, filter, update, complete and remove. Ready-made templates (LeetCode, job applications, interviews, …) create a tracker with one command; a fresh install starts with none. |
| **workspaces** | Launches a work session from your own scripts: numbered steps run in order, each either waited on or left running (an editor, a browser). A failed launch marks the workspace `dirty` until its cleanup script runs, so you never launch into a half-set-up session. |
| **queue** | The single pipeline every unit of background work runs through. Work is split into lanes, each with its own concurrency limit. |
| **scheduler** | Owns *when* work happens: recurring and one-shot triggers that enqueue tasks on the queue. It never runs anything itself. |
| **event bus** | How the pieces react to each other. Every change is published as an event (`records.item.completed`, …) and appended to a log. Modules never call each other directly. |

`records` and `workspaces` are modules. The queue, scheduler and event bus are core services
inside the daemon. On top of them, the CLI has **command packs**: optional themed names for its
commands (`ikuzo deep-work` for `shimmer workspaces activate deep-work`).
The full design, including the mechanisms that aren't built yet (fetchers, notifications and a
calendar), is in [`CLAUDE.md`](CLAUDE.md) §1.1.

## How it works

One long-lived **daemon** owns all state. Every other program is a thin client that talks to
it over a Unix domain socket:

```
shimmer (CLI) ──→ Unix socket ──→ daemon ──→ files in $SHIMMER_HOME ──→ SQLite index (derived)
```

- **The daemon is the only writer.** It hosts the modules, runs the queue and scheduler, and is
  the only process that changes your data.
- **Clients hold no logic.** The CLI sends a request such as `records.list`, then formats the
  reply. It finds out which modules and operations exist by asking the daemon (`shimmer manifest`),
  so nothing about a module is hardcoded into the client.
- **You never start the daemon yourself.** The first `shimmer` command starts it in the background
  if it isn't running, the same way `tmux` or `ssh-agent` do.
- **The protocol is newline-delimited JSON**, with requests and responses plus a stream of
  events a client can subscribe to. You can talk to the daemon by hand with `nc -U`. The
  contract is in [`docs/protocol.md`](docs/protocol.md).

**Your files are the truth.** Everything durable is a human-readable file under `$SHIMMER_HOME`:

```
$SHIMMER_HOME/
  config.toml                                Settings (see below)
  data/records/collections/<collection>.toml One file per collection definition
  data/records/items/<collection>/<id>.toml  One file per record
  data/records/trash/<collection>/<id>.toml  Removed records, until restored or purged
  data/workspaces/<name>/                    A workspace: workspace.toml, steps/, its state
  data/scheduler/triggers/<id>.json          One file per scheduled trigger
  packs/<name>/                              Your own command packs
  events/YYYY-MM-DD.jsonl                    Append-only log of every event
  index.sqlite                               Derived from the files; safe to delete
  logs/                                      Daemon logs (daemon.log.<date>, rotated daily at
                                              UTC midnight, oldest deleted past 8 files) and
                                              workspace step logs
```

`$SHIMMER_HOME` defaults to `~/.local/share/shimmer` on Linux and `~/Library/Application Support/shimmer` on
macOS. Set the `SHIMMER_HOME` environment variable to use another folder. The SQLite index exists
only to make queries fast: delete it and the daemon rebuilds it from the event log on its next
start. Writes are atomic, so a crash leaves either the old file or the new one, never half of
one. The folder ships with a `.gitignore` for the derived and machine-local parts, so you can
commit it as-is.

**Settings** live in `config.toml`, read when the daemon starts (`shimmer shutdown`, then any
command, to apply a change). Every section is optional:

```toml
[general]
local_timezone = "America/New_York"   # for "today": completion dates, today+7 in filters.
                                      # Detected and written here on first start.
[lanes]
workspaces = 1                        # how many tasks a lane runs at once

[modules.records]                     # a module's own settings, handed only to that module
```

`[http]` sets the network gateway's rate limits and cache, for modules that reach the network
(none on master yet); [ADR 0027](docs/decisions/0027-real-http-gateway.md) lists its keys.

## Running it

You need Rust 1.89 or newer (installed with [rustup](https://rustup.rs)), a C compiler and git,
on Linux or macOS. Windows isn't supported yet. The repository pins the stable toolchain, so
rustup fetches the right version on the first build. On Debian or Ubuntu, `sudo apt install
build-essential curl git python3` and then rustup covers everything; see
[CONTRIBUTING.md](CONTRIBUTING.md#on-debian-or-ubuntu).

```sh
git clone https://github.com/Verifiedz/Shimmer.git
cd Shimmer
cargo build
cargo run -q -- ping          # starts the daemon if needed, then: "pong (daemon up 0s)"
```

`cargo run -q --` is the `shimmer` command, and it only works from inside the repository. To type
`shimmer` from any folder, run `.dev/install-dev-launcher.sh` once (see
[CONTRIBUTING.md](CONTRIBUTING.md#the-dev-launcher)). The examples below assume you have.

The daemon keeps running the code it started with. After pulling or changing the code, run
`shimmer shutdown`; the next command starts a daemon with the new code.

### Track records

```sh
shimmer records templates                    # ready-made trackers to start from
shimmer records new leetcode --from leetcode # create a LeetCode tracker from its template
shimmer records collections                  # list collections and their fields
shimmer records add leetcode two-sum --title "Two Sum" --difficulty easy
shimmer records list leetcode --status todo  # --FIELD VALUE filters on an exact value
shimmer records complete leetcode/two-sum    # mark done; stamps last_solved with today's date
shimmer records show leetcode/two-sum
shimmer records update leetcode/two-sum --url https://leetcode.com/problems/two-sum/
shimmer records remove leetcode/two-sum
```

You can write a record as `leetcode/two-sum` or `leetcode two-sum`. There is more:

- **Finding records:** `list` filters (`--filter "difficulty=hard"`, `--or`, `--has`,
  `--missing`, dates like `today+7`), searches (`--search`), sorts (`--sort -last_solved`) and
  pages (`--limit`, `--offset`).
- **Undo:** `remove` moves a record to the trash; `restore` brings it back, `list --removed` lists it, and
  `purge` deletes it for good (it asks first). `reopen` makes a done record todo again.
- **Moving data:** `import` from CSV or JSON, `export` to either.
- **Changing a collection:** `edit` opens its file; `check`, `rename-field`, `rename-collection`,
  `copy-collection`, `remove-collection` and `restore-collection`.
- **Templates:** `templates` lists them by category, `peek` shows everything about one.

`shimmer records --help` has every command and option.

### Add your own tracker

A new tracker is a TOML file in `$SHIMMER_HOME/data/records/collections/`. For example,
`jobs.toml`:

```toml
[collection]
id = "jobs"
label = "Job applications"
stamp_on_complete = "applied_on"   # `complete` sets this date field to today

[[field]]
name = "company"
type = "string"
required = true

[[field]]
name = "stage"
type = "enum"
values = ["saved", "applied", "interview", "offer"]

[[field]]
name = "applied_on"
type = "date"
```

It's usable straight away, with no restart:

```sh
shimmer records add jobs acme --company Acme --stage saved
shimmer records list jobs --stage saved
```

Every collection created from a template (`shimmer records new ID --from TEMPLATE`) lands in the
same folder as an ordinary, commented file, and is a good reference. You can edit those too.

### Workspaces, the queue and the scheduler

```sh
shimmer workspaces templates               # ready-made workspaces, by category
shimmer workspaces peek web-project        # what one does before you use it
shimmer workspaces new site --from web-project
shimmer workspaces list                    # every workspace and its state (ready, active, dirty)
shimmer workspaces activate site           # run its steps in order; add --wait to follow along
shimmer workspaces show site               # steps, last session, and why it's dirty if it is
shimmer workspaces stop site               # stop what it started
shimmer queue list                     # what is running and waiting, per lane
shimmer scheduler list                 # your schedules, when each next fires and its state (--all: modules' too)
shimmer scheduler add weekdays 09:00 --catch-up skip workspaces activate site   # the command as you'd type it
shimmer scheduler add mon,wed,fri 18:00 --catch-up skip records complete habits/gym
```

A workspace is a folder in `$SHIMMER_HOME/data/workspaces/<name>/` holding a `workspace.toml`
and a `steps/` folder of scripts. [`CLAUDE.md`](CLAUDE.md) §10 shows an example and ADR 0012
has every rule. `shimmer workspaces --help`, `shimmer queue --help` and
`shimmer scheduler --help` cover the rest.

### Notifications

```sh
shimmer notify test                        # check that messages reach you (a desktop notification)
shimmer notify send "Build finished" --body "v1.4 is live" --priority high
shimmer notify list                        # your messages, newest first; --unread, --from automations, --text …
shimmer notify show 0A3F6K                 # one in full (the ending list shows is enough)
shimmer notify quiet 22:00-08:00           # quiet hours: low and normal messages wait, then one summary
shimmer notify pause 2h                    # do not disturb for a while; shimmer notify resume to stop
shimmer notify remove --read               # clean up (asks first)
shimmer scheduler add weekdays 08:45 --catch-up skip notify send "Standup in 15"
```

Every message lands in an inbox first (`$SHIMMER_HOME/data/notify/inbox/`), then appears as a
desktop notification: `notify-send` on Linux (the `libnotify-bin` package), `osascript` on macOS.
Nothing is lost if it can't be shown. Normal and high priority pop up; on GNOME, low priority goes
straight to the notification list without popping up (on macOS every priority pops up the same). Shimmer also tells you by itself when a workspace couldn't
launch or a scheduled task failed: built-in rules (see Automations below). On a machine with no screen,
`[desktop] show = false` in `config.toml` logs notifications instead. ADR 0032 has the details;
`shimmer notify --help` lists every command.

### Automations

Rules: when something happens, run a command. A rule is a small file you can read and edit:

```toml
# data/automations/rules/solved.toml
[rule]
on = "records.item.completed"              # an event: shimmer automations events lists them
when = { collection = "leetcode" }         # conditions, as in records' filters
[[do]]
op = "notify.send"                         # any command
params = { title = "Solved: {title}" }     # {field} is filled from the event
[[do]]
op = "notify.send"
params = { title = "Time to redo: {title}" }
after = "60d"                              # wait first: it becomes a schedule
```

```sh
shimmer automations templates              # ready-made rules
shimmer automations new solved --from notify-on-complete   # asks a few questions, shows the rule, makes it
shimmer automations                        # your rules (and the built-in ones), on or off, last fired
shimmer automations test solved --event sample.json        # what it would do, without doing it
shimmer automations history                # what your rules did
shimmer automations disable workspace-dirty                # switch a built-in rule off
```

A rule never sets itself off forever: a chain of rules stops at 3 (`shimmer automations depth`),
and a rule never fires twice in one chain. Rules only run while the daemon runs. ADR 0035 has the
details.

**Try it: a job hunt.** With a jobs collection (`shimmer records new jobs --from job-applications`):

```sh
shimmer automations new new-posting --from notify-on-add --set COLLECTION=jobs
shimmer automations new follow-up --from follow-up-reminder --set COLLECTION=jobs --set DAYS=3
shimmer automations new prep --from stage-creates-workspace   # interviewing: its own prep workspace
shimmer automations new offer --from stage-message --set COLLECTION=jobs --set FIELD=stage --set VALUE=offer
```

A posting is added → you're told. You apply (`records complete`) → three days later, a reminder to
follow up. It reaches interviewing → a prep workspace is made for it and opened; it closes → that
workspace stops. An offer → you're told, loudly.

**Try it: LeetCode.** Practice every weekday morning, and redo what you solved:

```sh
shimmer scheduler add weekdays 09:00 --catch-up skip workspaces activate practice
shimmer automations new solved --from notify-on-complete --set COLLECTION=leetcode --set PRIORITY=low
shimmer automations new redo --from spaced-review --set COLLECTION=leetcode --set DAYS=60
shimmer automations new stuck --from field-threshold --set COLLECTION=leetcode --set FIELD=attempts --set OVER=3
```

The morning workspace is a schedule (rules react to events, not times). A problem solved → a quiet
"Solved"; 60 days later → "time to redo it"; more than three attempts → "this one's giving you trouble".

### Command packs

A command pack gives the commands themed names, each standing for `shimmer` and a whole command:

```sh
shimmer packs list                  # the built-in packs (anime-tropes, ship-it, starship, short) and yours
shimmer packs show short            # a pack's names and what each one runs
shimmer packs use short             # turn it on: then `wgo site` = shimmer workspaces activate site
shimmer packs use none              # turn packs off
```

The real names always work. [`docs/packs/README.md`](docs/packs/README.md) shows every built-in
pack and how to make your own.

### Other commands

```sh
shimmer manifest              # registered modules, their operations and lanes
shimmer call OP '{...}'       # send any operation with JSON params, e.g. shimmer call queue.list
shimmer shutdown              # stop the daemon
shimmer startup on            # start the daemon every time you log in, so schedules run after a restart
shimmer startup status        # whether it's on; 'shimmer startup off' turns it off
shimmer daemon                # run the daemon in the foreground with its logs (stop any running one first)
shimmer --help                # every command
```

Add `--json` to any command to see the daemon's raw reply, and `--socket PATH` to talk to a
daemon on a socket other than the default.

### Tab completion

Tab can complete commands, options, and your own workspaces, collections, records and
templates (and the active pack's aliases), the way it does for `git` or `cargo`:

```sh
shimmer wor<Tab>                          # shimmer workspaces
shimmer workspaces activate <Tab>         # your workspaces
shimmer records complete <Tab>            # lc/   then <Tab> again: lc/two-sum
shimmer records list lc --difficulty <Tab>   # easy  medium  hard
```

Set it up once, by adding one line to your shell's startup file:

| Shell | Add this line | To |
|---|---|---|
| bash | `source <(shimmer completions bash)` | `~/.bashrc` |
| zsh | `source <(shimmer completions zsh)` | `~/.zshrc`, after `compinit` |
| fish | `shimmer completions fish \| source` | `~/.config/fish/config.fish` |

Then open a new terminal and try `shimmer wor<Tab>`. In bash, press Tab twice to see the list
when more than one thing matches.

- **zsh needs `compinit`.** Most setups (oh-my-zsh, prezto, many distributions' `~/.zshrc`)
  already run it. If yours doesn't, put `autoload -Uz compinit && compinit` above the line;
  without it, loading says `shimmer: load completion after compinit`.
- **bash** needs nothing else (not even the `bash-completion` package; macOS's bash 3.2 works).
- **With the dev launcher,** that line would run `cargo` each time a terminal opens, rebuilding
  first if the code changed. Save the script once instead, and load the file:
  ```sh
  shimmer completions bash > ~/.shimmer-completion.bash     # or zsh / fish
  echo 'source ~/.shimmer-completion.bash' >> ~/.bashrc
  ```
  It calls the built binary directly, so Tab stays instant. Save it again after switching
  pack (it lists the pack's aliases) and, in zsh, after commands are added (its colours list
  them).
- **After `shimmer packs use NAME`,** open a new terminal so the pack's aliases complete.
- **Without a daemon running,** completion doesn't start one: you still get commands and
  options, and your own names once the daemon is up (any `shimmer` command starts it).

What each shell shows:

| | bash | zsh | fish |
|---|---|---|---|
| The suggestions | ✓ | ✓ | ✓ |
| What each one is, next to it | – | ✓ | ✓ |
| Grouped and coloured | – | ✓ (Shimmer's colours) | your fish theme |

In zsh, the suggestions are grouped and coloured in Shimmer's palette; a `[colors]` section changes
them, and `NO_COLOR=1` turns them off. That's read when your shell starts, so set it in your
environment; `shimmer --no-color completions zsh` prints the same script. bash can't show
descriptions.

### Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup, the checks to run before pushing, and the
branch workflow. [`CLAUDE.md`](CLAUDE.md) is the source of truth for the design, and
[`docs/decisions/`](docs/decisions/) holds one numbered file per design decision.
