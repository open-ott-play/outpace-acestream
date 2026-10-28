# Playback and integration

Install a [release binary](installation.md) or deploy the
[container](containers.md) first. Commands below use a source checkout where
`cargo run` is shown; replace `cargo run -p ace-engine --bin outpace --` with
`outpace` to use an installed binary. All `<content-id>` and `<infohash>` values
are placeholders. Obtain a current authorized source; no example identifier in
this repository is a working channel.

## First playback

Run the daemon:

```bash
cargo run -p ace-engine --bin outpace -- serve
```

With no subcommand, `outpace` defaults to `serve`.

For native live playback, use `/streams/ace/cid:<content-id>` (or a bare content
id), adding `.m3u8` for HLS. Both forms share status and DELETE session keys.
Infohash playback uses `/streams/ace/ih:<infohash>` and resolves a verified descriptor
through the catalog when needed. **Migration:** bare hex ids on native live routes now mean content ids,
matching compat `id=`; update older infohash URLs to include `ih:`. See the
[native API](native-api.md) for selector and session details.

Play a known Acestream link to stdout:

```bash
cargo run -p ace-engine --bin outpace -- play 'acestream://<content-id>' > live.ts
```

The descriptor may instead select a direct HLS origin. For that source, use
`outpace play --resolve 'acestream://<content-id>'` and pass the printed URL to
your player. Native and compatibility HTTP playback routes redirect directly;
the JSON compatibility response sets `is_direct: true` and owns no local controls
or recording. See [source resolution and playback ownership](source-resolution.md)
for HLS, VOD container limits and cancellation semantics.

`play` accepts four mutually exclusive selectors, applied in precedence order
`content_id` > `infohash` > `url` > `magnet`:

- `acestream://<content_id>` or `acestream:?content_id=<40-hex>`;
- `acestream:?infohash=<40-hex>` - resolves the live descriptor through the signed catalog;
- a transport-file URL - either passed directly (`play 'https://host/x.acelive?a=1&b=2'`,
  which handles a URL carrying its own `&`-joined query) or as
  `acestream:?url=<https://…>` (in the `acestream:?` form the URL's own query must
  be percent-encoded, e.g. `%26` for `&`). The descriptor is fetched over http/https
  with SSRF protection (private, loopback, and link-local hosts are blocked), a 1 MiB
  size cap, disabled redirects, and a request timeout; unsafe, oversized, or
  non-transport responses fail closed;
- `magnet:?xt=urn:btih:<40-hex-or-32-base32>` - a BitTorrent v1 magnet, reduced to
  its infohash (v2 `urn:btmh:` magnets are rejected), then resolved through the signed catalog.

The `/ace/getstream` compatibility route accepts the same selectors plus legacy
`id=<40-hex>`, which is an alias for `content_id` (not `infohash`). Its precedence is
`content_id` > `infohash` > `id` > `url` > `magnet`. A `url=` selector uses a self-contained
playback id, so playback works after a daemon restart without any server-side alias table.

### Content-id resolution

Live and VOD content-id inputs use the catalog by default. A catalog failure returns an
error rather than automatically querying BEP-9 metadata peers. The daemon can also resolve
its own broadcasts through its trusted local broadcast registry without a catalog lookup.

`OUTPACE_BEP9_CONTENT_ID_FALLBACK=1` explicitly enables the raw transport-hash convention
used by outpace broadcasts, after catalog failure. This applies to both `serve` and standalone
`play` (including `--vod`). Peer metadata is accepted only when the SHA-1 of the complete
wrapped transport bytes equals the requested identifier. This check does not establish the
binding of a public catalog content id; that derivation remains unresolved. See
[the content-id policy](protocol/notes/63-bep9-content-id-policy.md).

### Infohash inputs

Catalog replies require the engine-compatible response signature, matching request nonce and
transport checksum. The response key is embedded in clients, so this does not provide
server-only authentication over plain HTTP; see [the protocol evidence and trust limits](protocol/notes/62-catalog-response-authentication.md).

A bare infohash identifies a swarm but carries no piece geometry or source key. For
native `ih:`, compat `infohash=`/`magnet=`, and CLI infohash/magnet inputs, outpace
first checks its own broadcasts and verified descriptor index, then queries the
signed catalog directly by infohash. Catalog response authentication, request
nonce and checksum checks precede decoding, and the descriptor's computed swarm
infohash must match the requested value before discovery or indexing.

The verified descriptor supplies geometry, pubkey and trackers. Successful lookups
populate the bounded in-process index; failures are suppressed for five seconds
in a separate cache capped at 128 entries. A verified local descriptor takes
precedence over a cached failure. Unavailable or mismatched descriptors fail
closed without guessing geometry or falling back to BEP-9. Native playback returns
`422` with a reason; compat JSON URL creation permits cold inputs, with resolution
errors reported when playback is opened. CLI playback exits with an error.

