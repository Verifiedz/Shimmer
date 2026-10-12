# Contributing to Shimmer

This page gets you from a fresh clone to passing tests and a working `shimmer ping`. For the design
itself, `CLAUDE.md` is the source of truth: if this page and `CLAUDE.md` disagree, `CLAUDE.md`
wins.

## Prerequisites

- **Rust 1.89 or newer, installed with [rustup](https://rustup.rs).** `rust-toolchain.toml` pins
  the stable channel with `rustfmt` and `clippy`, so rustup fetches the right toolchain the first
  time you build. 1.89 is the minimum (`rust-version` in `Cargo.toml`); any current stable is fine.
- **A C compiler.** SQLite is compiled into the binary (`rusqlite`'s `bundled` feature), and the
  TLS stack (`rustls` with `ring`) builds a little C too. No OpenSSL or system SQLite is needed.
- **git.**
- **Linux or macOS.** Windows isn't supported yet: the daemon talks over a Unix socket, the
  workspace launcher is Unix-only, and the Windows versions of both are deferred
  (`docs/protocol.md`, ADR 0010 §7).
- **python3**, for the dependency-rule check below. Any recent version, no packages needed.

### On Debian (or Ubuntu)

Most of us develop natively on Debian. On a fresh install:

```sh
sudo apt install build-essential curl git python3
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # then open a new terminal
rustc --version                                                  # 1.89 or newer
```

Use rustup rather than Debian's `rustc`/`cargo` packages: those are often older than 1.89, and
they don't read `rust-toolchain.toml`. If `apt` installed them earlier, remove them
(`sudo apt remove rustc cargo`) so the rustup versions in `~/.cargo/bin` are the ones on your
`PATH`.

On macOS, `xcode-select --install` provides the compiler and git.

## Setup

```sh
git clone https://github.com/Verifiedz/Shimmer.git
cd Shimmer
cargo build
cargo test --workspace
cargo run -q -- ping        # "pong (daemon up 0s)": the daemon started and answered
cargo run -q -- shutdown
```

`cargo run -q --` is the `shimmer` command. The first command you run starts the daemon in the
background; you never start it by hand. Your data goes to `~/.local/share/shimmer` (Linux) or
`~/Library/Application Support/shimmer` (macOS). To experiment without touching it, point `SHIMMER_HOME`
at a scratch folder: `SHIMMER_HOME=/tmp/shimmer-play cargo run -q -- ping`.

### The dev launcher

Typing `cargo run -q --` gets old, and it only works from inside the repo. Install a launcher:

```sh
.dev/install-dev-launcher.sh
```

It writes a small `shimmer` script into `~/.cargo/bin` (already on your `PATH` if you installed Rust
with rustup) that runs `cargo run -q --manifest-path <your clone>/Cargo.toml -p shimmer -- "$@"`.
So `shimmer` works from any folder and always runs your latest code, rebuilding first when something
changed. It points at the clone you ran the script from, wherever that is.

It refuses to replace an existing `~/.cargo/bin/shimmer`. If that file is an older launcher (say, from
a clone you moved), re-run with `--force` to replace it.

One thing to know: the daemon keeps running the binary it was started from. After changing daemon
or module code, run `shimmer shutdown` and the next command starts a daemon with your new code.

## Before every push

CI (`.github/workflows/ci.yml`) runs these four checks on Linux and macOS. Run them locally
first; all four must pass.

```sh
python3 .dev/check-deps.py                              # dependency rules, CLAUDE.md §3
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo fmt --all` (without `--check`) fixes formatting for you.

`check-deps.py` enforces who may depend on whom. In short: modules never depend on each other
or on `store`/`daemon`, and clients (`cli`, `tui`, `mockd`) depend only on `core` and `proto`.
If it fails, the fix is almost always to emit an event instead of making a direct call.

## Who owns what

From `CLAUDE.md` §13:

| Dev | Owns |
|---|---|
| A | `crates/daemon` (registry, bus, queue, scheduler, gateway, IPC, indexer), `crates/store`, `crates/mockd`, `crates/app` |
| B | `crates/modules/*`, `crates/cli` |
| C | `crates/tui` |

Changes to a crate you don't own go through a PR its owner reviews. Two exceptions spelled out
in §13: Dev C may add fixture cases to `crates/mockd/fixtures`, and Dev B may add a
module-registration line in `crates/app` without Dev A's review.

**`crates/core` and `crates/proto` are shared and change-controlled** (§4). They are the
contract everyone builds against, so a change there needs an ADR in `docs/decisions/` plus
sign-off from both Dev A and Dev B. A protocol change also needs Dev C told and the protocol
version bumped. If your change seems to need a new type or field in `core`, don't copy the type
locally to get around this; write the ADR.

## Branch workflow

- **Larger work** gets a long-lived umbrella branch with a **draft PR into `master`**, opened
  early so everyone can see where it's heading. **Name it `integration/<name>`** (e.g.
  `integration/fetchers`; `post-<name>` works too).
- Build it in **small child branches**, each with its own PR **into the umbrella branch**. Small
  PRs are quick to review.
- When the feature is done, mark the draft PR ready. **`master` needs 1 approval** to merge.

The prefix is what protects the umbrella. GitHub's rulesets (`.github/rulesets/`) give
`integration/*` and `post-*` branches the same rules as `master`: every PR into them needs an
approval and green CI, and nothing can be pushed to them directly. An umbrella with any other name
has **no protection at all**, so PRs merged into it skip review and only get looked at as one big
PR at the end, which is exactly when problems are hardest to spot. Rename one with the wrong name
(GitHub retargets its open PRs automatically) rather than working around it.

Ordinary feature branches have no rules on purpose, so you can push to them freely.

A small, self-contained change (a doc fix, one bug) can be a single branch with a PR straight
into `master`. It still needs 1 approval.

### Merging a PR

The rulesets enforce most of this; knowing why saves a greyed-out merge button.

- **Bring it up to date first.** On `master`, GitHub requires it: click **Update branch**, then
  wait for both CI checks (`ubuntu-latest`, `macos-latest`) to go green. If **Update branch**
  shows a conflict, don't force anything; ask the author of the other change.
- **Approvals cover the reviewed code only.** Pushing a commit (including **Update branch**)
  dismisses earlier approvals, and the last person to push can't approve their own push. Ask for
  a quick re-approval; it's a formality when only `master` came in.
- **Resolve every review conversation** before merging (also enforced).
- **Merge commits only**: no squash or rebase. Stacked PRs depend on it: squashing a base PR
  rewrites the commits the next PR is built on.
- **Stacked PRs** (a PR whose base is another PR's branch):
  - When the base PR merges, **delete its branch**. GitHub then moves the next PR onto the
    umbrella automatically. Never merge a PR whose base has already been merged: its commits land
    on a dead branch and never reach `master`.
  - **"No conflicts" doesn't mean "compiles together."** When the umbrella has moved since a PR
    was opened, merge the umbrella into the PR and let CI run again before merging it. Two
    changes can each pass alone and break each other.

Never commit straight to `master`.

## Writing an ADR

An ADR (architecture decision record) writes down a design choice that `CLAUDE.md` and
`docs/protocol.md` leave open, so it is decided once, in the open, rather than silently in code.
Write one when you change `core` or `proto`, change something `CLAUDE.md` specifies, or make a
design call the next person would otherwise have to guess at.

1. Copy an existing one in `docs/decisions/`. `0003-queue-ordering-and-lane-defaults.md` is a
   short example; `0008-records-collections-and-storage.md` a fuller one.
2. Take the next free number: if the highest is `0010-…`, yours is `0011-short-title.md`.
3. Keep the shape: a `# NNNN. Title` heading; a `Status:` line (start at `proposed`) saying who
   raised it and whose sign-off it needs; then `## Context` (the problem), `## Decision` (what
   we'll do and why), and optionally what is deliberately left out.
4. Open it in a PR, alone or with the code it describes, and ask the people the `Status:` line
   names to review. Update the status to `accepted` when they sign off.

Docs-only changes and changes inside a crate you own that follow existing rules don't need one.

## What a module can reach: `Ctx`

A module never builds its own file handles, HTTP clients or processes. The daemon hands it one
`Ctx` (`crates/core/src/ctx.rs`, `CLAUDE.md` §5), and **what is in `Ctx` is everything that
module is allowed to do.** This is dependency injection with a security purpose: because the
daemon builds each module's `Ctx`, it decides what each module can touch, and tests swap in
fakes for every piece.

| Field | What the module gets | The boundary |
|---|---|---|
| `store` | Reads and atomic writes, plus `transaction` for multi-file changes | Only its own `data/<namespace>/` folder; never a raw path |
| `http` | The rate-limited, cached HTTP gateway (ADR 0027) | Only if its manifest declares `"network"`; otherwise every call fails `unavailable` |
| `launcher` | Runs a workspace's numbered steps and its cleanup script (ADR 0010) | Only if its manifest declares `"process"`; takes a step and an id, never a path |
| `bus` | Emits events | Emit only; reactions arrive through `on_event` |
| `queue` | Enqueues tasks | Can't run them itself or skip a lane |
| `clock`, `local_tz` | The current time and the configured timezone | Injected, so tests use a fake clock |
| `cancel` | Cancellation for long tasks | Long tasks must poll it, and report how far along they are with `ctx.progress` |
| `config` | Its own section of `config.toml` | Only its own section; it can't see any other module's settings |
| `module_id` | Its own id | Fixed by the daemon: every event it emits is stamped with this id, and its topics must start with it |

`Ctx` also has two methods. `ctx.progress(fraction, note)` reports progress from inside a queued
task. `ctx.retry_with_backoff(policy, …)` is the one shared retry curve: retrying is the module's
decision, never the queue's (`CLAUDE.md` §11.2), but every module uses this rather than writing
its own.

So, in module code (`CLAUDE.md` §12 rule 4):

- no `std::fs`, use `ctx.store`;
- no `reqwest` or other HTTP client, use `ctx.http`;
- no `std::process::Command`, use `ctx.launcher`;
- no `Utc::now()`, use `ctx.clock`.

`check-deps.py` stops a module depending on `store` or `daemon` directly, so the only way to
reach those is through `Ctx`.

Adding a field to `Ctx` gives a new power to every module, so it's a security decision, not a
convenience. It changes `core`, so it needs an ADR and sign-off (§4). The same goes for a new
capability name. Some things can never go in `Ctx`: a raw data-folder path, a raw HTTP client, a
database connection, a handle to another module, or anything about user identity (§1.5, §5).

In tests, `crates/core/src/testing.rs` builds an in-memory `Ctx` with a fake clock, so a module
test never touches the real filesystem, network or system time.

## Where to start reading

1. **`CLAUDE.md`** §1–§3 for what we're building and how it's laid out. The rest as you need it.
2. **`crates/core/src/module.rs`**: the `Module` trait. Every feature is a module implementing it.
3. **`crates/core/src/ctx.rs`**: `Ctx`, the only thing a module receives. What's in it is
   everything a module is allowed to do.
4. **`crates/core/src/testing.rs`**: the in-memory `Ctx` used in module tests. Every module is
   tested against it, not a real filesystem.
5. **`crates/modules/records`**: a complete module using all three, and its tests.
6. **`docs/protocol.md`**: how clients talk to the daemon.

## Trying the mock daemon

`shimmer mockd` serves the real protocol from canned responses and a scripted event timeline, so you
can build or test a client without a real daemon. It's the main tool for front-end work: the TUI
(`crates/tui`, Dev C) is built against `mockd` and `docs/protocol.md` alone, so it never waits on
daemon changes. `mockd` is a server built like a client: it depends only on `core` and `proto`.
Start it with:

```sh
shimmer mockd --fixtures crates/mockd/fixtures --socket /tmp/shimmer-mock.sock
```

In a second terminal, point any command at it with `--socket`:

```sh
shimmer --socket /tmp/shimmer-mock.sock ping
shimmer --socket /tmp/shimmer-mock.sock manifest
```

Both flags are required, so the mock can never take over the real daemon's socket, and a command
given `--socket` never auto-starts a daemon. Fixtures cover the paths that are hard to reach on a
real daemon: a `confirmation_required` round-trip, a `workspace_dirty` failure, a lagged event
stream and a long task with progress. The fixture format is in `crates/mockd/fixtures/README.md`;
add a case by adding a file there. If your client needs a response the fixtures don't have yet, add
it there in a PR that Dev A, who owns the crate, reviews (`CLAUDE.md` §13). Don't hardcode
the response in the client.

Without the dev launcher, replace `shimmer` with `cargo run -q --`.
