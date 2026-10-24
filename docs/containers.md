# Docker and Compose

The public image runs the `outpace` engine and serves HTTP playback to your
player or proxy. It supports Linux `amd64`, `arm64`, and `arm/v7`. Docker Desktop
can run it on macOS or Windows through its Linux VM; a native binary is also
available in the [installation guide](installation.md).

These instructions pin **v0.8.1** to its published multi-platform image digest:

```bash
export OUTPACE_IMAGE='ghcr.io/open-ott-play/outpace-acestream@sha256:fd83adbe4286f4d6b6578370d9b8a92bf9fa3f2e79be7ed15d0db439d4c385a6'
docker pull "$OUTPACE_IMAGE"
```

Docker selects the matching platform from that index. A tag such as `latest`
can move; record the digest you actually deploy and review the
[release notes](https://github.com/open-ott-play/outpace-acestream/releases).
No registry credentials are required to pull this public image.

## Start one container

This example keeps HTTP local to the host, persists identity and warm-peer
hints, and allows outbound P2P with reciprocal uploads. It does not publish an
inbound peer port. Choose this or Compose below; do not start both on port 6878.

```bash
docker volume create outpace-data
docker run -d --name outpace \
  --restart unless-stopped \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges:true \
  --tmpfs /tmp:rw,nosuid,noexec,size=64m \
  --mount type=volume,source=outpace-data,target=/var/lib/outpace \
  --publish 127.0.0.1:6878:6878/tcp \
  --env OUTPACE_BIND=0.0.0.0:6878 \
  --env OUTPACE_RTMP_BIND=127.0.0.1:1935 \
  --env OUTPACE_DATA_DIR=/var/lib/outpace \
  --env OUTPACE_CACHE_DIR=/var/lib/outpace/cache \
  --env OUTPACE_CACHE_TYPE=memory \
  --env OUTPACE_ACE_COMPAT=1 \
  --env OUTPACE_ENABLE_INBOUND=0 \
  --env OUTPACE_ENABLE_SEEDING=1 \
  --env OUTPACE_ENABLE_PORT_MAPPING=0 \
  --health-cmd 'curl --fail --silent --show-error http://127.0.0.1:6878/healthz || exit 1' \
  --health-interval 10s --health-timeout 3s \
  --health-retries 6 --health-start-period 10s \
  --log-opt max-size=10m --log-opt max-file=3 \
  "$OUTPACE_IMAGE"

docker ps --filter name=outpace
docker logs --tail 80 outpace
curl --fail http://127.0.0.1:6878/healthz
curl --fail http://127.0.0.1:6878/source.json
```

The image already selects its non-root `outpace` user. Do not add an arbitrary
`--user` value to this named-volume example: the image's directory ownership
and that user's numeric UID must agree. The Kubernetes example provisions a
different explicit numeric identity and volume access policy.

The health command checks process health, not a working channel. An unhealthy
Docker health status alone does not restart a running container. Follow
[playback verification](playback.md) with a current authorized public source;
successful playlist retrieval plus decoded media is stronger evidence than
an open TCP port. Status and logs can contain source identifiers: redact them
before sharing.

## Use the repository's Compose file

Install a current Docker Engine/Desktop and Compose plugin. From a checked-out
copy of this repository, copy `.env.example` to `.env` if you do not already
have one, then edit it. Preserve an existing operator configuration.

Set these values in `.env`:

```dotenv
OUTPACE_IMAGE=ghcr.io/open-ott-play/outpace-acestream@sha256:fd83adbe4286f4d6b6578370d9b8a92bf9fa3f2e79be7ed15d0db439d4c385a6
OUTPACE_ACE_COMPAT=1
OUTPACE_CACHE_TYPE=memory
OUTPACE_CACHE_DIR=/var/lib/outpace/cache
OUTPACE_ENABLE_PORT_MAPPING=0
```

Create a local `compose.override.yml` to apply the restricted runtime:

```yaml
services:
  outpace:
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    tmpfs:
      - /tmp:rw,nosuid,noexec,size=64m
```

Read the port behavior before starting: the checked-in Compose file binds
HTTP 6878 and RTMP 1935 to **host loopback**, but publishes **peer TCP 8621 on
all host interfaces** and enables inbound serving. This is a deliberate full
P2P mode. For an outbound-only setup, also set `OUTPACE_ENABLE_INBOUND=0` in
`.env`; the mapping remains declared, but no peer listener starts. Keep
`OUTPACE_ENABLE_SEEDING=1` if reciprocal uploads to connected peers are wanted.
Setting both flags to zero is the pure-leecher option.

```bash
docker compose config --quiet
docker compose pull
docker compose up -d --wait
docker compose ps
docker compose logs --tail 80 outpace
curl --fail http://127.0.0.1:6878/healthz
```

Compose's volume is named using the project name (normally
`outpace_outpace-data`), not the separate `outpace-data` volume in the
`docker run` example. Switching methods without migrating the existing volume
creates a different identity. Keep the Compose project name stable.

