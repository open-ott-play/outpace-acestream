# Contributing

Use [GitHub issues](https://github.com/open-ott-play/outpace-acestream/issues) for non-sensitive bug reports, questions and
feature proposals. Include the exact version/commit, environment, expected and
actual behavior, and a minimal sanitized reproduction. Check existing issues
first and keep follow-up evidence in the original thread. For vulnerabilities,
use the [private security process](SECURITY.md).

Submit a focused pull request against `main`. Describe the user-visible problem,
the resulting behavior, compatibility implications and checks performed. Preserve
existing authorship and third-party license/provenance records. Discuss changes
to protocols, storage, device safety or dependency/runtime requirements before
making an incompatible change. English is the common language for code review
and project documentation.

## Development and validation

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 tools/hygiene/check_identifiers.py
```

See [development](docs/development.md) for clone, build and focused-test guidance.
The release workflow and [Linux portability](docs/linux-portability.md) record the
project's target build setup.

The [CI workflow](.github/workflows/ci.yml) is the authoritative list of required jobs.
Use isolated test data and temporary outputs. Never run a device write, unlock,
deployment or publication command merely to validate a documentation change.

## Test and review policy

Changes to behavior must add or update automated tests that fail for the old
defect and cover the new boundary; regression fixes should include the relevant
failure case. If automation is infeasible, explain why in the PR and document
the reproducible manual procedure and limits. Update user/API documentation and
release notes for user-visible changes. Keep compiler, lint, static-analysis and
test assertions enabled, resolve new warnings, and document any remaining
warning with its reason and scope. Do not suppress a real security finding to
obtain a passing check. Wait for required checks and independent review before
merging; do not use an administrator bypass.

## Contribution rights

Contribute original material under the root AGPL-3.0-or-later license only when
you have the right to do so. Preserve third-party attribution and license terms;
record imported code and assets in `NOTICE` or the linked component notices. A
public download URL alone is not a redistribution license.

See [conduct](CODE_OF_CONDUCT.md) and the
[roadmap](docs/roadmap.md). Major functionality changes require meaningful automated
regression coverage and relevant negative cases, with test invocation documented
in the pull request. The same automation exception above applies: explain why automation is infeasible and document a reproducible
manual procedure and its limitations in the pull request.
