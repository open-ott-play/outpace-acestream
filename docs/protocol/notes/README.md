# Protocol research notes

The numbered notes record what the engine learned about the Ace Stream wire
protocol while reverse-engineering it, one finding or milestone per note. Each
note describes the state of understanding when it was written, including
blockers that were later resolved; the behaviour the engine ships today is in
[wire protocol](../wire-protocol.md) and the
[compatibility matrix](../compat-matrix.md). Notes are listed in numeric order.

## Capture and static analysis

- [Phase-0 Inventory](00-inventory.md) — the engine module inventory with sizes, hashes and Cython status.
- [01 — Official Engine Sandbox Setup](01-sandbox.md) — standing up the closed engine 3.2.11 in Docker for later capture tasks.
- [Task 2 — Known-good test streams + environment findings](02-test-streams.md) — the working test stream identifiers and local environment findings.
- [Task 3/4 — Peer discovery + transport observations](03-discovery-and-transport.md) — peer discovery and transport observations captured on a live stream.
- [Task 5 — Encrypter / peer handshake](05-crypto.md) — the peer handshake captured through Frida socket hooks on public content.
- [07 – node.so: peer and tracker logic](07-node.md) — Ghidra analysis of the native module carrying peer and tracker logic.
- [07 – Transport.so: transport-file format and hashing](07-transport.md) — Ghidra analysis of the transport-file format and its hashing.
- [Modern Python bytecode](73-modern-python-bytecode.md) — static inspection of the 3.2.11 package and the behavior contracts it implies.
- [Native live timing, underrun and request reconciliation](74-native-live-recovery.md) — three selected Cython paths followed for live recovery semantics.
- [Reconciliation: `distance_from_source`](98-distance-from-source-semantics.md) — evidence reconciliation with no wire-code behaviour change.

## Transport format

- [Task 8: infohash = SHA1(transport file)](08-idmath-validation.md) — early infohash validation, later superseded by note 29.
- [BitTorrent lineage and the mixed Python/native engine](08-key-finding-bittornado-lineage.md) — the engine's BitTorrent ancestry, corrected on 2026-11-07.
- [Phase 3 gate — transport body is encrypted](12-transport-body-blocker.md) — the observation that the transport body needed decoding before pieces could be read.
- [13 – Transport descriptor: body cipher cracked](13-transport-descriptor.md) — the body transform and recovered field layout, validated offline.
- [29 — Transport infohash formula cracked](29-infohash-formula-cracked.md) — the formula the official engine and this engine now agree on.
- [Transport loss boundaries and cached access-point recovery](59-transport-resync-boundaries.md) — how resync reports aligned packets and ordered loss boundaries.

## Peer identity and handshake

- [Live extended-handshake `mi` dict](11-live-extended-handshake.md) — the extended handshake captured against a real swarm peer.
- [14 — The unchoke blocker is a node-identity gate](14-live-unchoke-recon.md) — live recon that characterized the gate before it was cracked.
- [15 — Node identity: Ed25519 from `device.key`](15-node-identity-cracked.md) — the identity structure, with the signature preimage still open at the time.
- [16 — The node-handshake signer is `LiveSourceAuth.sign`](16-livesourceauth-located.md) — locating the signer and the bytes its signature covers.
- [17 — Node signature preimage cracked](17-node-signature-cracked.md) — the preimage verified against six live samples.
- [18 — Node-identity gate passed live](18-identity-gate-passed.md) — a minted identity accepted by the official engine's verifier.
- [Peer exchange and custom message classification](49-peer-exchange-and-custom-message-classification.md) — identifying message id 12 as Acestream peer-exchange gossip.

## First working playback

- [10 — Interop capstone](10-interop.md) — an independent Rust client accepted by a real swarm peer.
- [19 — Live piece download working](19-live-download-working.md) — connecting, getting unchoked and requesting pieces end to end.
- [20 — Live playback through the daemon](20-vlc-playback.md) — live video pulled from the swarm and served through the daemon.
- [21 — Seeder ground truth](21-seeder-ground-truth.md) — the 8-byte piece-header structure and an interop proof, with one negative result.
- [22 — The live edge never advanced](22-live-edge-never-advances.md) — root cause of a download that froze after the initial window.
- [23 — Peer reconnects spliced the stream](23-reconnect-continuity.md) — root cause of the reported stutter, with quantitative evidence.
- [24 — Seeder self-announce wired](24-seeder-self-announce.md) — tracker and DHT self-announce added to the production path.
- [34 — Acestream-compatible `getstream` by content_id](34-acestream-getstream-content-id-route.md) — mounting the minimal compatibility routes.
- [54 — VOD transport and playback](54-vod-transport-and-playback.md) — single-file VOD support.
- [55 — Legacy `getstream` selector and direct-playback contract](55-legacy-getstream-direct-mode.md) — corrections to the opt-in HTTP adapter's selector handling.
- [56 — Legacy live HLS adapter and evidence boundary](56-legacy-live-hls-adapter.md) — exposing the native live HLS window through the legacy route family.
- [57 — Deprecated port 62062 fails the value gate](57-old-engine-api-value-gate.md) — why the old TCP engine API was not implemented.

## Broadcasting and seeding

