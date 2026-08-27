#!/usr/bin/env bash
# All-to-all DKMS sobre la demo-star ORR+QKC.
#
# Cada SAE (4 en total, uno por DKMS) hace COUNT peticiones POST
# /api/v1/keys/<slave>/enc_keys hacia los 3 otros SAEs. 12 flujos en
# paralelo, igual que saturate-orr.sh, pero un nivel arriba: aquí
# medimos la ruta completa ETSI 014 SAE→DKMS→ORR→QKC→…→ORR→DKMS→
# PendingStore y luego dec_keys del SAE destino.
#
# Verifica integridad: por cada key entregada, el SAE destino la
# recupera vía dec_keys y comparamos los bytes.
#
# Uso:
#   ./saturate-dkms.sh             # 1 key por flujo (smoke)
#   ./saturate-dkms.sh 10
#   ./saturate-dkms.sh 100 256

set -euo pipefail
COUNT=${1:-1}
SIZE_BITS=${2:-256}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP=/tmp/dkms-star-demo/saturate-dkms
mkdir -p "$TMP"
TLS="$HERE/tls"

declare -A SAE_DKMS=(
    [sae_aa]=11 [sae_bb]=22 [sae_cc]=33 [sae_dd]=44
)
declare -A DKMS_PORT=(
    [11]=8411 [22]=8422 [33]=8433 [44]=8444
)
SAES=(sae_aa sae_bb sae_cc sae_dd)
TOTAL_FLOWS=$(( ${#SAES[@]} * (${#SAES[@]} - 1) ))
TOTAL_KEYS=$(( COUNT * TOTAL_FLOWS ))

echo "── DKMS all-to-all"
echo "   count/flujo: $COUNT  size: ${SIZE_BITS} bits"
echo "   flujos: $TOTAL_FLOWS (4 SAEs × 3 destinos)"
echo "   keys esperadas: $TOTAL_KEYS"
echo

curl_sae() {
    local sae="$1"; shift
    curl -sS --max-time 30 \
        --cert "$TLS/$sae.crt" --key "$TLS/$sae.key" --cacert "$TLS/net-ca.crt" \
        -H 'content-type: application/json' "$@"
}

# Función que ejecuta UN flujo: master pide COUNT claves al DKMS del
# master para slave; por cada clave obtenida, el slave hace dec_keys
# y verifica que recupera los mismos bytes. Salida: una línea por
# clave entregada con éxito (`OK <key_id>`) o `FAIL ...` si falla.
run_flow() {
    local master="$1"
    local slave="$2"
    local count="$3"
    local out="$4"
    local master_dkms="${SAE_DKMS[$master]}"
    local slave_dkms="${SAE_DKMS[$slave]}"
    local master_url="https://127.0.0.1:${DKMS_PORT[$master_dkms]}"
    local slave_url="https://127.0.0.1:${DKMS_PORT[$slave_dkms]}"

    local ok=0
    local fail=0
    for i in $(seq 1 "$count"); do
        # enc_keys (master side)
        local enc
        enc=$(curl_sae "$master" \
            "$master_url/api/v1/keys/$slave/enc_keys" \
            -d "{\"number\":1,\"size\":$SIZE_BITS}" 2>/dev/null || echo "")
        # Cuerpo esperado: {"keys":[{"key_ID":"<uuid>","key":"<base64>"}]}
        local kid
        kid=$(echo "$enc" | jq -r '.keys[0].key_ID // empty' 2>/dev/null || echo "")
        local kval
        kval=$(echo "$enc" | jq -r '.keys[0].key // empty' 2>/dev/null || echo "")
        if [ -z "$kid" ] || [ -z "$kval" ]; then
            fail=$((fail + 1))
            echo "FAIL enc $master→$slave i=$i: $enc" >> "$out"
            continue
        fi
        # El ORR puede tardar microsegundos en entregar; pequeño retry.
        local dec=""
        local kval2=""
        for try in 1 2 3 4 5; do
            dec=$(curl_sae "$slave" \
                "$slave_url/api/v1/keys/$master/dec_keys" \
                -d "{\"key_IDs\":[{\"key_ID\":\"$kid\"}]}" 2>/dev/null || echo "")
            kval2=$(echo "$dec" | jq -r '.keys[0].key // empty' 2>/dev/null || echo "")
            if [ -n "$kval2" ]; then
                break
            fi
            sleep 0.05
        done
        if [ "$kval" = "$kval2" ]; then
            ok=$((ok + 1))
        else
            fail=$((fail + 1))
            echo "FAIL dec $master→$slave i=$i kid=$kid: $dec" >> "$out"
        fi
    done
    echo "SUMMARY $master→$slave ok=$ok fail=$fail" >> "$out"
}

# Comprobación rápida de jq
if ! command -v jq >/dev/null; then
    echo "✗ falta jq — apt install jq"
    exit 2
fi

echo "── lanzando $TOTAL_FLOWS flujos en paralelo…"
WALL_START=$(date +%s.%N)
PIDS=()
for master in "${SAES[@]}"; do
    for slave in "${SAES[@]}"; do
        [ "$master" = "$slave" ] && continue
        out="$TMP/${master}-to-${slave}.txt"
        : > "$out"
        run_flow "$master" "$slave" "$COUNT" "$out" &
        PIDS+=("$!")
    done
done
for pid in "${PIDS[@]}"; do
    wait "$pid" || true
done
WALL_END=$(date +%s.%N)
ELAPSED=$(awk "BEGIN{printf \"%.3f\",$WALL_END-$WALL_START}")

# Suma de resultados
TOTAL_OK=0
TOTAL_FAIL=0
for master in "${SAES[@]}"; do
    for slave in "${SAES[@]}"; do
        [ "$master" = "$slave" ] && continue
        out="$TMP/${master}-to-${slave}.txt"
        line=$(grep '^SUMMARY' "$out" | tail -1)
        ok=$(echo "$line" | awk -F'ok=' '{print $2}' | awk '{print $1}')
        fail=$(echo "$line" | awk -F'fail=' '{print $2}')
        TOTAL_OK=$((TOTAL_OK + ok))
        TOTAL_FAIL=$((TOTAL_FAIL + fail))
        printf "  %s→%s ok=%-4s fail=%s\n" "$master" "$slave" "${ok:-0}" "${fail:-0}"
    done
done

echo
echo "── agregado:"
echo "  ok:        $TOTAL_OK / $TOTAL_KEYS"
echo "  fail:      $TOTAL_FAIL"
echo "  wall:      ${ELAPSED}s"
if [ "$TOTAL_OK" -gt 0 ]; then
    fps=$(awk "BEGIN{printf \"%.0f\",$TOTAL_OK/$ELAPSED}")
    echo "  agregado:  $fps keys/s entregadas E2E"
fi
echo
echo "  detalle por flujo en $TMP/<master>-to-<slave>.txt"
exit $((TOTAL_FAIL > 0 ? 1 : 0))
