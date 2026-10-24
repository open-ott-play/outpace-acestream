# Troubleshooting

Diagnose the layer that failed before changing buffers, network policy or
identity state. Outpace can be healthy while a source is unavailable, and can
serve media that a particular player cannot decode. This guide applies to the
current code and calls out published **v0.8.1** differences.

## Establish a small baseline

1. Record the running version, target and source revision from `/source.json`,
   not only the binary found in your interactive shell. A service manager may
   launch a different path or retain an older environment.
2. Check process health, then inspect active sessions privately. For the default
   local listener on Linux or macOS:

   ```sh
   curl --fail --show-error --max-time 5 http://127.0.0.1:6878/healthz
   curl --fail --show-error --max-time 5 http://127.0.0.1:6878/source.json
   curl --fail --show-error --max-time 5 http://127.0.0.1:6878/streams
   ```

   On Windows, use `Invoke-RestMethod` for the same URLs. For a remote deployment,
   use its trusted access route or a local port-forward. Do not publish the
   control API just to inspect it.
3. Note the actual consumer: a browser, VLC, hls-proxy, a container or a TV. Record
   the route family and whether the input is a content ID, infohash, transport
   URL or VOD descriptor. These are different contracts.
4. Reproduce with one source and one consumer for a bounded interval. Check
   media/segment progress and actual decoding. Compare another currently known
   working source if that helps distinguish source-specific behavior. Preserve
   deployed routing, DNS and proxy settings during the baseline.

