# 0040. The store: a committed write has happened, and a damaged one never blocks a start

Status: proposed · Raised by Dev B (#260, data safety before dogfooding) · Needs sign-off: Dev A
(`crates/store`, CLAUDE.md §7.1) · No `core` or `proto` change

## Context

CLAUDE.md §7.1 stages a multi-file write under `.staging/<txid>/`, writes a `COMMITTED` marker,
then applies it (files into place, events appended, staging removed). Recovery rolls a marked
folder forward and deletes an unmarked one. The robustness audit (#260) found four ways this goes
wrong:

1. **A failed last step was reported as a failed write.** With the event log unwritable,
   `records.add` said `internal: Permission denied`, but the record file was already in place. A
   retry made a second record, and after a restart recovery finished the first: two records.
2. **A damaged staging folder stopped every start.** A marked folder with a corrupt
   `manifest.json`, or with a staged file gone, made `store.recover()` fail, and `Daemon::start`
   returns that error: the daemon refused to start until someone deleted the folder by hand.
3. **Commits got slower all day.** Every commit re-read the whole day's event log to check each
   event wasn't already in it (0.5 ms on an empty day, 97 ms at 9 MB).
4. **Two writes to one file at once failed.** `write_atomic`'s temp name was `.{name}.tmp-{pid}`,
   the same for every writer in the daemon, so one writer's rename took the other's file (20 out
   of 20 races). The HTTP cache does this for two requests to one url.

## Decision

1. **`COMMITTED` is the commit point for the caller too.** Once the marker is written the write
   has happened: `commit` returns success even if applying it fails. The folder is kept, retried
   before the next commit, and rolled forward by recovery on the next start. `shimmer doctor`
   warns about one still waiting after a minute. A caller is never told "failed" about a write
   that happened, so it never retries one into two.
   - **Durable first, published second still holds** (#309's review). A waiting transaction's
     events aren't in the log, so they aren't published either: `Store::commit_logged` hands
     back only the events that reached the log in that call (waiting ones that landed, oldest
     first, then its own), and the daemon publishes exactly those. Modules and the index see
     a waiting save's events when it lands, never before. One that lands through recovery at
     start is read by the indexer from the log; modules don't see it (as with any crash).
   - **Commit order holds** (#309's review). While a transaction waits, every later one waits
     behind it, staged (even a lone file write, which otherwise skips staging), and they land
     oldest first, stopping at the first that still fails. Recovery applies folders in the
     order they were made: a folder is named `<nanoseconds, never repeating>-<txid>`, since a
     ULID alone is ordered only to the millisecond.
2. **Recovery takes each staging folder on its own.** A marked one that can't be applied is moved
   to `.staging/broken/<txid>/`, whole: never deleted, since it may hold committed data. It's
   logged with the reason, returned in `Recovery::broken`, and reported by `shimmer doctor` (which
   counts `.staging/broken/`: the CLI never links the store, CLAUDE.md §3). The daemon starts. An
   unmarked folder that can't be deleted is logged and left; it never happened, so it blocks
   nothing. `.staging/` is gitignored, so `broken/` stays on the machine where it happened.
3. **Only recovery checks the log for an event before appending it**, and it reads each day's file
   once. A fresh commit has just staged its events, so they can't be in the log yet.
4. **`write_atomic`'s temp name is unique per call** (`.{name}.tmp-{pid}-{n}`, `n` a counter).
   The last rename wins whole, and no writer fails. Pairing the HTTP cache's body and metadata
   files as one unit is left to #115.

## Proposed change to CLAUDE.md §7.1

To the **Recovery on daemon start** bullet, add: "Each folder is recovered on its own: a marked
one that can't be applied is moved to `.staging/broken/` and kept, and the daemon starts."
After **Multi-file mutations**, add: "Once `COMMITTED` is written the transaction has happened: a
failure applying it is finished later (before the next commit, or by recovery), never reported to
the caller as a failed write."

## Consequences

- A write can return success while its last step is still pending, in practice only when the disk
  is failing or a permission was changed under the daemon. Until it's finished, its event isn't in
  the log yet and some of its files may not be in place, the same state a crash leaves, finished
  the same way. Doctor makes it visible. While one waits, later writes wait behind it: they
  succeed, but nothing they change is visible until the problem is fixed and the next write (or
  the next start) lands them all, in order.
- A damaged staging folder needs a person to look at it once; nothing reads `broken/` again.
- Tests: a corrupt manifest and a missing staged file each leave a running daemon, a kept
  `broken/` folder and a doctor warning; an unwritable log gives a successful add, one record and
  one event once fixed; 20 writers of one file never fail and leave one writer's whole content.
  The commit-speed fix has no timing test (one would be flaky on CI); the duplicate-event recovery
  test covers the path it changes.
