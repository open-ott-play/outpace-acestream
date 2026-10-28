# Source resolution and playback ownership

An Ace identifier selects a descriptor, not a media format. Resolve the descriptor
before selecting a playback path: live swarm, single-file VOD or direct origin HLS.
The typed result carries metadata without inventing a swarm identity, piece geometry,
live/VOD classification or local controls for an HLS origin.

## Origin HLS

Once resolution identifies HLS, compatibility HTTP routes hand the original manifest
URL to the client. Direct JSON identifies this with `is_direct: true`; redirects preserve the URL,
including its percent escapes and signed query. Native stream routes also hand off
direct HLS. The engine creates no local source session, statistics URL, command URL
or recording for this path. Responses are not cacheable: a subsequent start must
resolve a fresh origin URL.

A lazy `getstream` JSON request may issue a provisional local lease before its
source is resolved, with unknown media flags. If opening it later discovers HLS,
playback redirects to the origin and revokes the provisional lease. Previously
issued controls then expire; they do not control the origin player.

Use `outpace play --resolve 'acestream://<content-id>'` to print a direct HLS URL
for a player. Ordinary `play` writes TS bytes, and `play --vod` writes verified VOD
file bytes; neither silently changes its stdout format to a playlist URL.

The player owns manifest refresh, variant selection, alternate audio/subtitles,
relative segment URLs, redirects and supported HLS tags. Local HLS packaging of a
TS swarm is a separate path. Passing an origin URL does not implement Ace's hybrid
HLS-over-P2P protocol, local HLS recording or persistent programme catch-up.

Official context: [HLS broadcasting](https://docs.acestream.media/broadcasting/details/hls/).
The direct-response distinction also appears in the public
[Android engine client](https://github.com/acestream/acestream-engine-android-core).
Static client compatibility is not a device playback measurement.

## Resolution lifetime and trust

Concurrent catalog requests for the same query share an in-flight result. Query
kinds stay separate, unrelated keys do not wait on a global network lock, and the
in-flight map is bounded. Once callers finish, that result is not retained as an
indefinite cache of signed origin URLs. Cancellation drops the owned request;
another waiter can complete resolution without a detached fetching task.
Waiting and taking over a cancelled initializer share one per-caller catalog
deadline, derived from the existing host count and timeout; takeover does not
restart that budget.

Existing catalog validation and infohash binding remain required. Caller-supplied
transport URLs and opted-in peer metadata do not populate the trusted infohash
index. An HLS result has no fabricated infohash. Unknown live/encryption properties
remain unknown in metadata responses.

Live discovery retries a completed empty first run once. The first run, optional
retry and warm-cache initialization share a ten-second initial deadline. A silent
run is bounded by that deadline rather than spawning another discovery generation.
An active run continues supplying late candidates after the first candidate starts
playback. Cancellation drops its owner.

## VOD container boundary

The native VOD byte-range API and `play --vod` preserve arbitrary verified file
bytes. HLS VOD packaging has a narrower contract: aligned 188-byte MPEG-TS packets,
bounded playlist geometry and a qualifying prefix. Unsupported containers return
HTTP 415 before HLS media is advertised. Packet validation also covers subsequent
streamed data. A late failure terminates the response; it cannot retract HTTP
headers or partial packet bytes already sent before that failure was detected.

The existing VOD HLS layout uses byte-based segment sizes and nominal durations.
It does not provide a container-aware presentation-time index, GOP-aligned random
access, MP4/MKV remuxing or frame-accurate seeking. Use raw ranges for arbitrary
containers. These limits are separate from live HLS packaging and retained replay.

## Replay and qualification

[Retained replay](timeshift.md) is a per-viewer, finite snapshot of media packaged
by this engine. Its generation checks, source ownership, pruning and explicit
return-to-live behavior are tested independently. Original `liveseek` units and
UTC programme archive semantics are not inferred from method names or piece
timestamps. Unsupported original controls remain explicit errors.

Offline regression coverage includes direct JSON versus engine-owned sessions,
signed-URL preservation, metadata classification, catalog coalescing/cancellation,
empty/silent discovery, VOD prefix and streaming failures, and H.264/HEVC access
points confined to validated PES payload. RC fuzzing exercises the production
media gate with generated PAT/PMT/PES seeds and arbitrary read boundaries.
Synthetic FFmpeg acceptance measures decoded retained replay and independent
viewers. Passing these checks does not establish availability of every public
source or complete original-engine equivalence.

Managed origin-HLS acquisition, adaptive selection within the engine, hybrid
HLS-over-P2P, multi-file selection and persistent DVR require separate backends
and qualification. They are not implied by direct origin handoff.
