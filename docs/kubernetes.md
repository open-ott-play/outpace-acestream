# Kubernetes and k3s

The [checked-in example](../examples/kubernetes/README.md) deploys one Outpace
instance with a persistent identity and an internal HTTP Service. It is a
portable baseline for Kubernetes or k3s, not a copy of any private cluster.
It targets the **v0.9.0** multi-platform image:

```text
ghcr.io/open-ott-play/outpace-acestream:0.9.0
```

Verify the [release assets](https://github.com/open-ott-play/outpace-acestream/releases/tag/v0.9.0)
and source metadata first. For production, replace the example tag in
`deployment.yaml` with the verified multi-platform index digest before rendering
or applying. The [container guide](containers.md) shows image inspection;
do not substitute a digest from an older release.

This index supports Linux `amd64`, `arm64`, and `arm/v7`. The example requires
a working storage provisioner, cluster DNS, and permitted outbound Internet
traffic to catalogs, trackers, and peers. It does not install a cluster,
ingress controller, certificate issuer, or storage driver. Use a `kubectl`
version compatible with your cluster and check the current context before
applying anything.

## What the example creates

- Namespace `outpace` and a named `outpace-data` PVC requesting 2 GiB with
  `ReadWriteOnce` access. The default StorageClass is selected unless you set
  `storageClassName` explicitly.
- A single-replica Deployment with `Recreate` replacement and three retained
  revisions. One process owns the persistent node identity; rolling two pods
  over the same data is not the intended model.
- A ClusterIP Service on TCP 6878. There is no default NodePort, LoadBalancer,
  Ingress, host network, host port, or exposed RTMP ingest.
- UID/GID 1000, no service-account token, a read-only root filesystem, dropped
  Linux capabilities, no privilege escalation, and the runtime's default
  seccomp profile. Only the data PVC and a bounded `/tmp` volume are writable.
- Explicit data/cache paths, memory piece buffering, and the supported Ace
  HTTP compatibility subset. Outbound P2P and reciprocal upload work normally;
  inbound peer serving and automatic router mapping are disabled in this
  baseline because no reachable inbound endpoint has been provisioned.
- Startup, readiness, and liveness probes against `/healthz`. They check the
  engine process, not the availability of any particular source.

The 256 MiB memory request and 2 GiB limit are starting resource values, not a
capacity guarantee. Streams retain startup queues, peer data, HLS segments,
and per-client state. Tune from measured concurrent load and inspect OOM
events; the per-stream cache byte budget does not cap total process memory.

## Prepare storage and render

Read [storage and identity](#storage-and-identity) before migrating existing
data. For a new installation, select a suitable StorageClass and confirm that
the runtime UID can create private subdirectories on its volume. Inspect the
four resources before installing:

```bash
kubectl config current-context
kubectl get nodes
kubectl get storageclass

# Offline: render the exact desired resources without contacting the API.
kubectl kustomize examples/kubernetes > /tmp/outpace-rendered.yaml
```

Adjust files in a local copy or a GitOps overlay. If your cluster contains
unsupported architectures, add a scheduling constraint: `kubernetes.io/os:
linux` alone does not exclude every unsupported CPU. Pinning a node may be
appropriate for local storage, but do not copy someone else's hostname.

When the selected context and desired resources are correct:

```bash
kubectl apply -k examples/kubernetes
kubectl -n outpace rollout status deployment/outpace --timeout=180s
kubectl -n outpace get pods,pvc,svc
kubectl -n outpace logs deployment/outpace --tail=80
```

For k3s, the bundled Local Path Provisioner is an option, but its data remains
on a node's local storage. Plan backup and node-loss recovery; an ordinary
local-path claim does not make a single engine highly available. See
[k3s storage](https://docs.k3s.io/storage/). A network-backed class has different
ownership, latency, locking, and recovery properties. Select one deliberately.

## Verify through a local port-forward

Start this command in a separate terminal and leave it running:

```bash
kubectl -n outpace port-forward --address 127.0.0.1 service/outpace 6878:6878
```

If port 6878 is already occupied locally, use `16878:6878` and substitute
16878 in the following URLs. The command deliberately binds only loopback.

```bash
curl --fail http://127.0.0.1:6878/healthz
curl --fail http://127.0.0.1:6878/networks
curl --fail http://127.0.0.1:6878/source.json
kubectl -n outpace get deployment outpace \
  -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
kubectl -n outpace exec deployment/outpace -- outpace --version
```

Check that `/source.json` identifies the intended clean release, matching
target, and source archive. The index digest is shared across architectures;
the selected platform manifest and metadata target identify the platform.
The embedded workspace digest may be identical across targets built from the
same source. [Source delivery](licensing.md) explains the complete bundle and
the embedded archive.

Then follow [playback verification](playback.md) with a current authorized
public source. Confirm an actual decoded segment or advancing player, not
just HTTP 200. A port-forward traverses the Kubernetes API path and can be
much slower than the final media route; use it for diagnostics, not a
throughput benchmark. Stop only the session/token created by your test, and
stop the port-forward with Ctrl-C when finished.

## Connect hls-proxy or a player

An in-cluster proxy can reach the Service at:

```text
http://outpace.outpace.svc.cluster.local:6878
```

`cluster.local` is the usual cluster domain; substitute your configured domain
if different. `http://outpace:6878` works for a client in the same namespace.
The example enables `OUTPACE_ACE_COMPAT=1` so existing middleware can use the
supported `/ace/*` adapter. Native `/streams/ace/...` routes are available
independently. Set the flag to `0` if only native routes are needed.

**ClusterIP is not directly reachable from a normal TV or another machine
outside the cluster.** A Service DNS name is also not normally resolvable
there. Configure your hls-proxy's server-side engine endpoint to the internal
Service; configure the player's endpoint to the reachable proxy. If that
proxy redirects clients to engine URLs instead of relaying media, those URLs
must also be reachable from the player. In particular, `127.0.0.1` in a TV's
playlist means the TV itself, and a Pod IP is not a stable external address.
These are consequences of the [Kubernetes Service model](https://kubernetes.io/docs/concepts/services-networking/service/).

Treat ClusterIP as routing, not authentication: other permitted cluster
workloads can reach it. Apply namespace/network policy appropriate to your
CNI and restrict access to the intended proxy if the cluster is shared.
There is no general API authentication layer in Outpace. Playback tokens
do not protect control, broadcast, or all other API routes.

### Opt-in external HTTP access

Prefer the existing authenticated/private media gateway or a VPN. An Ingress
or reverse proxy must handle long-lived responses, disable inappropriate
response buffering, allow the actual playback routes, preserve or correctly
rewrite generated media URLs, and allow startup longer than an ordinary
small web request. Check URLs received by the player after TLS termination;
do not assume a proxy header fixes all compatibility URLs. Preserve the
source offer for engine users, as described in [licensing](licensing.md).

For a deliberately trusted and firewalled LAN, an operator may expose the
entire HTTP Service with NodePort. First restrict TCP 30678 on the reachable
node interfaces to the intended clients. This example is **not** part of the
default manifests and is not appropriate for an unprotected Internet port:

```bash
kubectl -n outpace patch service outpace --type=merge \
  -p '{"spec":{"type":"NodePort","ports":[{"name":"http","port":6878,"targetPort":"http","protocol":"TCP","nodePort":30678}]}}'
```

Clients then use `http://<reachable-node-address>:30678`. The address, host
firewall, CNI rules, and routing must actually allow this. To remove the
opt-in exposure, restore the Service to ClusterIP and remove the node port:

```bash
kubectl -n outpace patch service outpace --type=json \
  -p '[{"op":"remove","path":"/spec/ports/0/nodePort"},{"op":"replace","path":"/spec/type","value":"ClusterIP"}]'
```

For GitOps, express either choice in the managed overlay rather than leaving
an imperative patch that the controller will revert. A LoadBalancer is
another option only when your cluster has an implementation and you have
configured its access restrictions; do not assume it creates a private
address or firewall by itself.

### Opt-in inbound P2P

Playback can use outbound peer connections without publishing an inbound
port. The baseline keeps `OUTPACE_ENABLE_SEEDING=1` for reciprocal uploads on
those connections and `OUTPACE_ENABLE_INBOUND=0` to avoid advertising an
unprovisioned listener.

To enable incoming peers, first design a reachable **TCP** endpoint whose
public address and port agree with what the engine announces. Then enable
`OUTPACE_ENABLE_INBOUND=1`, bind `OUTPACE_PEER_LISTEN=0.0.0.0:8621`, and route
external TCP 8621 to the pod's TCP 8621. A suitable TCP LoadBalancer plus
matching firewall/NAT rules is one possible design. Kubernetes NodePort's
usual high port alone does not make an advertised TCP 8621 reachable; an
additional correct external mapping would be necessary. Egress SNAT and
ingress may use different addresses, so verify a connection from outside
your network instead of inferring reachability from a local socket.

Do not enable UPnP/NAT-PMP inside the normal pod network. It does not configure
the cluster's Service, CNI, or edge firewall. Outpace has no fixed UDP 8621
listener; DHT requires outbound UDP and reply traffic, and trackers/peers
also need their actual destination ports. A blanket egress rule allowing
only DNS and HTTPS may prevent P2P discovery or media transfer.

## Storage and identity

The PVC holds the engine's persistent node and broadcast identity. Do not
share it between concurrent engine processes or treat it as disposable
scratch space. `ReadWriteOnce` is a mount-access mode, not a guarantee that
two pods on the same node cannot write concurrently; the single replica and
`Recreate` strategy are intentional. Horizontal scaling needs separate
identities and a deliberate session-routing design.

The example runs as UID/GID 1000 and asks the volume driver to provide group
access with `fsGroup: 1000`. `fsGroupChangePolicy: OnRootMismatch` avoids
unnecessary recursive ownership changes when a compatible volume root
already has the expected attributes. Support and implementation depend on
the volume type/CSI driver; some drivers perform their own handling. See
[Kubernetes volume ownership](https://kubernetes.io/docs/tasks/configure-pod-container/security-context/#configure-volume-permission-and-ownership-change-policy-for-pods).

Validate this before attaching an existing identity volume. A migration from
the image's default named user may have a different numeric owner. Back up
the data and plan a controlled ownership migration, or retain a compatible
explicit UID; never recursively make identity files world-writable.
Network filesystems must support the engine's private-file permissions,
exclusive locking, and atomic replacement semantics. Root-squash and driver
behavior may require storage-side provisioning rather than a privileged
init container. The example includes no privileged permission fixer.

The example keeps both paths explicit; this is required for **v0.8.1**:

```yaml
- name: OUTPACE_DATA_DIR
  value: /var/lib/outpace
- name: OUTPACE_CACHE_DIR
  value: /var/lib/outpace/cache
```

In that release, changing only the data path can leave the default cache
under a home directory outside the PVC. A read-only root then causes warm-peer
cache warnings even though playback may continue. Memory piece buffering
still uses the cache directory for persistent peer hints. This workaround
does not enable disk piece caching or an archive.

After a successful stream, inspect metadata without reading private contents:

```bash
kubectl -n outpace exec deployment/outpace -- \
  stat -c '%u:%g %a %F' /var/lib/outpace/cache \
  /var/lib/outpace/cache/recent-peers-v1 \
  /var/lib/outpace/cache/recent-peers-v1.lock
```

The cache directory must be owned by the runtime UID and must not be
group/world-writable. It is created with mode 0700; a setgid PVC parent can
make it inherit the setgid bit and appear as 2700, which also satisfies this
check. Hint/lock files are regular files with mode 0600. A newly installed
instance may not have written hints yet. An empty regular lock file is
normal. Inspect logs for warm-peer cache warnings; do not print seed or hint
contents into support reports. The existing PVC root does not have to be
rewritten to the private child directory's mode.

With the example's memory piece buffering, warm-peer hints survive restarts.
Disk mode clears its shared cache root at startup, including previous hints;
it does not provide persistent recording or a time-shift archive.

## DNS and routing

The example uses Kubernetes' normal `ClusterFirst` DNS policy. This permits
Service discovery as well as external lookups through the cluster resolver.
External-only DNS, VPN routing, and node pinning in an existing installation
are deployment-specific choices, not engine requirements. Do not copy those
values into an unrelated cluster.

When diagnosing a source that never starts, separate DNS resolution, catalog
access, tracker access, peer connectivity, and media availability. Test through
the same pod/egress route as the real engine. A successful laptop lookup or
`/healthz` response proves neither cluster egress nor source availability.
The [troubleshooting guide](troubleshooting.md) gives the next checks.

## Update and rollback

Keep the desired image, environment, security context, and storage in version
control. For GitOps, merge the reviewed change and let the controller apply
it; avoid a competing manual update. Before changing a version:

1. Record the current Deployment image, PVC identity, and public node ID
   privately. Check `/streams` and arrange a quiet window; `Recreate` causes
   an interruption and does not migrate live sessions.
2. Back up the identity volume with your storage tooling. Stop the engine if
   the backup method needs a quiescent filesystem. Do not copy private seeds
   into logs or Git.
3. Verify the new public release and matching source offer. Change only the
   intended image/environment in your overlay; preserve the PVC and runtime
   restrictions. Retain the old exact image digest for rollback.
4. Review the rendered diff, apply/reconcile, and wait for rollout completion.
5. Confirm health, source metadata, the same persistent identity, one real
   playback result, and absence of new storage warnings. Readiness alone is
   not the complete acceptance test.

For the unmanaged example after editing its desired files:

```bash
kubectl diff -k examples/kubernetes
kubectl apply -k examples/kubernetes
kubectl -n outpace rollout status deployment/outpace --timeout=180s
kubectl -n outpace get pods
kubectl -n outpace logs deployment/outpace --tail=80
```

`kubectl diff` exits 1 when it finds differences; review them rather than
treating that result as an apply failure. On a bad rollout, restore the
previous reviewed image/configuration in the files and apply again. An
emergency `kubectl rollout undo deployment/outpace -n outpace` restores a
retained prior pod-template revision, which can include environment changes
as well as the image. Reconcile that choice back into Git immediately, and
verify the resulting image and playback. Rolling back code does not reverse
changes to persisted data; retain a compatible backup and review release
migration notes.

## Stop without losing data

```bash
kubectl -n outpace scale deployment/outpace --replicas=0

# Resume later with the same PVC and identity.
kubectl -n outpace scale deployment/outpace --replicas=1
kubectl -n outpace rollout status deployment/outpace --timeout=180s
```

Express the replica change in Git instead when a controller owns it. Do not
use `kubectl delete -k examples/kubernetes` or delete the namespace as a stop
command: both can delete the PVC, and the StorageClass reclaim policy may
delete its backing data. Removing a deployment and removing its identity are
separate decisions.
