# 0033. `shimmer doctor`, and a `<module>.health` op per module

Status: proposed · Raised by Dev B · Needs review: Dev C (CLI and modules only) · Dev A notified
(two new rows in docs/protocol.md) · No `core` or `proto` change

## Context

When something goes wrong there is no one place to look. A dirty workspace shows in
`workspaces list`, a record that doesn't fit its collection only in `records check COLLECTION`
(one collection at a time), a collection file that won't parse only as `skipped` in
`records collections`, a broken command pack as one warning line on every command. A person has
to know each of these exists to find the problem.

CLAUDE.md §2 says a client holds no business logic, and §9 says a client must not hardcode module
knowledge. So `shimmer doctor` can't decide itself whether a workspace is broken, and can't have
a list of modules to ask.

## Decision

1. **A module may answer `<module>.health`**: inline, params `{}`, writes nothing. It answers
   `{"checks": [Check…]}`, where a check is

   ```json
   {"status": "ok" | "warn" | "fail", "message": "…", "subject": "…"?,
    "fix": {"summary": "…", "op": "…"?, "params": {…}?}?}
   ```

   - `fail`: something can't be used until it's fixed (a dirty workspace, a collection file that
     won't load). `warn`: works, but something is off (records that don't fit, a workspace whose
     detached steps nothing can stop). `ok`: one summary line, only when there is nothing else.
   - `message` stands on its own and names what it's about. `subject` is the id, for clients.
   - `fix.summary` says what to do, in plain words. `fix.op` and `fix.params`, when there is one
     op that does it, are a canonical op and its params — never a command line, never an alias
     (§15, §12 rule 16). The client turns them into a command.

2. **`shimmer doctor`** (CLI) runs these checks, in this order:
   - **daemon**: reachable (started as any command would, and says so when it had to), answers
     `core.ping`, and the `welcome` frame's `daemon_version` is the CLI's own version.
   - **packs**: the active pack's load warnings (packs are the client's own, §15.1).
   - **every module** in `core.manifest` that lists a `<module>.health` command, in manifest
     order. A module added later is checked with no CLI change; one without the op is skipped.
     An error from the op is shown as a `fail` line, and an answer that doesn't fit the shape is
     shown as it came, never dropped.

   A `fix` becomes `shimmer <words> <arg>` when the op has a command of its own (the CLI's pack
   target table) and its params are one plain value; otherwise `shimmer call OP 'PARAMS'`, which
   always works. `--json` prints the checks as `{"checks":[{"area","status","message","fix"}]}`.
   Exit 1 when any check is `fail`, 0 otherwise. Doctor never changes anything.

3. **First two modules:**
   - `records.health`: `fail` for each collection file that won't load; `warn` for each
     collection whose records `records.check` finds problems in, fix `records.check` on it.
   - `workspaces.health`: `fail` for each workspace that can't be read; `fail` for each dirty one
     with its step, reason and log, fix `workspaces.cleanup` (or `workspaces.reset` when it has no
     cleanup script); `warn` for each with a detached step and no cleanup script.

4. **`doctor` is not aliasable** and is a reserved word for packs, like `packs`: it is how a
   person finds out a pack is broken, so a pack must never stand in its way (ADR 0013 §6). It is
   also exempt from the "pack not loaded" warning at the top, because it lists that itself.

## Consequences

- New: `crates/cli/src/doctor.rs`; `records.health` and `workspaces.health`; two rows in
  docs/protocol.md; `doctor` in the packs reserved list. The CLI's `Client` keeps the
  `daemon_version` from `welcome`.
- Daemon-level checks (index needs a rebuild, leftover `.staging/` transactions, the queue) are
  Dev A's: a `core.health` op in the same shape would be shown by doctor with no CLI change.
  Not part of this ADR.
- Not closed: #194 (same version, different build) needs a build id in `welcome`, which is a
  `proto` change.
- Later, from notify (ADR 0032) and calendar: their own `.health` ops (e.g. desktop notifications
  can't be shown here, messages held by quiet hours).
