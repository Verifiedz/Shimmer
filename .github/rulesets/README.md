# Branch rulesets

GitHub rulesets are repository settings, not files: GitHub does not read this folder. These JSON
files are the rulesets we agreed on, kept here so changes to them are reviewed like code. The repo
owner applies them by hand.

| File | Ruleset name | Applies to | Why |
|---|---|---|---|
| `master.json` | `protect-master` | `master` | Everything reaches `master` through an approved PR with green CI. |
| `integration-branches.json` | `protect-integration-branches` | `post-*`, `integration/*` | Umbrella branches that collect several PRs before going to `master` get the same review, so nothing lands in them unreviewed. |

Feature branches (anything else) have no rules on purpose: their authors must be able to push to
them freely, including after a PR is open. They are protected by the PR into a protected branch.
A ruleset that matches every branch blocks those pushes (`GH013: Changes must be made through a
pull request`) and blocks deleting merged branches. That happened twice: on 2026-10-01 (the old
`master` and `master-rule` rulesets matched every branch) and again on 2026-10-03 (a ruleset
named `other-branches`, since deleted). **Never add a ruleset that matches all branches.**

Both rulesets:

- **Require a PR with 1 approval.**
  - On **`master`** the approval must not come from the person who pushed last
    (`require_last_push_approval`), so an author cannot add commits after someone else's approval
    and merge them unreviewed, and **approvals are dismissed when new commits are pushed**
    (`dismiss_stale_reviews_on_push`): an approval covers exactly the code that was reviewed.
  - On **integration branches** both are **off**. Merging one PR of a stack moves the merge-base
    of the others, which GitHub counts as a change and dismisses their approvals; updating a
    stacked branch counts as a new push. Together that meant re-approving every PR of a stack each
    time one landed (#246–#248, 2026-10-17). Nothing reaches `master` less reviewed: each
    integration branch goes to `master` through an umbrella PR, which has both rules.
- **Require review conversations to be resolved** before merging.
- **Require CI to pass** on both platforms (`check (ubuntu-latest)`, `check (macos-latest)`, the
  job names from `.github/workflows/ci.yml`; update them here if those change).
  - On **`master`**, the PR must also be **up to date with `master`**
    (`strict_required_status_checks_policy: true`): if `master` moved on since CI ran, GitHub
    shows **Update branch**, and CI runs again on the combined code. So two PRs that pass alone
    can't merge into `master` untested together.
  - On **integration branches** it isn't required (`false`): a PR must pass CI on its own
    commits, but needn't be re-run each time a sibling PR lands in the umbrella. The umbrella
    itself is held to the stricter rule when it merges into `master`.
  - Note: updating the branch adds a commit, so with stale-approval dismissal (above) a `master`
    PR may need re-approving after **Update branch**.
- **Allow only merge commits**, no squash or rebase merges. Stacked PRs (a PR based on another
  PR's branch) break when their base is squash-merged.
- **Block force-pushes.** `master` also blocks deletion; integration branches can be deleted once
  merged.
- **Have no bypass list**, including for the owner, so the rules apply to everyone.

Naming: give an umbrella branch a `post-` or `integration/` prefix (e.g. `post-m2-fixes`,
`integration/workspaces`) to get these rules.

## Applying them (repo owner)

From an up-to-date clone, either in the browser:

**Order matters:** import the new rulesets first, then delete the old ones. Deleting first leaves
`master` unprotected in between.

The old rulesets this replaces, all on `master`: `Master Branch`, `master` and `master-rule`
(the last two also carry a stray `required_deployments` rule). `other-branches`, which matched
every branch except `master`, was already deleted on 2026-10-03.

In the browser:

1. Settings → Rules → Rulesets → **New ruleset** → **Import a ruleset** → pick a file here.
2. Check the imported rules, then **Create**. Repeat for the other file.
3. Delete `Master Branch`, `master` and `master-rule` (each ruleset's page → **Delete**).

Or with the GitHub CLI. Deletes look each ruleset up **by name** and print it first, instead of
trusting hardcoded ids that change when rulesets are recreated:

    gh api -X POST repos/Verifiedz/Shimmer/rulesets --input .github/rulesets/master.json
    gh api -X POST repos/Verifiedz/Shimmer/rulesets --input .github/rulesets/integration-branches.json

    # See what exists: the two new ones (protect-*) and the three old ones.
    gh api repos/Verifiedz/Shimmer/rulesets --jq '.[] | "\(.id)  \(.name)"'

    for name in "Master Branch" "master" "master-rule"; do
      id=$(gh api repos/Verifiedz/Shimmer/rulesets --jq ".[] | select(.name==\"$name\") | .id")
      echo "deleting $name ($id)"
      [ -n "$id" ] && gh api -X DELETE "repos/Verifiedz/Shimmer/rulesets/$id"
    done

The new rulesets are named `protect-*` so they can never be mistaken for the old `master` when
deleting by name.

To change a ruleset later: edit its file here in a PR, then apply it with
`gh api -X PUT repos/Verifiedz/Shimmer/rulesets/<id> --input <file>` (or in Settings).

Check with `curl -s https://api.github.com/repos/Verifiedz/Shimmer/rules/branches/<branch>`:
`master` and `post-*` branches list the rules above, any other branch returns `[]`.

These are applied by hand on purpose. A GitHub Action could apply them automatically, but it
would need an owner-level token stored as a secret, letting anyone who can edit a workflow
rewrite the protection rules.