- [25 — Official source-node broadcasting: B0 ground truth](25-source-node-b0-groundtruth.md) — broadcasting ground truth plus an infohash formula bug.
- [26 — B1 loopback milestone](26-b1-loopback-milestone.md) — broadcast ingest and origination reaching a self-to-self loopback.
- [27 — B0 cracked: live-source piece signing](27-b0-signing-cracked.md) — the per-piece signing scheme implemented and verified.
- [28 — B1 self-announce fix and the infohash wall](28-b1-self-announce-and-infohash-wall.md) — the remaining reverse-direction blocker.
- [30 — Official engine consumes this engine after the infohash fix](30-official-consumer-proof-after-infohash-fix.md) — hash interop proven while peer discovery stayed blocked.
- [31 — Official consumer connects but never becomes interested](31-official-consumer-connected-but-not-interested.md) — narrowing the next blocker under controlled local networking.
- [32 — Official consumer requests but rejects pieces](32-official-consumer-requests-but-rejects-pieces.md) — the blocker after connection and interest were solved.
- [33 — Live piece header is a timestamp](33-live-piece-header-timestamp-and-official-consumer.md) — the finding that made the official consumer accept these pieces.
- [52 — Seeder self-announce advertises the peer port](52-seeder-announce-peer-port.md) — fixing the port the self-announce paths advertised.
- [51 — CLI broadcast/play and broadcast content IDs](51-cli-and-broadcast-content-id.md) — the three primary commands and broadcast identity.

## Live startup and recovery

- [35 — Live startup improved; static-upstream blocker narrowed](35-live-startup-static-upstream-and-id12-capture.md) — startup work against the public synthetic target.
- [36 — Rediscovery after stale upstream exhaustion](36-rediscovery-after-stale-upstream.md) — re-entering discovery when the upstream goes stale.
- [37 — Upstream selection ranks advertised live windows](37-upstream-window-selection.md) — choosing among peers by the live window they advertise.
- [38 — Reconnect uses the emit cursor](38-reconnect-cursor-and-stale-window-rejection.md) — resuming from emitted output and rejecting stale windows.
- [39 — Provider request frontier uses the shared scheduler](39-provider-uses-scheduler-frontier.md) — moving request ownership into the shared scheduler.
- [40 — Active-peer state in the provider request path](40-active-peer-state-for-provider.md) — carrying active peer state through provider requests.
- [41 — Multi-upstream coordinator](41-multi-upstream-coordinator.md) — a bounded coordinator, which still yielded one usable upstream on that swarm.
- [42 — Background refill and peer failure classes](42-background-refill-and-peer-failure-classes.md) — classifying upstream acquisition outcomes.
- [43 — Concurrent background discovery](43-concurrent-background-discovery-and-official-comparison.md) — discovery before stale timeout, where the official engine also stalled.
- [44 — `content_id` enters the `cid:` resolver path](44-content-id-route-uses-resolver-key.md) — correcting the routing choice made in note 34.
- [45 — `content_id` resolves through the signed catalog](45-content-id-catalog-resolver.md) — reproducing the official signed catalog resolver.
- [46 — Stale detection keys off emitted output](46-stale-deadline-uses-emitted-output.md) — measuring staleness by output rather than arrival.
- [47 — Source signature trailer stripping](47-source-signature-trailer-stripping.md) — the fix for per-piece frame drops.
- [48 — Stale-pool retry and per-piece retransmit](48-stale-pool-retry-and-request-retransmit.md) — handling intermittent freezes after the first window.
- [50 — Live robustness soak](50-live-robustness-soak-84aed.md) — a soak run against the healthy public target.
- [58 — Session reconnect candidates](58-session-reconnect-candidates.md) — retaining upstream addresses learned from peer exchange and announcements.
- [60 — Selecting a live start from window evidence](60-live-start-window.md) — withholding media requests until the start position is known.
- [53 — DHT lookups correlate KRPC responses](53-dht-krpc-response-correlation.md) — matching replies by source and transaction id instead of trusting any packet.
- [61 — Incremental live peer discovery](61-incremental-discovery.md) — starting the first candidate before discovery completes.
- [66 — Admit discovery during initial connection selection](66-incremental-initial-connect.md) — the follow-up that allowed both to run together.
- [63 — Content-id resolution and BEP-9 fallback](63-bep9-content-id-policy.md) — catalog by default with an explicit raw-hash fallback.
- [67 — Injected direct-TS SDT continuity](67-injected-sdt-continuity.md) — a gate that emits its own SDT at video access points.
- [71 — Authenticated catalog lookup by swarm infohash](71-cold-infohash-catalog.md) — looking up a cold swarm through the signed catalog.
- [72 — Bounded DHT routing cache evaluation](72-dht-routing-cache-evaluation.md) — why the evaluated cache was not shipped under the no-regression criterion.

## Measurement and acceptance

- [62 — Catalog response signatures and request nonces](62-catalog-response-authentication.md) — the verification step the resolver had omitted.
- [64 — Controlled startup revalidation for #166](64-startup-revalidation.md) — a fresh-state, simultaneous comparison against the official engine.
- [65 — Paced startup validation attempt for #166](65-paced-startup-validation.md) — a paced variant of that comparison.
- [68 — Longer live-player soak and bounded recovery probes](68-live-robustness-validation.md) — the soak run with fresh storage and a disposable engine container.
- [69 — Independently observable live statistics](69-live-statistics.md) — reading a source snapshot while the next output is pending.
- [70 — Controlled startup acceptance for #167](70-startup-acceptance.md) — the acceptance run for that startup issue.
- [Selected upstream compatibility and responsiveness changes](75-selected-upstream-adoption.md) — the downstream adaptation following the November 2026 review.
- [Infohash safety acceptance audit (#164)](../infohash-validation-164.md) — what a bare infohash is allowed to open.
