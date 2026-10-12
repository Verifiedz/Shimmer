# 0029. Command packs can alias every command

Status: accepted (2026-10-07). Amends ADR 0013 §6, ADR 0021 §6, ADR 0022 §4, ADR 0025 §8.

## Context

ADRs 0021, 0022 and 0025 kept the rarer and destructive commands out of packs ("type them in
full, on purpose"). In use, a pack that covers only some commands feels unfinished, and every
destructive command already asks before it acts (`purge`, `remove-collection`, `reset`,
`force-relaunch`) or can be undone (`remove` → `restore`). An alias doesn't skip either.

## Decision

1. A pack may alias **every** command except the plumbing ADR 0013 §6 already excludes
   (`daemon`, `mockd`, `call`, `packs`): a broken pack must never get in the way of fixing it.
   That adds 22 targets (50 in all): `records templates|new|trash|purge|import|export|check|
   rename-field|rename-collection|remove-collection|restore-collection`, `workspaces stop|remove|
   restore|rename|copy|reconfigure|edit|templates|peek|new`, `scheduler show`.
2. An alias's value stays a canonical op. A command with no daemon op of its own (`records
   export`, `workspaces edit`, `workspaces peek`) uses its canonical command name in the same
   form (`records.export`, `workspaces.edit`, `workspaces.peek`); two commands never share one.
3. The four built-in packs name all 50, once each (the existing test checks this). The new names
   follow each pack's style and the faith rule (e.g. starship avoids `rechristen`).

## Consequences

- `crates/cli/src/packs/pack.rs` `TARGETS`, the `shimmer packs` help, the four
  `crates/cli/packs/*.toml`, and tests. User packs may add the new aliases or not.
