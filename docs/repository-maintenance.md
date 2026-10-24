# Repository maintenance

The default branch requires one current approval and the five configured CI
checks. Version tags cannot be rewritten or deleted. Infrastructure settings are
managed by the organization's Terraform repository.

The organization permits automated approval. This repository opts into its
pinned metadata-only reviewer for non-draft, same-repository pull requests from
the explicitly listed trusted authors, after a maintainer adds the `automerge`
label. It uses the existing organization `BOT_PAT`, never checks out or executes
pull-request code, refuses self-approval and checks the current head and base
before approving. It does not merge a pull request or waive CI requirements.
Fork and dependency-bot pull requests are outside this wrapper's scope.

An automated approval is not an independent human security review. Changes to
protocol correctness, source distribution, credentials or release policy still
need substantive review appropriate to the change.
