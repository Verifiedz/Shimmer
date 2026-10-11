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
inside the daemon.
The full design, including the mechanisms that aren't built yet, is in
[`CLAUDE.md`](CLAUDE.md) §1.1.

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
  config.toml                                Settings
  data/records/collections/<collection>.toml One file per collection definition
  data/records/items/<collection>/<id>.toml  One file per record
  events/YYYY-MM-DD.jsonl                    Append-only log of every event
  index.sqlite                               Derived from the files; safe to delete
  logs/daemon.log.<date>                     Daemon logs, rotated daily at UTC midnight; oldest
                                              deleted past 8 files. A leftover undated
                                              logs/daemon.log predates this and is safe to delete.
```

`$SHIMMER_HOME` defaults to `~/.local/share/shimmer` on Linux and `~/Library/Application Support/shimmer` on
macOS. Set the `SHIMMER_HOME` environment variable to use another folder. The SQLite index exists
only to make queries fast: delete it and the daemon rebuilds it from the event log on its next
start. Writes are atomic, so a crash leaves either the old file or the new one, never half of
one. The folder ships with a `.gitignore` for the derived and machine-local parts, so you can
commit it as-is.

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

### Track records

```sh
shimmer records templates                    # ready-made trackers to start from
shimmer records new leetcode --from leetcode # create a LeetCode tracker from its template
shimmer records collections                  # list collections and their fields
shimmer records add leetcode two-sum --title "Two Sum" --difficulty easy
shimmer records list leetcode --status todo  # --FIELD VALUE filters on an exact value
shimmer records complete leetcode/two-sum    # mark done; stamps last_solved with today's date
shimmer records get leetcode/two-sum
shimmer records update leetcode/two-sum --url https://leetcode.com/problems/two-sum/
shimmer records remove leetcode/two-sum
```

`list` also takes `--limit N` and `--offset N`, and `update` takes `--unset FIELD`. You can
write a record as `leetcode/two-sum` or `leetcode two-sum`. Run `shimmer records --help` for the
details.

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
shimmer workspaces status site             # steps, last session, and why it's dirty if it is
shimmer workspaces stop site               # stop what it started
shimmer queue list                     # what is running and waiting, per lane
shimmer scheduler list                 # every trigger, when it next fires and its state
shimmer scheduler add records.list '{"collection":"leetcode"}' --every 1d --catch-up skip
```

A workspace is a folder in `$SHIMMER_HOME/data/workspaces/<name>/` holding a `workspace.toml`
and a `steps/` folder of scripts. [`CLAUDE.md`](CLAUDE.md) §10 shows an example and ADR 0012
has every rule. `shimmer workspaces --help`, `shimmer queue --help` and
`shimmer scheduler --help` cover the rest.

### Other commands

```sh
shimmer manifest              # registered modules, their operations and lanes
shimmer call OP '{...}'       # send any operation with JSON params, e.g. shimmer call queue.list
shimmer shutdown              # stop the daemon
shimmer daemon                # run the daemon in the foreground with its logs (stop any running one first)
shimmer --help                # every command
```

Add `--json` to any command to see the daemon's raw reply, and `--socket PATH` to talk to a
daemon on a socket other than the default.

### Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup, the checks to run before pushing, and the
branch workflow. [`CLAUDE.md`](CLAUDE.md) is the source of truth for the design, and
[`docs/decisions/`](docs/decisions/) holds one numbered file per design decision.
