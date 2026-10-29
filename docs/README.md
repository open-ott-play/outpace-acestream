# Documentation

This is the operator and contributor guide for **Outpace AceStream**, the Rust
engine maintained at [open-ott-play/outpace-acestream](https://github.com/open-ott-play/outpace-acestream).
The executable is `outpace`. It acquires and serves media; a separate player or
proxy supplies the viewing interface and channel catalog.

## Install and play

| Goal | Read |
| --- | --- |
| Select and verify a release for Windows, Linux or macOS | [Native installation](installation.md) |
| Run one container or a Compose service | [Docker and Compose](containers.md) |
| Deploy with persistent identity in k3s or Kubernetes | [Kubernetes guide](kubernetes.md) · [Example manifests](../examples/kubernetes/) |
| Open a source in VLC, hls-proxy or another consumer | [Playback and integration](playback.md) |
| Select environment values and buffering limits | [Configuration reference](configuration.md) |
| Diagnose startup, codecs, network, cache or deployment problems | [Troubleshooting](troubleshooting.md) |

Start with the platform-specific recipe. The examples target v0.9.0; verify
the published assets and source before installing. Historical version
differences remain explicit.
Preserve the data directory across upgrades; use the upgrade and rollback
sections before replacing an existing service.

## Understand and integrate

- [Architecture](architecture.md): media flow, shared sessions, crate map, storage
  and deployment boundaries, with self-contained SVG diagrams.
- [Native API](native-api.md): exact routes, source selectors, status, VOD and
  broadcast contracts.
- [Compatibility](deployment-compatibility.md): supported Ace HTTP behavior,
  token ownership and lifecycle; not full closed-engine parity.
- [Advanced deployment](deployment.md): detailed Compose/networking modes,
  managed sources and operator choices.
- [LXC with WARP](deployment-lxc-warp.md): a specialized topology, not a default
  requirement for native, Docker or Kubernetes installs.
- [Linux portability](linux-portability.md): musl, ARMv7 and platform boundaries.
- [Roadmap](roadmap.md): current baseline and future work, including peer timeshift
  and durable DVR. Live HLS and single-file VOD do not imply a channel archive.

## Develop and distribute

- [RC fuzz assessment](fuzzing.md): bounded targets, synthetic corpora and source-bound evidence.
- [CI and dependency maintenance](ci-cd.md): required checks, shared security analysis and Renovate.
- [Release acceptance](releasing.md): immutable releases, corresponding source and publication checks.
- [Retained replay](timeshift.md): retained-media replay, per-viewer controls, peer-history experiment and limits.
- [Source resolution](source-resolution.md): typed live/VOD/HLS contracts, direct handoff, optional managed origins, catalog lifetime and container qualification.

- [Development](development.md): source builds, meaningful validation and safe
  diagnostic reports.
- [Licensing and matching source](licensing.md): AGPL-3.0-or-later attribution,
  embedded source, complete source packages and offline rebuilding.
- [Repository maintenance](repository-maintenance.md): organization-managed
  protections and review policy.
- [Upstream integration](upstream-integration.md): the accepted upstream baseline
  and locally maintained changes.
- [Contributor instructions](../AGENTS.md): worktrees, tests, logging and the
  identifier-hygiene rule.

## Protocol and research

Use the current API and operator guides for supported behavior. The protocol
notes, design documents and implementation plans record findings at the time
they were written; older limitations or proposed features may have been
superseded and do not override the current code.

- [Wire protocol](protocol/wire-protocol.md)
- [Transport descriptors](protocol/transport-file.md)
- [Compatibility matrix](protocol/compat-matrix.md)
- [Public Ace Stream source review](protocol/public-code-review.md): pinned repository inventory and client-contract evidence.
- [Shared seeding and admission](protocol/shared-seeding.md)
- [Protocol research notes](protocol/notes/)
- [Design specifications](superpowers/specs/) and [implementation plans](superpowers/plans/)
- [Interop testing](testing/interop-swarm.md) and [live gateway validation](testing/live-gateway-checklist.md)

Illustrations are committed under [assets](assets/) as accessible, standalone
SVG files with no external fonts or script dependencies. They render directly
in GitHub and ordinary browsers.
