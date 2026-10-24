# CI and dependency maintenance

Outpace keeps its Rust and release checks in this repository. Shared workflows
are used where their contract fits: metadata-only pull-request approval and the
security analysis and bounded RC source assessment described in [fuzzing](fuzzing.md). They do not
replace Outpace's release versioning or AGPL source-distribution checks.

## Pull-request checks

Every pull request and push to `main` runs the following CI jobs, with stable
names because branch protection requires those exact check contexts:

- **Rust lint** checks workspace formatting and Clippy, plus the standalone fuzz
  crate's formatting and Clippy. Both crates use the pinned project toolchain.
- **Rust test** runs the offline workspace tests and deterministic fuzz seed
  replay. Mutational fuzzing does not run on a pull request or every beta.
- **A/B harness test** runs the Python capture/analysis, source-isolation and
  release-archive verification and workflow-contract tests. The YAML parser for
  contract tests lives in a disposable virtual environment. Network fixtures use
  loopback only.
- **HLS audio synthetic** requires FFmpeg/FFprobe and checks synthetic codec and
  lifecycle behavior. Missing required tools fail rather than silently skip.
- **acestream identifier hygiene** scans tracked files on every push and pull
  request. Real content identifiers never belong in tests, corpora or reports.

These required jobs have no path filters. Compose and ARMv7 portability checks
remain supplemental and path-filtered; emulated container tests require their
existing explicit manual inputs. The seven release binary targets and three
container architectures are maintained separately.

Third-party Actions are pinned to full upstream commit SHAs. Each newly approved
SHA has a justified entry in `tools/hygiene/allowed-identifiers.txt`; a workflow
pin is not a real stream identifier. The allowlist must never be generated from
arbitrary rejected strings. Local workflow checkouts do not persist credentials. Workflow permissions are
read-only unless a job uploads CodeQL results or publishes a release or image.

## Shared security analysis

The public-only `Security analysis` workflow calls the pinned shared security
workflow on pull requests and pushes to `main`. CodeQL analyzes Rust, Python and
Actions; Trivy checks the checkout and Cargo lockfiles for fixable high/critical
vulnerabilities and secrets; dependency review rejects newly introduced
high/critical vulnerable dependencies in pull requests. These are supplemental
checks and do not rename the five required checks or change branch protection.
Only the CodeQL-capable caller grants `security-events: write`; the source fuzz
assessment retains `contents: read` and receives no secrets.

## RC source assessment

The manual RC workflow is accepted only for a **public** repository, from the
**default branch**, with the explicit `channel=rc` input. It checks the exact
commit selected by that dispatch. The shared runner and its checkout use the
same reviewed, immutable toolkit revision. Target commands, corpora, limits and
invariants are documented in [fuzzing](fuzzing.md).

This is a source assessment, not a product RC publication. Its schema-3 candidate
report binds the results to the repository, source SHA, workflow run and run
attempt. Failed targets retain diagnostics and make the run fail. A successful
report remains an Actions artifact; it neither creates a version tag nor
publishes a GitHub Release or container. It must not be used as release
qualification or substituted for the source and archive acceptance described in
[releasing](releasing.md).

Run the assessment after the reviewed change is on the default branch:

```sh
gh workflow run fuzz-candidate.yml --ref main -f channel=rc
gh run list --workflow fuzz-candidate.yml --event workflow_dispatch --limit 5
gh run view RUN_ID
gh run download RUN_ID --name fuzz-candidate-ATTEMPT --dir rc-evidence
```

Use the selected run ID and its current attempt number in the last commands.
Confirm the run succeeded and inspect `fuzz-report.json`: the repository, source
SHA and attempt must identify the intended run, each configured target must
report successful execution, and the report must remain candidate evidence.
When a target fails, inspect the separate `fuzz-diagnostics-ATTEMPT` artifact;
no successful candidate report is published for that failed campaign.

To evaluate a new source revision, dispatch another RC assessment after its
reviewed changes reach the default branch. A green report for an older commit
does not establish results for newer code. Keep discovered crash inputs
synthetic and identifier-free, fix the production bug, add a deterministic
regression test, then repeat the assessment on the new commit.

## Renovate

`renovate.json` retains the recommended Cargo, Docker and GitHub Actions managers
and the custom Rust toolchain manager. `rust-toolchain.toml` remains the one
project compiler version; beta/nightly Rust releases are not proposed for it.
Security update handling is inherited from the recommended preset and has not
been disabled by a CI-only preset.

The shared CI conventions are applied locally: action and reusable-workflow
updates form one group, including major and digest updates; the shared runner
checkout pin is updated with the reusable-workflow pin by the shared JSONata
manager pattern. Pins use immutable
SHAs; stale branches rebase when behind the base; runner and `uses-with` migrations
stay explicit. CI updates require review and do **not** automerge. Review the
upstream commit, add its justified SHA allowlist entry, and rerun all checks in
the same PR. Do not disable identifier hygiene to accommodate a bot update.
Non-major Cargo dependencies retain their existing group and platform-automerge
request. Major updates require review. Three concurrent dependency PRs and two
new PRs per hour limit update churn; these limits do not disable security alerts.

Platform automerge does not bypass GitHub's required approval or checks. The
metadata-only approval wrapper intentionally excludes dependency bots. Repository
configuration alone does not install or authorize Renovate: its GitHub App or
organization runner must already have access. Check the bot's actual runs and
Dependency Dashboard before claiming automatic updates are operating.

## Validation and repository policy

Run `actionlint`, `python3 tools/hygiene/check_identifiers.py`, the ordinary Rust
checks, and `npx --yes --package renovate@44.169.0 renovate-config-validator
renovate.json` when changing CI. New action pins should be resolved against the
upstream repository and reviewed with their human-readable version comments.
See [main CI gates](main-ci-gates.md) for the effective branch policy and its
Terraform ownership. No workflow or repository-policy change should weaken the
required checks to make a pull request pass.
