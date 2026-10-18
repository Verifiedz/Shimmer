# Mock daemon fixtures

`shimmer mockd --fixtures crates/mockd/fixtures --socket /tmp/shimmer-mock.sock` reads every `*.json`
in this directory (not subdirectories), in filename order, and answers the protocol from them
(`docs/protocol.md`). Add a case by adding a file or a rule; open a PR to `crates/mockd`.
A bad fixture stops `shimmer mockd` at startup and names the file and the rule.

## Format

```jsonc
{
  "responses": [ /* rules */ ],
  "timeline":  [ /* events */ ]
}
```

### Rules

The first rule, across all files in order, whose `op` matches and whose `params` and `queue`
match wins.

| Field | Required | Meaning |
|---|---|---|
| `op` | yes | Canonical `<module>.<verb>`. Aliases never appear on the wire. |
| `params` | no | Matches when every key given is present in the request's params with a matching value, recursively. Absent matches anything. |
| `queue` | no | The same, against the request's `queue` block. A request without one counts as `{"priority":"normal","confirm":false}`. |
| `data` | one of | The response `data`. Cannot be `null`; use `{}`. |
| `error` | one of | `{code, message, detail}` as in `protocol.md`. |
| `delay_ms` | no | Hold the response back this long. |
| `emit` | no | Events pushed after the response (below). `after_ms` counts from the moment it is sent. |

An op with rules but none matching gets `invalid_params` ("none match"), so a gap in a fixture is
never mistaken for a missing op. An op with no rules gets `unknown_op`.

Built in unless a fixture overrides them: `core.ping`, `core.manifest` (an empty one: the
default lane, no modules), `core.shutdown`. `core.manifest` `data` must parse as the real
`ManifestData` type.

### Events (`emit` and `timeline`)

```json
{"after_ms": 250, "topic": "queue.task.progress", "source": "queue",
 "payload": {"task_id": "01JD2N…", "lane": "fetchers", "fraction": 0.4, "note": "page 2 of 5"}}
```

`topic` is concrete (`<module>.<entity>.<verb>`, no `*`). `source` defaults to the topic's first
segment. An event is delivered only if the connection subscribed to a matching pattern, exactly
as on the real stream. Use real ULIDs for ids (a test checks `task_id` and `before`).

`emit` events belong to the request that triggered them. `timeline` events are replayed once per
connection, starting at its first successful `subscribe`; every file's timeline is merged.

## What ships

| File | Covers |
|---|---|
| `core.json` | `core.manifest`, `queue.list`, `records.list` (a hit and a `not_found`) |
| `confirmation.json` | `fetchers.run` with `override`: refused with `confirmation_required`, accepted when the `queue_version` echoed is 41, refused again for any other |
| `workspace-dirty.json` | `workspaces.activate` on a dirty workspace (`workspace_dirty`), on a healthy one whose launch then fails (events), and the cleanup that recovers it |
| `long-task.json` | a queued fetch that emits `started`, five `progress` events, an item, and `finished` |
| `stream-lagged.json` | a timeline with a `core.stream.lagged` drop in the middle |
