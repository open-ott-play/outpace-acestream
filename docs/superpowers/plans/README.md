# Implementation plans

Dated step-by-step plans written before the corresponding work. Most accompany a
[design specification](../specs/README.md); the plans record the sequence of
tasks, the commands used to verify each step and the expected state after it.

## Foundation and protocol recovery

- [Phase 0 — Acestream Protocol Recovery Spike](2026-06-28-phase0-protocol-recovery.md) — the recovery spike tasks.
- [Phase 1 — `ace-wire` Foundation](2026-06-28-phase1-ace-wire.md) — the wire-format crate.
- [Phase 2 — `ace-tracker` + `ace-peer`](2026-06-28-phase2-tracker-peer.md) — tracker and peer crates.

## Daemon and compliance

- [outpace daemon (v1)](2026-06-29-outpace-daemon.md) — the daemon plan.
- [S1 — Compliance & Reciprocal Upload](2026-06-30-seeding-s1-compliance.md) — reciprocal upload tasks.
- [v2 offline foundations](2026-06-30-v2-offline-foundations.md) — listener plumbing and the pure codecs.
- [Inbound Seeding Lifecycle & Policy](2026-07-06-inbound-seeding-lifecycle.md) — inbound peer policy.
- [Canonical-stream Inbound Admission](2026-10-23-seeding-admission.md) — admission rules.
- [Rename Outpace To Outpace](2026-07-01-rename-outpace-to-outpace.md) — the rename across the tree.
- [Outpace CLI Broadcast Play Serve](2026-07-02-outpace-cli-broadcast-play-serve.md) — the three primary commands.

## Storage and configuration

- [Disk-Backed Piece Cache](2026-07-03-disk-backed-piece-cache.md) — the disk-backed store.
- [Env-Var Configuration of Existing Tunables](2026-07-03-env-var-config-tunables.md) — exposing tunables through the environment.
- [Live Recovery Configuration and Reset Behavior](2026-07-07-live-recovery-config-and-reset.md) — recovery tunables and reset.
- [Supported Ace Compatibility Configuration](2026-10-23-ace-compat-config.md) — compatibility knobs.

## Sources and playback

- [Transport-file URL + Magnet Playback Inputs](2026-07-07-transport-url-magnet-inputs.md) — URL and magnet inputs.
- [VOD Transport and Playback](2026-07-08-vod-transport-playback.md) — single-file VOD.
- [Extensionless MPEG-TS Playback](2026-07-14-extensionless-mpegts-playback.md) — extensionless source handling.
- [Native HLS Playback Reliability](2026-07-14-native-hls-playback.md) — native HLS reliability.
- [Native HLS Clean-Start](2026-07-15-native-hls-clean-start.md) — a clean HLS start.
- [Live Playback Prebuffer](2026-07-18-live-playback-prebuffer.md) — the live prebuffer.
- [Live HLS Audio Normalization](2026-10-20-hls-audio-normalization.md) — audio normalization.
- [Native HTTP MPEG-TS Broadcast Pull](2026-10-23-broadcast-pull.md) — pulling an HTTP MPEG-TS source.
- [Broadcast Startup Profiles](2026-10-24-broadcast-options.md) — startup profiles.
- [RTMP Broadcast Ingest](2026-07-03-rtmp-broadcast-ingest.md) — RTMP ingest.

## Metadata and statistics

- [Stream Metadata](2026-07-14-stream-metadata.md) — the metadata model.
- [Native HLS Title Metadata](2026-07-14-native-hls-title-metadata.md) — titles through native HLS.
- [SDT `service_name` Title Injection](2026-07-15-sdt-service-name-title.md) — SDT-derived titles.
- [Direct MPEG-TS Periodic SDT](2026-07-19-direct-ts-periodic-sdt.md) — periodic SDT output.
- [Live Statistics](2026-10-17-live-statistics-171.md) — observable statistics.

## Build, release and maintenance

- [Fast Portability CI](2026-07-16-fast-portability-ci.md) — the portability job.
- [Renovate](2026-10-03-renovate.md) — dependency updates.
- [CI Test Gate](2026-10-04-ci-test-gate.md) — the test gate.
- [Fail Closed on Infohash-Only Live Playback](2026-10-04-infohash-descriptor-164.md) — the fail-closed change for #164.
- [Production Live Follower Extraction](2026-10-18-live-follower-extraction.md) — separating the live follower.