`docker compose config` without `--quiet` is useful for local inspection but
prints resolved environment values, including any private source URLs. An
arbitrary variable in `.env` is not automatically passed to the daemon: the
Compose file must interpolate or forward it. For additional settings, add an
explicit `environment` entry in your override; see [configuration](configuration.md).

## Storage, cache, and identity

Keep `/var/lib/outpace` on persistent storage. It holds node and broadcast
identity, not just disposable media. Do not share a writable identity volume
between two simultaneously running engines.

**v0.8.1 requires `OUTPACE_CACHE_DIR` to be explicit when using a custom
`OUTPACE_DATA_DIR`.** Its default cache path was computed before the data-dir
environment override; depending on the runtime user it can point outside the
mounted volume. The examples use `/var/lib/outpace/cache`, which fixes that
case without making the root filesystem writable. The source branch includes
a later fix, but that does not change the already published v0.8.1 binary.

Even `OUTPACE_CACHE_TYPE=memory` persists small warm-peer hints under that cache
directory. On Unix the engine requests a private directory mode of 0700 and
regular files of mode 0600, owned by its runtime UID. A setgid parent can
produce directory mode 2700 through inheritance. An existing directory may have a
different mode if it remains daemon-owned and is not group/world-writable;
private files still require 0600. A mount must permit that owner to create
them; symlinks, unexpected ownership, and unsafe permissions are rejected.
Do not solve a cache warning with recursive `chmod 777`, or change the node's
private identity files. Check the configured path and ownership instead.

Prefer a new named volume for the first Docker deployment. A bind mount is
also possible, but provision its ownership for the image's actual numeric
user before starting. Do not assume the image user is UID 1000. Inspect the
chosen image with `docker run --rm --entrypoint id "$OUTPACE_IMAGE"` and plan
any migration of existing data with the service stopped.

Disk piece caching is optional and changes cleanup behavior. It uses a
dedicated cache root and a safety sentinel; disk pieces are removed on startup
and stream teardown. Disk-mode startup also clears previous warm-peer hints;
memory mode preserves those hints across restarts. The cache directory must not equal or contain the data
directory. See [cache configuration](configuration.md) before enabling it.
Memory buffers, warm-peer hints, and disk pieces are **not a catch-up archive**;
[playback and archive limits](playback.md) describe what seeking is supported.

## Networking and player access

- **HTTP TCP 6878:** playback, monitoring, source offer, and control API share
  this port. Outpace has no general API authentication layer. Compatibility
  playback tokens do not protect the full administrative surface. Keep it on
  loopback, a trusted private network, or behind a gateway with appropriate
  access controls.
- **RTMP TCP 1935:** optional broadcast ingest. It is not required for watching
  AceStream sources. Leave it private unless you deliberately operate ingest.
- **Peer TCP 8621:** optional inbound peer participation. Publish it only when
  inbound is enabled. In the `docker run` example that means recreating the
  container with `OUTPACE_ENABLE_INBOUND=1`,
  `OUTPACE_PEER_LISTEN=0.0.0.0:8621`, and `--publish 8621:8621/tcp`.
  Compose already declares this fixed mapping.

