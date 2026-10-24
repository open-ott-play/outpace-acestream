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

## Timeshift beta and remaining peer integration

The 0.9.0 beta implements bounded local HLS replay and an independent exact-range
peer-history reader. A bounded field experiment decoded older peer media twelve
seconds before its comparison range, with more than 200 progressing frames in
each sample. This establishes feasibility on that sampled source, not a general
archive depth. See [the beta contract and qualification](timeshift.md).

Local replay already has independent viewer generations, prepare/commit semantics,
URL fencing and explicit return to live. To connect the peer reader to playback:

1. Index accepted media PCR/PTS with explicit memory budgets and source epochs.
2. Find decoder-safe random-access boundaries and complete codec/table bootstrap.
3. Prepare peer-backed media while current playback continues; commit only after usable
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
