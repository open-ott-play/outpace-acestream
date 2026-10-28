# Offline fuzz assessment

Outpace tests its production protocol and media parsers with bounded,
coverage-guided libFuzzer targets. The harness links `ace-wire` and `ace-media`
directly; neither crate performs network or device I/O. All committed seeds are
synthetic. The harness never reads a local identifier registry, fetches a
tracker, joins a swarm, starts the daemon or tests a real stream.

## What is checked

- **Metadata:** permissive bencode prefix parsing and canonical round trips,
  plus strict single-file metainfo decoding. Accepted metainfo must have
  consistent piece geometry and bounded piece/tracker lists. The harness never
  allocates from advertised content sizes or contacts the listed addresses.
- **Peer frames:** frame length/consumption, byte-exact decode/encode agreement,
  incomplete frames and explicitly selected protocol handshakes. Arbitrary
  unknown message payloads must survive a round trip.
- **Peer extensions:** metadata control dictionaries and payload boundaries,
  peer-list record sizes, nonzero endpoints, valid advertised piece windows,
  and stable priority ordering without dropping candidates.
- **Media resynchronization:** MPEG-TS alignment across arbitrary input chunk
  boundaries, ordered loss boundaries, bounded loss accounting, PCR field
  ranges and packet-preserving HLS segmentation.

- **Origin HLS playlists:** the production bounded input parser, deterministic
  classification, exact identity rewrites, replacement URI spans, timeline and
  retention preservation. Resource-bearing interstitial tags are rejected; no
  URI is resolved or fetched.

The same target functions run against the committed corpus in ordinary CI.
Positive controls ensure valid metainfo, frames, metadata messages, peer records
and aligned media actually reach successful parse paths. Unexpected panics and
assertion failures are findings; the harness does not suppress them.

These five targets do not exercise the daemon's viewer leases, shared history
budget or runtime shutdown. Those stateful paths have separate deterministic
`ace-engine` tests, including multi-store retention and listener cleanup after
startup failure or cancellation. The generated-media integration test checks
decoded replay and independent viewers. A successful parser assessment does not
replace these checks or qualify every possible playback sequence.

## Budgets and execution policy

Each target accepts at most 16 KiB and has a 60-second mutation budget: five
minutes in total, excluding setup. The shared runner owns the 10-second input
timeout, 2 GiB RSS limit, process supervision, network isolation, corpus copies
and evidence artifacts. Production parser bounds remain active. The media
target limits its own segmentation parameter; it does not invent an untrusted
caller-controlled allocation size.

Random mutation runs automatically as a required stage of every `Release`
workflow request with `channel=rc` on the public default branch. There is no
separate fuzz dispatch. Ordinary pushes and PRs replay fixed inputs; stable and
beta tag publication do not start mutation campaigns. See [CI/CD](ci-cd.md) for
the shared workflow, trigger and source-evidence contract. A parser assessment by itself is not a claim of
live-stream, device, release-binary or deployment acceptance.

## Reproduce fixed regressions

Use the repository toolchain from `rust-toolchain.toml`:

```sh
cargo test --manifest-path fuzz/Cargo.toml --locked
cargo clippy --manifest-path fuzz/Cargo.toml --all-targets --locked -- -D warnings
cargo fmt --manifest-path fuzz/Cargo.toml --all --check
```

The separate `fuzz` Cargo workspace keeps the fuzz engine out of the product
dependency graph and signed build. Its `Cargo.lock` pins registry packages and
checksums; the two product crates are path dependencies, so the exact reviewed
source is tested. Default features do not compile or run libFuzzer.

When changing the product workspace version or parser dependencies, update both
the root `Cargo.lock` and `fuzz/Cargo.lock`, review the changes and rerun these
locked checks. The nested lock records the versions of its path dependencies
as well as registry packages. A standalone source assessment does not require
a product version change.

## Build instrumented RC targets

The shared workflow installs `nightly-2026-10-12` and `cargo-fuzz 0.13.1`;
the build adapter requires those exact versions. The harness pins
`libfuzzer-sys 0.4.13`. These are the upstream
[cargo-fuzz](https://crates.io/crates/cargo-fuzz/0.13.1) and
[Rust libFuzzer runtime](https://crates.io/crates/libfuzzer-sys/0.4.13).
The stable product compiler remains independent.

```sh
rustup toolchain install nightly-2026-10-12 --profile minimal --component rust-src
cargo install cargo-fuzz --version 0.13.1 --locked --jobs 2 --root .fuzz-tools
python3 fuzz/build.py
```

Cargo-fuzz builds with AddressSanitizer, overflow checks, debug assertions and
coverage instrumentation. The adapter checks the lock before the build and
rejects dependency resolution changes afterward, since cargo-fuzz does not
expose Cargo's `--locked` flag. Compiled binaries are copied into
`.fuzz-build/bin`; the shared runner executes those binaries directly, never a
build command that could silently fall back to fixed tests.

To replay an individual recorded input without starting a mutation campaign,
pass its local path to the relevant executable, for example:

```sh
.fuzz-build/bin/metadata fuzz/corpus/metadata/single-piece
```

## Handle a finding

1. Preserve the failed run's report, logs and input; check its exact source SHA
   and dependency/manifest identity.
2. Reproduce and minimize the input offline. Confirm the invariant reflects the
   documented parser contract before changing production code.
3. Add the minimized synthetic regression to the appropriate corpus and fix
   the production parser. Never add private captures, real identifiers or stream
   names to code, fixtures, logs or documentation.
4. Run deterministic checks and the normal workspace tests, then request a new
   `Release` run with `channel=rc` for the reviewed fix. Record actual measured results; a badge
   or successful older commit cannot establish coverage of a later commit.