For incoming peers, allow TCP 8621 through the host firewall and, when behind
NAT, forward the router's TCP 8621 to the same port on the host. Docker port
publication does not configure the router. The listener and advertised port
must agree; changing only the host side to another port is insufficient.
There is no fixed UDP 8621 listener: DHT uses outbound UDP sockets. Allow
outbound UDP and reply traffic, plus outbound TCP/HTTP(S) for discovery and
peers. UDP trackers may use ports other than 53 or 443.

Keep automatic UPnP/NAT-PMP mapping disabled inside a normal Docker bridge or
Kubernetes pod. Host networking is a separate Linux deployment choice with
different isolation and port ownership; it is not a portable fix for NAT or
Docker Desktop. Outbound P2P can still work behind NAT without inbound
publication, though available peers and network policy affect results.

A player on a different machine cannot reach the engine through
`127.0.0.1`; that address means the player's own machine. For a trusted LAN,
select the host's LAN address deliberately via `OUTPACE_HTTP_HOST` in Compose
or the host address in `--publish`, and restrict clients at the firewall or
gateway. A browser may also enforce HTTPS/mixed-content restrictions. For
an hls-proxy container on the same Compose network, the engine URL is normally
`http://outpace:6878`, not localhost. That internal hostname is not a URL for
the TV: the player must receive URLs reachable from its own network, either
through the proxy's media endpoints or a deliberately exposed engine route.

Use ordinary Docker DNS unless your installation has a reason to override it.
Custom resolvers, VPNs, and policy routing are operator choices; do not copy
another deployment's external-only DNS as a universal requirement.

## Verify source identity and media support

```bash
docker inspect --format '{{.Config.Image}}' outpace
docker exec outpace outpace --version
curl --fail http://127.0.0.1:6878/source.json
```

For Compose, replace `docker exec outpace` with `docker compose exec -T outpace`.
The metadata identifies the embedded workspace and links the matching complete
source bundle. `/source` presents the offer, and `/source.tar.gz` serves the
embedded archive. Preserve access to that offer through your integration; see
[license and source delivery](licensing.md).

The published base image does **not** include FFmpeg. Native HLS packaging is
available without it, but optional audio normalization needs a separately
provisioned compatible FFmpeg executable and explicit configuration. Enabling
`OUTPACE_HLS_AUDIO_NORMALIZE` alone does not install codecs or provide video
transcoding. See [HLS audio](playback.md#live-hls-audio).

A small image and a non-root process are not a claim of zero vulnerabilities.
Review image and dependency advisories for the exact digest you deploy and
keep the read-only root, dropped capabilities, and private control port in place.

## Update, stop, and roll back

Before updating, record the current image reference and review `GET /streams`
privately. Wait for active viewers and broadcasts to finish; a replacement
interrupts sessions. Take a protected snapshot/backup of the persistent volume
using your storage tooling with the engine stopped when consistency requires
it. Never publish private identity files as a diagnostic bundle.

For Compose:

1. Record the previous digest and preserve `.env` and the override.
2. Change `OUTPACE_IMAGE` in `.env` to the reviewed replacement digest.
   If the shell already exports that variable, update or unset it because
   the shell value takes precedence over `.env`.
3. Run `docker compose config --quiet`, `docker compose pull`, and
   `docker compose up -d --wait`.
4. Check health, `/source.json`, logs, and a bounded real playback test.
5. On failure, restore the previous exact digest and run
   `docker compose up -d --wait` again. Verify playback after rollback.

For a `docker run` installation, stop and remove the container, then repeat
the original command with the selected digest and **the same named volume**.
Container removal does not require deleting that volume. A code rollback
does not reverse changes a newer version made to persisted data; check release
compatibility and retain a backup.

Use `docker compose stop` / `docker compose start` for a temporary stop, or
`docker stop outpace` / `docker start outpace` for the single-container method.
`docker compose down` retains named volumes unless asked otherwise.
**Do not add `--volumes` / `-v`, run `docker volume rm`, or prune the identity
volume as part of routine maintenance.** See [troubleshooting](troubleshooting.md)
when health succeeds but media does not start.
