# Development roadmap

Outpace AceStream is an independently maintained AceStream engine derived from
Outpace. The executable remains `outpace`. A separate repository does not change
its AGPL license, its upstream attribution, or the capabilities of the peer
network. Protocol and media code should be reused where it is already correct;
changes need evidence, bounded resource use, and regression coverage.

## Foundation in this source tree

The integration starts from upstream 0.8.0, with the deployed local fixes reviewed
individually in [the integration matrix](upstream-integration.md). It preserves
exact piece coverage and verified seed publication, bounded request scheduling,
independent peer windows, HTTP tracker discovery, HLS startup/retention behavior,
EOF recovery, and playback-generation ownership. The verification key is parsed
once per reassembler; descriptor and VOD resolution avoid repeated or unrelated
blocking work. These are specific changes, not a measured whole-engine speedup.

Source distribution includes an embedded workspace archive, network-facing
source links, complete release source bundles with locked dependencies, and
upstream/dependency notices. Kubernetes should consume a qualified immutable
release and image digest, with the previous digest available for rollback.

## Measurement before tuning

Record catalog resolution, discovery, peer admission, first accepted piece,
first random-access point, startup readiness, first HLS response, and first
decoded frame separately. Compare the same source, route, resource budget,
player, and cold/warm state. Keep live identifiers and raw captures private.

Use CPU/allocation profiles and repeated channel-switch resource measurements
to choose subsequent changes. Candidate work includes reducing verified media
copies, maintaining incremental ready-buffer observations, and assigning peer
requests from measured delivery reliability. Preserve signature validation,
bounded queues, and cancellation when optimizing. HTTP success or received byte
counts alone do not establish successful playback.

## Peer-network timeshift: not implemented yet

The next proof is a fresh Rust acquisition of older media that peers still
advertise. Use a separate bounded historical follower first, select peers that
cover the requested interval, and verify decoded media timestamps against a
live baseline. A larger seed cache, a changed status position, or an older
request number is not evidence of an archive.

After that proof, introduce independent playback sessions:

1. Retain immutable accepted pieces with explicit memory and time-index budgets.
2. Give each viewer a cursor, mode, cancellation owner, and command generation.
3. Prepare a seek while current playback continues; commit only after usable
   historical media is ready. Failure leaves existing playback intact.
4. Fence old queued data, peer replies, and segment URLs at the generation change.
5. Pace historical delivery and support an explicit return to live. Recovery in
   delayed mode must not silently jump to the current live head.

Treat external piece timestamps as untrusted timing hints and corroborate them
with accepted media PCR/PTS and source epochs. Report coverage intervals and
gaps, rather than promising every position between a minimum and maximum.

## Player and HLS integration

Expose session-specific capabilities and actual available history through the
native API and a thin Ace-compatible adapter. Each seek needs a distinct HLS
generation and an explicit player reload/discontinuity contract. Qualify direct
TS first, then HLS through hls-proxy and the supported Tauri, LG, and Fire clients.
Do not advertise provider catch-up days for an opportunistic peer buffer.

Required checks include two viewers seeking independently, failed preparation,
rapid seek/stop, backpressure, stale responses, source restarts, unavailable
history, and channel-switch teardown. Keep acquisition sharing separate from
the initial correctness proof.

## Optional persistent DVR

Predictable long retention requires recording selected sources while they are
available. It needs durable media/index storage, recovery after restart,
disk-space admission, pruning, coverage gaps, and a recorder lifetime independent
of viewers. Recording cannot recover programmes from before it began. The
existing disposable seeding cache is not DVR storage.

Choose retention and channel selection from an explicit capacity budget. Add
crash recovery, disk-full, pruning, and index-recovery acceptance tests before
exposing durable archive capabilities to players.
