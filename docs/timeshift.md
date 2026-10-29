# Timeshift beta: retained replay and peer-history preparation

The 0.9.0 beta introduces finite replay of media this engine has already packaged,
plus an experimental Rust API for fetching exact historical peer-piece ranges.
These are separate capabilities. Neither is a persistent programme archive or a
promise that any past programme is available.

## Enable retained replay

Add these values to the **engine service's** environment and restart it:

```sh
OUTPACE_ACE_COMPAT=1
OUTPACE_HISTORY_SECONDS=600
OUTPACE_HISTORY_BYTES=134217728
OUTPACE_HISTORY_TOTAL_BYTES=268435456
```

History is disabled by default (`OUTPACE_HISTORY_SECONDS=0`). The time limit is
at most 3,600 seconds. The per-source byte budget defaults to 128 MiB; the shared
budget shared by native packagers and managed-origin viewers in one engine
defaults to 256 MiB and must cover the
per-source budget. Retention obeys both elapsed time and accepted media duration,
plus a hard 4,096-segment limit. The first limit reached wins. At 8 Mbit/s,
128 MiB holds roughly two minutes, not ten; backing allocation capacity is
charged, so usable payload can be smaller still.

These budgets cover retained history allocations. Live windows, advertised
segment grace, in-flight HTTP bodies, peer buffers and runtime metadata add to
process memory. A viewer does not get another copy of the history store. Replay
snapshots hold only coordinates and cannot prevent pruning.

Ordinary live playlist, segment and readiness reads also prune expired history.
A stalled source therefore releases expired history reservations while its
clients continue polling, without requiring a separate timeshift-status request.
Its current live window remains available under the normal live retention rules.

Active native HLS packagers and eligible managed-origin viewers collect history.
Raw native `/ace/getstream` sessions, external HLS redirects and inactive channels
do not secretly start recording. Native history
ends when the packager is removed and disappears on process restart. The
existing disposable peer seed cache is a different store.

## Managed-origin TS replay

Starting with **0.9.1-beta.6**, `OUTPACE_MANAGED_HLS=1` and nonzero
`OUTPACE_HISTORY_SECONDS` also retain completed media fetched by an active origin
viewer. Existing installations with both settings enabled opt into this additional
use of their existing shared budget. There is no background fetch or durable
recording, and the normal thirty-second HTTP cache is a separate mechanism.

This first backend accepts a root media playlist with `EXT-X-INDEPENDENT-SEGMENTS`,
unique full-segment URLs and complete HTTP-200 MPEG-TS bodies. Each retained body
must have CRC-checked PAT/PMT tables and first-selected-video access-point
evidence. The packet-level access-point detector is a conservative heuristic,
not a complete access-unit or dependency validator; generated-media decoder tests
qualify the tested formats. Master/alternate-rendition playlists, initialization maps, fMP4,
byte ranges and audio-only media remain outside this replay subset. Their supported
live relay behavior is unchanged. A declaration or filename alone is insufficient
to make media seekable. Gaps in captured segments cannot be skipped by a replay.

An overlapping playlist refresh does not cancel recording of a segment that is
already being fetched. The completed segment may still enter history after it
slides out of the live playlist, provided its original descriptor remains in the
retention window. A changed source epoch, replaced descriptor, pruning or stopped
session prevents that old request from publishing into the new history. HTTP
cache freshness and history ownership are checked separately.

Managed playback JSON includes `command_url=/origin-hls/<token>/cmd`. The commands
`timeshift_status`, `timeshift_seek`, `timeshift_live` and `stop` use the HTTP-200
JSON envelope and generation checks described below. Status identifies
`kind=retained_origin_ts`, `time_basis=observed_media_duration`, and reports
`enabled`, `supported` and `reason`. Inspect `history.seekable_start_ms` and
`history.end_ms` before seeking; supported format does not mean media has arrived.
`back` uses seconds behind the retained head; `position_ms` uses the observed
media-duration timeline, never UTC or original Ace peer-piece coordinates.

Each origin viewer owns its retained sequence table and one finite replay view.
Its replay URL is `/origin-hls/<token>/replay/<generation>/index.m3u8` with
`EXT-X-ENDLIST`. Reload the URL returned by a successful seek. A failed seek leaves
the previous view intact; a successful seek or live return invalidates prior
replay generations. **The original `/origin-hls/<token>/0.m3u8` URL stays live**
and is also the URL returned by `timeshift_live`. This differs from the native
generation-scoped live URLs described below. Another viewer remains independent.

