# TUI design

**Status:** proposed, for review by Dev A and Dev B · **Owner:** Dev C · **Issue:** #21 ·
**Milestone:** M5

This is the design for `crates/tui`, written before any TUI code exists so the boundaries are
reviewed first. It contains no implementation. Where it needs something the protocol or
`CLAUDE.md` doesn't provide, it says so in [§5 Open questions](#5-open-questions-for-dev-a-and-dev-b)
instead of assuming an answer.

Nothing in this document changes `core`, `proto`, `docs/protocol.md`, `CLAUDE.md` or the
mock-daemon fixtures. Proposed fixture cases (§4) are a list for a later PR to `crates/mockd`.

---

## Contents

1. [Scope of responsibility](#1-scope-of-responsibility)
2. [What the TUI represents](#2-what-the-tui-represents)
3. [Look and feel](#3-look-and-feel)
4. [Mock daemon requirements](#4-mock-daemon-requirements)
5. [Open questions for Dev A and Dev B](#5-open-questions-for-dev-a-and-dev-b)
6. [Out of scope](#6-out-of-scope)

---

## 1. Scope of responsibility

### 1.1 What `crates/tui` owns

Everything under `crates/tui`, and only that (CLAUDE.md §13):

| Area | Includes |
|---|---|
| Rendering | Every widget, table, panel, dialog, and the header and status bar, drawn with `ratatui` (CLAUDE.md §3). |
| Layout | Panel arrangement, responsive breakpoints, focus, scrolling. |
| Input | Keybindings, modes, the command palette, forms, mouse handling. |
| Navigation | Tabs, drill-down from list to detail, back-stack, help overlay. |
| Event-driven display | Holding the latest daemon-reported state per view, patching it from pushed events, refetching when told to (`core.stream.lagged`) or on reconnect. |
| Connection lifecycle | Handshake, reconnect with backoff, disconnected state, version-mismatch message. |
| Presentation config | The colour palette, glyph set, and (proposed, §3.6) command-pack aliases and animations inside the TUI. |
| Tests | Rendering and interaction tests against the mock daemon, never a real daemon or real files. |

### 1.2 What it does not own

| Not owned | Belongs to | Consequence for the TUI |
|---|---|---|
| What data means: what "done" is, what fields a collection has, how a lane orders work | Modules (Dev B), daemon services (Dev A) | The TUI shows what the daemon reports. It never computes a status, never infers a state transition, never re-orders a lane locally. |
| State | The daemon, the single writer (CLAUDE.md §1.6) | No optimistic updates. A row changes when the response or event says it did. |
| Files in `$SWE_HOME` | The daemon (via `store`) | The TUI never reads or writes data, events, the index or config, with the possible exception of `packs/` (CLAUDE.md §15.1) and log files (Q11). |
| Retry, fallback, scheduling, promotion rules | Queue and scheduler (Dev A) | The TUI drives the flows the protocol defines (e.g. the `confirmation_required` round-trip) and adds nothing to them. |
| Protocol shape | Change-controlled (CLAUDE.md §4) | A missing op, field or topic is reported to Dev A/B. It is never worked around with a local duplicate type, a file read, or a second connection. |

### 1.3 The dependency rule

`tui → core, proto`. Never `store`, `daemon`, `modules/*`, and also never `cli` (CLAUDE.md §3).
Two practical effects that reviewers should be aware of:

- **Code the CLI already has cannot be reused directly.** The autostart logic
  (`crates/cli/src/autostart.rs`) and, once built, the command-pack loader live in `cli`.
  The TUI may not import them and must not copy them silently. See Q8 and Q9.
- **`swe tui` needs one dispatch line in `crates/app`,** which Dev A owns (CLAUDE.md §13).
  See Q10.

### 1.4 How gaps are routed

1. Dev C hits something the protocol doesn't expose.
2. Dev C builds the screen against a **mock fixture** that shows the shape needed, so work
   isn't blocked (`docs/protocol.md`, "Mock daemon").
3. Dev C opens an issue naming the op or field, the screen that needs it, and the proposed
   shape, and adds it to §5 of this doc.
4. Dev A and Dev B decide. If it touches `core` or `proto`, they write an ADR and bump the
   protocol version per CLAUDE.md §4. The TUI adapts to whatever they decide, not to the
   fixture.

---

## 2. What the TUI represents

### 2.1 Principles

- **The manifest decides what exists.** On connect the TUI requests `core.manifest`. A
  tab exists only when the ops it needs are registered. A daemon with only `records`, which
  is the real daemon today, shows Records, Queue, Activity and Commands, and nothing for
  workspaces, fetchers, notifications or calendar.
- **Bespoke views are an explicit, small exception to "no module knowledge".** Tables for
  records and lists for workspaces have to know those modules' ops. Each bespoke view is
  gated on the presence of its ops in the manifest, and the generic **Commands** view
  (§2.9) can invoke any op from any module, including ones written after the TUI. Q2
  asks Dev A/B to confirm this reading of CLAUDE.md §9.
- **Push, not poll.** One subscription per connection, opened after the manifest. Views
  refetch only on first open, on `core.stream.lagged`, and after reconnect. There are no
  timers that poll.
- **`execution` comes from the manifest.** An op the manifest marks `queued` shows a
  "queued as task …" toast and follows the task through `queue.task.*` events. An inline op
  shows its result. The TUI never assumes which kind an op is.
- **Canonical names on screen.** Ops, topics and error codes are shown by their canonical
  names. Pack aliases (§3.6) are an addition in the palette, never a replacement.

### 2.2 Screen map

| Tab | Shown when | M5 data | Status at M5 |
|---|---|---|---|
| Dashboard | Always | Summaries of the other tabs | Real |
| Records | `records.list` present | Real daemon | **Real: the first build target** |
| Queue | Always (Q1) | Real daemon | Real, except reorder (Q12) |
| Scheduler | Always (Q1) | Real daemon | Real list; add form later |
| Workspaces | `workspaces.list` present | Mock only until M3 lands | Built against mock |
| Activity | Always | Event stream | Real |
| Commands | Always | Manifest | Real |
| Notifications | `notify.*` present | None exists (Q6) | Placeholder |
| Calendar | `calendar.*` present | None exists | Placeholder |

Fetchers get no tab of their own. Their work appears in the Queue tab (the `fetchers` lane)
and their results in Activity (`fetchers.item.found`), which is exactly how the event-bus
design means them to be seen.

### 2.3 Records (first build target)

The only module that exists today. The view is generic over collections: everything about a
collection comes from `records.collections`, so a user-defined `jobs.toml` renders with no
TUI change.

- **Collections list:** `records.collections`, with each collection's `total` from a
  `records.list` with `limit: 1` (the daemon accepts 1–500, so a count-only request isn't
  possible; this is cheap enough not to need one).
- **Done/todo counts** on the dashboard come from two filtered `records.list` calls
  (`status: done`, `status: todo`), each with `limit: 1`, reading `total`. The TUI counts
  nothing itself.
- **Table:** columns are `id`, `status`, then each schema field in definition order.
  Required fields marked. Unset values show as `–`.
- **Paging:** `limit`/`offset`, with `total` shown. Never loads everything.
- **Filter (`/`):** builds `filter` from `field=value` pairs. The protocol supports exact
  equality only, so the filter bar says "exact match" and offers enum values from the
  schema. An unknown field is caught by the daemon's `invalid_params` and shown verbatim.
- **Detail pane:** `records.get`.
- **Actions:** add (form generated from the collection's fields), edit (`records.update`,
  with explicit unset), complete (`records.complete`), remove (`records.remove`, confirmed).
- **Live updates:** `records.item.*` events update or invalidate the visible page
  (Q3 decides which).
- **Neutral enums.** `difficulty: hard` is not coloured red. Colouring enum values would
  be the TUI assigning meaning to module data. Enum values render plain.
- **Heatmap:** a collection's `views` may declare `{"kind":"heatmap","source":…}`. That
  is records data, not a `ViewSpec`. The TUI renders a placeholder until there is a data
  source for completion history (Q4, M4).

```
 Records ▸ LeetCode                                   filter: status=todo (exact)   96 of 137
╭ Collections ────────────╮╭ LeetCode ─────────────────────────────────────────────────────────╮
│▌LeetCode            137 ││  ID                           STATUS  TITLE                       │
│ Job applications      3 ││▌ lru-cache                    ○ todo  LRU Cache                   │
│                         ││  median-of-two-sorted-arrays  ○ todo  Median of Two Sorted Arrays │
│                         ││  valid-anagram                ○ todo  Valid Anagram               │
│                         ││                                                                   │
│                         ││  ── page 1 of 2 ──────────────────────────────── ctrl-d / ctrl-u ─│
│ ╭ Heatmap ──────────╮   │╰───────────────────────────────────────────────────────────────────╯
│ │ waiting on a      │   │╭ lru-cache ────────────────────────────────────────────────────────╮
│ │ history op (M4)   │   ││  status       ○ todo              difficulty   medium             │
│ ╰───────────────────╯   ││  title        LRU Cache           last_solved  –                  │
╰─────────────────────────╯│  url          –                                                   │
                           ╰───────────────────────────────────────────────────────────────────╯
 NORMAL  a add  e edit  c complete  D remove  / filter  enter detail  ? help
```

Columns beyond the terminal width scroll horizontally (`H`/`L`); the detail pane shows every
field regardless.

### 2.4 Queue

Data: `queue.list` (lanes, each with `running`, `queued`, `queue_version`) plus
`queue.task.*` events.

- **Grouped by lane**, because reordering is within a lane only (CLAUDE.md §6.2). Lanes come
  from the response, never a hardcoded list.
- **Running tasks** show a progress bar and note from `queue.task.progress`. Progress
  is streamed only and not logged, so a reconnect shows the last value from `queue.list`.
- **Queued tasks** show arrival order, priority tier (`scheduled` / `normal` /
  `overridden`) and origin (`user`, `scheduler` + trigger id, `module` + id). The tier is
  shown as a label, never as a rank, because `scheduled` and `normal` share one
  arrival-order queue (ADR 0003).
- **Cancel (`x`):** `queue.cancel`. If the task is running, the TUI asks first.
  `not_cancellable` is shown verbatim.
- **Promote (`P`):** runs the full `confirmation_required` round-trip (§2.10).
- **Reorder (`J`/`K`):** shown disabled with a "waiting on ADR 0006" hint until
  `queue.reorder` exists (Q12).
- **Failures:** `queue.task.failed` flashes the lane header and adds to Activity. Failed
  tasks leave the lane (CLAUDE.md §11.1), so they are not kept in this view; history is
  Activity's job (Q5).

```
╭ Queue ──────────────────────────────────────────────────────────────────────────────────────╮
│ fetchers    1/8 running · 3 queued · v41                                                    │
│   ⟳ fetchers.fetch      user              ████████░░░░░░░░░░░░  40%  page 2 of 5            │
│ ▌ 1 fetchers.fetch      scheduled · usr-01jd2q                    queued 09:00              │
│   2 fetchers.fetch      scheduled · usr-01jd2q                    queued 09:00              │
│   3 fetchers.fetch      scheduled · usr-01jd2r                    queued 09:00              │
│ workspaces  0/1 · idle                                                                      │
│ notify      0/2 · idle                                                                      │
│ index       0/1 · idle                                                                      │
│ default     0/4 · idle                                                                      │
╰─────────────────────────────────────────────────────────────────────────────────────────────╯
 NORMAL  x cancel  P promote  J/K reorder (ADR 0006)  enter task detail  ? help
```

### 2.5 Scheduler

Data: `scheduler.list` plus `scheduler.trigger.*` events.

- **Table:** trigger id, schedule, op, lane, `catch_up`, next due, last run, paused.
- **Schedules shown two ways:** a short human form ("Mondays 09:00 UTC", "every 2 days",
  "once, 4 Oct 09:00 UTC") and the raw value on the detail pane. Cron is UTC
  (`docs/protocol.md`); the TUI labels it as UTC rather than converting silently (Q7).
- **`catch_up` always visible.** It is the field that decides what happens after a laptop
  sleeps (CLAUDE.md §6.3), so it gets its own column.
- **`scheduler.trigger.skipped` and `.missed`** add a marker on the row and an entry in
  Activity. Overlap skips are normal; missed firings are worth seeing.
- **Pause and resume (space):** `scheduler.pause` / `scheduler.resume`.
- **Remove (`D`):** `scheduler.remove`, confirmed.
- **Add (`a`), later:** the form must make `catch_up` a required choice with no default
  selected, matching the protocol's rule. Fallback, if offered, is limited to `notify.*`
  ops (ADR 0005).

```
╭ Scheduler ──────────────────────────────────────────────────────────────────────────────────╮
│  TRIGGER      SCHEDULE              OP               CATCH-UP   NEXT DUE          LAST RUN  │
│▌ usr-01jd2q   Mondays 09:00 UTC     fetchers.fetch   skip       Mon 5 Oct 09:00   28 Sep    │
│  usr-01jd2r   every 2 days          fetchers.fetch   run_once   Thu 1 Oct 14:00   29 Sep  ▲ │
│  usr-01jd2s   once, 4 Oct 09:00     notify.send      backfill   Sun 4 Oct 09:00   –       ‖ │
╰─────────────────────────────────────────────────────────────────────────────────────────────╯
 ▲ missed firings dropped by catch-up   ‖ paused        space pause/resume  D remove  ? help
```

### 2.6 Workspaces (built against mock until M3)

Data: `workspaces.list`, `workspaces.status`, `workspaces.session.*` and the `workspaces` lane.

- **States** `ready`, `launching`, `active`, `dirty`, each with a glyph and a word.
- **`active` is labelled "launched",** with the time. The protocol says `active` makes no
  liveness claim and a client must not present it as running.
- **`launching`** shows the step and progress from the `workspaces` lane's running task.
- **`dirty` is loud:** an error-coloured row, a dashboard banner, and the dirty reason
  inline. Activating it is not possible from a normal keypress: `enter` opens the recovery
  dialog instead, exactly as the protocol says clients should offer.
- **Recovery dialog** for `workspace_dirty`: run cleanup, open the log, force relaunch,
  reset. Force and reset require typing the workspace id. Force relaunch is a distinct op,
  never a flag (CLAUDE.md §10.3). "Never auto-force" is honoured by never making force the
  default focus.

```
╭ Workspaces ─────────────────────────────╮╭ deep-work ──────────────────────────────────────╮
│▌✖ deep-work   dirty                     ││  state       ✖ dirty                            │
│ ● review      launched 09:12            ││  reason      step 3/7 tmux-session exited 1     │
│ ◐ focus       launching · step 2/7      ││  failed at   09:12:44 UTC                       │
│ ○ notes       ready                     ││  log         logs/deep-work-01JD2T.log          │
│                                         ││  cleanup     cleanup.sh present                 │
╰─────────────────────────────────────────╯╰─────────────────────────────────────────────────╯

          ╭ deep-work is dirty ───────────────────────────────────────────╮
          │                                                               │
          │  Step 3/7 tmux-session failed at 09:12:44 UTC.                │
          │  It was not cleaned up, so it can't be activated.             │
          │                                                               │
          │  ▌ c  run cleanup.sh             recommended                  │
          │    l  open the log                                            │
          │    F  force relaunch…            type "deep-work" to confirm  │
          │    R  reset without cleanup…     type "deep-work" to confirm  │
          │                                                               │
          │                                            esc  close         │
          ╰───────────────────────────────────────────────────────────────╯
```

### 2.7 Notifications (placeholder)

No `notify` module or ops exist yet (M6). Until they do, the Notifications tab is hidden;
`notify.sent`, `notify.fallback_used` and `notify.exhausted` events still show in Activity
if they arrive. A real view needs a way to list deliveries that exhausted every sink, which
today only exist in `notifications/failed.jsonl` (Q6).

### 2.8 Calendar and fetchers (placeholders)

- **Calendar** (M6): hidden until `calendar.*` ops exist. Its natural form is a month grid
  plus an agenda list, and it is the obvious home for the heatmap's sibling. Not designed
  further here.
- **Fetchers** (M6): no tab (§2.2). If a source list is needed later, it comes from a
  `fetchers.*` list op in the manifest.

### 2.9 Activity and Commands (always present)

**Activity** is a live, filterable log of the subscription stream: time, glyph, topic,
source and a one-line payload summary. It is the only place the §1.3 chain
(`fetchers.item.found` → `records.item.updated` → … → `notify.sent`) is visible end to end,
and it is how failures surface without polling. It starts empty on connect (Q5).
`queue.task.progress` is excluded by default because it is noisy.

**Commands** lists every op in the manifest grouped by module, with summary and
execution (`inline` / `queued: lane`). Selecting one opens a JSON params editor and sends
it, like `swe call`. This is what keeps the TUI useful for modules it has no bespoke view
for. When `params_schema` is complete (advisory before M3) it can become a form.

### 2.10 Confirmation round-trip

The two-step promotion flow from `docs/protocol.md`, drawn once because Queue and Commands
both use it.

```
          ╭ Jump the fetchers lane? ──────────────────────────────────────╮
          │                                                               │
          │  fetchers.fetch {"source":"linkedin"} would run before:       │
          │                                                               │
          │    01JD2P…02  fetchers.fetch   scheduled                      │
          │    01JD2P…03  fetchers.fetch   scheduled                      │
          │    01JD2P…04  fetchers.fetch   scheduled                      │
          │                                                               │
          │  This is logged as queue.task.promoted.        lane v41       │
          │                                                               │
          │                         y  promote        ▌ n  cancel         │
          ╰───────────────────────────────────────────────────────────────╯
```

- The list is exactly `detail.would_displace`; the TUI adds nothing.
- `y` re-sends with `confirm: true` and the `queue_version` that was shown.
- If the daemon answers `confirmation_required` again (the lane changed), the dialog
  refreshes with a "the lane changed, review again" note. It never auto-confirms.
- Default focus is **cancel**.

### 2.11 Dashboard

One screen that answers "what is my machine doing and does anything need me", using only
the views above.

```
 ▗▅▖ shimmer   Dashboard  Records  Queue  Scheduler  Workspaces  Activity  Commands      ● up 3h12m
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
 ✖ workspace deep-work is dirty: step 3/7 tmux-session exited 1                     enter  recover
╭ Records ───────────────────────────────────╮╭ Queue ──────────────────────────────────────────╮
│ LeetCode           41 done · 96 todo       ││ fetchers    1/8 · 3 queued                      │
│ Job applications    0 done ·  3 todo       ││   ⟳ fetchers.fetch  ████████░░░░  40%           │
│                                            ││ workspaces  0/1 · idle                          │
│ heatmap: waiting on a history op (M4)      ││ default     0/4 · idle                          │
╰────────────────────────────────────────────╯╰─────────────────────────────────────────────────╯
╭ Workspaces ────────────────────────────────╮╭ Activity ───────────────────────────────────────╮
│ ✖ deep-work   dirty                        ││ 14:03 ✓ records.item.completed  leetcode/two-sum│
│ ● review      launched 09:12               ││ 14:01 ✖ queue.task.failed   workspaces · focus  │
│ ○ notes       ready                        ││ 13:58 ↻ scheduler.trigger.fired   usr-01jd2q    │
╰────────────────────────────────────────────╯╰─────────────────────────────────────────────────╯
 ● connected   NORMAL   1-7 tabs  tab focus  : command  / filter  ? help  q quit
```

The underline under the header is the **shimmer band**: the iridescent gradient from the
logo (§3.2). The banner line appears only when something needs attention (dirty workspace,
lagged stream, disconnected) and disappears when it's resolved.

### 2.12 Connection states

| State | What the user sees |
|---|---|
| Connecting | Header shows `◐ connecting`; panels show skeleton rows. |
| Connected | `● up 3h12m` from `core.ping`, refreshed on reconnect only. |
| Lagged | Banner "missed N events, refreshing" while every open view refetches. |
| Disconnected | Banner "daemon not reachable, retrying in Ns"; last data stays on screen, dimmed and marked stale. Actions are disabled, never queued locally. |
| Version mismatch | Full-screen message with the daemon's `unsupported_version` detail and the supported versions. No retry loop. |

Whether the TUI auto-starts the daemon like the CLI does is Q9.

---

## 3. Look and feel

### 3.1 Direction

**Calm, dense, dark, with one bright signature.** The logo is a pixel-art potion flask: a
magenta cork, lavender glass, violet liquid, an iridescent mint-to-pink band where the
liquid meets the glass, and small ice-white glints, all on a near-black plum ground. The
TUI borrows that structure, not just the colours:

- **Dark plum ground, lavender structure.** Borders, titles and labels are lavender and
  muted violet. Most of the screen is quiet.
- **Magenta is the cork:** one small, saturated point. It marks the focused panel, the
  selected row edge and the active tab, and nothing else.
- **The iridescent band is the signature.** It appears exactly twice: the shimmer band under
  the header and the leading edge of progress bars. Used anywhere else it stops being
  special.
- **Ice-white glints are for what you must read:** key hints and the selected row's text.
- **Pixel-art echo:** block and half-block glyphs (`▀ ▄ ▌ ▐ █`) for the header mark, the
  progress bars and the heatmap, so the TUI reads as the same object as the logo.

Density: closer to `btop` and `k9s` than to a wizard. A full 80×24 terminal shows a useful
dashboard. Every row earns its place; no decorative padding beyond one blank column inside
panel borders.

### 3.2 Palette

Sampled from the logo (`docs/design/CleanShot_2026-09-30_at_6.04.37_PM2x.png`), with three
derived colours (`muted`, `rule`, and the status colours) chosen to sit in the same family.
Contrast is measured against `void`.

| Token | Hex | 256-colour | Source | Role | Contrast |
|---|---|---|---|---|---|
| `void` | `#12040e` | 232 | logo background | Optional background (see below) | – |
| `flask` | `#2e192c` | 235 | flask interior | Selected-row and dialog background | – |
| `rule` | `#3d2a47` | 237 | derived | Unfocused borders, separators | 1.5 |
| `muted` | `#8b7a9e` | 103 | derived | Secondary text, timestamps, hints | 5.1 |
| `lavender` | `#bfa4f1` | 147 | glass | Primary text accents, titles, labels | 9.3 |
| `violet` | `#9e5df6` | 135 | liquid | Progress fill, heatmap high end | 5.1 |
| `cork` | `#da007d` | 162 | cork | Focus: focused border, active tab, selection edge | 4.1 |
| `ice` | `#def2fc` | 195 | glints | Emphasised text, key hints, selected-row text | 17.4 |
| `mint` | `#cefff7` | 195 | band, left | Shimmer band start; success | 18.4 |
| `cream` | `#eef8d8` | 230 | band, middle | Shimmer band middle | 18.2 |
| `pink` | `#fd99d8` | 212 | band, right | Shimmer band end | 10.2 |
| `amber` | `#f5c97a` | 222 | derived | Warning: lagged, skipped, missed, stale | 12.9 |
| `coral` | `#ff6b81` | 204 | derived | Error: failed, dirty, not cancellable | 7.3 |

Rules:

- **`cork` is never used for body text.** At 4.1:1 it is below the 4.5:1 text threshold. It
  is for borders, the selection bar and bold single-glyph marks only.
- **Colour is never the only signal.** Every state has a glyph and a word (`✖ dirty`,
  `○ todo`). The UI must be fully usable with `NO_COLOR` set, which the TUI honours.
- **Don't paint the background by default.** Many users run transparent or themed terminals
  (Hyprland setups especially). The TUI draws on the terminal's own background; `void` is
  an opt-in setting for a "full logo" look.
- **Three colour depths.** Truecolour when the terminal says so (`COLORTERM`), the
  256-colour column otherwise, and a 16-colour mapping (magenta, bright blue, bright white,
  green, yellow, red) as the last fallback. `ice` and `mint` collapse to the same 256 index;
  that's acceptable because `mint` is only used where a glyph also carries the meaning.
- **Module data gets no colours.** Status colours are for daemon and queue states the
  protocol defines (failed, dirty, lagged, paused). Record field values render plain (§2.3).

### 3.3 Header

One row plus the shimmer band:

```
 ▗▅▖ shimmer   Dashboard  Records  Queue  Scheduler  Workspaces  Activity  Commands      ● up 3h12m
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
```

- `▗▅▖` is the flask in three cells: a lavender body with a single `cork` pixel on top in
  truecolour. With fewer colours it is plain lavender.
- `shimmer` in lowercase, bold `lavender`. Until the rename (#15) only the binary is
  `swe`; the product name in the UI is Shimmer.
- Tabs: inactive in `muted`, active in `ice` with a `cork` underline cell. Only tabs whose
  ops exist are shown (§2.2).
- Right side: connection glyph and uptime, or the connection state from §2.12.
- **Shimmer band:** a row of `▀` whose colour runs `mint → cream → pink` across the width,
  interpolated per cell in truecolour, three equal segments in 256-colour, and a plain
  `rule`-coloured line with `NO_COLOR` or 16 colours.
- **Optional one-shot shimmer on start:** the gradient sweeps once across the band (under a
  second), like light catching glass. Never repeats, never delays input, off with
  `--no-anim`, `NO_COLOR` or a non-TTY.

### 3.4 Layout

- **Fixed frame:** header (2 rows), optional banner (1 row), content, status bar (1 row).
- **Content is panels with rounded borders.** The focused panel's border is `cork`; others
  are `rule`. Panel titles sit in the top border in `lavender`.
- **Breakpoints:**

| Width | Layout |
|---|---|
| ≥ 120 cols | Dashboard 2×2 grid; list views get a detail pane on the right. |
| 80–119 | Dashboard 2×2 with narrower panels; detail opens below the list. |
| < 80 or < 24 rows | One panel at a time; tabs become a `‹ Records ›` switcher; detail replaces the list. |
| < 60×16 | A single "terminal too small (need 60×16)" message. |

```
 ▗▅▖ shimmer  ‹ Records ›                      ● up 3h
 ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
╭ LeetCode · 96 of 137 · status=todo ──────────────╮
│▌○ lru-cache                    LRU Cache         │
│ ○ median-of-two-sorted-arrays  Median of Two …   │
│ ○ valid-anagram                Valid Anagram     │
╰──────────────────────────────────────────────────╯
 NORMAL  a add  c complete  ? help
```

- **Status bar:** current mode, then the 4–6 most relevant keys for the focused panel,
  then a transient message slot for toasts ("queued as 01JD2N…", "copied log path").

### 3.5 Keybindings

**Philosophy: vim-flavoured navigation, no editor-style modality, everything discoverable.**

The users are engineers who live in terminals, tmux and tiling WMs. `hjkl`, `g`/`G`, `/` and
`:` are muscle memory and cost nothing to support. Full vim modality (an "insert" mode you
can get stuck in) is not worth it for a dashboard, so the TUI follows `lazygit`/`k9s`
instead: single keys act on the selection, arrow keys always work, and the status bar
always shows the current mode and what to press next.

**Modes,** always named in the status bar. `esc` always returns to NORMAL.

| Mode | Entered by | Purpose |
|---|---|---|
| NORMAL | default, `esc` | Navigate and act on the selection. |
| FILTER | `/` | Type `field=value` filters for the focused list. `enter` applies. |
| COMMAND | `:` | Command palette: fuzzy search over manifest ops, views and pack aliases. |
| FORM | `a`, `e`, Commands view | Edit fields. `tab` moves between fields, `ctrl-s` submits. |
| DIALOG | confirmations | Only the keys the dialog lists work. |

**Global keys:**

| Key | Action |
|---|---|
| `1`–`9` | Jump to tab by position |
| `tab` / `shift-tab` | Cycle panel focus |
| `j`/`k`, `↓`/`↑` | Move selection |
| `h`/`l`, `←`/`→` | Collapse or expand, scroll columns in wide tables (`H`/`L`) |
| `g` / `G` | Top / bottom |
| `ctrl-d` / `ctrl-u` | Half page down / up (loads the next/previous page) |
| `enter` | Open detail or default action |
| `esc` | Back, close, or clear the filter |
| `/` | Filter |
| `:` | Command palette |
| `r` | Refetch the focused view |
| `?` | Help overlay listing every key for the current view |
| `q` | Quit (never `esc`) |

**Action keys** are lowercase for safe, reversible actions and **uppercase for destructive
or audited ones**, which always open a confirmation:

| View | Safe | Destructive / audited (confirmed) |
|---|---|---|
| Records | `a` add, `e` edit, `c` complete | `D` remove |
| Queue | `enter` detail | `x` cancel (confirm only if running), `P` promote |
| Scheduler | `space` pause/resume | `D` remove |
| Workspaces | `enter` activate or recover, `c` cleanup, `l` log | `F` force relaunch, `R` reset (typed confirmation) |

Mouse: on by default (click a tab, a row, scroll), because it costs nothing and helps on
laptops; disabled with `--no-mouse` for users who want terminal selection back.

Keys are fixed in M5. User-remappable keys are a later decision and, if added, would be
presentation config read by the client like packs.

### 3.6 Command packs in the TUI: proposal

Today command packs (CLAUDE.md §15.1) are CLI aliases plus optional animations, and they
aren't implemented yet. Nothing says they extend into the TUI. **This section is a proposal
for Dev A/B to accept, change or reject**, split into tiers so the safe part can go ahead
alone.

| Tier | What | Needs | Recommendation |
|---|---|---|---|
| 0 | **Aliases in the command palette.** Typing `bankai` in `:` finds `workspaces.activate`. Results always show both: `bankai → workspaces.activate`. | A pack loader the TUI can use (Q8). No contract change. | **Do at M5.** |
| 1 | **Pack animations in the TUI.** When an op with an `[anim]` entry is sent, its frames play in a small overlay or the status-bar slot. | Same loader. Within §15.1 as written. | **Do at M5,** under the rules below. |
| 2 | **Pack colour themes.** An optional `[theme]` table in `pack.toml` overriding the palette tokens in §3.2 by name. | An amendment to §15.1 (packs are currently aliases and animations only). | **Propose for after M5** (Q13). |

Rules for any tier, all from CLAUDE.md §15 and §12 rule 16:

- **Canonical names stay primary.** Alias appears beside the canonical name, never instead
  of it. Errors, toasts, the help overlay, Activity and Commands show canonical names only.
- **Animations never gate execution.** The request is sent first; the animation plays
  alongside, is skippable with any key, is capped at about 1.5 s, and is off with
  `--no-anim`, `NO_COLOR` or a non-TTY.
- **Packs are data.** The TUI reads `packs/` read-only and only the active pack named in
  `config.toml`. A pack that fails to load falls back to the built-in default with a toast,
  matching the CLI's behaviour.
- **A theme can't break legibility.** Under Tier 2, overrides that fail the contrast rules
  in §3.2 are ignored token by token, and status tokens (`coral`, `amber`) can be restyled
  but never removed.

### 3.7 Feel and responsiveness

- **Input is never blocked.** Requests are in flight in the background; the affected row
  shows a small spinner. A request without a response after 5 s shows "still waiting" and
  stays cancellable from the client side.
- **Redraws are coalesced** to at most about 30 per second, so a burst of progress events
  doesn't flicker.
- **No surprise moves.** A pushed event never moves the selection. New rows appear with a
  brief `ice` highlight that fades over a second.
- **Every error is shown verbatim** with its code: `not_found: no collection 'nope'`. The
  TUI does not rewrite daemon messages.

### 3.8 References

- **`lazygit`, `k9s`:** single-key actions on a selection, context-sensitive status-bar hints,
  `?` overlay.
- **`btop`:** density, block-glyph meters, graceful colour-depth fallback.
- **`helix`:** discoverable keys, mode always visible.
- **`yazi`:** responsive panel collapse at small widths.
- **Charm (`lipgloss`, `gum`):** restrained colour and rounded borders as an aesthetic, not
  as code.

---

## 4. Mock daemon requirements

### 4.1 Coverage today

The existing fixtures in `crates/mockd/fixtures/` against what each view in §2 needs:

| Need | Fixture | Covered? |
|---|---|---|
| `core.manifest` with lanes and modules | `core.json` (five lanes; records, fetchers, workspaces) | Yes, fully loaded |
| Manifest of the **real M1 daemon** (default lane, records only) | – | **No** (4.2 #1) |
| `records.collections` | `core.json` (LeetCode only) | Partly: one collection |
| A second, differently shaped collection | – | **No** (4.2 #2) |
| `records.list` page, `not_found` | `core.json` | Yes |
| `records.list` with filter and paging | rules match any `leetcode` list | Partly: filter and offset ignored (4.2 #3) |
| `records.get` / `add` / `update` / `complete` / `remove` success | `core.json` | Yes |
| `records.*` errors (`conflict`, `invalid_params`) | – | **No** (4.2 #4) |
| `records.item.*` events with the real payload shape | `stream-lagged.json` (no `item` field) | **No** (4.2 #5) |
| `core.stream.lagged` | `stream-lagged.json` | Yes |
| `queue.list` | `core.json` | Yes |
| `queue.task`, `queue.cancel` | – | **No** (4.2 #6) |
| Confirmation round-trip, including a stale version | `confirmation.json` | Yes |
| Long queued task with progress | `long-task.json` | Yes |
| `scheduler.list` and trigger events | – | **No** (4.2 #7) |
| `workspaces.list` / `status` / `activate` dirty / `cleanup` | `workspace-dirty.json` | Yes |
| `workspaces.force_relaunch`, `reset`, a `launching` state | – | **No** (4.2 #8) |
| `notify.*`, `calendar.*` | – | Not needed at M5 |

### 4.2 Fixture cases to add

None of these are added by this PR. Dev C contributes them to `crates/mockd/fixtures` by
ordinary PR, which Dev A reviews (CLAUDE.md §13). **P0** is needed before the first Records
and Queue screens; **P1** before the remaining tabs.

| # | Pri | Case | Why |
|---|---|---|---|
| 1 | P0 | **M1-only fixture set** in a subdirectory, `crates/mockd/fixtures/m1/`: a manifest with only the `default` lane and only `records`, plus the records rules. `swe mockd` reads only top-level `*.json`, so a subdirectory works as a separate `--fixtures` target. | The only realistic shape today. Proves tabs hide when ops are missing. |
| 2 | P0 | A second collection in `records.collections` with different field types (e.g. `jobs`: required `company`, an enum `stage`, a date `applied_on`) and a `records.list` for it. | Proves the Records view is generic and nothing is LeetCode-specific. |
| 3 | P0 | `records.list` rules keyed on `filter` and `offset`: `status: "todo"` returning only todo items, and `offset: 50` returning a second page. | Exercises the filter bar and paging. |
| 4 | P0 | Errors: `records.add` for an existing id → `conflict`; `records.list` with an unknown filter key → `invalid_params`. | The error display path. |
| 5 | P0 | A timeline of `records.item.created` / `updated` / `completed` / `removed` with the payloads the real daemon emits (`collection`, `id`, `item`, and `changed` on update). | Live updates. Shape depends on Q3. |
| 6 | P1 | `queue.task` for the running fetch; `queue.cancel` succeeding for a queued task (with `queue.task.cancelled`) and returning `not_cancellable` for another. | Task detail and cancel. |
| 7 | P1 | `scheduler.list` with three triggers covering `cron`/`every`/`once`, all three `catch_up` values and one paused, plus a timeline with `scheduler.trigger.fired`, `.skipped` (`reason: "overlap"`) and `.missed`. | The whole Scheduler tab. |
| 8 | P1 | `workspaces.force_relaunch` (emitting `workspaces.session.forced`), `workspaces.reset`, and a `workspaces.list` entry in `launching`. | The rest of the recovery dialog. |

---

## 5. Open questions for Dev A and Dev B

None of these are decided here. Each names who decides and whether a shared contract
(CLAUDE.md §4) is involved.

| # | Question | Blocks | Who | Contract? |
|---|---|---|---|---|
| Q1 | **How does a client discover core services?** `core.manifest` lists modules only; `queue.*` and `scheduler.*` are not in it, and `docs/protocol.md` lists only `core.*` as "always present". Either document queue and scheduler ops as always present, or add services to the manifest. | Showing the Queue and Scheduler tabs without hardcoding. | Dev A | `protocol.md`; possibly `proto` |
| Q2 | **Is "bespoke views gated on the manifest, plus a generic Commands view" an acceptable reading of "clients must not hardcode module knowledge" (CLAUDE.md §9)?** | The approach in §2.1. | Dev A, Dev B | No |
| Q3 | **Are event payloads part of the contract?** The real daemon's `records.item.*` events carry the full `item` (and `changed` on update); `protocol.md` doesn't specify payloads and the fixtures omit `item`. If the TUI can rely on `item`, it patches rows; if not, it refetches the page on each event. | Live-update strategy; fixture #5. | Dev B (records), Dev A (protocol) | `protocol.md` |
| Q4 | **Where does heatmap data come from?** `records.list` returns only the latest `last_solved`; repeated solves exist only in the event log. A heatmap needs an aggregate op (e.g. completions per day for a collection over a range), presumably at M4. | The Records heatmap. | Dev A, Dev B | Yes, new op |
| Q5 | **Can a client read recent history on connect?** Subscriptions deliver only future events, and failed tasks leave `queue.list`. Activity starts empty and a failure from five minutes ago is invisible. A bounded "recent events" op would fix both. | Activity; "what failed while I was away". | Dev A | Yes, new op |
| Q6 | **How will notifications be visible?** No `notify.*` ops exist, and exhausted deliveries live only in `notifications/failed.jsonl`, which clients must not read. | The Notifications tab (M6). | Dev B | Yes, at M6 |
| Q7 | **What timezone should the TUI display?** `[general] local_timezone` (ADR 0009) is in `config.toml`, which the daemon reads; it isn't on the wire. The TUI either shows UTC everywhere, reads `config.toml` itself, or gets the zone from the daemon. | Every timestamp on screen. | Dev A | Possibly `proto` |
| Q8 | **Where does the command-pack loader live?** Packs aren't built yet. If the loader and the embedded default pack land in `cli`, the TUI can't use them (§1.3). Options: a small client-side crate both depend on (a §3 layout change), or a home in `core` (whose dependency rules don't currently forbid packs, though §12 rule 16 suggests keeping them out of shared contracts). | Tiers 0 and 1 of §3.6. | Dev A, Dev B | CLAUDE.md §3 |
| Q9 | **Does the TUI auto-start the daemon?** CLAUDE.md §2 says the CLI does. The logic is in `crates/cli/src/autostart.rs`, which the TUI can't import. Same shape as Q8: share it or define TUI behaviour as "show disconnected and the command to start it". | §2.12 connection states. | Dev A | CLAUDE.md §2/§3 |
| Q10 | **`swe tui` dispatch.** `crates/app` needs a `tui` arm (CLAUDE.md §13: Dev A reviews `app/`). Confirm the flags match the CLI: `--socket PATH` (never auto-starts, as in the CLI), plus TUI-only `--no-anim` and `--no-mouse`. | Running the TUI at all. | Dev A | No |
| Q11 | **Can the TUI open workspace logs?** The protocol says clients should offer "open the log", and gives a path relative to `$SWE_HOME`. Reading it directly is a client reading the data directory. Options: allow read-only access to `logs/` (like `packs/`), open it in `$PAGER`, or add a log-tail op. | The recovery dialog's `l` action. | Dev A | Possibly |
| Q12 | **`queue.reorder` is blocked on ADR 0006.** The TUI shows reorder as disabled until it lands. Dev C asks to be notified when it does, per §4. | Queue reorder. | Dev A | Already in progress |
| Q13 | **Pack themes (§3.6 Tier 2).** Should `pack.toml` gain an optional `[theme]` table? This is an amendment to §15.1. | Tier 2 only; M5 doesn't need it. | Dev A, Dev B | CLAUDE.md §15.1 |
| Q14 | **Server-side sort for `records.list`.** Only exact-match filters and paging exist. Sorting a page client-side is wrong across pages. Is a `sort` param wanted, or is daemon order (by id?) the order? | Sorting in the Records table. | Dev B | `protocol.md` |

---

## 6. Out of scope

- Any `crates/tui` code, which starts after this doc is reviewed.
- Any change to `core`, `proto`, `docs/protocol.md` or `CLAUDE.md`. Gaps are in §5.
- A `ViewSpec` or any shared rendering abstraction (CLAUDE.md §5). The views above are
  deliberately concrete; the spec gets extracted from them at M5.
- GUI, locked-in mode, heatmap-driven nudges, and the RPG layer (CLAUDE.md §1.7).
- New fixtures, which come in a separate PR to `crates/mockd` (§4.2).
