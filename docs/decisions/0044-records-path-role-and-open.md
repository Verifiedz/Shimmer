# 0044. Records: a `path` field role, and opening it from the CLI

Status: proposed · Raised by: Dev B (user request — quick access to a record's own files,
e.g. a tailored résumé on a job application) · Needs sign-off: Dev A (`docs/protocol.md`'s
role table) · Dev C notified (new role value; a new CLI-only command, in case the TUI later
wants to render or trigger it too) · No `core` or `proto` change

## Context

A user tracking job applications in `records` wants to get from a record straight to a file
they manage themselves outside Shimmer — most concretely, a résumé they tailored for that
specific application, or a saved copy of the job posting. Today a collection can only store
such a thing as a plain `string` field (CLAUDE.md's `documents.toml` template already has
`copy_location`, `where_kept` for exactly this), which records it but gives no way to *act*
on it: nothing marks that string as "a path, not just text," so nothing can open it, and
nothing renders it as something you can click.

This is a field-*meaning* problem, which `records` already has a mechanism for: field `role`
(ADR 0017 §3). A role doesn't change how a value is stored or validated; it tells other code
what the field *means* so that code can act on every collection that uses it, forever, without
`records` knowing those collections exist (CLAUDE.md §8). `deadline` and `url` are the two
roles that exist today; this ADR adds a third: `path`.

**Why "open" can't be a module/`Ctx` capability.** The obvious place to put "open this file"
would be `Ctx`, next to `ctx.launcher` or `ctx.desktop`. It can't go there. `Ctx.launcher`
(ADR 0010) is deliberately scoped to a *logical* step inside one workspace's own directory —
its doc comment is explicit that nothing upstream of the daemon's backend may "name a script
outside a workspace's own directory," and `core`'s `Ctx` forbids a raw `PathBuf` to anywhere
in general (CLAUDE.md §5: "Forbidden in `Ctx`, permanently... a way to name a script outside a
module's own workspace directory"). A `records` field can point at any path on the user's
disk — that is the whole point of the feature — so handing that path to a daemon capability
would mean handing the daemon (and by extension, any future module) the ability to run an
arbitrary local file. That is a capability decision for which this ADR has no mandate, and
§1.5-style posture suggests it would be the wrong one even with a mandate.

What the CLI already does, elsewhere, is the right shape instead: `crates/cli/src/editor.rs`
opens a file in `$EDITOR`/`$VISUAL` for `records edit` and `workspaces edit` — the client
shelling out to a local program as a *presentation* action on a value it already has, not a
decision about data (the daemon still owns and re-validates whatever gets saved). "Open this
résumé" is the same category of action, just handed to the OS's own file-type association
instead of to an editor.

## Decision

### 1. `role = "path"`: a string field that names a file

```toml
[[field]]
name = "resume_path"
type = "string"
role = "path"
```

- Allowed only on `string` fields, checked the same way as the existing `role = "url"` check
  in `schema.rs`'s `Collection::parse` (role on the wrong type is `invalid_params`, naming the
  file).
- **Unlike `url`, not capped at one per collection.** `url` means "the record's one canonical
  link"; `path` has no such single-canonical-thing meaning — a job application might
  reasonably have `resume_path` and `cover_letter_path` both set. A collection may define as
  many `role = "path"` fields as it wants.
- Stored and validated exactly like any other `string` — no new `FieldType`, no new wire
  shape. The daemon/module never checks that the path exists: `schema.rs` lives in a module,
  and a module can only touch its own namespaced store (`ctx.store`), never an arbitrary
  filesystem path (§12 rule 4) — so existence-checking is structurally a client-side-only
  question, never a schema one, and this ADR doesn't attempt it anywhere.
- No expansion (`~`, environment variables) happens in `records` or on the wire. If `~`
  expansion is wanted, it happens client-side, only at the moment the CLI opens the path (§3)
  — the stored value is exactly what the user typed.
- Same portability caveat the `documents.toml` template already carries for `copy_location`:
  the field stores a pointer to a file that lives outside `$SHIMMER_HOME`, so it stops
  resolving if you copy `$SHIMMER_HOME` to a machine that doesn't have that file at that path.
  That is inherent to pointing at user-managed files and is not new to this feature.

### 2. `records show` renders a `path` field as a clickable link

