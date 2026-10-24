# Development

Use [installation](installation.md) for prebuilt releases. This guide is for
changing Outpace AceStream or validating a source build. Read [AGENTS.md](../AGENTS.md)
for repository rules and [architecture](architecture.md) for the crate map.

## Clone and build

```sh
git clone https://github.com/open-ott-play/outpace-acestream.git
cd outpace-acestream
cargo build --locked --release -p ace-engine --bin outpace
```

The Rust toolchain is pinned in `rust-toolchain.toml`. Install it through rustup
and provide your target's native compiler/linker prerequisites. The output is
`target/release/outpace` (`outpace.exe` on Windows). Cross-compilation also needs
a target linker and any target-specific system tooling; adding a Rust target
alone is not sufficient. The release workflow and [Linux portability](linux-portability.md)
record the project's target build setup.

Run work on a topic branch in a separate worktree, leaving the root checkout on
`main`. Use a `feat/`, `fix/`, `docs/`, `test/` or `chore/` prefix. Keep changes
scoped, preserve concurrent work, and submit a pull request to this repository.
Do not redirect repository-management commands to the original upstream.

## Validate a change

The normal local gate is:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 tools/hygiene/check_identifiers.py
```

Use focused tests while editing, then run the appropriate complete gates before
integration. Tests should demonstrate a contract or reproduce a defect, rather
than repeat implementation details. Documentation changes need accurate links,
valid examples, readable diagrams and the repository's required PR checks.

Most tests use offline synthetic inputs. Live-network tests are ignored by
default because results depend on peer availability and network conditions.
Only run them with a current authorized source, bounded duration, known client
and server settings, and explicit operator intent. Follow the live-comparison
procedure in [AGENTS.md](../AGENTS.md); preserve deployed routing and DNS policy
when evaluating an existing deployment.

The optional FFmpeg suite uses synthetic media:

```sh
OUTPACE_TEST_HLS_AUDIO_FFMPEG=1 \
  cargo test --locked -p ace-engine --test hls_audio_ffmpeg -- --nocapture
```

It requires the documented FFmpeg/ffprobe capabilities and fails when opted in
without them. Without the opt-in, it reports a skip. See
[HLS audio](playback.md#live-hls-audio) for the supported codec contract.

## Source and release integrity

Builds embed the actual indexed workspace source and record tracked modifications.
Add new intended source files to Git before building; unindexed compilation
inputs are rejected. Do not stage private state or credentials. Complete source
bundles add locked dependency sources and notices; use their `REBUILD.md` for an
offline rebuild. See [licensing](licensing.md) before distributing a modified
binary or making it available over a network.

Release tags are immutable. Never replace an existing tag to publish a fix,
pretend a modified binary came from a clean release, or pair a new executable
with an old source archive. Repository protections and the normal review path
are described in [repository maintenance](repository-maintenance.md).

## Useful reports

Report the exact version/target from `/source.json`, route family, whether the
failure is reproducible with synthetic input, and the smallest relevant redacted
log excerpt. Separate process health, descriptor resolution, peer/media progress,
HLS readiness and actual decoding. A successful HTTP request alone is not a
playback test; a single unavailable source alone is not proof of a general engine
failure.

Real content IDs, infohashes and channel names are excluded from tracked code,
documentation, issues and pull requests. Use a synthetic placeholder in a
reproducer. Keep private captures, tokens, source URLs, identity seeds and
operator state outside the repository. The identifier-hygiene gate rejects
unapproved 40-hex strings; do not add real stream identifiers to its allowlist.
