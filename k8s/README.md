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
