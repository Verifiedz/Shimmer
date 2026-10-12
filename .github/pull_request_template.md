<!-- What this changes and why, in a few lines. Stacked? Say what it sits on. -->



Closes #

### Checklist (delete lines that don't apply)

- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test --workspace` pass (CLAUDE.md §12 rule 12), and so does `python3 .dev/check-deps.py` (the dependency rules, §3)
- [ ] Small enough to review; bigger work is split into stacked PRs (the size limit is still being decided in #154)
- [ ] Base branch is `master`, an `integration/*` or `post-*` umbrella, or the PR this one is stacked on (CONTRIBUTING, Branch workflow)
- [ ] New or changed op or event: `docs/protocol.md` **and** the fixtures in `crates/mockd/fixtures/` updated, Dev C told; a breaking change also bumps the protocol version `v` (§4, §9)
- [ ] A design choice CLAUDE.md doesn't answer: ADR in `docs/decisions/` (§12 rule 14)
- [ ] Touches `crates/core` or `crates/proto`: ADR and sign-off from Dev A and Dev B (§4)
- [ ] Touches a crate the author doesn't own (§13), e.g. `crates/app` or `crates/mockd`: its owner reviews
- [ ] New module, `Collection`, `Source` or `Sink`: tested against the in-memory `Ctx` (§12 rule 13); a new built-in template: covered by its module's every-template tests
