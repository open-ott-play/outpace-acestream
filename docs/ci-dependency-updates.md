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

## Cargo signing dependencies

`ace-wire` currently combines RSA 0.9 and ed25519-dalek 2. Their signing and
key-generation interfaces require `sha1` 0.10 (digest 0.10) and `rand` 0.8
(rand_core 0.6). A grouped update to a newer incompatible minor must not replace
those APIs independently.

Renovate's [allowed version rules](https://docs.renovatebot.com/configuration-options/#allowedversions)
limit only these two direct dependencies in `crates/ace-wire/Cargo.toml`.
Compatible patch updates remain eligible. Other crates and workspace dependency
updates keep their existing policy. Raise the bounds only in a coordinated API
migration with offline signing/key-generation tests, then regenerate the lock.
No production stream or live peer is needed to validate this compatibility.
