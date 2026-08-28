# k8s/

Each module ships its own manifests under `<module>/k8s/`. Apply just the
modules you want to deploy — none of them require all the others to be
present (gRPC clients dial lazily and degrade to retries).

```bash
# Deploy only SDN + DKMS:
kubectl apply -f sdn/k8s
kubectl apply -f dkms/k8s

# Or everything at once:
for m in qkc orr sdn dkms quditto; do
  kubectl apply -f "$m/k8s"
done
```

A shared `Namespace` / `NetworkPolicy` / `Ingress` chart can live here
later. For now we stay un-opinionated about the cluster.

## Certificates

The manifests here carry no TLS material. The DKMS needs its node certificate
for the SAE plane regardless, and since 2026-08-28 the ORR's gRPC runs under
mTLS **by default** (`grpc_tls`), so an ORR pod with no `[tls]` section exits
at boot with a message saying so. Mount a secret with `<orr_id>.crt/.key` and
`net-ca.crt` (generated with `docker/gen-certs.sh <orr_id> <ip>`, ML-DSA-65 by
default) and point `[tls]` at it — or, only for an in-cluster deployment
where DKMS and ORR share a trusted network, opt out explicitly with
`grpc_tls = false` in the ORR config and `orr_tls = false` in the DKMS's
`[southbound]`.