A BEP-9 content-id result or caller-supplied transport URL does not populate the
catalog-provenance index. Transport-URL streams reopen by their `turl-` id.

### Live HLS audio

Live HLS uses its existing native selection by default: the first ADTS AAC
track wins; without ADTS AAC, audio passes through unchanged. Direct/raw TS and
VOD retain their existing behavior.

Set `OUTPACE_HLS_AUDIO_NORMALIZE=true` (default `false`; accepts `true`/`false`
or `1`/`0`) to convert selected AC3 or MPEG Layer II/III audio into AAC-LC stereo,
48 kHz, 128 kbit/s. `OUTPACE_FFMPEG_PATH` selects a direct FFmpeg executable
(default `ffmpeg`, resolved through PATH); it must be nonempty when enabled.
These are the only normalization options. Disabled mode never looks up or probes
FFmpeg. Native ADTS AAC and video-only streams start no conversion process.

Enabled mode requires unencrypted 188-byte TS, one program, one MPEG-2/H.264/HEVC
video stream, valid PCR and current CRC-valid single-packet PAT/PMT sections.
Audio-only, multiple video/programs, fragmented PSI, unidentified private streams
and unsupported selected codecs fail closed. MPEG PMT types do not identify a
layer: bounded elementary-stream frame checks admit Layers II/III and reject
Layer I, malformed, free-format or unaligned MPEG framing. LATM, EAC3 and DTS
cannot be selected. The first ADTS AAC wins, otherwise the first recognized
MPEG audio or AC3 wins; this does not search for a different track after rejection.
Enabled strictness can reject inputs that disabled mode historically passes through.

Conversion copies video without encoding it; player compatibility (including
Safari) is not guaranteed. Stereo conversion can discard surround channels and
lossy re-encoding reduces quality. At most four conversion children run globally;
this is a process bound, not an RSS/CPU capacity promise. Recovery retains completed
segments but playlists wait for fresh-generation readiness. A process, codec,
resource or output failure is terminal for that session: HTTP 503 with
`Retry-After: 1` does not restart a failed worker. Stop/idle teardown and a fresh
session are required. Authorization and expiry still take precedence.

Operators supply and maintain FFmpeg with MPEG-TS mux/demux, native AAC encoding,
MP2/MP3/AC3 decoding and pipe support. Startup checks capabilities and performs a
bounded AC3 conversion before opening listeners. Enabled containers need an
operator-built image containing this binary; the base Docker/compose setup does
not install it. Wrappers launching descendants are unsupported. Embedders must
await manager shutdown before destroying the runtime so children and pumps are
reaped; Drop only signals cancellation.

Synthetic codec acceptance is optional locally: run
`OUTPACE_TEST_HLS_AUDIO_FFMPEG=1 cargo test --locked -p ace-engine --test hls_audio_ffmpeg -- --nocapture`.
Without opt-in it prints an explicit skip. Opt-in requires FFmpeg/ffprobe,
synthetic video/sine inputs and AC3/MP2/MP3/AAC encoders; a missing prerequisite
fails the suite. The designated CI job always opts in. No public media is used.

### VOD (single-file)

For a **VOD** transport (a descriptor carrying a `pieces` SHA1 list), add `--vod` to
download and verify the file to stdout instead of following a live stream:

```bash
cargo run -p ace-engine --bin outpace -- play --vod 'acestream://<content-id>' > movie.mp4
```

Each piece is SHA1-checked against the transport's `pieces` before any bytes are written.
The daemon serves the same content over `GET /vod/<network>/<id>` with a `Content-Length`,
advertises `Accept-Ranges: bytes`, and honors single `Range` requests (`206 Partial Content`)
so players can seek — each served range is still SHA1-verified against the covering pieces.
Multi-file VOD is intentionally unsupported and fails with a clear error. Native
VOD HLS is available at `/vod/<network>/<id>/manifest.m3u8`, with verified segment
byte ranges. These routes require an actual single-file VOD descriptor; they do
not provide an archive of a live channel. The Ace-compatible VOD adapter remains
outside the supported subset.

### Broadcast and managed sources