Keep raw session output, identifiers, URLs and media private. Before sharing a
report, remove real content IDs, infohashes, channel names, tokens, source URLs
and credentials. Never attach `identity.seed` or peer-hint files. See
[development reports](development.md#useful-reports).

## The process does not start

| Symptom | Check and next action |
| --- | --- |
| `command not found` | Use the installed absolute path or add its directory to `PATH`. Service managers do not inherit your interactive shell configuration. |
| Wrong architecture / executable format | Select the native target from the [installation table](installation.md#choose-an-architecture). Docker also needs a published image platform or an explicitly configured emulator. |
| Windows reports `VCRUNTIME140.dll` missing | Install Microsoft's [Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist) for the selected x64 or ARM64 binary. Do not copy an arbitrary DLL into the package. |
| macOS blocks an unrecognized download | Verify the release and checksum, then follow [Apple's downloaded-app guidance](https://support.apple.com/en-us/102445). The release does not promise notarization. |
| `address already in use` | Identify the existing owner of the HTTP, RTMP or peer port. Stop the correct service or deliberately assign different ports and a separate data directory. |
| Invalid environment value | Read the first startup error and the [configuration contract](configuration.md). Empty strings are not a universal way to disable options; strict booleans reject malformed values. |
| Audio capability/startup probe failure | If normalization is enabled, provide a supported FFmpeg executable and codecs. The base image does not include it. If the feature is not needed, disable it explicitly. |
| Identity or data-directory error | Verify that the configured service account can access its intended state directory. Preserve existing state; do not delete the identity to hide a permissions problem. |

Use the owning service manager's logs: `journalctl -u outpace.service` for the
systemd example, the configured stderr file for the LaunchAgent, `docker logs`
for a named container, or `kubectl logs` for the selected pod. Limit log output
to the relevant interval; logs may contain private operational details.

## The API works but playback never starts

### Source selection and resolution

A content ID is not an infohash. Native live routes use `cid:` and `ih:` to make
that distinction explicit. Bare native hex IDs mean content IDs. Compatible
`id=` also means a content ID; `infohash=` requires an actual swarm infohash.
Renaming a query parameter does not convert between them.

A cold infohash lookup needs a verified descriptor carrying geometry and source
key information. Native `422` responses describe descriptor-resolution failures;
finding peers alone cannot repair missing or mismatched metadata. Compatibility
JSON URL creation can succeed before playback resolution fails. Use a current
source and inspect the playback request, not just the URL-creation response.
Do not bypass descriptor verification. See [resolution rules](playback.md#infohash-inputs).

For a broadcast created by another Outpace instance, the raw transport-hash link
has a separate opt-in resolution policy. Read
[content-ID resolution](playback.md#content-id-resolution) before using
`OUTPACE_BEP9_CONTENT_ID_FALLBACK=1`; it is not a general fix for all catalog IDs.

### Discovery and media progress

No productive peers can result from an unavailable or stale source, failed
catalog/discovery access, egress restrictions, unreachable candidates or peers
that have no useful current media. A zero-peer counter by itself does not prove
a channel is permanently dead. Compare a known working source through the same
engine, route and client before drawing that conclusion.

Check the actual workload's DNS, outbound TCP and outbound UDP with return
traffic. A container or pod can have a different network path from its host.
Inbound TCP exposure may improve reachability, but opening a port cannot make a
source exist or make an unavailable descriptor valid. DHT does not require a
fixed inbound UDP 8621 mapping. Preserve intentional external-DNS or VPN routing
policy in an existing installation; isolate any network experiment and restore
its settings afterward.

### Startup and buffering

Default HLS readiness asks for six completed segments with a requested duration
of five seconds each. That can require roughly 30 seconds of usable media after
acquisition starts; keyframe spacing, input pacing, prebuffering and retries
can add delay. It is not a 30-second wall-clock promise. The default HLS
playlist-readiness wait is 45 seconds after stream/packager acquisition and can
return retryable `503` if readiness has not been reached. It does not bound the
whole HTTP request: earlier descriptor resolution can add time.

Inspect whether bytes and completed segments are progressing. A larger buffer
cannot solve no media, an invalid descriptor or an unsupported codec. Reducing
startup targets trades resilience for latency. Account for engine, proxy and
player buffers together; do not independently increase all three. Change one
setting at a time and compare actual startup and sustained decode results.

## HTTP errors and stale sessions

| Symptom | Meaning and remedy |
| --- | --- |
| `/ace/...` returns `404` while native routes work | Compatibility is off by default. Set `OUTPACE_ACE_COMPAT=1` in the running service environment and restart/recreate it. Confirm the endpoint is in the supported subset. |
| Old `/pid/.../stream.mp4` URL fails | It is not an Outpace route. Use the [native or compatibility URL](playback.md#hls-proxy-vlc-and-remote-players), with the correct identifier type. |
| Native live request returns `422` | Inspect the returned resolution reason. Supply a current verified descriptor path instead of treating any 40-hex string as interchangeable. |
| HLS returns `503` during startup | It may still be collecting segments; inspect progress and retry according to the response. A failed normalization session is different and does not restart just because the client retries. |
| Old playlist or segment URL returns `404` | The segment may have left the retained window, or a compatibility token may refer to an expired client or replaced packager. Open a fresh source URL; do not save tokenized segment URLs as permanent catalog entries. |
| Playlist loads but segments fail | Check the URLs the client actually requests, reverse-proxy path rewriting, scheme and reachability. Every playlist and segment request must reach the intended engine/session. |
| Broadcast source attachment conflicts | Another producer may own the broadcast. Inspect its source status; detach the correct producer before replacing it. Deleting the broadcast also removes its identity. |

Compatible clients have independent idle leases and an absolute authorization
limit. Successful media/manifest access renews idle time; status polling does
not. A pending manifest request protects startup. Refer to
[compatibility ownership](deployment-compatibility.md#compatibility-hls-ownership)
for exact behavior.

A native stream `DELETE` force-stops shared acquisition and can interrupt other
viewers. Prefer normal client closure and idle cleanup. If a failed session
requires explicit teardown, inspect active consumers before deleting it.

## VLC works but a TV, browser or proxy does not

- **Addressing:** loopback belongs to the process doing the request. For a TV,
  `127.0.0.1` is the TV; for a container it is that container. A Kubernetes
  ClusterIP or service DNS name is normally reachable only inside the cluster.
- **Complete URL path:** if the proxy fetches and relays media, the engine URL
  must be reachable by that proxy. If it returns a playlist directly, the
  player must also reach every referenced segment. Rewriting only the first
  URL may not be enough.
- **Browser policy:** inspect browser network errors for mixed HTTP/HTTPS
  content or cross-origin restrictions. Solve them at the trusted proxy and
  origin boundary, without disabling browser protections.
- **Video codec:** a playable byte stream can still use an unsupported codec,
  profile, resolution or bit depth. Try a source the device demonstrably
  supports. HLS packaging does not transcode video, and 4K/HEVC support is not
  guaranteed by the engine.
- **Audio codec:** raw TS and default HLS can contain audio unsupported by the
  device. Optional normalization covers selected AC3/MP2/MP3 inputs, not every
  audio format or transport layout. See the next section.

The engine does not own playlist curation or the player's UI. An empty or
unreadable catalog is a separate proxy/player problem unless the failing
request is actually an engine media/API request.

## HLS audio normalization fails

`OUTPACE_HLS_AUDIO_NORMALIZE=true` requires an operator-provided FFmpeg at
`OUTPACE_FFMPEG_PATH` (or `ffmpeg` on the process's `PATH`). The base container
intentionally does not install it. A host FFmpeg is not automatically present
inside a container. Package and maintain it in a derived image if needed.

The feature copies video and converts only the supported selected audio to
AAC-LC stereo. It has strict MPEG-TS layout and codec constraints and a global
limit of four conversion children. It is not a video-transcoding service, a
surround-preserving conversion or an unlimited worker pool. Read the complete
[HLS audio contract](playback.md#live-hls-audio) before enabling it.

A process, codec, resource or output failure is terminal for that session.
`503` with `Retry-After: 1` does not mean the worker will be relaunched. Correct
the cause, allow teardown or deliberately stop the affected session after
checking consumers, then open a fresh source session. Native AAC/video-only
inputs do not need an audio-conversion child.

## Cache, permissions and identity

### v0.8.1 uses the wrong cache directory

In published v0.8.1, overriding `OUTPACE_DATA_DIR` does not recompute the default
cache directory. In a service account or container, this can produce warm-peer
cache I/O warnings even while playback works. Set an explicit matching path,
for example `OUTPACE_CACHE_DIR=/var/lib/outpace/cache`, and restart the owning
service. The correction in `main` is for a subsequent build; documentation does
not change an installed v0.8.1 binary.

### Unix cache ownership

The durable warm-peer backend requires an existing cache directory to belong
to the running user and have no group/world write permissions. Newly created
directories have owner-only `0700` permission bits (a setgid parent can also
contribute the inherited setgid bit, producing `2700`); private regular files
must have mode `0600`. The daemon
can create a missing directory under a writable parent. If one already exists,
inspect its owner and mode before making a narrowly scoped correction. Do not
recursively change ownership or permissions of a whole PVC or shared home
folder. Never paste the cache contents into an issue.

Memory-mode media buffering can still use this small durable peer cache. Disk
mode clears the shared cache root at startup, including previous peer hints.
Hints are optional discovery aids; they are not media recordings or proof that
an address still serves a source.

### Windows peer hints

The current durable hint backend requires Unix filesystem checks. On Windows,
use the [PowerShell 7.5+ recipe](installation.md#windows-install-and-run-with-powershell)
with memory mode and an explicitly empty `OUTPACE_CACHE_DIR`. Older PowerShell
versions remove empty environment variables and therefore do not produce the
same configuration. Granting more filesystem permissions does not implement
the missing Windows persistence backend.

### Disk media cache

Disk mode uses disposable piece files. Startup accepts an empty dedicated cache
or a directory with the regular `.outpace-cache` sentinel, then clears it. An
unmarked nonempty directory is refused to protect unrelated files. Select a
new empty dedicated directory instead of adding a sentinel to an unknown path.
Never use the identity directory itself as the disposable cache root.

If a per-stream disk store later fails, playback can continue with **zero piece
retention** for that stream; it does not silently allocate the configured disk
budget in RAM. Resolve the specific I/O or capacity issue. The disk cache does
not reload a playback history on restart and does not provide DVR.

### Identity cannot be loaded

Retain `identity.seed` and broadcast state during upgrades. An unreadable or
invalid existing identity should be investigated and restored from the correct
private backup, not deleted to force a new node identity. Stop the owning
instance before migration. Two independent daemons must not share one writable
identity directory.

## Container and cluster problems

Follow [Docker](containers.md) or [Kubernetes](kubernetes.md) for complete,
version-pinned examples. Check the workload rather than assuming host state:

- **Image pull failure:** verify registry access, the exact tag/digest and a
  supported platform. Public GHCR images do not require an obsolete private
  registry mapping. Do not silently substitute another engine image.
- **Pod Pending / PVC Pending:** inspect scheduling events, resource requests,
  StorageClass availability, volume topology and access mode. k3s local-path
  storage is node-local; a claim is not an automatic cross-node backup.
- **CrashLoopBackOff:** inspect current and previous container logs. Check the
  first startup error, data/cache environment and writable mounts. A read-only
  root filesystem needs a separate writable data volume and temporary storage.
- **Healthy pod, unreachable client:** inspect the Service selector/endpoints
  and the client's network boundary. `kubectl port-forward` is a diagnostic
  local tunnel, not the permanent TV access route or a P2P reachability test.
- **Repeated restarts:** distinguish application exit from a failed probe,
  eviction or OOM termination. Do not hide a storage/configuration error by
  disabling probes or running as root.

Use one replica for the supplied shared-identity deployment. Horizontal scaling
requires separate identities and client/session routing design; changing
`replicas` is not sufficient for a shared stateful media service.

## High memory, stalls or slow starts

The seed-store byte budget and prebuffer cap are scoped limits, not a global
process memory limit. Multiple distinct sources, per-client queues, retained
HLS segments, protocol state and optional FFmpeg children add up. Compare
active source/client counts, workload memory and actual output progress before
changing limits. A process or cgroup OOM requires a different response from a
source that simply stopped sending useful data.

Reduce unnecessary concurrent sessions and ensure clients close their leases.
Tune one measured bottleneck at a time. Smaller HLS segments depend on input
keyframes, and larger connection races may add sockets/work without finding
better peers. Record warm/cold state when comparing startup; a warm cache or
already active shared source changes the experiment.

A rollback should restore the verified earlier binary/image and its appropriate
configuration while preserving compatible identity state. Keep the previous
artifact, exact source pairing, configuration and private backup before any
upgrade. Never retag an existing release or delete the PVC as a rollback step.