Replay serves only retained bytes, without re-fetching the origin. Whole segments
and an initial `Range: bytes=0-` probe return 200; other ranges and `If-Range` are
unsupported. Pruned media or stale replay generations return 410; stopped or
expired viewers return 404. Sequence/identity rollback clears incompatible
history. Stop, expiry and process restart discard recording. Existing session,
request and body limits still apply, and in-flight retained responses hold their
memory reservation until consumed or dropped.

## Native playback contract

1. Start HLS with `GET /ace/manifest.m3u8?content_id=0123456789abcdef0123456789abcdef01234567&format=json`.
   Replace the synthetic ID with your own current source. Keep the returned
   `playback_url`, `command_url` and `playback_session_id`.
2. Play `playback_url`. Query `command_url?method=timeshift_status` to inspect
   the available history and current viewer generation.
3. Request `command_url?method=timeshift_seek&back=30&generation=0` to prepare
   a replay starting at least 30 seconds behind the latest completed retained
   segment. The boundary aligns backwards to a known video access point.
4. Check the JSON `error` field. On success, replace the player's media source
   with the returned `response.playback_url`. `reload_required` is true and
   `response.view.generation` is the new generation. On failure, keep playing
   the existing URL; the previous view was not changed.
5. To return to live, request `command_url?method=timeshift_live&generation=1`,
   using the current generation, then reload the returned URL. End the lease
   with the existing `command_url?method=stop`.

`timeshift_status`, `timeshift_seek` and `timeshift_live` are **Outpace
extensions**, not claims of complete compatibility with the original engine's
control protocol. Control responses use the existing HTTP-200 JSON envelope;
callers must check `error`. Sending the expected generation is recommended to
reject reordered client commands. Server-side compare-and-swap also prevents
concurrent preparations from replacing a newer view.

`timeshift_seek` also accepts `position_ms` instead of `back`, for example
`command_url?method=timeshift_seek&position_ms=12000&generation=0`. This is an
unsigned integer coordinate on the same `retained_media_duration` timeline
reported by status, not Unix time or an offset from the viewer's current frame.
It keeps the selected position stable while the live head advances. A position
of zero is valid only while that beginning is retained and independently
decodable. The completed `end_ms` is an exclusive boundary; targets at or beyond
it are unavailable. The start aligns backwards to an independent access point,
and the returned snapshot ends at the completed head observed during preparation.
Read the returned snapshot's `start_ms` for the actual aligned start.

Supply exactly one of `back` (positive seconds behind the current head) or
`position_ms` (absolute media milliseconds). Duplicate command parameters,
including percent-encoded spellings of the same name, are rejected. Negative,
fractional, signed or overflowing `position_ms` values, conflicting selectors,
expired positions, missing ranges and stale generations leave the current view
unchanged. `timeshift_live` accepts neither seek selector. Coordinates belong to
one source/packager lifetime and must not be reused after source replacement or
engine restart. This extension does not implement original-engine `liveseek`
or UTC programme catch-up.

The status reports `peer_archive=false`, `persistent=false`, `utc_seek=false`.
History positions are accumulated **media duration**, not Unix timestamps.
`seekable_start_ms` identifies the earliest independent entry contiguous with
the current retained head, or is null when no such entry is available. Intervals
identify retained sequence runs and discontinuities. Retained bytes
alone do not guarantee a seekable video access point. The existing provider
`livepos` statistics remain provider observations; they are not a viewer cursor.

A replay is a finite, immutable HLS VOD snapshot ending at the head observed at
seek time. It has `EXT-X-ENDLIST` and starts at its first segment. It does not
silently grow into live playback. Use the explicit live command at its end.
Each viewer has independent mode and generation, while acquisition stays shared.
A successful transition invalidates the previous generation's URLs, including
the original live URL. Clients must discard queued old requests and use the
new URL; returning to live never revives the original URL.

## Expiry and recovery

Old generation URLs return HTTP 410. A requested replay segment that has been
pruned also returns 410. A consumed prefix may expire while still-retained
subsequent segments continue playing from an already-loaded playlist. Reloading
a snapshot whose prefix is gone returns 410 rather than a silently truncated
playlist. A stopped/expired lease or replaced source returns 404. Failed seeks
and unavailable live returns leave the current view intact.

