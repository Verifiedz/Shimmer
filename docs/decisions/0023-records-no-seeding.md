# 0023. A fresh install has no collections; LeetCode is a template like the others

Status: accepted (records roadmap, #101) · Raised by Dev B (records roadmap) · Signed off: Dev A
(changes ADR 0008's seeding rule; small changes to end-to-end tests in `crates/app`) · Dev C
notified · No `core` or `proto` change

## Context

ADR 0008 seeds a LeetCode collection on first start "so a fresh install has something to track",
and re-seeds it whenever no collection exists. That was the right call at M1, when there was no
other way to get a collection. Since ADR 0022 there is: `shimmer records templates` lists
ready-made trackers, and `shimmer records new ID --from TEMPLATE` creates one.

Seeding now gets in the way:

- **It decides for the person.** Someone tracking job applications starts with a LeetCode tracker
  they didn't ask for, and every list of collections shows it.
- **It comes back.** Remove every collection and LeetCode reappears on the next start; the only
  way to have none is to keep an empty one around.
- **It's the odd one out.** The other trackers are templates the person picks; LeetCode alone
  is forced on everyone, in an older, thinner shape than the job templates.

## Decision

1. **Nothing is created on first start.** `records`' `init` writes nothing and emits nothing. A
   fresh install has no collections, and `shimmer records collections` says how to get one:
   `no collections yet: see 'shimmer records templates', then 'shimmer records new ID --from
   TEMPLATE'`.
2. **LeetCode is a template**, listed and created like the others, and redesigned to match them
   (ADR 0017–0022): `title`, `url` (role `url`, unique), `number`, `difficulty`, `topic`,
   `pattern` (a fixed list, so filtering by pattern is reliable), `list` ("Blind 75"),
   `company`, `confidence`, `attempts`, `time_min`, `solved_without_help`, `next_review` (role
   `deadline`: when to redo it), `last_solved` (stamped by `records complete`, every solve
   counts), `notes`, and a heatmap of solved days.
3. **Existing data is untouched.** A `leetcode` collection already on disk is the person's file
   and keeps working exactly as it does; nothing reads, rewrites or removes it. To start fresh,
   `shimmer records remove-collection leetcode` (ADR 0021) puts it in the trash, and it no longer
   comes back on restart.
4. **Tests write their own collection.** The records tests and the end-to-end tests used the
   seeded collection; they now write the same file in themselves, from one fixture
   (`crates/modules/records/tests/fixtures/leetcode.toml`), so what they check is unchanged.

## Consequences

- `crates/modules/records`: `init` does nothing; the old `leetcode.toml` becomes a test fixture;
  `templates/leetcode.toml` is the new template.
- `crates/app/tests`: a `Home::with_leetcode()` helper writes the fixture before the daemon
  starts; the index-recovery test expects one event fewer (no seeding event).
- `crates/cli`: the empty `records collections` points at templates.
- ADR 0008's "Seeding" paragraph is marked superseded. `docs/protocol.md`'s
  `records.collection.created` row no longer mentions seeding.
- `crates/mockd/fixtures` keep their LeetCode data: the mock daemon shows a populated example for
  the TUI, not a fresh install.

## Not done here

- A first-run prompt ("what do you want to track?") in the CLI or TUI. The empty state's message
  points at the templates; a guided first run can come later, likely with kits (#79).
