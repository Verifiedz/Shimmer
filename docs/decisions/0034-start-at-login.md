# 0034. Start the daemon at login: `shimmer startup on`

Status: proposed · Raised by Dev B (#221) · Needs sign-off: Dev A (§5's exit status is daemon /
`crates/app` code; the unit files run the daemon Dev A owns; CLAUDE.md §2 gains a line) · Dev C notified ·
No `core` or `proto` change

## Context

The daemon starts only when a `shimmer` command runs (CLAUDE.md §2: the CLI auto-starts it, like
`tmux`). After a reboot or a logout, nothing runs until someone types a command, so:

- scheduler triggers don't fire at their time; `catch_up` only covers firings missed *while the
  daemon was running but the machine slept*, not "no daemon at all";
- notify (ADR 0032), scheduled fetches and, later, automations (#228) do nothing;
- a workspace scheduled for 09:00 doesn't launch.

The MVP chain (§1.3) only works while someone is already using Shimmer. This ADR adds one opt-in
command that asks the operating system to start the daemon at login.

What already holds, and this ADR relies on:

- **One daemon per Shimmer folder.** `lock_home` takes an exclusive lock on
  `$SHIMMER_HOME/.daemon.lock`, released by the OS when the process dies, crash or not
  (`crates/daemon/src/lib.rs`). A second daemon gets `conflict: another daemon already owns …`.
- **Two commands racing to auto-start** already resolve: one wins the lock, the other's daemon
  exits, and the command connects to the winner (`crates/cli/src/autostart.rs`).
- **A stale socket** left by a daemon that died is cleared on start, after checking nobody answers
  on it (`bind`, same file).
- **The daemon logs to `$SHIMMER_HOME/logs/`** itself (`init_daemon_logging`,
  `crates/app/src/main.rs`), so a service manager's own log isn't needed.
- **Writing outside the Shimmer folder safely** has a precedent: packs' bare-command links
  (`crates/cli/src/packs/link.rs`, ADR 0013 §10) never overwrite a file, only remove what they
  made, and record what they made in the CLI's own `cli.toml` (`settings.rs`, `linked`).

## Decision

### 1. The command

```
shimmer startup on       start the daemon at every login, from now on
shimmer startup off      stop doing that (removes only what `on` wrote)
shimmer startup status   whether it's on, how, and whether the running daemon is the one it starts
```

- **Off by default.** Nothing is installed without being asked.
- **Per user, no `sudo`,** nothing system-wide.
- A CLI command (Dev B). It writes a small service file and calls the OS's service manager; the CLI
  may run programs (it already runs your editor and auto-starts the daemon). Modules never do
  (§12 rule 4).
- `on` again is safe: it rewrites its own file (e.g. after a reinstall moved the binary).

### 2. Linux: a systemd user unit

`~/.config/systemd/user/shimmer.service`:

```ini
# Written by `shimmer startup on`. Remove with `shimmer startup off`.
[Unit]
Description=Shimmer daemon
# Only for a desktop session (see §4):
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=/home/me/.cargo/bin/shimmer daemon
# Always: the folder and socket your commands use, resolved when `startup on` ran:
Environment=SHIMMER_HOME=/home/me/.local/share/shimmer
Environment=SHIMMER_SOCKET=/run/user/1000/shimmer/daemon.sock
Restart=on-failure
RestartSec=2
SuccessExitStatus=3
RestartPreventExitStatus=3

[Install]
WantedBy=graphical-session.target    # or default.target, see §4
```

Then `systemctl --user daemon-reload` and `systemctl --user enable --now shimmer.service`.

- `ExecStart` is the **full, resolved path** of the binary that ran `startup on`, so a second
  install elsewhere never starts the wrong one.
- `SHIMMER_HOME` and `SHIMMER_SOCKET` are **always written, as your commands resolve them**
  (`crates/proto/src/paths.rs`) when `startup on` runs. A user unit runs with systemd's user
  manager's environment, not your shell's: a `XDG_DATA_HOME` or `XDG_RUNTIME_DIR` exported in a
  shell rc isn't there, and leaving the unit to its own defaults would put the login daemon on
  another folder, two daemons with the schedules you use never firing after a restart. Moving the
  folder later means `startup on` again.
- `RestartSec=2` rather than systemd's 100 ms, so a slow start doesn't use up its five tries in
  half a second.
- `Restart=on-failure`, not `always`: a deliberate `shimmer shutdown` stays respected.
- Exit status 3 is "already running" (§5): success, never retried.

### 3. macOS: a LaunchAgent

`~/Library/LaunchAgents/io.github.verifiedz.shimmer.plist`: `ProgramArguments` = the resolved
binary + `daemon`; `RunAtLoad` = true; `KeepAlive` = `{ SuccessfulExit = false }` (restart only after
a failure); `EnvironmentVariables` with `SHIMMER_HOME` / `SHIMMER_SOCKET`, resolved, as in §2.
Loaded with `launchctl bootstrap gui/$UID <plist>` when no daemon is running (otherwise at the next
login). `off` removes the file and, when no daemon is running, unloads it with `launchctl bootout`;
with one running, `bootout` would stop it (§6), so the agent stays loaded until logout, and `off`
says that until then macOS still restarts the daemon if it crashes.

A LaunchAgent runs in the user's GUI session, so workspaces can open windows and notify can show
popups with no extra step. launchd can't name exit codes, so §5's "already running" exits 0 when
the daemon was started by launchd (see §5).

### 4. Which login, chosen automatically (Linux)

A daemon started by the CLI inherits the terminal's environment (`WAYLAND_DISPLAY`, `DISPLAY`,
`DBUS_SESSION_BUS_ADDRESS`). That's why workspace steps can open windows and notify can reach the
desktop today. A daemon started by systemd only has what the systemd user manager has. So
`startup on` looks at where it's run and picks:

| `startup on` runs… | Detected by | The unit |
|---|---|---|
| **on a machine with no desktop** (a server, over SSH) | no `WAYLAND_DISPLAY` / `DISPLAY` | `WantedBy=default.target`: starts at **any** login. Such a machine has no window-opening workspaces anyway; script-only ones (update-everything, health) still work. |
| **in a desktop whose session reaches `graphical-session.target`** (GNOME, KDE, Hyprland under a session manager like uwsm) | a display variable set, and `systemctl --user is-active graphical-session.target` | `WantedBy=` / `PartOf=` / `After=graphical-session.target`: starts **with the desktop**, after it has exported its environment. |
| **in a desktop that never activates that target** (e.g. Hyprland started directly) | a display variable set, target inactive | `WantedBy=default.target`, and `startup on` prints the **one line to add to the compositor's startup**, e.g. for Hyprland: `exec-once = systemctl --user import-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP && systemctl --user try-restart shimmer.service`. Restarting there is harmless: at that moment the daemon has only just started. |

`startup status` says which case it chose. If the running daemon has no display while the machine
has one, `status` (and doctor, #216) warns: "workspaces can't open windows: add this line to your
compositor's startup".

### 5. Never two daemons, and no restart loops

There is never more than one daemon per Shimmer folder: that's the existing lock. Every way two
could meet, and what happens:

| Situation | Result |
|---|---|
| The login daemon is running; the person types a command | The command finds it on the socket. Nothing new starts. |
| At login, a **CLI-started** daemon already runs (after a `shimmer shutdown` and any later command) | The login daemon hits the lock and exits with **"already running"**. The unit treats that as success, so it isn't retried. |
| Two commands at the same moment, no daemon running | One wins the lock; the other connects to it (today's behaviour). |
| `shimmer daemon` run by hand while one runs | Refused with the conflict message, as today. |
| `startup on` while a CLI-started daemon runs | The service manager's start ends as "already running". `startup on` **never stops the running daemon** (its running tasks would be cancelled); `status` says "on: this daemon was started by a command; from your next login, startup runs it". |
| A different `SHIMMER_HOME` (tests, a sandbox) | A separate daemon on purpose, with its own lock. Not a clash. |

**The one daemon change (Dev A):** when `shimmer daemon` finds the folder locked by another daemon
**that answers on the socket**, it exits with a distinct status, **3**, and the message "already
running", instead of the generic failure. Under launchd, which can't name exit codes, it exits
**0** instead: launchd sets `XPC_SERVICE_NAME` to the job's label, and only our own label
(`io.github.verifiedz.shimmer`) counts, since terminals set the variable too (Terminal.app to `0`,
iTerm2 and VS Code to their own).

A daemon takes the lock **before** it answers: it recovers transactions, opens the index and starts
modules first. So one that's still starting up isn't taken for dead: on a lock conflict, `shimmer
daemon` waits up to **5 seconds** for the socket to answer, and exits 3 as soon as it does. A lock
whose holder never answers in that time is still a real failure, and is retried (§2's
`RestartSec`).
Without this, `Restart=on-failure` / `KeepAlive` would retry the login daemon every few seconds
while a CLI-started one runs.

### 6. Safety rules (as packs' links)

- **Never overwrite a file Shimmer didn't write.** If `shimmer.service` or the plist exists and
  doesn't start with the `Written by shimmer startup on` header, `on` refuses and says where it is.
- **`off` removes only its own file,** recorded in `cli.toml` (`startup = { file = "…", target =
  "…" }`), then disables and stops nothing else. It does **not** stop a running daemon; that's
  `shimmer shutdown`.
- **`status` warns when the unit is stale:** its binary no longer exists, or isn't the `shimmer`
  on `PATH` (after a reinstall elsewhere). `on` again fixes it. With #194's build id, it can also
  say the running daemon is an older build.
- **Tests never touch a real service manager:** the calls to `systemctl` / `launchctl` sit behind
  one small function, and the file contents are pure functions, tested on their own.

### 7. A one-time nudge for things that run while you're away

The first time someone creates **anything that only works while the daemon runs**, the CLI says,
once:

```
note: schedules only run while Shimmer's daemon is running.
      `shimmer startup on` starts it every time you log in.
```

- Today that's a schedule (`shimmer scheduler add`). Later: automations (#228),
  scheduled fetch sources, reminders (a record with a deadline qualifies through the reminder
  feature, not through records itself).
- **The test is "does it need to happen while you're not typing?"** Immediate commands
  (`records add`, `workspaces activate`…) never nudge: the CLI auto-starts the daemon and they're
  done.
- One shared CLI check that each such command calls; "already told" is kept in `cli.toml`, so it's
  never repeated. Skipped when startup is already on, and never printed with `--json` or without a
  terminal (a script never sees it).

### 8. CLAUDE.md

§2 gains: "The CLI auto-starts the daemon if the socket is dead. `shimmer startup on` also has the
operating system start it at every login (ADR 0034); it's off by default."

## Consequences

- Schedules, reminders and (later) automations fire after a reboot, with no account and no
  network.
- The CLI writes one file outside `$SHIMMER_HOME` per platform, under the same rules as packs'
  links: never overwrite, only remove its own, recorded in `cli.toml`.
- The daemon gains one exit status (§5). Nothing in `core`, `proto` or the protocol changes.
- Built after sign-off as one CLI PR (`crates/cli/src/startup.rs`, the nudge helper, tests) plus a
  small Dev A PR for §5.

## Not done here

- **Windows** (#114, #164): a Task Scheduler entry or a Startup-folder shortcut, once the daemon
  runs there.
- **Starting at boot without anyone logging in** (`loginctl enable-linger` on Linux, a
  LaunchDaemon on macOS): needs more privilege and a different threat model; not for the MVP.
- **Several Shimmer folders, each started at login:** one unit, for the folder `startup on` was run
  with.
- **Restoring queued tasks after a crash:** the queue is in memory; a separate decision (the queue
  persistence issue).

## Alternatives considered

- **`shimmer daemon install`:** `shimmer daemon` already means "run it here, now", and is what the
  unit itself runs; giving it subcommands mixes two meanings.
- **`Restart=always` / `KeepAlive = true`:** would undo a deliberate `shimmer shutdown`.
- **Always `graphical-session.target`:** many setups never activate it (plain Hyprland, every
  headless machine), so the daemon would never start there.
- **Always `default.target`:** on a desktop it starts before the compositor exports its
  environment, so workspaces couldn't open windows until the import line is added; §4 asks for that
  line only where it's needed.
- **Socket activation** (systemd starts the daemon on the first connection): doesn't help
  schedules, which need the daemon running before anyone connects.
