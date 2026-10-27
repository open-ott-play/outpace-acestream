# Configuration reference

The daemon reads these values at process startup. Restart a binary service or
recreate a container after changing its environment; editing a shell profile or
Compose `.env` does not change an already running process. Native and compatibility
playback share the same underlying runtime settings.

This reference describes the checked-in source. For **v0.8.1**, explicitly set
`OUTPACE_CACHE_DIR` whenever you change `OUTPACE_DATA_DIR`; the automatic derivation
was fixed after that release. See [installation](installation.md),
[container deployment](containers.md), and [Kubernetes](kubernetes.md) for complete
examples. Persistent warm-peer hints currently require Unix; Windows uses
in-memory hints when an explicitly empty cache directory is supplied with memory
buffering. Windows disk-cache and filesystem durability have separate limits.

## Common settings

Environment variables parsed by the daemon include:

- `OUTPACE_BEP9_CONTENT_ID_FALLBACK` - raw transport-hash BEP-9 fallback for content-id
  resolution after catalog failure, default off. Set to `1` on the receiving daemon or
  standalone `play` process for outpace broadcast Ace links from another daemon. Applies to
  live and VOD; peer results do not populate the catalog-provenance infohash index.
- `OUTPACE_BIND` - HTTP bind address, default `127.0.0.1:6878`.
- `OUTPACE_RTMP_BIND` - RTMP ingest bind address, default `127.0.0.1:1935`.
- `OUTPACE_BROADCAST_OPTIONS` - strict named startup profile JSON, default `{}`; at most
  64 names and 262144 UTF-8 bytes. Optional title, bitrate in bytes/second, piece length,
  quality and ordered categories; invalid values fail with `invalid_broadcast_options`.
  Defaults: path name, `8375`, `65536`, `auto`, `["other"]`. No hot reload or profile-only mint.
  See [bounds and immutable resume](native-api.md#broadcast-startup-profiles).
- `OUTPACE_BROADCAST_SOURCES` - startup source JSON map, default `{}`; at most 64 names,
  256 KiB UTF-8, URLs up to 8,192 bytes. Duplicate/unknown fields fail startup. Source timing
  defaults: connect 5s, read 15s, reconnect initial 1s, maximum 30s; see the native API limits.
- `OUTPACE_ALLOW_NON_GLOBAL_BROADCAST_SOURCE` - independent operator-only source address
  opt-in; only exact `1` enables it, default off. It cannot be set in an API request.
- `OUTPACE_DATA_DIR` - persistent identity/cache root, default platform data dir
  plus `outpace`.
- `OUTPACE_PEER_LISTEN` - inbound peer listener bind, default `0.0.0.0:8621`.
- `OUTPACE_SEED_STORE_BYTES` - byte budget for retained piece data (sizes both cache backends),
  default `134217728` (128 MiB). A hard safety cap; on a live stream
  `OUTPACE_SEED_RETENTION_SECS` is the primary limiter.
- `OUTPACE_SEED_RETENTION_SECS` - age bound (seconds) for a live reseed store, default `45`:
  retain roughly this much recent downloaded data for reseeding instead of filling
  `OUTPACE_SEED_STORE_BYTES`, so RAM tracks bitrate rather than always growing to the byte cap.
  `0` disables the age bound (byte-only, the pre-0.2 behavior). VOD stores are always byte-only.
- `OUTPACE_CACHE_TYPE` - where the seed store keeps piece data: `memory` (default) or `disk`.
  `disk` trades RAM for capacity, mirroring Acestream's disk-cache option.
- `OUTPACE_CACHE_DIR` - root dir for warm-peer hints in both cache modes and
  disk-mode piece files (one subdir per served stream; see below). Defaults to
  `<OUTPACE_DATA_DIR>/cache` after selecting the data directory; an explicit cache path
  takes precedence. In disk mode, startup wipes this directory only when it is empty or
  already contains a regular `.outpace-cache` sentinel
  file (written automatically on first successful disk-cache start). A symlink is not accepted
  as a sentinel. An unmarked non-empty path is refused rather than deleted.
  Unix memory mode preserves peer hints across restarts; disk mode clears this
  shared cache root at startup, including its previous hints.
- `OUTPACE_PREFETCH_PIECES` - optional exact number of pieces behind the live edge to start at.
  When unset, outpace derives the depth from the startup target and advertised bitrate, falling
  back to `32` pieces when the bitrate is unavailable.
- `OUTPACE_SESSION_BUFFER` - per-client fan-out channel depth, default `256`;
  must be at least `1`.
- `OUTPACE_PREBUFFER_MS` - target server-resident live startup queue duration, default `30000`.
  The server intentionally waits before sending the first media so clients begin with more
  tolerance for swarm jitter. `0` disables startup prebuffering and preserves the previous
  immediate-release behavior.
- `OUTPACE_PREBUFFER_BYTES` - byte threshold for releasing the startup queue, default
  `134217728` (128 MiB). The input chunk crossing this threshold is retained intact before
  release, so queued payload can exceed the threshold by at most one input chunk. Queue
  metadata, backing-allocation capacity and the rest of the daemon add memory overhead.
- `OUTPACE_PREBUFFER_TIMEOUT_MS` - absolute deadline after the first nonempty media chunk for
  reaching the target duration, default `15000`. Source gaps and transport-clock resets discard
  stale queued media but do not restart this deadline. On expiry, outpace releases the remaining
  queue and then passes through incoming media. This does not bypass the source's live-head
  catch-up gate or guarantee that a player has received its first decodable frame.
- `OUTPACE_REQUEST_TIMEOUT_MS` - per-piece request timeout before re-requesting or skipping an
  evicted gap, default `1500`; must be lower than `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS`. A live
  player drains in realtime, so raising this leaves a stuck piece to be healed by the much slower
  `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS` pool teardown instead — a visible playback gap.
- `OUTPACE_STALE_UPSTREAM_TIMEOUT_MS` - whole-upstream no-progress timeout before reconnecting,
  default `12000`.
- `OUTPACE_REQUEST_CHECK_INTERVAL_MS` - request timeout sweep interval, default `1000`; must be
  lower than or equal to `OUTPACE_REQUEST_TIMEOUT_MS`.
- `OUTPACE_MAX_ACTIVE_UPSTREAMS` - active upstream peer cap for one live follower, default `4`;
  max `256`.
- `OUTPACE_MAX_PARALLEL_CONNECT` - peer connect race batch size, default `12`; max `1024`.
- `OUTPACE_MAX_PIECE_ADVANCE` - max pieces scheduled in one forward step, default `256`;
  max `16384`.
- `OUTPACE_MAX_REASM_PIECES_AHEAD` - max pieces accepted ahead of the emit cursor, default `512`;
  must be at least `OUTPACE_MAX_PIECE_ADVANCE`, max `65536`.
- `OUTPACE_HLS_SEGMENT_PACKETS` - hard MPEG-TS packet ceiling per HLS segment and packet-count
  fallback for streams without usable PCR, default `65536` (about 12.3 MB). This is a
  memory-safety bound, not the primary cut mechanism: segments normally cut on a keyframe once
  the target duration elapses, so the ceiling must comfortably hold one target-duration segment
  of a peaky high-bitrate stream (a 2160p HEVC GOP can burst well past its average).
- `OUTPACE_HLS_WINDOW_SEGMENTS` - retained HLS live window size, default `8`.
- `OUTPACE_HLS_SEGMENT_DURATION_MS` - requested PCR-timed HLS segment duration, default `5000`.
- `OUTPACE_HLS_STARTUP_SEGMENTS` - completed segments retained before a new HLS playlist request
  becomes ready, default `6`. `0` is accepted as a compatibility setting and behaves as `1`.
  The effective count `max(1, OUTPACE_HLS_STARTUP_SEGMENTS)` must not exceed
  `OUTPACE_HLS_WINDOW_SEGMENTS`; an inconsistent configuration fails startup. If a source ends
  cleanly before reaching this count, complete segments from its current generation may still
  form a finite playlist once initial source catch-up is complete. An incomplete segment,
  startup deadline failure, explicit stop or failed generation is not promoted this way.
- `OUTPACE_HLS_STARTUP_TIMEOUT_MS` - maximum HLS playlist startup wait, default `45000`; must be
  at least `OUTPACE_HLS_SEGMENT_DURATION_MS`. Native `.m3u8` requests and compatibility
  `/ace/manifest.m3u8` requests use one deadline for acquisition and response preparation,
  including descriptor resolution on the compatibility route and waiting for a shared source
  to start. The request returns retryable HTTP `503` with `Retry-After: 1` if this budget expires.
  Waiting for completed segments does not restart the request budget. This setting does not
  impose a new deadline on continuous MPEG-TS, VOD or existing replay requests.
- `OUTPACE_HLS_IDLE_TIMEOUT_MS` - compatibility-HLS client idle lease, default `60000`,
  range `1..=21600000`. Successful manifest/media requests renew their client's
  lease; status polling does not. A pending manifest request protects startup.
  An independent six-hour authorization deadline still applies.
- `OUTPACE_HLS_AUDIO_NORMALIZE` - optional live-HLS audio conversion, default `false`.
  Accepts `true`, `false`, `1`, or `0`. Selected supported AC3/MP2/MP3 audio becomes
  AAC-LC stereo; video is copied. This is not general video transcoding.
- `OUTPACE_HISTORY_SECONDS` - 0.9.0 beta retained-HLS replay duration, default `0`
  (disabled), maximum `3600`. Only active HLS sources collect history; it is not
  persistent or UTC-indexed. Retention also obeys the byte and segment-count limits.
- `OUTPACE_HISTORY_BYTES` - retained history backing-allocation budget per HLS
  source, default `134217728` (128 MiB). Must be at least 188 bytes and within
  the platform's safe in-memory pool limit.
- `OUTPACE_HISTORY_TOTAL_BYTES` - shared retained-history budget across all HLS
  sources in one manager, default `268435456` (256 MiB). Must cover the per-source
  budget and remain within the platform limit. This is not a total RSS cap.
  See [timeshift](timeshift.md) for controls, expiry and memory accounting.
- `OUTPACE_FFMPEG_PATH` - direct FFmpeg executable used when audio normalization is
  enabled, default `ffmpeg`. It must support the required codecs and startup probe.
  The published base container does not include FFmpeg. See
  [audio behavior and constraints](playback.md#live-hls-audio).
- `OUTPACE_MAX_UNCHOKED` - stable desired unchokes per exact served key, default `8` (U).
  For I interested peers, the coordinator selects `min(I, U + 1)` desired states in
  interest-entry order, with one rotating tail peer on nominal 10-second ticks. `0` still
  selects one optimistic peer. Watch updates and pending writes make this a desired selection,
  not an instantaneous wire-visible bound. Metadata replies bypass Interested/Unchoke.
- `OUTPACE_SEED_TTL_SECS` - idle-TTL (seconds, default 300; 0 disables) after which an *ownerless*
  seed-registry entry (one with no live producer lease) is force-evicted by the reaper. A backstop
  only — normal teardown rides the lease drop, and entries held by a live producer or a broadcast
  are never reaped. This TTL limits registry lifetime, not connection lifetime.
- `OUTPACE_MAX_INBOUND` - retained inbound connection limit, default `64` (G); `0` acts as `1`.
  Unidentified/incomplete handshakes consume this capacity too. Excess accepted sockets close
  immediately without queuing behind retained connections.
- `OUTPACE_MAX_INBOUND_PER_STREAM` - admitted connections per canonical stream, default `16`
  (N), serialized in Config as `max_inbound_per_stream`. Broadcast media and metadata aliases
  share this allowance, including idle, uninterested, choked and metadata-only connections.
  At defaults the 17th combined admission closes before a handshake response. `0` restores
  global-only admission and preserves U; positive N greater than G is valid and redundant for
  occupancy. Empty, negative, malformed, overflow and non-Unicode environment values fail
  startup; absence retains 16. Changes require restart/recreation. For rollback set
  `OUTPACE_MAX_INBOUND_PER_STREAM=0`; Compose requires an explicit service environment override,
  shown in [deployment.md](deployment.md), rather than `.env` interpolation alone.
- `OUTPACE_ENABLE_SEEDING` - reciprocal upload gate over outbound leech connections
  (answering peers' chunk requests). Self-announce is gated on `OUTPACE_ENABLE_INBOUND`.
  Outbound reciprocal serving is outside the inbound admission/coordinator budgets.
- `OUTPACE_ENABLE_INBOUND` (default: on) - inbound peer serving (S2) gate. On by default,
  intentionally matching the Acestream engine's out-of-the-box behavior: a full P2P participant
  that binds its peer port (`OUTPACE_PEER_LISTEN`), accepts inbound peers, seeds, and
  self-announces to trackers + DHT. Only the HTTP API (`OUTPACE_BIND`) stays on localhost by
  default; the exposed surface is the peer port, as with Acestream. Set
  `OUTPACE_ENABLE_INBOUND=0` to disable the inbound listener and seeder
  self-announcement. This does not disable reciprocal upload on outbound
  connections; that is controlled separately by `OUTPACE_ENABLE_SEEDING`.
- `OUTPACE_ENABLE_PORT_MAPPING` - best-effort UPnP/NAT-PMP mapping for the inbound TCP peer
  listener, default off. Use only when the process owns the host/LAN-facing network address;
  normal bridge-mode containers should use a manual router-to-Docker-host TCP forward.
- `OUTPACE_PORT_MAP_BACKEND` - `auto`, `upnp`, `natpmp`, or `none`; default `auto`.
- `OUTPACE_PORT_MAP_EXTERNAL_PORT` - optional external TCP port requested from the gateway.
- `OUTPACE_ACE_COMPAT` - enables the supported Ace HTTP subset, default off. Accepts
  exactly `1`, `true`, `0`, or `false`. A present value takes precedence over the legacy
  alias, including `0`/`false`; an empty, malformed, or non-Unicode selected value fails
  startup. Set `0` to explicitly disable compatibility routes.
- `OUTPACE_EXPERIMENTAL_ACE_COMPAT` - deprecated fallback, parsed only when
  `OUTPACE_ACE_COMPAT` is unset, with the same strict boolean rules. Rename it to
  `OUTPACE_ACE_COMPAT` when migrating; an unused invalid alias is ignored.
- `OUTPACE_ACE_PEERS` - comma-separated bootstrap peer list for the live path.
- `OUTPACE_TRACKERS` - comma-separated `scheme://…` (typically `udp://host:port/announce`)
  tracker URLs minted into fresh broadcast descriptors for every producer path. Self-announce
  uses that stored list for both keys on resume; process changes affect future identities only,
  and empty stored lists remain empty.
  When set (non-empty after trimming) it fully replaces the built-in public default
  (`udp://t1.torrentstream.org:2710/announce`); unset leaves that default in place. Entries with
  no `scheme://` prefix (or longer than 256 bytes) are dropped with a warning, and the list is
  clamped to 64 entries. Note outpace only **self-announces** to lowercase `udp://` trackers;
  entries with any other scheme are kept in the minted descriptor for other clients but warned
  about, since outpace itself never announces there. Use this to point a broadcast at a
  private/local tracker (announcing to a private-address tracker also requires the non-global
  tracker allowance, tracked separately).
- `OUTPACE_TRACKER_ALLOW_NON_GLOBAL` - set to `1` to allow announcing to trackers that resolve
  to non-globally-routable addresses (private/LAN/loopback/link-local/multicast). Default deny;
  the SSRF guard on tracker destinations stays closed for normal use. Opt in only for controlled
  self-hosted or offline/test swarms on a trusted network. Scoped to tracker traffic.
- `OUTPACE_ALLOW_NON_GLOBAL_TRANSPORT` - set to `1` to allow fetching a transport descriptor
  file from a private/LAN/loopback host. Default deny; this relaxes the SSRF guard on the
  transport-file fetch **specifically** (not peer or tracker traffic), and even when enabled it
  still blocks link-local (e.g. the `169.254.169.254` cloud-metadata endpoint), unspecified,
  multicast, broadcast, and documentation ranges. Opt in only for controlled self-hosted / LAN
  swarms. If that transport file also points at a private *tracker*, announcing to it
  additionally needs `OUTPACE_TRACKER_ALLOW_NON_GLOBAL`.

These transport/tracker `*_ALLOW_NON_GLOBAL*` knobs enable on the exact string `1` only (unlike the general
boolean gates below, `true` does **not** enable them).

Boolean gates accept exactly `1`, `true`, `0`, or `false`; other values are configuration errors
rather than silently disabling a feature.

## Storage and lifetime

Keep the persistent identity separate from disposable media. Memory buffering
still uses a small Unix peer-hint file when a cache directory is configured.
Increasing a cache size does not enable catch-up, historical seeking or DVR.

Inbound admission grants no bandwidth/equal-share or class reservation guarantee; metadata-only
clients can fill a stream's allowance. DELETE removes registry/producer ownership, while existing
connections retain data and coordinator Arcs; their old coordinators stop receiving registry
rotations. Aborting only the listener leaves detached handlers running. The serve loop's 250 ms
polling can cancel reads and their inner timeouts, so those per-operation timeouts do not establish
a whole-session idle deadline. See [shared seeding](protocol/shared-seeding.md) for the
admission, coordination and lifetime boundaries.

The disk cache is **ephemeral**: a dedicated cache directory is cleared when a store is created and
never reloaded across restarts (live piece data goes stale), which also avoids serving
evicted-stale pieces. Disk I/O is currently synchronous. The first disk-cache start writes a
`.outpace-cache` sentinel so later wipes can distinguish a dedicated cache from an accidental
operator path. An existing non-empty cache without that regular sentinel file fails startup
instead of being deleted; use an empty directory or create the file only after verifying that
all existing contents may be deleted.

Disk mode never silently converts `OUTPACE_SEED_STORE_BYTES` into an equal per-stream RAM
allocation. An invalid/unwritable cache root fails daemon startup. If a new per-stream directory
cannot be created later (for example after a permission change), outpace logs an escalating
`disk store creation failure #N` error and keeps playback running with **zero piece retention** for
that stream: no memory cache fallback is allocated, and reciprocal/inbound seeding has no retained
pieces to serve. This favors the operator's RAM bound over transient seeding capacity.

In disk mode each served stream keeps its pieces under
`<OUTPACE_CACHE_DIR>/<infohash_hex>-<generation>` (a process-unique suffix per store instance).
The directory is removed automatically when the stream is torn down (leech consumer disconnects,
broadcast `DELETE`, or process exit), and a dedicated cache root is wiped on startup only if it
is empty or contains `.outpace-cache`, so no per-stream directories accumulate.
