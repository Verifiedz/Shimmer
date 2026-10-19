# 0039. One socket per Shimmer folder: which folder a daemon serves, short socket paths, and its working folder

Status: proposed · Raised by Dev B (#266, findings 1, 2, 3 and 6) · Needs sign-off: Dev A (`proto`'s
default socket path and its `welcome` frame, the daemon's handshake and start-up, `shimmer daemon`'s
exit 3 from ADR 0034 §5) · Dev C notified (an additive `welcome` field; no version bump) · No `core` change

## Context

A Shimmer folder (`$SHIMMER_HOME`) has exactly one daemon: the daemon locks `.daemon.lock` inside
the folder before anything else (CLAUDE.md §1.6 "one writer"). Two *different* folders are two
separate sets of data and may each have their own daemon.

But on Linux the default socket is `$XDG_RUNTIME_DIR/shimmer/daemon.sock` whatever the folder is
(`proto::paths::socket_path`). So with a daemon running for folder A (#266, confirmed):

- `SHIMMER_HOME=B shimmer records list` is answered by **A's** daemon, from A's data. Writes land in
  A. Nothing says so: the `welcome` frame doesn't name a folder.
- B can't start its own daemon: `a daemon is already listening on …/daemon.sock`.
- `shimmer daemon` for B exits 3, "already running", because *something* answers on the socket
  (ADR 0034 §5), so a login service for B reports success while B has no daemon at all (#266,
  finding 6).

Who has two folders: a developer with their real data and a test folder (`SHIMMER_HOME=~/play` to
try a template or a purge), a work/personal split, a scratch folder to try a rule. During
dogfooding that's all three of us, and the failure is silent: the real data changes.

macOS already keeps the socket inside the folder (`$SHIMMER_HOME/daemon.sock`), and two users get
two `$XDG_RUNTIME_DIR`s, so neither has this problem.

Two smaller start-up problems sit in the same code (#266):

- **A long folder path stops the daemon starting (finding 2).** A Unix socket path is limited to
  about 104 bytes on macOS and 108 on Linux. Where the socket is `$SHIMMER_HOME/daemon.sock` (macOS,
  and Linux without `XDG_RUNTIME_DIR`), a deep folder or a long username passes it. The daemon says
  `path must be shorter than SUN_LEN`; an auto-starting command only says `cannot connect`.
- **The daemon keeps the working folder of whatever command started it (finding 3).** Run your first
  command from a USB drive's folder and the drive can't be unmounted ("target is busy") while the
  daemon runs. Anything it starts without setting a folder inherits it too.

## Decision

### 1. The default socket is per folder

`proto::paths::socket_path()`, when `SHIMMER_SOCKET` isn't set:

- **Linux with `XDG_RUNTIME_DIR`:** `$XDG_RUNTIME_DIR/shimmer/<id>.sock`, where `<id>` is 16 hex
  digits of a 64-bit FNV-1a hash of the folder's absolute path. The path is resolved the same way
  every time: the longest part of it that exists is canonicalised (symlinks followed), and the rest
  is appended, so a folder that doesn't exist yet gets the same `<id>` before and after it's made.
  The hash is written out in `proto` (no new dependency) and **never changes** once shipped, since
  scripts and login units hold the path.
- **Otherwise** (macOS, Linux without `XDG_RUNTIME_DIR`): `$SHIMMER_HOME/daemon.sock`, as today.
- **`SHIMMER_SOCKET`** still wins, as today. Workspace scripts and tests keep using it.

The runtime folder `…/shimmer/` stays `0700`; the socket stays `0600`.

### 2. The `welcome` frame names the folder

The daemon adds `"home"`: the absolute, resolved path of the folder it serves.

```jsonc
{"v": 2, "kind": "welcome", "daemon_version": "0.3.0", "session": "01JD2K…", "home": "/home/me/.local/share/shimmer"}
```

It's optional on the wire (`home` absent from an older daemon or from `shimmer mockd`, which serves
fixtures, not a folder). Clients already ignore unknown fields, so **this is additive: no version
bump.**

### 3. The CLI refuses a daemon serving another folder

After the handshake, if `welcome.home` is present and isn't the folder the CLI resolved (resolved
the same way as §1), the command fails with `unavailable`:

> this daemon serves /home/me/.local/share/shimmer, not /home/me/play; set SHIMMER_SOCKET to that
> folder's daemon, or unset it

That catches a `SHIMMER_SOCKET` left pointing at the wrong daemon, and the old shared path below.

### 4. "Already running" means this folder's daemon

`shimmer daemon` exits 3 (ADR 0034 §5) only if the daemon answering on the socket names **this**
folder in its `welcome`. One serving another folder is a real conflict: exit 1 with the message from
§3, so a login service retries and the problem shows.

### 5. A socket path that would be too long gets a short one

When the default from §1 is longer than the platform allows (104 bytes on macOS, 108 on Linux, with
room for the terminating byte), the default is instead `/tmp/shimmer-<uid>/<id>.sock`, with the same
`<id>` as §1. The folder `/tmp/shimmer-<uid>/` is created `0700`, and the daemon refuses to use it
if it exists but isn't a directory owned by the user, or is readable by anyone else (someone else
could have made it first in a shared `/tmp`).

An explicit `SHIMMER_SOCKET` that's too long is never shortened (scripts rely on it); the command
and the daemon both say exactly that: "SHIMMER_SOCKET is 131 bytes; a socket path can be at most
103 here".

### 6. The daemon runs in `/`

The daemon changes its working folder to `/` as it starts, and auto-start spawns it with
`current_dir("/")` too, so it never holds onto the folder a command happened to run in.
Workspace steps get their own folder separately (#269, finding 2).

### 7. Startup installs from before this

`shimmer startup on` writes the resolved `SHIMMER_SOCKET` into the unit (ADR 0034 §2), so a unit
written before this change still pins the old shared path, while commands now look at the new one.
`shimmer startup status` warns when the unit's `SHIMMER_SOCKET` isn't the current default for its
folder: "the login service uses an old socket path; run 'shimmer startup on' again". Running it
rewrites the unit. Nobody outside the team has run `startup on` yet, so there's no wider migration.

## Not chosen

- **One socket per user, and the daemon serves many folders.** That's a multi-tenant daemon: every
  op would need to say which folder, and "one writer per folder" would move inside one process.
  Much bigger, for no gain over §1.
- **Put the socket in the folder on Linux too.** Simple, but `$SHIMMER_HOME` is meant to be synced
  with git and copied between machines; a socket in it is noise, and long folder paths hit the
  socket path limit sooner (#266, finding 2). `$XDG_RUNTIME_DIR` is where sockets belong on Linux.
- **The folder's name instead of a hash.** Two folders called `shimmer` in different places would
  collide, and names can be longer than a socket path allows.
- **Only §3 (refuse the wrong daemon), keep the shared path.** Safe, but folder B still can't run
  at the same time as A.

## Consequences

- Two folders on one machine each get their own daemon; a command always reaches its own folder's.
- A client pointed at the wrong daemon is refused instead of silently changing the wrong data.
- `shimmer daemon`'s exit 3 can no longer be fooled by another folder's daemon.
- `proto`: `socket_path` changes on Linux, `ServerFrame::Welcome` gains an optional `home`.
  protocol.md documents both; no version bump.
- A long folder path no longer stops the daemon starting; an explicit `SHIMMER_SOCKET` that's too
  long fails with a message that says so.
- The daemon never keeps a command's folder busy.
- Not covered here: the daemon's environment (`PATH`, #266 finding 4), start failures reaching the log
  (finding 5), and log size (finding 7).

## Tests

- Two folders, two daemons: each folder's commands reach their own daemon and data.
- A client whose `SHIMMER_SOCKET` points at another folder's daemon is refused with §3's message.
- `shimmer daemon` exits 3 only when the answering daemon serves the same folder; 1 otherwise.
- The default path: two folders get different sockets; the same folder (through a symlink, before
  and after it exists) gets the same one; `SHIMMER_SOCKET` still wins.
- A folder whose `daemon.sock` would be too long gets `/tmp/shimmer-<uid>/<id>.sock`, and the daemon
  starts and answers there; a `/tmp/shimmer-<uid>` owned by someone else is refused; a too-long
  `SHIMMER_SOCKET` gives the message from §5.
- A daemon auto-started from a temporary folder has `/` as its working folder.
- `startup status` warns about a unit pinning a different socket.
