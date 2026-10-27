# Shared CI dependency updates

The candidate assessment and security scan use the same immutable CI toolkit
revision. The candidate's `uses` reference, its `with.toolkit-ref` checkout and
the security workflow reference must move together. Keep the `# main` ref
annotations: Renovate needs them to discover updates to SHA-pinned workflows.

The identifier-hygiene gate also requires every legitimate Git revision to be
listed in `tools/hygiene/allowed-identifiers.txt`. The marked shared-toolkit
entry is managed by Renovate in the same `ci-workflows` update group as the
workflow references and runtime checkout. Only that explicitly marked entry is
updated; other reviewed hashes and the ban on content identifiers remain in
force. Review the upstream toolkit changes before accepting a new hash.

CI workflow updates retain manual review (`automerge: false`). The workflow
contracts check that the candidate and security references agree, the marked
hash entry matches them, and all three updater types share the same group.
Rust-toolchain updates use a separate custom manager and are not included in
this group. Candidate assessment remains an explicit, read-only operation;
updating the references does not run it or deploy an application.
