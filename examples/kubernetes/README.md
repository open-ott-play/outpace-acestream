# Single-node-identity Kubernetes example

Read the [Kubernetes operator guide](../../docs/kubernetes.md) before applying.
This directory is a portable starting point, not a copy of a private cluster.

It creates namespace `outpace`, PVC `outpace-data`, one `Recreate` Deployment,
and a **ClusterIP-only** HTTP Service. It targets the v0.9.0 multi-platform
image, enables the supported Ace HTTP subset, and runs with a read-only root
filesystem as UID/GID 1000. Outbound peer connections and reciprocal uploads
are enabled; inbound peer listening and automatic router mapping are disabled.

Verify publication and matching source metadata before installing. For
production, replace the image tag in `deployment.yaml` with the verified
v0.9.0 index digest, as described in the operator guide.

Check that your StorageClass supports the required ownership and permissions.
The default class is selected unless you add `storageClassName` to `pvc.yaml`.
The image supports Linux amd64, arm64, and arm/v7; constrain scheduling further
if your cluster includes unsupported Linux architectures.

From the repository root:

```bash
# Offline rendering; this does not contact a cluster.
kubectl kustomize examples/kubernetes

# Review the current context and the rendered resources before installation.
kubectl config current-context
kubectl apply -k examples/kubernetes
kubectl -n outpace rollout status deployment/outpace --timeout=180s
kubectl -n outpace get pods,pvc,svc
kubectl -n outpace port-forward --address 127.0.0.1 service/outpace 6878:6878
```

The port-forward runs in the foreground. From another terminal:

```bash
curl --fail http://127.0.0.1:6878/healthz
curl --fail http://127.0.0.1:6878/source.json
```

For LAN players, updates, rollback, storage troubleshooting, and deliberate
inbound peer setup, follow the operator guide. **Do not use `kubectl delete -k`
as a routine stop command:** it also requests deletion of the PVC and namespace.
Scale the Deployment to zero to stop while retaining its identity.
