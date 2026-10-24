# Upstream integration audit

This project starts from Outpace 0.8.0 and retains its authorship and AGPL notices.
The older deployment used Outpace 0.2.0 plus the fifteen ordered patches below.
Those patches must not be applied mechanically to the newer tree: the live follower
now lives in `ace-swarm`, and HLS has an optional audio-normalization worker with
its own generation fencing. This document tracks the behavior being preserved,
not whether a historical patch applies without conflicts.

## Deployment patch matrix

| Historical patch | Upstream 0.8.0 assessment | Integration decision |
| --- | --- | --- |
| `stability.patch` | Several live-window/recovery paths have been rebuilt upstream. Exact completion, completed-piece scheduling, and bounded command admission still need the local fixes. | Retain provider-key/provider-session fields in the descriptor infohash, exact completion, no re-request of completed ahead pieces, nonblocking queue admission, peer-local window advancement, and eviction pointers. Keep upstream discovery/fairness machinery. Safe identity loading is retained; the compatibility integration adds typed descriptor handling and bounded descriptor handoff. The removed legacy single-peer driver is superseded by upstream VOD coordination and its exact block-coverage tests. |
| `deterministic-hls-test.patch` | The old lease test and owner layout have changed. | Replace with current deterministic lease/lifecycle tests; do not copy obsolete wall-clock fixture code. |
| `lg-hls.patch` | Typed HLS descriptor passthrough and observed 8 KiB chunk support are not equivalent upstream. | Retain in the compatibility integration. An HLS origin must remain an origin redirect; never invent swarm geometry or modify signed URLs. |
| `http-trackers.patch` | HTTP(S) tracker discovery remains a distinct required addition. | Retain the bounded public-address-validated HTTP tracker path in the compatibility integration. Preserve UDP/DHT discovery and seeding announcements. |
| `deterministic-disk-test.patch` | Upstream has a revised disk actor and its own synchronized cancellation tests. | Keep the actor implementation; run its current budget, cancellation, and I/O ownership tests. Add verified seed-publication coverage for both memory and disk. |
| `hls-retention.patch` | Upstream drops segments as soon as they leave the advertised window. | Retain a separate grace cache for previously advertised segments: at most 120 seconds, 32 segments, and 64 MiB of backing allocations. Reads never renew expiry. |
| `hls-startup.patch` | Upstream segment count alone can publish a playlist in the middle of initial catch-up. | Carry readiness with each media batch through live output, prebuffer, and session events. Native HLS opens only after the entire ready batch is packaged. Optional normalization discards historical startup batches before opening its codec gate. |
| `mpeg2-hls.patch` | Upstream now has opt-in MPEG-2 I-picture detection and more detailed bounded PES validation. | Keep the newer upstream parser. It detects a candidate access point, not proof of complete decoding independence; codec support and usable source headers remain required. |
| `scheduler-horizon.patch` | The upstream horizon still shrinks with free slots, and completion forgets ahead pieces. | Retain the fixed bounded horizon, completed-piece set, and exclusive reassembler acceptance bound, including saturating integer edges. |
| `hls-byte-ceiling.patch` | Upstream permits a forced cut only after the target duration, so a faster source can repeatedly lose its entire run. | Retain the latest clean interior boundary and replay its tail without loss. If a whole GOP exceeds the ceiling, permit a shorter continuous segment only after positive PCR progress. Cap backing allocations too. |
| `retry-progress.patch` | Upstream receipt bookkeeping does not distinguish newly covered bytes from duplicates. | Refresh a requested piece's timer only on new byte coverage. Duplicate writes remain provenance contributions but cannot keep a missing piece alive. |
| `code-audit.patch` | Upstream already owns discovery/peer tasks and validates direct runtime configuration. Exact coverage, safe identity loading, EOF propagation, media guards, and RTMP timestamp/clock fixes remain needed. | Retain those missing fixes. Reject transport-error/invalid adaptation clocks and unusable access-point packets. Preserve an existing invalid identity instead of replacing it. Keep new upstream task/discovery architecture. |
| `session-reopen.patch` | Audio worker generation fencing does not replace session EOF/reopen or client-token generation binding. | Restore weak session sender ownership, EOF visibility, and count-only pins here. The manager reopens ended sessions through cancellation-safe per-source cohorts; client tokens and native HLS pins are bound to their session generation. |
| `playback-performance.patch` | Upstream adds warm-peer caches and new observations but does not subsume every local optimization. | Retain packet-at-a-time startup PCR scanning here. The compatibility integration preserves descriptor handoff, per-key VOD resolution, idle cache reaping, parallel tracker discovery, and descriptor-cache limits of 128 entries / 8 MiB. |
| `review-fixes.patch` | Upstream already bounds complete peer acquisition and connects through owned task sets. It still writes provisional bytes into the seed store. | Retain complete-piece verification before seed publication and explicit source-owned task cancellation. Keep upstream absolute connection deadline and slot-refill behavior. |

## Preserved invariants

- Piece completion is the exact union of received byte ranges. Short retries and
  overlaps cannot fill a hole implicitly. Empty/stale/completed submissions are
  no-ops. Existing source-signature rejection semantics are unchanged.
- Only complete accepted wire pieces enter either seed backend. Source headers
  and signature tails are preserved. With no parseable source key, the stream is
  structurally checked but is not cryptographically authenticated.
- A source public key is parsed once per reassembler. Verification still uses
  the same RSA PKCS#1 v1.5/SHA-1 scheme and rejects corrupt/wrong-key signatures.
- Every peer advances its own requestable window even when the global live head
  is newer. Eviction pointers raise its minimum monotonically so recovery can
  skip unavailable pieces. Raw signed-window observations stay separate from
  inferred scheduler coverage.
- Request admission never waits on a full peer command queue. Busy request queues
  retry on the periodic sweep; a blocked control path retires its peer. The
  source owns its downloader task, including announcements and pending connects.
- The first native HLS playlist reflects the completed catch-up batch. Later
  recovery does not reopen that initial gate. Segment eviction grace changes
  fetchability, not the advertised live edge or source lifetime.
- The byte ceiling is a hard retained-allocation bound. Forced mid-GOP segments
  remain continuous for sequential playback but are not promised to decode when
  opened in isolation. No transcoding or remote provider archive is implied.
- Source EOF closes existing and late session receivers. Completed native HLS
  media can finish with `EXT-X-ENDLIST`; an incomplete tail is not published.

## Validation

Synthetic tests cover exhaustive small-range reassembly, short-block repair,
signature failure followed by a valid retry, both seed backends, acceptance-window
bounds, queue saturation, lifecycle cancellation, startup event ordering, HLS
retention expiry and resource caps, high-bitrate continuity, and invalid MPEG-TS
clock/access packets. No operator content IDs or private stream material are
required.

Warm-peer-cache test fixtures canonicalize the system temporary directory before
creating their private cache root. This preserves the production no-symlink policy
while supporting macOS systems whose temporary path traverses `/var`.

```sh
CARGO_BUILD_JOBS=2 cargo test --workspace --locked
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
python3 tools/hygiene/check_identifiers.py
```

A green library suite is necessary but does not by itself demonstrate live
provider availability or archival support. The final integrated build must also
pass its compatibility/provider and client-facing session-generation tests before
replacing the deployed engine.
