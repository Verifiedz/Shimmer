# 0031. Scheduling in plain words: any command with --at/--every, in local time

Status: proposed · Raised by Dev B (issue #77) · Needs sign-off: Dev A (§2 changes `Schedule` in
`crates/core` and the scheduler; §4 adds id prefixes to `queue.*` and `scheduler.*`; CLAUDE.md §4)
· Dev C notified (additive protocol changes, a new `schedule` shape) · Changes `core`

## Context

Scheduling today asks for a daemon op and raw JSON, in UTC:

    shimmer scheduler add workspaces.activate '{"id":"site"}' --cron "0 13 * * 1-5" --catch-up skip

People already know the command they want to run (`shimmer workspaces activate site`), and think
in their own time ("9am on weekdays"). The first thing a teammate tried was a plain command after
`--`, which failed (#77). Cron in UTC also means a 9am schedule moves to 8am or 10am when the
clocks change, and has to be rewritten twice a year.

Three more rough edges: `scheduler list` shows op + JSON rather than what a schedule does; ids are
26 characters; and `--catch-up` (required, CLAUDE.md §6.3) is jargon.

## Decision

### 1. Schedule any command by adding when to it (CLI)

Any schedulable command takes scheduling options, and is then scheduled instead of run:

    shimmer workspaces activate site --at 09:00 --on weekdays --catch-up skip
    shimmer records list leetcode --status todo --every day --catch-up run-once
    shimmer workspaces stop site --once "2026-10-20 18:00" --catch-up skip

| Option | Meaning |
|---|---|
| `--at HH:MM`, with `--on DAYS` | local time; `DAYS` is `daily` (the default when `--on` is left out), `weekdays`, `weekends`, or a list like `mon,wed,fri` |
| `--every DURATION` | `30m`, `2h`, `day`, `1w`, as `scheduler add --every` takes today |
| `--once TIME` | `2026-10-20 18:00` in local time, or RFC 3339 |
| `--cron "EXPR"` / `--cron-local "EXPR"` | five-field cron in UTC / in local time |
| `--catch-up skip\|run-once\|backfill` | required (§6.3); see §5 |
| `--lane LANE` | rarely needed, as today |

- **How it's recognised:** any of `--at`, `--every`, `--once`, `--cron`, `--cron-local` means
  "schedule". `--on`, `--catch-up` or `--lane` without one of them is an error. These names are
  reserved on every command; none uses them today.
- **How it becomes a trigger:** the CLI parses the command exactly as it does to run it, and takes
  the single op + params that run would send (`records::request`; workspaces gets the same
  function). Then it sends today's `scheduler.add`, and the daemon stores op + params as it does
  now. Nothing new is computed in the client: this is the parsing the CLI already does.
- **Pack aliases** are resolved to canonical words first, so `ikuzo site --at 09:00` stores
  `workspaces.activate` (CLAUDE.md §12 rule 16), and switching packs never breaks a schedule.
- **What can be scheduled:** a command whose run is exactly one module op (records and workspaces
  today; fetchers and notify as they arrive). Not schedulable, with an error saying why: commands
  that are several calls or client-side work (`records edit`, a rename that moves schedules,
  `workspaces new`), and anything that isn't a module op (`scheduler`, `queue`, `packs`, `ping`,
  `shutdown`). The daemon already refuses non-module ops (`unknown_op`), so a schedule can never
  create, cancel or remove schedules or tasks.
- Options that only make sense when running now (`--wait`, `--yes`) are an error when scheduling.
- On success it says what was scheduled, in the same words `scheduler list` uses (§3), e.g.
  `✓ scheduled workspaces activate site · weekdays 09:00 (America/New_York) · first run Mon 09:00
  · usr-01m4k…`.

`shimmer scheduler add OP [PARAMS] WHEN …` stays, for ops with no friendly command yet and for
scripts that already use it. `scheduler list|show|pause|resume|remove` are unchanged.

### 2. Local-time schedules (daemon, core)

`Schedule` gains a local-time cron:

    Cron(String)        UTC, as today: existing triggers keep their meaning
    CronLocal(String)   evaluated in config.toml's [general] local_timezone (ADR 0009)

The scheduler computes each next run in that zone, so `0 9 * * 1-5` stays 9am local across a clock
change. A time that doesn't exist (skipped when the clocks go forward) runs at the next valid
minute; one that happens twice (when they go back) runs once, at the first. `next_due` stays
stored in UTC, as today. On the wire: `{"cron_local": "0 9 * * 1-5"}`. If `local_timezone` changes,
local triggers follow it from their next run.

`--at`/`--on` and `--cron-local` produce `CronLocal`; `--cron` stays `Cron`; `--every` and `--once`
are unchanged (`--once` is converted to UTC by the CLI, as now).

### 3. Lists show what a schedule does (CLI)

`scheduler list` and `show` show each trigger as the command it runs, and its schedule in words:

    usr-01m4k…  workspaces activate site   weekdays 09:00 (local)   next Mon 09:00   active

The command is rebuilt from op + params with the op → command words table packs already has
(`TARGETS`, ADR 0029), and the params as positional arguments or `--field value`. An op with no
command shows as op + JSON, as today. `--json` is unchanged.

### 4. Short ids (daemon)

Every op that takes a `task_id` or `trigger_id` also accepts a unique prefix of one: `queue show
01M4K`, `scheduler pause usr-01m4`. The daemon resolves it: a single match is used, several are
`conflict` listing them, none is `not_found`. Resolving in the daemon gives every client the same
behaviour, without each one fetching and searching full lists (#167).

### 5. `--catch-up`, in plain words (CLI)

It stays required: a guessed default is how duplicate runs happen (§6.3). At a terminal, leaving it
out asks instead of failing:

    If your computer is off or asleep at 09:00:
      1) skip that time
      2) run it once when it wakes
      3) run every time it missed
    >

Without a terminal (scripts, a workspace step) it is still an error naming the flag.

## Consequences

- People schedule the commands they already use, in their own time, and read schedules back the
  same way. The UTC trap and the twice-yearly rewrite go away.
- The daemon gains one `Schedule` variant and id-prefix resolution; the CLI gains one shared step
  that recognises the scheduling options, plus the workspaces equivalent of `records::request`. No
  new logic moves into the client: the op + params are what running the command already sends.
- Six option names are reserved on every command: `--at`, `--on`, `--every`, `--once`, `--cron`,
  `--cron-local` (with `--catch-up` and `--lane` only meaningful next to them).
- Protocol: `{"cron_local": …}` as a schedule, and prefixes for ids, both additive; protocol.md
  updated and Dev C told when each lands.
- Built in small PRs after sign-off: the core and scheduler change (§2), id prefixes (§4), then the
  CLI (§1, §3, §5).

## Not done here

- A stop time on a workspace schedule (`--until 18:00`, adding the matching `stop`).
- Scheduled launches reporting back (notifications): with `notify`, M6.
- A `--` form: not needed once commands take their own scheduling options.