When stdout is a terminal (same gating `style.rs` already uses for colour: a real TTY, not
`--json`, not a "dumb" term — `should_color`'s posture, reused, not colour itself), a set
`role = "path"` field's value is wrapped in an OSC 8 hyperlink escape sequence pointing at
`file://<value>`. Terminals that support OSC 8 (most modern ones) render it as a clickable
link; terminals that don't simply show the plain text, so the field is always at least
copyable. This needs no subprocess and no app-detection — it is pure output formatting,
`crates/cli/src/render.rs`'s job.

### 3. `shimmer records open <collection>/<id> [field]`

A new, CLI-only command, with no new daemon op — it's built entirely out of the existing
`records.show`:

1. Fetch the record (`records.show`, unchanged).
2. Resolve which field to open:
   - `field` given: use it; error if it isn't a `role = "path"` field of that collection.
   - `field` omitted, exactly one `role = "path"` field is **set** on the record: use it.
   - `field` omitted, zero or more than one are set: error, naming the candidate field(s), so
     the next command the user types just adds the field name.
3. Open the resolved value with the OS's own file-type association — `xdg-open` on Linux,
   `open` on macOS, `cmd /c start` on Windows, selected by `cfg!(target_os)` — the same
   shell-out-and-check-the-exit-status posture `editor.rs::open` already uses for `$EDITOR`.
4. If there is no such opener, or it fails to run, or it exits non-zero: don't hard-fail
   silently. Print the resolved path plainly (so the "or copying the path" half of the
   original ask is always satisfied, even on a machine with no file-association system at
   all, e.g. a bare container) alongside the error, the same "never leave the caller checking
   a file nobody could touch" posture `editor.rs` already takes on its own failure path.

This is deliberately **not** "enumerate installed apps and let the user choose." There is no
uniform cross-platform way to answer "what can open a `.pdf`" — on Linux in particular it
means parsing desktop-entry/MIME association files by hand — and the OS's own default
association already encodes "what the user wants to open this with" for the overwhelming
majority of cases. If the OS default turns out to be wrong often enough to matter, the
cheaper fix is a future per-field `opener = "..."` override key (flagged below, not decided
here), not app enumeration.

## Consequences

- `crates/modules/records/src/schema.rs`: `Role` gains `Path`; `Collection::parse`'s role/type
  check gains the `(Role::Path, FieldType::String)` case, and — unlike `Role::Url` — no
  "at most one" check.
- `docs/protocol.md`: the role line (currently `"deadline" on a date or datetime, "url" on a
  string`) gains `"path" on a string`. `records.collections` needs no other change — a field's
  `role` already passes through untouched.
- `crates/cli/src/records.rs`: a new `RecordsCmd::Open { collection, id, field: Option<String> }`,
  parsed and dispatched the same way `Edit` is today.
- `crates/cli/src/opener.rs` (new, sibling to `editor.rs`): the OS-opener shell-out and its
  fallback-to-printing-the-path behaviour, unit-tested the same way `editor.rs::editor_command`
  is — as a pure function of "which OS, does this program exist" rather than actually spawning
  anything in tests.
- `crates/cli/src/render.rs`: the OSC 8 wrapping for a `role = "path"` field in `records show`,
  gated on the same TTY/`--json` check `style.rs` exposes for colour.
- Templates: `job-applications.toml` gains an example `resume_path` field (`role = "path"`),
  the same way `leetcode.toml` gained `title = "{title}"` in ADR 0017 — a concrete worked
  example, not a requirement that every collection add one.
- No `core` or `proto` change, no version bump: `role`'s wire shape is unchanged (still a
  string on the `Field` the client already passes through); this only adds an allowed value.

## Not done here

- **Opening long free-text fields (a job description, match notes) comfortably from the
  CLI.** That's a separate, smaller problem — `type = "string"` already stores arbitrarily
  long text with no schema change needed at all; the gap is purely CLI input ergonomics
  (e.g. an `--edit <field>` flag that round-trips through `$EDITOR`). It doesn't touch `role`
  or the schema, so it doesn't need an ADR — tracked separately, not blocked on this one.
- **A per-field `opener = "..."` override**, for when the OS default picks the wrong program.
  Flagged in §3; add only if the OS-default behaviour proves insufficient in practice.
- **Tracking that a path was opened** (a `records.item.opened` event, for a future "you
  haven't looked at this in N days" automation). `open` as specified here is a pure read plus
  a local action — nothing is written back to the record or the event log.
- **Checking that the path exists**, at write time or at open time beyond "the opener command
  failed." Structurally a client-side-only question (see §1); not attempted here even
  client-side — `open`'s own failure path already surfaces "couldn't open it," which covers
  "it doesn't exist" without a dedicated check.
- Windows and macOS openers (`cmd /c start`, `open`) are specified but, like ADR 0010's own
  Windows launch backend, not necessarily exercised by CI the way the Linux `xdg-open` path
  is — same posture, not a new gap.
