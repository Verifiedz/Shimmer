# 0030. Records catch up with workspaces: categories, peek, schedules, copy, edit, origin

Status: proposed · Raised by Dev B · Needs sign-off: Dev A (ops in `records`, docs/protocol.md;
the CLI reads `scheduler.list` and calls `scheduler.add`/`remove`) · Dev C notified · No `core`
or `proto` change

## Context

ADR 0025 and its follow-ups gave workspace templates a set of commands people asked for once
they had more than a handful of templates: categories, `peek` to see one before making it,
`rename` that moves the schedules pointing at the old name, `copy`, `edit` and remembering
which template a workspace came from. Record collections have had templates since ADR 0022, and
sixteen of them now, but none of that: `records templates` is one flat table, the only way to see
what a template holds is to create it, and renaming a collection silently breaks any schedule
that names it.

Some workspace commands don't apply. Record templates have no questions, so there is nothing to
`reconfigure`; collections don't run, so there is nothing to `stop`; and they have no scripts, so
`peek --scripts` becomes `peek --file`. Rename, remove/restore, trash and `new` already exist.

This ADR covers four changes, each its own PR into `integration/records-parity`.

## Decision

### 1. Categories and `peek` (PR A)

Each built-in template belongs to one category, set in the module next to the template
(`templates.rs`), not in the template file: the file is exactly what the person gets (ADR 0022
§1), and a category means nothing once it is theirs.

| Category | Heading | Templates |
|---|---|---|
| `job-hunt` | Job hunt | interview-questions, interviews, job-applications, networking-events, offers, outreach, stories |
| `practice` | Practice | leetcode |
| `career` | Career history | certifications, education, employment, projects |
| `life` | Life admin | addresses, charity, documents, subscriptions |

`records.templates` takes `{"id"?, "file"?}`, both optional, so `{}` still works:

- templates come sorted by category (in `categories`' order), then id, and the result has
  `"categories":[{"id","label"}]` for the headings;
- each template is still the collection as `records.collections` shows it, plus `category`,
  `notes` and `examples`, read from its header comment: `examples` are the lines under
  `How it works:` as written, and `notes` every other paragraph after the first, each joined
  into one line (privacy warnings, what it can't do, what goes with it). The first paragraph is
  the description and "Created from…", which say nothing new before it's created;
- `id` returns only that template (`not_found` naming the others); `file: true` adds `file`,
  the template's text exactly as `records.create_collection` copies it.

A test holds every built-in to this: a known category, and at least one `shimmer records`
example under `How it works:`.

CLI:

```
shimmer records templates [CATEGORY | TEMPLATE]   grouped under headings; a category shows only
                                                  its own; a template's name shows it in full
shimmer records peek TEMPLATE [--file]            everything about one: description, fields,
                                                  what completing does, notes, examples, what
                                                  goes with it; --file prints the file exactly
```

`peek` says what completing does in words, from `completable`, `stamp_on_complete` and
`repeat_complete` (ADR 0021), since that's the part of a collection people can't guess from its
fields. A category's name given to `peek` points at `templates CATEGORY`.

### 2. Schedules follow a rename (PR B)

A schedule is a trigger whose `op` and `params` name a collection, e.g. `records.list
{"collection":"jobs"}`. When a collection or record is renamed, those triggers are left pointing
at a name that no longer exists, and fail every time they fire.

The scheduler doesn't know what its triggers' params mean (§1.2), and `records` must not reach
into the scheduler (§1.3). So, as for workspaces, the CLI does it, with ops that already exist:

- `records rename-collection ID NEW_ID` finds triggers whose `op` starts with `records.` and
  whose `params.collection` is `ID`. `records rename COLLECTION/ID NEW_ID` finds those whose
  `params.collection` is `COLLECTION` and `params.id` is `ID`.
- After the rename succeeds, it lists them. At a terminal it asks whether to move them;
  `--move-schedules` moves them without asking; anywhere else it lists them with the command to
  move each by hand. Moving one is `scheduler.add` of a copy with the new name, then
  `scheduler.remove` of the old one, so a failure part-way leaves the old one in place rather
  than neither.
- `records remove-collection ID` lists the schedules that name it after removing it: they will
  fail until it is restored, or until they are removed (`shimmer scheduler remove TRIGGER`). It
  never removes them itself: restoring the collection makes them work again.

The listing and moving helpers move out of `cli/workspaces.rs` into a shared `cli/schedules.rs`.

### 3. `records copy-collection` (PR C)

`records.copy_collection {"id","to","label"?,"with_records"?}` copies a collection's file with
only its id (and label, when given) changed, like `records.create_collection` does a template's,
so comments and layout are kept. A `ref` field that points at the collection itself is pointed at
the copy, so the copy's records refer to each other rather than into the original.

With `with_records: true`, every live record is copied too, statuses and stamps unchanged; the
trash is not. Without it the copy starts empty: the common case is "another tracker like this
one", e.g. a second job hunt.

One transaction writes everything and emits `records.collection.copied {"collection": to,
"from": id, "records": N}`, plus one `records.item.created` per copied record, as
`records.import` does, so anything that reacts to new records (the calendar's deadlines, §1.3)
sees them. `conflict` if `to` exists; `not_found` if `id` doesn't.

CLI: `shimmer records copy-collection ID NEW_ID [--label TEXT] [--with-records]`.

### 4. `records edit` and where a collection came from (PR D)

`shimmer records edit COLLECTION [--path]` opens the collection's file in `$VISUAL`, `$EDITOR`,
`nano` or `vi`, like `workspaces edit`, then runs `records.check` on it and prints the report, so
a field renamed by hand that no longer fits its records is caught at once. `--path` prints the
path only. It is a client command: the daemon is still the only writer of anything it manages,
and the collection file is the one file ADR 0022 hands to the person to edit.

Each collection in `records.collections` gains `template`: the built-in it was made from, read
from the "Created from Shimmer's X template" line its header starts with, or absent for one
made by hand, one whose line was edited away, or one whose template no longer exists. Nothing
new is stored: the file already says it. `records collections` shows it, and `peek` of that
template is the way back to its notes and examples.

## Consequences

- Templates can be browsed and inspected before anything is created, and the list stays usable
  as it grows.
- Renaming no longer silently breaks schedules, and removing says what will break.
- No new file, no new event other than `records.collection.copied`, no `core` or `proto` change.
- `records.templates` with `{}` returns the same templates as before in a different order, with
  more keys; a client that sorted by id itself is unaffected.

## Pack aliases

`records peek`, `copy-collection` and `edit` join the commands a pack can name (ADR 0029), as
`records.peek`, `records.copy_collection` and `records.edit`; `peek` and `edit` have no op of
their own, so they're named like `workspaces.edit`. The built-in packs name them:

| Command | `anime-tropes` | `ship-it` | `starship` | `short` |
|---|---|---|---|---|
| records peek | `hora` | `preview` | `probe` | `rpeek` |
| records copy-collection | `futago` | `copy-pasta` | `twin-fleet` | `rcpcol` |
| records edit | `kaizou` | `tweak-schema` | `drydock` | `redit` |

## Not done here

- User-defined template folders: built-ins only, as in ADR 0022.
