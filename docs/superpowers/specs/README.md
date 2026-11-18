# Design specifications

Dated design documents written before or alongside the work they describe. Each
one records the constraints, alternatives and decision taken at that point;
later notes and release notes record what actually shipped. Implementation
plans for the same work are in [implementation plans](../plans/README.md).

## Foundation and protocol recovery

- [Open-Source Acestream Engine — Reimplementation Plan](2026-06-28-acestream-engine-reimplementation-design.md) — the overall reimplementation plan.
- [Phase 0 — Findings & Go/No-Go](2026-06-28-phase0-findings.md) — the decision to proceed, with the evidence behind it.
- [Implemented Code Review](2026-06-28-225219-implemented-code-review.md) — a review of code written during the phase.
- [Security Review Follow-ups - 2026-06-30](2026-06-30-security-review-followups.md) — the follow-ups raised by that review.

## Daemon and compliance

- [outpace daemon — productization design](2026-06-29-outpace-daemon-design.md) — turning the protocol work into a service.
- [P2P compliance, seeding and broadcasting](2026-06-30-compliance-seeding-broadcasting-design.md) — reciprocal upload, seeding and broadcast design.
- [Inbound Seeding Lifecycle & Policy](2026-07-06-inbound-seeding-lifecycle-design.md) — policy for inbound peers.
- [Canonical-stream inbound admission (#207)](2026-10-23-seeding-admission-design.md) — admission rules for canonical streams.

## Storage, configuration and state

- [Cache Configuration & Disk-Backed Piece Store](2026-07-03-cache-config-and-disk-backing-design.md) — disk-backed pieces and their configuration.
- [Outpace memory hardening](2026-07-15-memory-hardening-design.md) — the memory ownership design.
- [Persist Broadcast State & Continuous Ingest Resume](2026-07-05-persist-broadcast-state-design.md) — resuming a broadcast after a restart.
- [Live Recovery Configuration and Reset Behavior](2026-07-07-live-recovery-config-and-reset-design.md) — recovery tunables and reset semantics.
- [Supported Ace compatibility configuration](2026-10-23-ace-compat-config-design.md) — the compatibility knobs the engine supports.

## Sources and playback

- [Transport-file URL and magnet playback inputs (#50)](2026-07-07-transport-url-magnet-inputs-design.md) — accepting URL and magnet inputs.
- [VOD transport and playback support](2026-07-08-vod-transport-playback-design.md) — single-file VOD.
- [Extensionless MPEG-TS Playback](2026-07-14-extensionless-mpegts-playback-design.md) — handling extensionless sources.
- [Native HLS Playback Reliability](2026-07-14-native-hls-playback-design.md) — the reliability design for native HLS.
- [Native HLS Clean-Start](2026-07-15-native-hls-clean-start-design.md) — starting a native HLS session cleanly.
- [Live Playback Prebuffer](2026-07-18-live-playback-prebuffer-design.md) — the live prebuffer design.
- [Live HLS audio normalization (#200)](2026-10-20-hls-audio-normalization-design.md) — normalizing audio across renditions.
- [Native HTTP MPEG-TS broadcast sources (#206)](2026-10-23-broadcast-pull-design.md) — pulling from an HTTP MPEG-TS source.
- [Broadcast startup profiles](2026-10-24-broadcast-options-design.md) — startup profiles for broadcasts.

## Metadata and statistics

- [Stream Metadata](2026-07-14-stream-metadata-design.md) — the metadata model for streams.
- [Native HLS Title Metadata](2026-07-14-native-hls-title-metadata-design.md) — carrying titles through native HLS.
- [SDT `service_name` title injection](2026-07-15-sdt-service-name-title-design.md) — using SDT for the stream title.
- [Direct MPEG-TS Periodic SDT](2026-07-19-direct-ts-periodic-sdt-design.md) — periodic SDT in direct TS output.
- [Live statistics for issue #171](2026-10-17-live-statistics-171-design.md) — the observable statistics design.

## Build, release and maintenance

- [RTMP Broadcast Ingest Design](2026-07-03-rtmp-broadcast-ingest-design.md) — accepting RTMP ingest.
- [Release Binaries and Docker Image Design](2026-07-07-release-binaries-and-docker-design.md) — published binaries and the container image.
- [Fast Portability CI Design](2026-07-16-fast-portability-ci-design.md) — the portability job.
- [Renovate Design](2026-10-03-renovate-design.md) — automated dependency updates.
- [Production live follower extraction (#190)](2026-10-18-live-follower-extraction-design.md) — separating the live follower.
