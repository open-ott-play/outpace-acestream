# Release and source acceptance

The product release path remains `.github/workflows/release.yml`. It runs for a
stable `vX.Y.Z` tag and first verifies that the tag matches the `ace-engine`
version from Cargo metadata. Prerelease tags are excluded. Update workspace
versions through a reviewed PR before creating a new stable tag. Published tags
are immutable; never replace an existing tag or its assets.

The RC source assessment in [CI/CD](ci-cd.md) is a separate manual fuzz campaign.
It produces source-bound evidence without publishing a product version. Passing
that campaign does not claim a complete product RC, runtime acceptance or release
qualification. In particular, its schema-3 candidate report is not a substitute
for the stable release's archive checks.

## Exact source for every binary

All seven binary targets must build successfully: Linux musl for x86-64,
AArch64 and ARMv7; macOS for x86-64 and AArch64; Windows for x86-64 and AArch64.
The release workflow retains target-specific executable-resolution tests on
Windows. Each binary package includes its corresponding engine source,
`source.json`, license, attribution and third-party notices. Each target also
gets a complete source archive containing the engine source and vendored
registry dependencies.

Before publication, `tools/release/prepare_container.py` verifies the complete
set of seven binary archives, seven complete-source archives and `SHA256SUMS`.
It checks the source revision, version, target, source digest, complete-source
URL, legal notices and vendor content. The source revision must match the tag's
resolved Git commit. A partial build or incomplete source set fails publication.
Do not add new assets to this closed archive contract without extending the
verifier and its negative tests together.

## Containers from verified release artifacts

`publish-container.yml` accepts an existing, published stable release, downloads
its complete archive set, and verifies it before building runtime images. It
uses the release executables rather than recompiling a potentially different
source revision. The image labels retain that release's version and source SHA.
All three platforms (amd64, arm64 and ARMv7) must publish immutable digests before
a complete versioned image index and `latest` are created. ARMv7 uses QEMU only
for runtime package installation. An older release cannot replace `latest`.

The manual container workflow is also useful for an already published release;
it does not authorize changing that release's tag or assets. Its source,
checksum and complete-architecture checks apply on every run. New product RC
packaging or source-assessment promotion would require a separately reviewed
contract that preserves these guarantees.
