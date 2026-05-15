#!/usr/bin/env bash
# All-to-all saturation entre los 4 ORRs (uno por hoja), parametrizado
# por max_hops:
#
#   max_hops=0   → passthrough (sin onion, OTP del QKC protege)
#   max_hops=1   → PQC end-to-end (1 capa ML-KEM contra destino)
#   max_hops=2   → onion truncada (2 hops random del path SDN)
#   max_hops=-1  → onion full (1 capa por cada hop del path)
#
# Para max_hops != 0,1 hace falta `orr_path` en el app_header. En esta
# topología estrella, el path lógico ORR entre 2 hojas atraviesa
# todas las otras hojas como posibles intermedios.
#
# 12 flujos (4 hojas × 3 destinos). Reporta agregado y per-flujo.

set -euo pipefail
COUNT=${1:-2000}
BYTES=${2:-64}
MAX_HOPS=${3:-0}
# USE_SDN=1 → no inyecta `orr_path` en el app_header para modos -1 y >=2.
# El ORR origen tendrá que pedir el path a la SDN vía `GetOrrPath`.
# Esto es lo que ejerce de verdad la integración QKC+SDN+ORR.
USE_SDN=${USE_SDN:-0}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CLIENT="$ROOT/target/release/orr-test-client"
TMP=/tmp/dkms-star-demo/saturate-orr
mkdir -p "$TMP"

# gRPC + payload listener address de cada ORR-leaf
declare -A GRPC=(
    [11]=http://127.0.0.1:50511
    [22]=http://127.0.0.1:50522
    [33]=http://127.0.0.1:50533
    [44]=http://127.0.0.1:50544
)
LEAVES=(11 22 33 44)
EXPECTED_PER_LEAF=$((COUNT * 3))
TOTAL_EXPECTED=$((COUNT * 12))

echo "── ORR all-to-all saturation"
echo "   count/flujo: $COUNT  bytes/frame: $BYTES  max_hops: $MAX_HOPS"
echo "   flujos: 12 (4 ORRs × 3 destinos)"
echo "   esperados por hoja receptora: $EXPECTED_PER_LEAF"
echo "   esperados totales: $TOTAL_EXPECTED"
if [ "$USE_SDN" = "1" ]; then
    echo "   path resolution: SDN (orr_path omitido del app_header)"
else
    echo "   path resolution: hint del cliente (orr_path inyectado)"
fi
echo

# Listeners (uno por ORR, gRPC StreamDeliveries)
echo "── arrancando listeners gRPC…"
LISTENER_PIDS=()
for leaf in "${LEAVES[@]}"; do
    out="$TMP/listen-$leaf.txt"
    : > "$out"
    "$CLIENT" listen --addr "${GRPC[$leaf]}" --count "$EXPECTED_PER_LEAF" \
        --subscriber-id "sat-$leaf" > "$out" 2>&1 &
    LISTENER_PIDS+=("$!")
done
# Espera a que los listeners se registren en el broadcast del ORR
sleep 0.5

# Path SDN para los modos onion. Para un origen X y destino Y, los
# hops intermedios son los otros dos ORRs (la estrella no tiene
# transit por ORR de hub — el hub es solo QKC). Si max_hops=2 elige 2
# random; si -1 usa los 3 hops del path.
build_orr_path() {
    local src=$1 dst=$2
    local mids=()
    for x in "${LEAVES[@]}"; do
        [ "$x" = "$src" ] && continue
        [ "$x" = "$dst" ] && continue
        mids+=("orr_$x")
    done
    # path = mids + dst
    local path=""
    for m in "${mids[@]}"; do
        path+="$m,"
    done
    path+="orr_$dst"
    echo "$path"
}

# Senders: 12 flujos en paralelo
echo "── lanzando 12 senders…"
SENDER_PIDS=()
WALL_START=$(date +%s.%N)
for src in "${LEAVES[@]}"; do
    for dst in "${LEAVES[@]}"; do
        [ "$src" = "$dst" ] && continue
        out="$TMP/send-${src}-to-${dst}.txt"
        : > "$out"
        path_arg=""
        if [ "$MAX_HOPS" != "0" ] && [ "$MAX_HOPS" != "1" ] && [ "$USE_SDN" != "1" ]; then
            p=$(build_orr_path "$src" "$dst")
            path_arg="--orr-path $p"
        fi
        "$CLIENT" stress \
            --addr "${GRPC[$src]}" \
            --dest "orr_$dst" \
            --count "$COUNT" \
            --bytes "$BYTES" \
            --max-hops "$MAX_HOPS" \
            $path_arg > "$out" 2>&1 &
        SENDER_PIDS+=("$!")
    done
done

# Wait for senders
for pid in "${SENDER_PIDS[@]}"; do
    wait "$pid" || true
done
SEND_DONE=$(date +%s.%N)
SEND_ELAPSED=$(awk "BEGIN{printf \"%.3f\",$SEND_DONE-$WALL_START}")
echo "── senders terminados en ${SEND_ELAPSED}s"

# Wait for listeners (timeout configurable, default 120s tras los senders)
LISTENER_TIMEOUT=${LISTENER_TIMEOUT:-120}
SUNSET=$((SECONDS + LISTENER_TIMEOUT))
for pid in "${LISTENER_PIDS[@]}"; do
    while kill -0 "$pid" 2>/dev/null && [ "$SECONDS" -lt "$SUNSET" ]; do
        sleep 0.1
    done
    kill "$pid" 2>/dev/null || true
done
WALL_END=$(date +%s.%N)
TOTAL_ELAPSED=$(awk "BEGIN{printf \"%.3f\",$WALL_END-$WALL_START}")

# Resumir
echo
echo "── resultados por hoja receptora:"
TOTAL_RECEIVED=0
for leaf in "${LEAVES[@]}"; do
    out="$TMP/listen-$leaf.txt"
    received=$(grep -cE '^\[[0-9]+\] origin=' "$out" 2>/dev/null || echo 0)
    TOTAL_RECEIVED=$((TOTAL_RECEIVED + received))
    echo "  orr_$leaf: $received / $EXPECTED_PER_LEAF frames"
done

echo
echo "── agregado (max_hops=$MAX_HOPS):"
echo "  delivered:    $TOTAL_RECEIVED / $TOTAL_EXPECTED"
echo "  wall-time:    ${TOTAL_ELAPSED}s"
if [ "$TOTAL_RECEIVED" -gt 0 ]; then
    AGG_FPS=$(awk "BEGIN{printf \"%.0f\",$TOTAL_RECEIVED/$TOTAL_ELAPSED}")
    PER_FLOW=$(awk "BEGIN{printf \"%.0f\",$AGG_FPS/12}")
    PAYLOAD=$(awk "BEGIN{printf \"%.2f\",($TOTAL_RECEIVED*$BYTES)/$TOTAL_ELAPSED/1048576.0}")
    echo "  agregado:     $AGG_FPS fps  ($PAYLOAD MB/s payload entregado)"
    echo "  por flujo:    ~$PER_FLOW fps (aprox)"
fi
echo
echo "  detalle por sender en $TMP/send-*.txt"
echo "  detalle por listener en $TMP/listen-*.txt"
