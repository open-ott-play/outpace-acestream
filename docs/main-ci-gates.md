# Main branch merge gates

Repository policy is managed by the organization's Terraform configuration in
[`4alvit/terraform-github-open-ott-play`](https://github.com/4alvit/terraform-github-open-ott-play).
Do not apply repository rulesets directly from this checkout. The historical
[`.github/main-ruleset.json`](../.github/main-ruleset.json) is an upstream reference,
not the active policy or an apply payload for this fork.

The `Public software - required review and CI` ruleset targets the default
branch, requires a pull request, and blocks deletion and non-fast-forward
updates. Ordinary merges require one approving review, dismiss stale reviews
after new commits, require approval of the most recent push and require
resolution of review threads. The branch must include the current base before
an ordinary merge.

The 2026-10-24 readback had no bypass actors. The subsequent
[Terraform owner policy](https://github.com/4alvit/terraform-github-open-ott-play/blob/main/docs/public-security.md#emergency-owner-merges)
permits an explicit organization-owner emergency merge **through a pull request
only**. Record the reason, reviewed commit and available check evidence in the
PR. This does not authorize direct pushes or grant automation a bypass. Required
checks remain mandatory for ordinary merges, and missing or failed checks must
not be described as passing. Read back the live rulesets to verify which policy
has been applied.

The five required check contexts are bound to the GitHub Actions app:

- `Rust lint`
- `Rust test`
- `A/B harness test`
- `HLS audio synthetic`
- `acestream identifier hygiene`

Their workflow names and conditions are described in [CI/CD](ci-cd.md). Do not
rename required job contexts without a coordinated, reviewed Terraform change.
These checks have no path filters; Compose and ARMv7 supplemental checks do.
A green branch-push run cannot substitute for a failing PR merge-commit run.

The `Release standard - immutable version tags` ruleset prevents updating,
deleting or force-pushing version tags, with no bypass actors. The existing
stable publisher and complete-source acceptance are described in
[releasing](releasing.md).

## Bots and approvals

The read-only access inventory on 2026-10-24 confirms `californiantiramisu` has
write access and repository platform automerge is enabled. The organization
makes `BOT_PAT` available to this repository; no credential values were read.
The metadata-only approval workflow is limited to its explicitly trusted,
same-repository authors and opt-in `automerge` label. It never executes PR code
and does not merge a PR or waive checks. Dependency-bot and fork PRs are outside
that wrapper's scope. Automated approval is not substantive security review.

Renovate configuration does not prove its installation can access this repository.
Verify its runner or app separately. Any required access, automerge setting or
ruleset change belongs in the Terraform repository and needs review within that
scope, not an ad hoc API mutation here.

## Read-only verification

Read the repository's effective policy without changing it:

```sh
gh api repos/open-ott-play/outpace-acestream/rulesets
gh api repos/open-ott-play/outpace-acestream/rules/branches/main
gh api repos/open-ott-play/outpace-acestream --jq '{allow_auto_merge,default_branch}'
```

When a PR is blocked, inspect its current head, base, reviews, unresolved review
threads and check runs. Correct the cause and wait for the required checks.
Never remove protection or bypass a failed check to complete a release. GitHub
configuration changes must go through the Terraform owner and be read back after
application; this documentation does not apply settings automatically.
