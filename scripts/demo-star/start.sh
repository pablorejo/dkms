#!/usr/bin/env bash
# Arranca 8 qudittos + 9 QKCs en topología estrella:
#
#   HOJA-11 ─ qd-1b ─ INT-1 ─ qd-1a ─┐
#                                    │
#   HOJA-22 ─ qd-2b ─ INT-2 ─ qd-2a ─┤
#                                    ★ HUB-0
#   HOJA-33 ─ qd-3b ─ INT-3 ─ qd-3a ─┤
#                                    │
#   HOJA-44 ─ qd-4b ─ INT-4 ─ qd-4a ─┘
#
# R0 = 100M keys/s, buffer = 1M claves por quditto.
# Tras arrancar, popula las forwarding tables para que las hojas
# puedan llegar a las otras 3 hojas pasando por hub.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/pids
mkdir -p "$LOGS"
cd "$ROOT"

# ─── 1. Limpieza previa ───────────────────────────────────────────────
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    echo "── encontré $PIDS no vacío. Ejecutando stop.sh primero…"
    "$HERE/stop.sh" || true
fi
: > "$PIDS"

# Chequear puertos.
ALL_PORTS=(8011 8012 8021 8022 8031 8032 8041 8042 \
           7000 7001 7002 7003 7004 7011 7022 7033 7044 \
           7100 7101 7102 7103 7104 7111 7122 7133 7144 \
           7200 7201 7202 7203 7204 7211 7222 7233 7244)
conflict=0
for port in "${ALL_PORTS[@]}"; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port ya está en uso"
        conflict=1
    fi
done
if [ "$conflict" -ne 0 ]; then
    echo "Apaga lo anterior y reintenta: $HERE/stop.sh"
    exit 2
fi

# ─── 2. Compilar ──────────────────────────────────────────────────────
echo "── compilando (release)…"
cargo build --release -p quditto -p qkc --bin quditto --bin qkc --bin qkc-test-client 2>&1 \
    | tail -3
for bin in quditto qkc qkc-test-client; do
    test -x "$ROOT/target/release/$bin" || { echo "✗ falta $bin"; exit 1; }
done

# ─── 3. Helpers ───────────────────────────────────────────────────────
start_bg() {
    local name=$1; shift
    local logf=$LOGS/$name.log
    : > "$logf"
    nohup "$@" > "$logf" 2>&1 &
    local pid=$!
    echo "$pid $name" >> "$PIDS"
    echo "  ▶ $name (pid $pid)"
}
wait_tcp() {
    local host=$1 port=$2 label=$3 timeout=${4:-15}
    local elapsed=0
    while ! (echo > "/dev/tcp/$host/$port") 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$((timeout * 10))" ]; then
            echo "✗ TIMEOUT $label en $host:$port"
            tail -10 "$LOGS/$label.log" 2>/dev/null | sed 's/^/    /'
            return 1
        fi
    done
}
wait_http() {
    local url=$1 label=$2 timeout=${3:-15}
    local elapsed=0
    while ! curl -fsS -o /dev/null -m 1 "$url" 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$((timeout * 10))" ]; then
            echo "✗ TIMEOUT $label en $url"
            tail -10 "$LOGS/$label.log" 2>/dev/null | sed 's/^/    /'
            return 1
        fi
    done
}

# ─── 4. Arrancar los 8 qudittos ───────────────────────────────────────
# Parametrizable por env vars para pruebas de QKD realista:
#   QD_R0           — keys/s a distancia 0 (default 100M para no estrangular)
#   QD_ALPHA        — atenuación dB/km (default 0)
#   QD_DISTANCE_KM  — distancia óptica del enlace (default 0)
#   QD_BUFFER       — max-buffer (default 1M claves)
#   QD_KEY_BITS     — key-size-bits (default 1024)
QD_R0=${QD_R0:-100000000}
QD_ALPHA=${QD_ALPHA:-0}
QD_DISTANCE_KM=${QD_DISTANCE_KM:-0}
QD_BUFFER=${QD_BUFFER:-1048576}
QD_KEY_BITS=${QD_KEY_BITS:-1024}
echo "── arrancando 8 qudittos (R0=$QD_R0 alpha=$QD_ALPHA d=$QD_DISTANCE_KM km buf=$QD_BUFFER kbits=$QD_KEY_BITS)…"
QD_PORTS=(8011 8012 8021 8022 8031 8032 8041 8042)
QD_NAMES=("qd-1a" "qd-1b" "qd-2a" "qd-2b" "qd-3a" "qd-3b" "qd-4a" "qd-4b")
for i in "${!QD_PORTS[@]}"; do
    port=${QD_PORTS[$i]}
    name=${QD_NAMES[$i]}
    start_bg "$name" "$ROOT/target/release/quditto" \
        --listen "127.0.0.1:$port" \
        --r0 "$QD_R0" --alpha "$QD_ALPHA" --distance "$QD_DISTANCE_KM" \
        --max-buffer "$QD_BUFFER" --key-size-bits "$QD_KEY_BITS"
