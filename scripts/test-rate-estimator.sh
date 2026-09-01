#!/usr/bin/env bash
# Prueba en vivo del estimador de tasa QKD: 2 QKC + 1 quditto compartido,
# SIN tráfico de datos — exactamente el caso que antes era inobservable.
#
#   QKC-1 ←→ [quditto :18081] ←→ QKC-2
#
# Qué esperar (ver qkc/src/rate_estimator.rs):
#   * Fase 1 (arranque): el fill inicial + el banking drenan el quditto →
#     ventanas válidas → rate ≈ R(d) = r0·10^(−α·d/10) con quality=measured.
#   * Fase 2 (anillos llenos): el stock toca techo → censura → la estimación
#     se congela con quality=floor. Jamás decae a 0.
#   * Con QUDITTO_RATE_STEP="T:F" y una tasa baja (fill largo), se ve al
#     estimador seguir el escalón en vivo.
#
# Uso:
#   scripts/test-rate-estimator.sh [duración_s]
# Knobs por env:
#   R0 (2000) ALPHA (0.2) DIST (5)             modelo físico del quditto
#   QUDITTO_FULL_MODE (drop|pause)             comportamiento al llenarse
#   QUDITTO_BLOCK_KEYS (0)                     entrega por bloques (escalera)
#   QUDITTO_RATE_STEP  ("")                    p. ej. "45:0.5"
#
# Ejemplos:
#   scripts/test-rate-estimator.sh                                  # nominal
#   R0=200 DIST=0 QUDITTO_RATE_STEP=45:0.5 scripts/test-rate-estimator.sh 110
#   R0=200 DIST=0 QUDITTO_FULL_MODE=pause QUDITTO_BLOCK_KEYS=256 \
#       scripts/test-rate-estimator.sh 90

set -euo pipefail
# printf %.1f con punto decimal, venga el locale que venga.
export LC_ALL=C

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DUR="${1:-75}"
R0="${R0:-2000}"
ALPHA="${ALPHA:-0.2}"
DIST="${DIST:-5}"
FULLMODE="${QUDITTO_FULL_MODE:-drop}"
BLOCK="${QUDITTO_BLOCK_KEYS:-0}"
STEP="${QUDITTO_RATE_STEP:-}"

OUT="$ROOT/tests/results/rate-est-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
cd "$ROOT"

THEO=$(python3 -c "print(f'{$R0 * 10**(-$ALPHA*$DIST/10):.1f}')")
echo "── teórica R(d) = $THEO keys/s  (r0=$R0 α=$ALPHA d=${DIST}km, full_mode=$FULLMODE block=$BLOCK step='${STEP}')"
echo "── artefactos en $OUT"

# ── puertos libres ────────────────────────────────────────────────────
for port in 18081 17001 17002 17101 17102 17201 17202; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port en uso; para el demo/prueba previa primero" >&2
        exit 1
    fi
done

# ── build ─────────────────────────────────────────────────────────────
cargo build -q -p quditto -p qkc --bin quditto --bin qkc

PIDS=()
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

# ── quditto ───────────────────────────────────────────────────────────
STEP_ARGS=()
[ -n "$STEP" ] && STEP_ARGS=(--rate-step "$STEP")
RUST_LOG="${RUST_LOG:-info}" "$ROOT/target/debug/quditto" --tls off \
    --listen 127.0.0.1:18081 --r0 "$R0" --alpha "$ALPHA" --distance "$DIST" \
    --max-buffer 8192 --key-size-bits 1024 \
    --full-mode "$FULLMODE" --block-keys "$BLOCK" "${STEP_ARGS[@]}" \
    > "$OUT/quditto.log" 2>&1 &
PIDS+=($!)

for _ in $(seq 1 50); do
    curl -sf "http://127.0.0.1:18081/healthz" >/dev/null 2>&1 && break
    sleep 0.2
done

# ── 2 QKC contra el mismo quditto ─────────────────────────────────────
for i in 1 2; do
    other=$((3 - i))
    cat > "$OUT/qkc$i.toml" <<EOF
qkc_id       = $i
peer_listen  = "127.0.0.1:1700$i"
local_listen = "127.0.0.1:1710$i"
admin_http   = "127.0.0.1:1720$i"

[[links]]
neighbor_id        = $other
neighbor_peer_addr = "127.0.0.1:1700$other"
quditto_url        = "http://127.0.0.1:18081"
key_size_bits      = 1024
EOF
    RUST_LOG="${RUST_LOG:-info}" "$ROOT/target/debug/qkc" --config "$OUT/qkc$i.toml" \
        > "$OUT/qkc$i.log" 2>&1 &
    PIDS+=($!)
done

sleep 1

# ── muestreo ──────────────────────────────────────────────────────────
CSV="$OUT/rates.csv"
echo "t_s,kme_stored,q1_rate,q1_quality,q1_enc,q2_rate,q2_quality,q2_enc" > "$CSV"
printf "%5s %10s │ %9s %-9s %6s │ %9s %-9s %6s\n" \
    "t(s)" "kme_stored" "qkc1_rate" "calidad" "enc1" "qkc2_rate" "calidad" "enc2"

sample_qkc() { # $1 = admin port, $2 = peer id
    curl -sf "http://127.0.0.1:$1/stats" 2>/dev/null | python3 -c "
import json,sys
try:
    l = json.load(sys.stdin)['links']['$2']
    r = l.get('rate_est_kps'); q = l.get('rate_quality')
    print(f\"{r if r is not None else 'nan'},{q or '-'},{l['enc_buffered']}\")
except Exception:
    print('nan,-,0')"
}

T0=$(date +%s)
while :; do
    NOW=$(( $(date +%s) - T0 ))
    [ "$NOW" -ge "$DUR" ] && break
    STORED=$(curl -sf "http://127.0.0.1:18081/api/v1/keys/x/status" 2>/dev/null \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['stored_key_count'])" 2>/dev/null || echo "?")
    IFS=, read -r R1 Q1 E1 <<< "$(sample_qkc 17201 2)"
    IFS=, read -r R2 Q2 E2 <<< "$(sample_qkc 17202 1)"
    echo "$NOW,$STORED,$R1,$Q1,$E1,$R2,$Q2,$E2" >> "$CSV"
    printf "%5s %10s │ %9s %-9s %6s │ %9s %-9s %6s\n" \
        "$NOW" "$STORED" "$(printf '%.1f' "$R1" 2>/dev/null || echo "$R1")" "$Q1" "$E1" \
        "$(printf '%.1f' "$R2" 2>/dev/null || echo "$R2")" "$Q2" "$E2"
    sleep 3
done

echo
echo "── teórica: $THEO keys/s · CSV en $CSV"
echo "── quditto (última línea de stats):"
grep "quditto.stats" "$OUT/quditto.log" | tail -1 || true
