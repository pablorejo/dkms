#!/usr/bin/env bash
# All-to-all saturation entre las 4 hojas {11, 22, 33, 44}.
#
# Cada hoja manda `COUNT` frames a las otras 3 hojas en paralelo
# → 4 × 3 = 12 flujos cruzando el HUB.
# Cada hoja-destino recibe 3 × COUNT frames.
# El HUB ve 12 × COUNT cifrados + 12 × COUNT descifrados.
#
# Reporta:
#   * throughput agregado e2e (sumando los 4 listeners).
#   * tiempo total.
#   * frames delivered vs esperados.
#
# Uso:
#   ./saturate.sh                 # 5000 frames de 64 B por flujo
#   ./saturate.sh 10000 64
#   ./saturate.sh 2000 1024

set -euo pipefail
COUNT=${1:-5000}
BYTES=${2:-64}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CLIENT="$ROOT/target/release/qkc-test-client"
TMP=/tmp/dkms-star-demo/saturate
mkdir -p "$TMP"

LEAVES=(11 22 33 44)
# Puertos local (donde se conecta el test client) por hoja:
declare -A LOCAL=(
    [11]=127.0.0.1:7111
    [22]=127.0.0.1:7122
    [33]=127.0.0.1:7133
    [44]=127.0.0.1:7144
)

EXPECTED_PER_LEAF=$((COUNT * 3))           # cada hoja recibe de las otras 3
TOTAL_DELIVERED_EXPECTED=$((COUNT * 12))   # 12 flujos × COUNT

echo "── all-to-all saturation"
echo "   count/flujo: $COUNT  bytes/frame: $BYTES"
echo "   flujos: 12 (4 hojas × 3 destinos)"
echo "   esperados por hoja receptora: $EXPECTED_PER_LEAF"
echo "   esperados totales: $TOTAL_DELIVERED_EXPECTED"
echo

# ─── 1. Arrancar 4 listeners (uno por hoja) ─────────────────────────
echo "── arrancando listeners…"
LISTENER_PIDS=()
for leaf in "${LEAVES[@]}"; do
    out="$TMP/listen-$leaf.txt"
    : > "$out"
    "$CLIENT" listen --addr "${LOCAL[$leaf]}" --count "$EXPECTED_PER_LEAF" > "$out" 2>&1 &
    LISTENER_PIDS+=("$!")
done

# Espera a que los listeners hayan conectado (es localhost, instantáneo
# pero dejamos margen).
sleep 0.5

# ─── 2. Lanzar 12 senders en paralelo ───────────────────────────────
echo "── lanzando 12 senders en paralelo…"
SENDER_PIDS=()
WALL_START=$(date +%s.%N)
for src in "${LEAVES[@]}"; do
    for dst in "${LEAVES[@]}"; do
        [ "$src" = "$dst" ] && continue
        out="$TMP/send-${src}-to-${dst}.txt"
        : > "$out"
        "$CLIENT" stress \
            --addr "${LOCAL[$src]}" \
            --dest "$dst" \
            --count "$COUNT" \
            --bytes "$BYTES" > "$out" 2>&1 &
        SENDER_PIDS+=("$!")
    done
done

# ─── 3. Esperar a senders ────────────────────────────────────────────
for pid in "${SENDER_PIDS[@]}"; do
    wait "$pid" || true
done
SEND_DONE=$(date +%s.%N)
SEND_ELAPSED=$(awk "BEGIN{printf \"%.3f\",$SEND_DONE-$WALL_START}")
echo "── senders terminados en ${SEND_ELAPSED}s"

# ─── 4. Esperar a listeners (timeout suave: 30s tras senders) ────────
# El `qkc-test-client listen` solo sale por (a) count alcanzado o (b)
# EOF del peer. Si se pierde algún frame en vuelo (LS_err > 0), n < count
# y el listener cuelga indefinidamente. Tras 30s lo matamos para que el
# script termine y reporte el delivered real.
SUNSET=$((SECONDS + 30))
for pid in "${LISTENER_PIDS[@]}"; do
    while kill -0 "$pid" 2>/dev/null && [ "$SECONDS" -lt "$SUNSET" ]; do
        sleep 0.1
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
done
WALL_END=$(date +%s.%N)
TOTAL_ELAPSED=$(awk "BEGIN{printf \"%.3f\",$WALL_END-$WALL_START}")

# ─── 5. Resumir ──────────────────────────────────────────────────────
# Preferimos las stats del admin HTTP del QKC (autoritativas) sobre
# greppear el stdout del listener: si SIGTERM mata al listener antes
# de imprimir "received N frames", el archivo queda sin esa línea
# pero el QKC sí sabe cuántos delivered locales hizo.
echo
echo "── resultados por hoja receptora:"
TOTAL_RECEIVED=0
declare -A ADMIN_PORT=( [11]=7211 [22]=7222 [33]=7233 [44]=7244 )
for leaf in "${LEAVES[@]}"; do
    port=${ADMIN_PORT[$leaf]}
    received=$(curl -fsS "http://127.0.0.1:$port/stats" 2>/dev/null \
        | jq -r '.service.incoming_delivered // 0' 2>/dev/null || echo 0)
    if [ "$received" = "null" ] || [ -z "$received" ]; then received=0; fi
    TOTAL_RECEIVED=$((TOTAL_RECEIVED + received))
    fps=$(awk "BEGIN{printf \"%.0f\", $received/$TOTAL_ELAPSED}")
    echo "  hoja-$leaf: $received / $EXPECTED_PER_LEAF frames  (~$fps fps incoming)"
done

echo
echo "── agregado:"
echo "  delivered total:    $TOTAL_RECEIVED / $TOTAL_DELIVERED_EXPECTED"
echo "  wall-time total:    ${TOTAL_ELAPSED}s"
AGGREGATE_FPS=$(awk "BEGIN{printf \"%.0f\",$TOTAL_RECEIVED/$TOTAL_ELAPSED}")
PAYLOAD_MBS=$(awk "BEGIN{printf \"%.2f\",($TOTAL_RECEIVED*$BYTES)/$TOTAL_ELAPSED/1048576.0}")
echo "  throughput AGREGADO: $AGGREGATE_FPS fps  ($PAYLOAD_MBS MB/s payload entregado)"
echo
echo "  los 12 flujos cruzan el HUB-0."
echo "  HUB descifra y recifra cada frame → ~$((AGGREGATE_FPS * 2)) operaciones/s en el hub."
echo
echo "  detalle por sender en $TMP/send-*.txt"
echo "  detalle por listener en $TMP/listen-*.txt"
