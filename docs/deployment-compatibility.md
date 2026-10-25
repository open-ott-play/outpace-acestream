# Deployment compatibility

Outpace AceStream keeps the `outpace` executable, `OUTPACE_*` configuration and
Ace-compatible HTTP routes used by existing local and Kubernetes deployments.
The repository name identifies the project; deployments do not need to rename
services, persistent data directories or player URLs.

## Descriptor and tracker support

A transport descriptor with `type=hls` is resolved as an HLS origin, without
inventing piece geometry or starting a swarm. `/ace/getstream` and
`/ace/manifest.m3u8` redirect to its original HTTP(S) manifest; JSON mode returns
that playback URL with `transport=hls`. Signed query bytes and relative playlist,
segment and key resolution remain the origin's responsibility. Malformed HLS
descriptors fail explicitly. This is origin handoff, not HLS-over-P2P relay.

Classic live descriptors support observed 8 KiB and 16 KiB chunk geometry while
retaining piece-size, divisibility and wire-index limits. Catalog-resolved swarm
descriptors pass through the verified infohash index before playback, avoiding a
second catalog fetch without trusting caller-supplied transport URLs as shared
index authority.

HTTP(S), UDP and DHT discovery operate concurrently. Each HTTP tracker request
uses a pinned public address, no environment proxy and no redirects. HTTP
collection has four concurrent requests, a five-second total deadline,
64 KiB response limits and at most 200 accepted public IPv4 peers. Individual
requests have a four-second deadline covering DNS and the response body.
Results become available as each tracker completes. Dropping the discovery
receiver cancels owned work. HTTP tracker destinations are always public even
when `OUTPACE_TRACKER_ALLOW_NON_GLOBAL=1` permits private UDP trackers.

## Compatibility HLS ownership

`OUTPACE_HLS_IDLE_TIMEOUT_MS` defaults to `60000` and accepts `1..=21600000`.
Successful media requests and manifest requests keep their own client lease
alive. Status polling and missing segment probes do not. A pending manifest
request protects cold startup from idle expiry; cancellation releases that
protection. The independent six-hour authorization deadline and explicit stop
still apply. One client's expiry does not stop another client's shared source.

Direct `/ace/getstream` requests own their hidden compatibility lease from the
start of source acquisition through the response body. Cancelling a pending
provider lookup or a queued same-source request releases that lease immediately,
without revoking another viewer's session.

Playback tokens bind to the source and HLS packager created for that playback.
After the source ends, a fresh request may open a replacement; queued requests
share that replacement even if it also ends immediately. Old tokens cannot read,
reopen, or stop the replacement. Pending startup and replacement cleanup keep
their source owned until the requesting client can take over.

A newly opened source starts yielding media only after its first reader is
attached. Temporary ownership pins do not start consumption, so an immediately
available or finite source cannot lose its first packets while startup waits for
an HLS worker or an earlier generation's cleanup. Metadata remains available
before that first reader; pull-driven statistics and EOF update after it starts.

Resolved live metadata is cached for its original TTL, with 128-entry and 8 MiB
owned-buffer limits. Reads do not prolong stale descriptors. VOD resolution is
serialized per source, so a slow catalog request cannot block unrelated sources;
idle cached VOD handles are released by the normal reaper.

## Release qualification

Synthetic tests cover descriptor routing, URL preservation, malformed metadata,
tracker response shapes, destination guards, response limits, deadlines and
client ownership. Passing them does not demonstrate public swarm availability,
first decoded frame latency or device codec support. Qualify a release using the
same bounded source, route, resource settings and player before switching an
existing deployment. Retain the previous immutable image digest for rollback.

Peer-network rewind and durable DVR are separate features. Neither a larger seed
cache nor these compatibility changes enables an archive by itself.
