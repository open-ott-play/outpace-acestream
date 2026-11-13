# Optional AceStream search

Set `OUTPACE_COMPAT_ACESTREAM_SERVICES=1` to mount `GET /search`. The default is
false. This setting is independent of legacy playback compatibility. Requests go
to the three native AceStream search-service hosts; no account API key is needed.
The protocol uses recovered global client constants and nonce-bound signatures.
These signatures provide protocol compatibility, not trusted descriptor provenance.

```sh
curl --get http://127.0.0.1:6878/search --data-urlencode 'query=synthetic query' \
  --data-urlencode 'page=0' --data-urlencode 'page_size=10'
```

The encoded query string is limited to 2048 bytes before form decoding. Accepted parameters are `query` (up to 256 bytes), `category` (64 bytes), `page`
(0–10000), and `page_size` (1–200). Duplicate and unknown parameters are rejected.
Results use the compatibility envelope `{"result":{"total":0,"results":[]},"error":null}`.
Only allowlisted grouped metadata and syntactically valid infohashes survive
normalization. Content IDs, arbitrary URLs and provider-specific extra fields are
not forwarded. Service failure is an HTTP error, never a fabricated empty result.

Outbound resolution uses the existing public-address-validated fetch path, with no
environment proxies or redirects. A request has a ten-second total deadline, a
three-second per-host limit and a shared 1 MiB download budget across all attempts.
Decoded and normalized responses are bounded separately. At most two uncached
searches run concurrently; admission allows one new request per second and failures
trigger a 30-second cooldown. Successful normalized results are cached for 30 seconds
with at most 32 entries and 8 MiB of serialized metadata. Caller cancellation drops
the entire operation; there are no detached search tasks.

Search does not start playback, grant access to protected sources, add channels to
a playlist, or bypass descriptor validation. The existing playlist generator's
language/category/name exclusions remain authoritative for generated playlists.
Clients should treat returned metadata as untrusted discovery suggestions. Existing
known-source playback remains independent of search-service availability.

The original search-service protocol uses HTTP. Its shared constants are public
compatibility material; nonce checks do not turn that transport into TLS or make
metadata authoritative. The endpoint should be exposed only to intended clients,
as with the rest of the engine API.
