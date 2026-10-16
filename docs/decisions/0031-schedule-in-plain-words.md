# 0031. Scheduling in plain words: `shimmer schedule WHEN COMMAND`, in local time

Status: proposed · Raised by Dev B (issue #77) · Needs sign-off: Dev A (§2 changes `Schedule` in
`crates/core` and the scheduler; §4 adds short ids to `queue.*` and `scheduler.*`; CLAUDE.md §4)
· Dev C notified (a new `schedule` shape that older clients must tolerate, §2; short ids, §4) ·
Changes `core`

## Context

Scheduling today asks for a daemon op and raw JSON, in UTC:

    shimmer scheduler add workspaces.activate '{"id":"site"}' --cron "0 13 * * 1-5" --catch-up skip

People already know the command they want to run (`shimmer workspaces activate site`), and think in
their own time ("9am on weekdays"). The first thing a teammate tried was a plain command after
`--`, which failed (#77). Cron in UTC also means a 9am schedule moves to 8am or 10am when the
clocks change, and has to be rewritten twice a year.

Three more rough edges: `scheduler list` shows op + JSON rather than what a schedule does; ids are
26 characters; and `--catch-up` (required, CLAUDE.md §6.3) is jargon.

A first draft put scheduling options on every command (`workspaces activate site --at 09:00`).
Review (#175) showed why that fails: `records add/update/list` take `--FIELD VALUE` for any field a
collection has, so a tracker with a field named `at`, `on` or `every` would collide. Records'
point is that anyone can define their own fields (CLAUDE.md §1, §8). Hence the rule below, and a
form that reserves nothing.

**Rule: command options never take a name away from records' fields.** Records reserves only `id`
and `status`. Where a command option and a field share a name, the option keeps its meaning and the
CLI says how to reach the field (the `records list` case is #178).

## Decision

### 1. `shimmer schedule WHEN [options] COMMAND…` (CLI)

The schedule comes first, in words; then the command, exactly as you would run it:

    shimmer schedule weekdays 09:00 --catch-up skip workspaces activate site
    shimmer schedule daily 18:00 --catch-up skip workspaces stop site
    shimmer schedule every 1d --catch-up run-once records complete habits/stretch
    shimmer schedule once 2026-10-20 18:00 --catch-up skip workspaces stop site

`WHEN` is one of:

| `WHEN` | Meaning |
|---|---|
| `daily HH:MM`, `weekdays HH:MM`, `weekends HH:MM`, `mon,wed,fri HH:MM` | local time (§2) |
| `every DURATION` | `30m`, `2h`, `1d`, `1w`, as `--every` takes today |
| `once DATE TIME` | `2026-10-20 18:00` in local time, or RFC 3339 |
| `cron "EXPR"` / `cron-local "EXPR"` | five-field cron in UTC / in local time |

Then the options that belong to the schedule, before the command: `--catch-up
skip|run-once|backfill` (required, §5) and, rarely, `--lane LANE`.

- **Where the command starts: by position.** Every `WHEN` has a fixed shape, and each schedule
  option takes exactly one value:

  | `WHEN` | Words |
  |---|---|
  | `daily`, `weekdays`, `weekends` or a day list (`mon,wed,fri`), then `HH:MM` | 2 |
  | `every DURATION`, `cron "EXPR"`, `cron-local "EXPR"` | 2 |
  | `once DATE TIME` (`2026-10-20 18:00`) | 3 |
  | `once RFC3339` (`2026-10-20T18:00:00Z`: one word, with a `T`) | 2 |

  So the CLI reads `WHEN`, then `--option value` pairs, and the first word after them starts the
  command. Everything from there on is the command, with its own options, so nothing it takes can
  clash with the schedule's and no `--` is needed. An option's value is never mistaken for the
  command: `--lane workspaces workspaces activate site` reads the first `workspaces` as the lane.
  **Nothing is reserved:** a pack may alias `daily` or `mon`, because the command never starts where
  the schedule's words go. A first command word that isn't a command or an alias is an error
  naming it.
- **How it becomes a trigger:** the CLI parses the command exactly as it does to run it, and takes
  the single op + params that run would send (`records::request`; workspaces gets the same
  function). Then it sends today's `scheduler.add`, and the daemon stores op + params as it does
  now. Nothing new is computed in the client: this is the parsing the CLI already does.
- **Packs:** an alias in the command (`shimmer schedule weekdays 09:00 ikuzo site`) is resolved to
  canonical words before anything is stored (CLAUDE.md §12 rule 16), so switching packs never
  breaks a schedule. `schedule` is itself a command a pack can name (ADR 0029).
- **What can be scheduled:** a command whose run is exactly one module op (records and workspaces
  today; fetchers and notify as they arrive). Not schedulable, with an error saying why: commands
  that are several calls or client-side work (`records edit`, a rename that moves schedules,
  `workspaces new`), and anything that isn't a module op (`scheduler`, `queue`, `packs`, `ping`,
  `shutdown`). The daemon already refuses non-module ops (`unknown_op`), so a schedule can never
  create, cancel or remove schedules or tasks. Options that only make sense when running now
  (`--wait`, `--yes`) are an error.
- **Relative values stay relative:** a filter like `--filter "apply_by<=today+7"` is sent as
  written and resolved by the daemon each time the trigger fires, so a scheduled command doesn't
  freeze "today". A future client-side shortcut for dates must keep it that way.
- On success it says what was scheduled, in the same words `scheduler list` uses (§3).

`shimmer scheduler add OP [PARAMS] WHEN …` stays, for ops with no friendly command yet and for
scripts that already use it. `scheduler list|show|pause|resume|remove` are unchanged.

### 2. Local-time schedules (daemon, core)

`Schedule` gains a local-time cron:

    Cron(String)        UTC, as today: existing triggers keep their meaning
    CronLocal(String)   evaluated in config.toml's [general] local_timezone (ADR 0009)

The scheduler computes each next run in that zone, so `0 9 * * 1-5` stays 9am local across a clock
change. `next_due` stays stored in UTC, as today. On the wire: `{"cron_local": "0 9 * * 1-5"}`.

Clock changes:
- A time that doesn't exist (the hour skipped when the clocks go forward) runs **once**, at the
  next valid minute, however many firings the expression has in that hour (`*/15` runs once, not
  four times).
- A time that happens twice (the hour repeated when they go back) runs once, at the first.
- Both are tested in the scheduler with a fake clock before relying on the `cron` crate's handling
  of local times.

`local_timezone` is read when the daemon starts. A `CronLocal` trigger stores the zone its
`next_due` was computed in. On start:

- **Same zone:** `next_due` is kept exactly. It is never recomputed from "now": catch-up decides
  from the stored `next_due` (§6.3), so recomputing would turn every run missed while the daemon was
  down into a future one, and `run-once` and `backfill` would act like `skip`.
- **Zone changed:** `next_due` becomes the same wall-clock time in the new zone (Monday 09:00 in
  New York becomes Monday 09:00 in London; a time the clocks skip runs at the next valid minute, as
  above). A run that was due while the daemon was down stays in the past, so catch-up still sees
  it. It is never placed at or before `last_run`: then it is the next occurrence after `last_run`
  in the new zone. Recomputing from `last_run` alone would lose the case of a trigger that has
  never run.
- Tested with a fake clock: a `CronLocal` trigger missed while the daemon was down still catches up
  after a restart, with and without a zone change.

`daily/weekdays/… HH:MM` and `cron-local` produce `CronLocal`; `cron` stays `Cron`; `every` and
`once` are unchanged (`once` is converted to UTC by the CLI, as now).

**Older clients:** the protocol version is bumped only for breaking changes, and this one is
additive. But a client that decodes `schedule` into a closed set of shapes (as the TUI might, with
core's `Schedule` enum) would fail on a whole `scheduler.list` reply containing `cron_local`. Dev C
is told this specifically: decode unknown schedule shapes as "other" rather than failing.

### 3. Lists show what a schedule does (CLI)

`scheduler list` and `show` show each trigger as the command it runs, and its schedule in words:

    hk7p2x  workspaces activate site   weekdays 09:00 (local)   next Mon 09:00   active

The command is rebuilt from op + params with the op → command words table packs already has
(`TARGETS`, ADR 0029), and the params as positional arguments or `--field value`. An op with no
command shows as op + JSON, as today. `--json` is unchanged. This stays in the client: command
words are presentation (CLAUDE.md §15), and the daemon never learns them.

### 4. Short ids, from the random end (daemon + CLI)

Task ids are ULIDs and trigger ids are `usr-` plus a lowercase ULID. A ULID's first ten
characters are its creation time, so ids made close together share their start (`01M4PP…`), and a
prefix can rarely tell them apart. Its last sixteen characters are random.

- **Every op that takes a `task_id` or `trigger_id` also accepts a unique ending of one**, case
  insensitive (`queue show hk7p2x`, `scheduler pause 3fq9tm`). The daemon resolves it: one match is
  used, several are `conflict` listing them, none is `not_found`. Full ids keep working. Resolving
  in the daemon gives every client the same behaviour without fetching and searching full lists
  (#167).
- **The CLI shows the same short ending** (the last six characters, more if needed to be unique
  among what it lists), so what you see is what you type.

### 5. `--catch-up`, in plain words (CLI)

It stays required: a guessed default is how duplicate runs happen (§6.3). At a terminal, leaving it
out asks instead of failing:

    If your computer is off or asleep at 09:00:
      1) skip that time
      2) run it once when it wakes
      3) run every time it missed
    >

Without a terminal (scripts, a workspace step) it is still an error naming the option.

## Consequences

- People schedule the commands they already use, in their own time and in their pack's words, and
  read schedules back the same way. The UTC trap and the twice-yearly rewrite go away.
- No field name is reserved, now or later: scheduling words sit before the command and never
  compete with a command's options.
- The daemon gains one `Schedule` variant, a stored zone per local trigger (its `next_due` moved only
  when the zone changed), and short-id resolution. The CLI gains `schedule` (one parser for `WHEN` plus the existing command parsers)
  and the workspaces equivalent of `records::request`. No new logic moves into the client: the op +
  params are what running the command already sends.
- No word is reserved for the schedule. `schedule` itself becomes a command word, so a user's pack
  that already uses `schedule` as an alias stops loading until it's renamed: Shimmer prints one
  warning and runs with the real names (ADR 0013), and `shimmer packs check` names the problem.
  No built-in pack uses it.
- Protocol: `{"cron_local": …}` as a schedule, and id endings: both additive. protocol.md updated
  and Dev C told when each lands.
- Built in small PRs after sign-off: the core and scheduler change (§2), short ids (§4), then the
  CLI (§1, §3, §5).

## Not done here

- Changing a schedule's time or command in place (`scheduler edit`); today it's remove + add.
- A stop time on a workspace schedule (`--until 18:00`, adding the matching `stop`).
- Moving schedules on a rename can leave an active duplicate if a later step fails (pre-existing,
  #150's review); #166 replaces that with an atomic daemon op.
- Scheduled launches reporting back (notifications): with `notify`, M6.
