#!/usr/bin/env bash
# Material TLS de la demo-star, con las DOS raíces de docs/SECURITY.md §2 y el
# mismo generador que los despliegues reales (docker/gen-certs.sh), así que
# sale ML-DSA-65 por defecto (KEY_ALG=rsa para clásico, que no es PQC):
#   * net-ca -> certs de nodo: dkms-11/22/33/44 y orr_11/22/33/44. El gRPC
#               del ORR va con mTLS por defecto: el ORR presenta el suyo y el
#               DKMS (o el orr-test-client) le presenta el del DKMS.
#   * sae-ca -> certs cliente SAE (sae_aa/bb/cc/dd, sae_001..100).
# Idempotente: gen-certs.sh reutiliza las CAs y aquí se salta lo ya emitido.
# `--force` borra y rehace todo (p. ej. para pasar de RSA a ML-DSA).

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$ROOT/../.." && pwd)"
TLS="$ROOT/tls"
GEN="$REPO/docker/gen-certs.sh"
mkdir -p "$TLS"
[ "${1:-}" = --force ] && rm -f "$TLS"/*.crt "$TLS"/*.key "$TLS"/*.srl

node() { [ -f "$TLS/$1.crt" ] || { bash "$GEN" "$1" 127.0.0.1 "$TLS" >/dev/null; echo "  ▶ generated $1"; }; }
sae()  { [ -f "$TLS/$1.crt" ] || { bash "$GEN" --sae "$1" "$TLS" >/dev/null; echo "  ▶ generated $1"; }; }

for id in 11 22 33 44; do node "dkms-$id"; node "orr_$id"; done
for s in aa bb cc dd; do sae "sae_$s"; done
# 100 SAEs para los tests de ramp (ramp-saes.sh). La asignación SAE→DKMS es
# config runtime; aquí sólo se emite el material.
for n in $(seq -f "%03g" 1 100); do sae "sae_$n"; done
rm -f "$TLS"/*.srl

echo
echo "TLS material listo en $TLS/ ($(/bin/ls "$TLS"/*.crt | wc -l) certs, firma: $(openssl x509 -in "$TLS/net-ca.crt" -noout -text | grep -m1 'Signature Algorithm' | sed 's/.*: *//'))"
