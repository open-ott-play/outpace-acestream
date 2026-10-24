# License, source and independent development

This repository is `open-ott-play/outpace-acestream`. It preserves Outpace's Git
history and **AGPL-3.0-or-later** license; creating an independent repository does
not change those obligations. See [LICENSE](../LICENSE), [NOTICE](../NOTICE) and
[AUTHORS.md](../AUTHORS.md). The command is still `outpace`, crates keep their existing
names, and `OUTPACE_*` environment variables remain compatible. This is an engine
for AceStream sources, with HTTP/HLS output for players such as hls-proxy; it is
not itself a player and is not an official Ace Stream product.

Every build embeds a workspace-source archive containing the actual tracked
source bytes, Cargo.lock, build scripts, tests, documentation and notices. The
running daemon offers it prominently at `/` and `/source`; `/source.tar.gz` serves
the archive and `/source.json` reports its SHA-256, base Git revision and whether
the tree was modified. An HTTP `Link` header advertises the offer on API responses.
The same archive is available offline with `outpace source > source.tar.gz`.
No runtime source checkout or writable filesystem is required for that archive.

Official releases additionally offer a **complete source asset** containing that
exact workspace archive, all Cargo.lock registry sources from `cargo vendor`,
original third-party notices, and offline rebuild instructions. Each platform's
asset name includes its version, target and full Git revision. Release binaries
link directly to it from `/source` and record the URL in `/source.json`. Container
publication verifies the source and binary packages refer to the same clean
revision, target and workspace digest before publishing an image. The vendored
sources stay outside the executable.

A local or modified build does not claim that a published release asset matches
it. Before distributing such a build or exposing it to other users, generate and
host its matching complete bundle. `tools/release/package_source.py` takes the
`--message-format=json-render-diagnostics` output of the build and copies its
workspace archive and metadata to a package directory; then run
`python3 tools/source/complete_source.py PACKAGE COMPLETE.tar.gz`. This verifies
the build's lockfile and every vendored file before packaging. The resulting
`THIRD_PARTY_NOTICES.txt` belongs with the binary; make the complete bundle's URL
available alongside the service source offer. Do not set `OUTPACE_RELEASE_BUILD`
for private or modified builds: that flag is reserved for the stable release
workflow and refuses a modified tracked tree.

Keep these endpoints reachable when putting a modified engine behind a proxy,
and provide the source link to users of an integrated player. A separately
modified surrounding program may have additional obligations; these mechanisms
address this engine, not a blanket legal assessment of every deployment.

### Building and reproducing source

Build with the Rust toolchain pinned in `rust-toolchain.toml` and
`cargo build --locked --release -p ace-engine --bin outpace`. Add new source files
to Git before building; unindexed files in compilation directories are rejected.
Tracked modifications are included and identified as modified. Ignored operator
state, credentials, the private identifier registry and Git metadata are never
part of the archive. Do not stage secrets: packaging deliberately publishes the
tracked project, subject to checks against reserved private paths and unsafe links.

For a complete release asset, follow its `REBUILD.md`; dependencies can be resolved
without contacting a registry. To rebuild the smaller embedded workspace archive,
extract it, enter `outpace-acestream`,
and run the same Cargo command. The included `SOURCE-MANIFEST.json` supplies a
hashed inventory when Git is absent. Cargo fetches the exact dependency versions
and checksums in Cargo.lock from their public registries; native system build
requirements are documented by the Dockerfiles and CI workflows. The archive
identifies source inputs; it is not a claim of bit-for-bit binary reproducibility
across different compilers, linkers, flags and hosts. GitHub's automatically generated
repository ZIP has no build inventory: use a Git clone or the packaged source
archives above.

For a container built from source, first prepare an isolated context containing
only indexed project files (Python 3 is required for this preparation step):

```bash
python3 tools/source/prepare_context.py /tmp/outpace-source-context
docker build -t outpace-acestream:local /tmp/outpace-source-context
```

The script refuses to overwrite an existing directory. CI uses the same process.
Binary release archives include the embedded workspace archive, license and
third-party notices; the complete source asset is published alongside each one. Published runtime images carry the exact release binary with its embedded
source and identify the release revision through OCI labels.