`outpace broadcast` starts its own serving runtime and enables inbound serving;
it is not a client command for an already running daemon. For an existing
service, use its [broadcast HTTP APIs](native-api.md#http-routes) instead. Run the
following standalone example only after stopping the owning service, or with
deliberately separate ports and data/cache directories:

```bash
cargo run -p ace-engine --bin outpace -- broadcast my-channel
```

The broadcast command prints:

- a raw MPEG-TS HTTP ingest URL;
- an RTMP ingest URL at `rtmp://<host>:<rtmp-port>/live/<name>`;
- an `acestream://<content_id>` link using the raw wrapped transport-file hash.

The originating daemon resolves that link through its trusted local broadcast registry.
A different receiving daemon or standalone `outpace play` needs
`OUTPACE_BEP9_CONTENT_ID_FALLBACK=1` to resolve it through BEP-9 `ut_metadata`; the default
catalog policy does not automatically resolve these raw-hash links. Alternatively, pass the
broadcast transport URL (`http://<host>:<http-port>/broadcast/<name>`) to `outpace play`
or use the `url=` selector. The receiver's transport-URL access policy still applies.

Set one immutable profile for every producer path (raw PUT, RTMP, managed pull and CLI):

```bash
OUTPACE_BROADCAST_OPTIONS='{"synthetic-origin":{"title":"Synthetic broadcast","bitrate_bytes_per_sec":500000,"piece_length":131072,"quality":"HD","categories":[]}}' \
  cargo run -p ace-engine --bin outpace -- broadcast synthetic-origin
```

An unused profile creates nothing. Fresh omissions retain the legacy defaults; resumed
identities retain raw transport/key bytes and match only explicit fields. Changing an
explicit option can fail with `broadcast_options_conflict`; use a new name or explicitly
DELETE/recreate, losing identity/cursor continuity. Each default/profile/stored piece must fit
one full wire piece in the store budget, including disk and unused profiles. See the
[complete profile schema and startup/resume rules](native-api.md#broadcast-startup-profiles).

Attach an engine-owned HTTP(S) MPEG-TS source with `PUT /broadcast/<name>/source`, or
set startup declarations in `OUTPACE_BROADCAST_SOURCES`:

```bash
OUTPACE_BROADCAST_SOURCES='{"demo-source":{"url":"https://source.invalid/live.ts"}}' \
  cargo run -p ace-engine --bin outpace -- serve
```

The URL above is synthetic; replace it with an authorized public, unencrypted MPEG-TS origin.
Acceptance reserves the signing identity and schedules a worker; check
`GET /broadcast/demo-source/source` for upstream health. A worker reconnects on finite EOF
and discards incomplete tails. It keeps exclusive production through backoff and failure;
DELETE the `/source` attachment before replacing it. Detach retains identity and cached data;
DELETE the broadcast itself to remove identity. API attachments last for this process only.
Current environment declarations reattach on restart, including declarations detached or
deleted during the previous process. Remove a declaration to keep it absent after restart.

Source URLs/query tokens stay out of status, logs and persisted broadcast records. Protect
the deployment environment and avoid dumping configuration. Default source policy denies
private/loopback addresses; only exact `OUTPACE_ALLOW_NON_GLOBAL_BROADCAST_SOURCE=1` adds
the documented LAN subset. This switch is independent of transport/tracker opt-ins and
never admits link-local metadata endpoints. Sources ignore proxies and reject redirects.
See [source API/configuration](native-api.md#managed-http-mpeg-ts-sources) and
[storage, restart and deployment guidance](deployment.md#managed-source-declarations).

## hls-proxy, VLC and remote players

Use native HLS when the consumer accepts a direct playlist URL:

```text
http://ENGINE_HOST:6878/streams/ace/cid:<content-id>.m3u8
```

If middleware generates Ace Engine URLs, enable `OUTPACE_ACE_COMPAT=1` and point
its engine base URL at this daemon. A content-id request uses:

```text
http://ENGINE_HOST:6878/ace/manifest.m3u8?id=<content-id>
http://ENGINE_HOST:6878/ace/getstream?id=<content-id>
```

`id=` is a content ID. A source that actually provides an infohash needs the
`infohash=` selector; changing the parameter name does not convert one identifier
type into another. Compatibility is a documented subset, not a promise that
every legacy AceProxy or closed-engine endpoint exists. In particular, use the
documented native or `/ace/` route instead of assuming an old
`/pid/<id>/stream.mp4` URL is supported.

For a local hls-proxy process, the engine can be `127.0.0.1:6878`. In a container,
`127.0.0.1` means that container; in a television browser it means the television.
Use a service name or reachable host address appropriate to the actual HTTP
requester. If the player consumes the returned playlist directly, it must also
reach every segment URL. A private cluster Service works for an in-cluster proxy,
but not directly for a TV outside the cluster. See [Kubernetes access patterns](kubernetes.md).

Outpace does not fetch or curate a channel catalog, invent EPG data, or determine
whether two channel labels refer to the same source. Keep those tasks in the
playlist/proxy layer. Persist reusable source URLs or identifiers there, not
short-lived compatibility-session tokens. Closing one client should release its
lease; a native `DELETE /streams/...` force-stops the shared source and can affect
other viewers.

## Live buffer, VOD and archive are different

Live HLS exposes a bounded sliding window of recently packaged media. Single-file
VOD serves an independently described file and can honor byte ranges. Neither
turns a live source into yesterday's programming. The 0.9.0 beta adds [finite retained-HLS replay](timeshift.md) and an experimental
peer-history reader. Continuous peer timeshift, UTC programme selection and durable DVR are
[planned work](roadmap.md), not features of v0.8.1.
