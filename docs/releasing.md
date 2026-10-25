# Release and source acceptance

The product release path is `.github/workflows/release.yml`. It accepts stable
`vX.Y.Z` and beta `vX.Y.Z-beta.N` tags, where `N` starts at 1. The shared
`tools/release/release_policy.py` validator rejects leading zeroes, other
prerelease names and build metadata, and requires an exact match with the
`ace-engine` version from Cargo metadata. Update workspace versions through a
reviewed PR before creating a tag. Published tags are immutable; never replace
an existing tag or its assets.

A beta is an installable product prerelease, with the same platform and source
requirements as stable. GitHub marks it as a prerelease and explicitly excludes
it from Latest. For example, `v0.9.0-beta.1` requires Cargo version
`0.9.0-beta.1`; the next correction uses a new numbered beta.

The RC source assessment in [CI/CD](ci-cd.md) is a separate manual fuzz campaign.
It produces source-bound evidence without publishing a product version. Passing
that campaign does not claim a complete product RC, runtime acceptance or release
qualification. In particular, its schema-3 candidate report is not a substitute
for any product release's archive checks. Creating a beta tag does not trigger
that separate campaign or turn its source assessment into runtime evidence.

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

## Publishing a beta

1. Merge the reviewed version and implementation changes, and confirm the
   required CI checks. Record runtime qualification separately, including the
   limitations of any experimental functionality.
2. Create a new `vX.Y.Z-beta.N` tag at that clean commit. The release workflow
   builds all seven platforms and validates the complete archive set.
3. The workflow creates a prerelease draft, uploads the assets, then downloads
   and validates the uploaded set before publishing with
   `--prerelease=true --latest=false`. Uploading while still a draft supports GitHub
   immutable releases. A failed upload leaves a draft that can be retried;
   an already published beta is never changed back into a draft or overwritten.
4. The container workflow consumes the published assets and updates only the
   version tag (for example `0.9.0-beta.1`) and `beta`. Verify the three-platform
   index and pin its digest for a controlled deployment.

If GitHub publication succeeded but container publication failed, run
`publish-container.yml` manually for the same existing tag. Do not rerun binary
publication for a published beta. Fix source or packaging defects in a new tag.
Moving from beta to stable requires a separate reviewed version change and a
new stable tag; it is not a rename of the beta release or its assets.

Stable releases use the same draft-upload-verification path. Only an unpublished
draft with the matching tag and prerelease status may be resumed; published
stable and beta assets are never overwritten by this workflow. GitHub
publication is serialized across tags, while platform builds remain parallel.
Immediately before publication, a fresh paginated release inventory determines
whether a stable version may become Latest. An older stable backport can be
published without replacing a newer stable release; beta never becomes Latest.
This guard covers workflow-driven publication, not independent manual edits.

## Containers from verified release artifacts

`publish-container.yml` accepts an existing, published stable or beta release,
checks that GitHub's prerelease flag matches its tag, downloads its complete
archive set, and verifies it before building runtime images. It
uses the release executables rather than recompiling a potentially different
source revision. The image labels retain that release's version and source SHA.
All three platforms (amd64, arm64 and ARMv7) must publish immutable digests before
a complete versioned image index and its moving channel are created. Stable
updates `latest`; beta updates `beta` and never `latest`. ARMv7 uses QEMU only
for runtime package installation.

Container publishers are serialized across both channels. Immediately before
updating a channel, the workflow checks the complete paginated GitHub release
list and rejects an older version within that channel. Numeric ordering makes
`beta.10` newer than `beta.2`. Drafts and the other channel do not affect this
comparison. Missing or inconsistent publication metadata fails closed. The
initial asset validation and final channel check use the same pinned tooling
revision, selected from `main` at the start of the workflow.

The manual container workflow is also useful for an already published release;
it does not authorize changing that release's tag or assets. Its source,
checksum and complete-architecture checks apply on every run. A newer published
release blocks a manual attempt to move its channel backwards, even if that
newer release's container job has not finished. New product RC packaging or
source-assessment promotion would require a separately reviewed contract that
preserves these guarantees.
