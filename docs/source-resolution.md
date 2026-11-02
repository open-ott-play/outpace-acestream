# Source resolution and playback ownership

An Ace identifier selects a descriptor, not a media format. Resolve the descriptor
before selecting a playback path: live swarm, single-file VOD or direct origin HLS.
The typed result carries metadata without inventing a swarm identity, piece geometry,
live/VOD classification or local controls for an HLS origin.

## Origin HLS

By default, once resolution identifies HLS, compatibility HTTP routes hand the original manifest
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

## Managed origin HLS (opt-in)

`OUTPACE_MANAGED_HLS=1` routes resolved HLS through the local engine. The default
remains direct handoff. Compatibility starts return a local playlist redirect or
JSON with `is_direct: true`, `managed_origin: true`, `playback_url`, `command_url`
and `stop_url`.
Follow the returned URL; `DELETE stop_url` cancels only that viewer. The managed
relay has no swarm statistics, peer controls or native audio conversion.
Finite managed HLS requests reuse a bounded pool of HTTP/1 connections. Every
request and redirect still validates the URL and resolves DNS afresh; a changed
address retires the previous pinned client. Each relay's pool stores at most 16
reusable client entries, with one idle connection per client, a 15-second idle
connection timeout and a fixed 60-second reuse lifetime. In-flight responses may
outlive eviction and remain subject to the existing four-response bound.
Pool reuse does not change request deadlines,
Range or encoding validation, cache policy, or the response-memory bound. It is
not used for continuous source streams and does not enable proxy environment
variables, automatic redirects, or cross-origin HTTP/2 coalescing. Reqwest policy
retries remain disabled. Hyper may transparently reopen an unusable reused
connection only before the request starts writing; failed or partially received
responses are not replayed.
With history explicitly enabled, eligible single-playlist TS origins also expose
[bounded retained replay](timeshift.md#managed-origin-ts-replay).
Metadata/discovery responses still describe the origin; they do not start
this relay. `play --resolve` continues to print the original URL.

The relay rewrites master variants, alternate renditions, I-frame playlists,
media segments and initialization maps. Relative references use the final URL
after bounded redirects. Child URLs retain their own query, without inheriting a
parent signature. Media/discontinuity sequences, programme-date-time, explicit
and implicit byte ranges, and ENDLIST remain player-visible. The player chooses
variants and performs buffering; media is relayed without transcoding.

Acquisition accepts HTTP(S), validates every redirect and DNS result, disallows
credentials and non-global destinations in production, and does not use system
proxies, cookies or ambient authorization. HTTPS-to-HTTP redirects fail. There are
at most three redirects, four active upstream fetches and sixteen admitted client
requests. Acquisition, including queueing, is bounded by thirty seconds. A returned
body retains its admission until consumed or dropped. Manifests are bounded
by 1 MiB, 16 KiB per line and 512 references; media responses by 16 MiB. Larger
segments fail explicitly. A session keeps at most 1,024 URL entries and 512 KiB of
URL text. Idle playback expires after sixty seconds and every playback after six
hours, even if active. At most sixty-four playbacks coexist.

Origin playlists may use HTTP `Content-Encoding: gzip`. The relay bounds both
the compressed wire body and the decoded playlist independently to 1 MiB, checks
the original Content-Length before decoding, and validates every gzip checksum
and footer. Up to sixteen concatenated gzip members are accepted; truncated
members, trailing junk, unsupported or stacked encodings fail explicitly.
Decoding shares the request's cancellation and thirty-second deadline, including
time spent queueing. Rewritten playlists are served uncompressed with their new
length. Media and byte-range resources continue to require identity encoding;
they are never transparently decompressed.

Only media with explicit fresh origin cache permission is cached, up to 16 MiB
and 1,024 entries globally, for at most thirty seconds. Age, Date, max-age and
s-maxage reduce freshness; private/no-cache/no-store, Vary, Set-Cookie and ranges
prevent reuse. Media URLs stay stable across identical and sliding playlist reloads.
An observed media- or discontinuity-sequence rollback creates a new epoch; it is
an explicit restart boundary, not a claim of seamless origin recovery.
Cache generations are separate from URL identity,
and an older in-flight body cannot repopulate a newer generation.
Master/rendition references and each rendition's current media references remain
pinned. Removed media/map references have a grace interval derived conservatively
from playlist duration plus maximum segment duration, capped by playback expiry.
If the bounded registry cannot admit an update, it fails atomically rather than
deleting referenced or unexpired resources. This keeps URLs addressable, not
media stored indefinitely; ordinary live resource requests still depend on the
origin. Explicit retained replay has its own byte and time limits and can serve
previously captured segments after their origin URLs expire.
Stop and expiry cancel
pending work and make its URLs unavailable.

Identical playlist reloads preserve fresh media cache entries. A fingerprint of
the playlist bytes and rewritten resource identities still invalidates the cache
when a playlist or resolved origin changes. This does not renew media freshness:
the original response headers and thirty-second ceiling continue to apply.

Local playlists use `.m3u8`; supported media filename suffixes are retained as
validated hints for normal FFmpeg extension checks. Unknown or extensionless origin
media remains opaque and is not qualified for players requiring filename extensions.
Initialization maps with implicit byte-range offsets remain unsupported.
`GET /origin-hls/status` reports only `enabled` and `active_sessions`, including lazy
admissions; operational idle checks must examine it as well as `/streams`.

An origin HTTP rejection retains the gateway's `502` response and adds the numeric
`X-Outpace-Upstream-Status` header. For example, an upstream `403` produces that
header with value `403` and the body `origin_upstream_forbidden`. Statuses `401`,
`404`, `410`, `429`, and `503` use `origin_upstream_unauthorized`,
`origin_upstream_not_found`, `origin_upstream_gone`, `origin_upstream_rate_limited`,
and `origin_upstream_unavailable`; other rejected statuses use
`origin_upstream_http_status`. Origin URLs, headers, and error bodies are never
forwarded. Transport failures and invalid successful responses have no upstream
status header; their existing error codes and gateway statuses remain unchanged.

Encrypted keys, content steering, variable substitution and low-latency HLS parts,
preload hints, delta updates and server controls are explicitly unsupported.
Ordinary date ranges are preserved, but resource-bearing interstitial date ranges
are rejected rather than exposing unmanaged asset URLs to the player.
This is an HTTP origin relay, not Ace's hybrid HLS-over-P2P transport. Its limits
and errors are described in the [HTTP compatibility contract](protocol/compat-matrix.md).

## File selection and metadata

The documented direct-manifest gateway is available at
`/hls/manifest.m3u8?manifest_url=<percent-encoded HTTP(S) HLS URL>` when both
`OUTPACE_ACE_COMPAT=1` and `OUTPACE_MANAGED_HLS=1` are enabled. It accepts exactly one
`manifest_url` parameter and returns a `302` redirect to a private local playlist.
Admission is lazy: following that playlist performs the guarded origin fetch and
HLS validation. Invalid queries do not admit a viewer or fetch a source. With the
compatibility gate off the route is absent (`404`); with only managed HLS off it
returns `501`. Other parameters, including `format=json`, are unsupported here.
It uses the managed relay's existing limits and public-destination policy; it does
not create a swarm or file table. This is the
[documented HLS gateway](https://wiki.acestream.media/Engine_HTTP_API#How_to_play_HLS_broadcast),
not support for the separate `direct_url` selector or hybrid HLS-over-P2P.

URI `index` and HTTP `_idx` are separate contracts. Only a valid decimal zero is
accepted while playback supports a single file; non-default, malformed, duplicate
or unsupported selection requests fail before starting a source. The sampled
Android HTTP client uses `stream_id=-1` to express no stream selection; this
sentinel is accepted. Other stream/playlist selections, `direct_url`, and transport
uploads are not silently treated as another selector. SDK expansion flags remain
accepted for ordinary supported descriptors. Malformed legacy `pieces` and
multi-file descriptors cannot fall through as live. Single-file VOD piece counts
must agree with the verified length and geometry.

The sampled Android GET builder emits its descriptor query twice. HTTP source
admission therefore coalesces equal decoded repetitions of `content_id`,
`infohash`, `url`, `magnet`, and the SDK's optional `transport_file_cache_key`.
Different values still fail before resolution; other repeated parameters,
including file selection, options and commands, remain errors. Cache keys remain
inert and do not create a descriptor cache. This exception does not change CLI
URI parsing or the direct-manifest gateway's single-parameter contract.

`get_media_files` carries `type: live` or `type: vod` only after classification,
and verified single-file VOD `size`. Standard single-file metainfo can also supply
its advisory `info.name` as `filename`: nonempty UTF-8, at most 4,096 bytes, without
control characters. Invalid or missing names are omitted without changing the
original infohash or piece geometry. The name is display metadata, not a trusted
filesystem path. Verified live sources expose the supported output MIME
`video/mp2t` and the display fallback `stream.ts`; that fallback is not a filename
extracted from the descriptor. Legacy live titles are not filenames. VOD MIME is
not inferred from an extension and remains absent when unknown.

Infohash and magnet metadata queries consult the provider's existing verified
live descriptor before reporting an unresolved identity. The lookup accepts only
the exact requested infohash and neither opens a source nor performs network I/O.
Cold hashes remain unresolved. HLS and unresolved identities still have no
invented file table. An Android discovery-first flow requiring a nonempty HLS
file list, or a VOD player requiring known MIME, remains unqualified. A successful
direct playback response does not establish those separate client contracts.

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
by this engine or qualifying TS segments captured by the managed-origin relay.
Its generation checks, source ownership, pruning and explicit
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

Adaptive selection within the engine, hybrid HLS-over-P2P, multi-file playback and
persistent DVR require separate backends and qualification. They are not implied
by direct origin handoff or the managed relay.