done
for port in "${QD_PORTS[@]}"; do
    wait_http "http://127.0.0.1:$port/healthz" "qd-port-$port" 15
done

# ─── 5. Arrancar los 9 QKCs ───────────────────────────────────────────
echo "── arrancando 9 QKCs…"
QKC_IDS=(0 1 2 3 4 11 22 33 44)
for id in "${QKC_IDS[@]}"; do
    start_bg "qkc-$id" "$ROOT/target/release/qkc" \
        --config "$HERE/qkc-${id}.toml"
done

# admin http: 7200, 7201..7204, 7211, 7222, 7233, 7244
ADMINS=(7200 7201 7202 7203 7204 7211 7222 7233 7244)
for p in "${ADMINS[@]}"; do
    wait_http "http://127.0.0.1:$p/healthz" "qkc-$p" 15
done
# peer + local tcp listeners
PEER_LOCAL_PORTS=(7000 7001 7002 7003 7004 7011 7022 7033 7044 \
                  7100 7101 7102 7103 7104 7111 7122 7133 7144)
for p in "${PEER_LOCAL_PORTS[@]}"; do
    wait_tcp 127.0.0.1 "$p" "tcp-$p" 5
done

# ─── 6. Forwarding tables ─────────────────────────────────────────────
# Si la SDN va a empujar las tablas (SKIP_FORWARDING=1), no hace falta
# que las populemos nosotros con curl — la SDN las recalcula desde su
# topología y hace POST a cada QKC al arrancar y en cada version bump.
if [ "${SKIP_FORWARDING:-0}" = "1" ]; then
    echo "── (skipping forwarding bootstrap; será empujado por SDN)"
else
echo "── poblando forwarding tables…"
# HUB-0: vecinos directos {1,2,3,4}. Las hojas NO son vecinos directos
# (su único enlace es con su intermedio), así que hay que enseñar
# explícitamente al hub cómo llegar a ellas.
curl -fsS -X POST http://127.0.0.1:7200/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"11":1,"22":2,"33":3,"44":4}}' >/dev/null

# INT-1: hoja-11 directa, todo lo demás vía hub.
curl -fsS -X POST http://127.0.0.1:7201/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":0,"2":0,"3":0,"4":0,"22":0,"33":0,"44":0}}' >/dev/null

# INT-2
curl -fsS -X POST http://127.0.0.1:7202/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":0,"1":0,"3":0,"4":0,"11":0,"33":0,"44":0}}' >/dev/null

# INT-3
curl -fsS -X POST http://127.0.0.1:7203/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":0,"1":0,"2":0,"4":0,"11":0,"22":0,"44":0}}' >/dev/null

# INT-4
curl -fsS -X POST http://127.0.0.1:7204/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":0,"1":0,"2":0,"3":0,"11":0,"22":0,"33":0}}' >/dev/null

# HOJA-11: solo conoce INT-1; cualquier dest no-vecino va por 1.
curl -fsS -X POST http://127.0.0.1:7211/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":1,"2":1,"3":1,"4":1,"22":1,"33":1,"44":1}}' >/dev/null

# HOJA-22
curl -fsS -X POST http://127.0.0.1:7222/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":2,"1":2,"3":2,"4":2,"11":2,"33":2,"44":2}}' >/dev/null

# HOJA-33
curl -fsS -X POST http://127.0.0.1:7233/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":3,"1":3,"2":3,"4":3,"11":3,"22":3,"44":3}}' >/dev/null

# HOJA-44
curl -fsS -X POST http://127.0.0.1:7244/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"replace":{"0":4,"1":4,"2":4,"3":4,"11":4,"22":4,"33":4}}' >/dev/null

fi  # end SKIP_FORWARDING

echo
echo "✓ topología estrella arriba (1 hub + 4 ramas × 2 nodos)."
echo "  PIDs: $PIDS"
echo "  Logs: $LOGS"
echo
echo "  $HERE/status.sh          # comprobar estado"
echo "  $HERE/saturate.sh        # all-to-all entre las 4 hojas"
echo "  $HERE/stop.sh"
