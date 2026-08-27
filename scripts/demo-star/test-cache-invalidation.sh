#!/usr/bin/env bash
# Verifica que el StreamTopology invalida cachés en ORR y DKMS.
#
# 1) Calienta cachés con un flujo enc/dec (asume DKMS+ORR+SDN ya arriba).
# 2) Captura líneas de log antes de la mutación.
# 3) Muta la SDN vía HTTP (PUT /sae/sae_dd con dkms_target trivial → bump version).
# 4) Espera ~300 ms (200 ms tick + margen).
# 5) Comprueba en los logs de ORR/DKMS los eventos `invalidated`.
# 6) Lanza un nuevo enc/dec — basta con que devuelva OK (la cache se
#    rellenará tras pegar a SDN otra vez; no podemos verlo desde aquí
#    sin enable debug logging).

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-star-demo
TLS="$HERE/tls"

echo "── precondiciones: SDN, QKC, ORR y DKMS deben estar corriendo"
for p in 50053 50055 50511 8411; do
    (echo > "/dev/tcp/127.0.0.1/$p") 2>/dev/null || {
        echo "✗ puerto $p no responde — arranca primero start-sdn, start, start-orrs, start-dkms"
        exit 2
    }
done

echo "── 1) warmup: un enc/dec sae_aa→sae_bb (calienta caches DKMS+ORR)"
warmup=$(curl -sS --cert "$TLS/sae_aa.crt" --key "$TLS/sae_aa.key" --cacert "$TLS/net-ca.crt" \
    -H 'content-type: application/json' \
    "https://127.0.0.1:8411/api/v1/keys/sae_bb/enc_keys" \
    -d '{"number":1,"size":256}')
echo "    enc → $(echo "$warmup" | jq -c '.keys[0] | {key_ID}')"

# Snapshot del fin de los logs antes de mutar
before_dkms=$(wc -l < "$LOGS/dkms-11.log")
before_orr=$(wc -l < "$LOGS/orr-11.log")

echo "── 2) mutar SDN: re-bind sae_dd a dkms-44 (mismo valor → bump version sin romper nada)"
curl -fsS -X PUT http://127.0.0.1:50055/sae/sae_dd \
    -H 'content-type: application/json' \
    -d '{"dkms_id":"dkms-44"}' >/dev/null

# Espera a que el version watcher tickee y propague
sleep 0.5

echo "── 3) revisar logs nuevos en DKMS-11 y ORR-11:"
sed -n "$((before_dkms+1)),\$p" "$LOGS/dkms-11.log" | grep -E "sae_binding_cache invalidated|topology_subscriber connected" || true
sed -n "$((before_orr+1)),\$p" "$LOGS/orr-11.log" | grep -E "path_cache invalidated|topology_subscriber connected" || true

echo
echo "── 4) post-mutación: otro enc/dec para comprobar que no se rompió"
post=$(curl -sS --cert "$TLS/sae_aa.crt" --key "$TLS/sae_aa.key" --cacert "$TLS/net-ca.crt" \
    -H 'content-type: application/json' \
    "https://127.0.0.1:8411/api/v1/keys/sae_bb/enc_keys" \
    -d '{"number":1,"size":256}')
kid=$(echo "$post" | jq -r '.keys[0].key_ID')
kval=$(echo "$post" | jq -r '.keys[0].key')
dec=$(curl -sS --cert "$TLS/sae_bb.crt" --key "$TLS/sae_bb.key" --cacert "$TLS/net-ca.crt" \
    -H 'content-type: application/json' \
    "https://127.0.0.1:8422/api/v1/keys/sae_aa/dec_keys" \
    -d "{\"key_IDs\":[{\"key_ID\":\"$kid\"}]}")
kval2=$(echo "$dec" | jq -r '.keys[0].key')

if [ "$kval" = "$kval2" ]; then
    echo "    ✓ enc/dec post-invalidación OK (key_ID $kid)"
else
    echo "    ✗ enc/dec post-invalidación: mismatch"
    echo "      enc=$kval"
    echo "      dec=$kval2"
    exit 1
fi