All Ace compatibility responses, including errors and control results, carry
`Cache-Control: no-store`. Preserve this policy in a proxy: caching a temporary
404 or 503 can hide subsequent recovery or return stale playback state.

Starting with beta.4, completed HLS segments release spare assembly-buffer
capacity of at least 256 KiB before their storage is shared by live playback,
history and the advertised-segment grace window. Smaller spare capacity stays
accounted for. This improves retention within the same byte limit without
changing media bytes, sequence numbers, expiry rules or the configured limits.
Actual retained duration still depends on bitrate and access-point boundaries.

A paused or slow viewer can fall outside retention. Show an explicit message
and offer a new seek or return to live. Increasing the limit cannot recover
media already pruned or media from before acquisition began. Retaining more
bytes also cannot repair an unavailable source or unsupported codec.

## hls-proxy and television players

The engine exposes the capability; the player must retain the JSON control
session, render the available range, issue a seek and reload the returned HLS
URL. Proxies must carry the tokenized `/ace/t/...` playlists and `/ace/tc/...`
segments with their generation intact. Existing `/ace/m/...` and `/ace/c/...`
live clients continue to work when no seek is requested. Managed-origin clients
must also proxy the returned `/origin-hls/<token>/cmd` control URL and
`/origin-hls/<token>/replay/<generation>/...` media URLs without removing their
token or generation. They must use the status time basis rather than treating
these coordinates as programme dates.

A classic EPG catch-up menu normally asks for a programme by UTC start time.
This beta cannot satisfy that contract. Do not add `catchup-days`, fabricate
programme dates or map a short peer buffer onto several days of guide history.
Tauri, LG and Fire remote-control integration needs its own range/reload tests;
API and desktop decode qualification does not establish those device tests.

## Experimental historical peer acquisition

`ace_swarm::history::fetch_history` accepts a freshly authenticated descriptor,
a finite piece range, candidate peers and explicit limits. It opens independent
connections and reassemblers, verifies complete piece payloads, and either
returns the entire requested range or a typed error. It does not call the live
follower, publish to the shared seed store, move existing viewers or jump ahead
when a piece is unavailable. Dropping the future cancels its connection.

Hard limits include 128 pieces, 64 MiB requested payload, 32 candidate peers,
60 seconds, bounded control/wire input and one active connection at a time.
The receive allowance covers the whole operation, including handshakes,
read-ahead, incomplete frames and bytes received before cancellation or peer
fallback. Replacing a connection does not reset the allowance. The wire limit
is twice the requested byte budget plus 512 KiB; a separate decoded-message
limit also applies.
The operator example has tighter defaults and a 90-second overall budget that
also includes resolution/discovery and two comparison fetches:

```sh
# Supply ACE_CONTENT_ID privately; the probe never reads the local ID registry.
export ACE_HISTORY_OUTPUT=/absolute/path/to/a/new-private-directory
export ACE_HISTORY_BEHIND_PIECES=32
export ACE_HISTORY_PIECES=24
cargo run --locked -p ace-swarm --example history_probe
```

The example is distributed as source, not another installed daemon. It writes
`history.ts`, `reference.ts` and a diagnostic report. A peer window only claims
availability. The source signature binds media payload, **not the external
piece timestamp or wire index**. Thus successful verified acquisition is not
itself proof of rewind. Decode both samples, compare PCR and decoded frame PTS
within the same source epoch, require advancing media and check discontinuities.
Keep raw captures and live identifiers private.

The first bounded field experiments obtained older signed payloads and decoded
frames preceding the comparison range. That establishes feasibility on the
sampled source, not universal history depth. Integrating this reader into
continuous playback still needs a trusted media-time index, decoder-ready
range preparation, pacing, cancellation and device-level qualification.

## Qualification

Offline tests exercise budget accounting, discontinuities, missing ranges,
non-keyframe continuations, viewer isolation, racing commands, stale URLs,
source replacement, expiry and stop. For actual generated-video qualification:

```sh
cargo test -p ace-engine --test media_history \
  generated_media_rewinds_decodes_and_returns_live_without_moving_another_viewer \
  -- --ignored --exact --nocapture
```

This opt-in test requires `ffmpeg` and `ffprobe`, generates its own media and uses
loopback HTTP only. Missing tools fail the opted-in run. The normal production
playback path still has no mandatory FFmpeg dependency.
