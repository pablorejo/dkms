#!/usr/bin/env bash
# Arranca 2 quditto + 3 qkc en background, popula las forwarding tables
# para una topología en serie 1 ↔ 2 ↔ 3, y deja PIDs+logs en /tmp.
#
#   QKC-1 ←→ [quditto-A:8081] ←→ QKC-2 ←→ [quditto-B:8082] ←→ QKC-3
#
# Después de arrancar:
#   ./status.sh                    qué está vivo
#   ./send-one.sh                  un mensaje
#   ./stress.sh 10000 64           stress test

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-rust-demo
PIDS=$LOGS/pids
mkdir -p "$LOGS"

cd "$ROOT"

# ─── 1. Limpiar arranque previo si lo hubiera ─────────────────────────
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    echo "── encontré $PIDS no vacío. Ejecutando stop.sh primero…"
    "$HERE/stop.sh" || true
fi
: > "$PIDS"

# ─── 1b. Comprobar que ningún puerto del demo está en uso ─────────────
DEMO_PORTS=(8081 8082 7001 7002 7003 7101 7102 7103 7201 7202 7203)
conflict=0
for port in "${DEMO_PORTS[@]}"; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port ya está en uso (probablemente un demo previo)"
        conflict=1
    fi
done
if [ "$conflict" -ne 0 ]; then
    echo
    echo "Apaga lo que esté escuchando ahí o ejecuta:"
    echo "    $HERE/stop.sh"
    echo "Si arrancaste en otra sesión y no recuerdas el PID:"
    echo "    pgrep -u \$(id -u) -ax 'quditto|qkc'"
    exit 2
fi

# ─── 2. Compilar (release) ────────────────────────────────────────────
echo "── compilando (release). La primera vez puede tardar 30s-2min…"
cargo build --release -p quditto -p qkc --bin quditto --bin qkc --bin qkc-test-client 2>&1 \
    | tail -3

for bin in quditto qkc qkc-test-client; do
    if [ ! -x "$ROOT/target/release/$bin" ]; then
        echo "✗ ERROR: $ROOT/target/release/$bin no existe tras cargo build"
        exit 1
    fi
done

# ─── 3. Helpers ───────────────────────────────────────────────────────
start_bg() {
    local name=$1; shift
    local logf=$LOGS/$name.log
    : > "$logf"
    nohup "$@" > "$logf" 2>&1 &
    local pid=$!
    echo "$pid $name" >> "$PIDS"
    echo "  ▶ $name (pid $pid)  log: $logf"
}

# Espera hasta que un puerto TCP esté aceptando conexiones, o aborta.
wait_for_tcp() {
    local host=$1 port=$2 label=$3 timeout=${4:-15}
    local elapsed=0
    while ! (echo > "/dev/tcp/$host/$port") 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$((timeout * 10))" ]; then
            echo "✗ TIMEOUT esperando $label en $host:$port (${timeout}s)"
            echo "   últimas líneas del log:"
            tail -20 "$LOGS/$label.log" 2>/dev/null | sed 's/^/    /'
            return 1
        fi
    done
}

# Espera hasta que un endpoint HTTP responda 2xx.
wait_for_http() {
    local url=$1 label=$2 timeout=${3:-15}
    local elapsed=0
    while ! curl -fsS -o /dev/null -m 1 "$url" 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge "$((timeout * 10))" ]; then
            echo "✗ TIMEOUT esperando $label en $url (${timeout}s)"
            echo "   últimas líneas del log:"
            tail -20 "$LOGS/$label.log" 2>/dev/null | sed 's/^/    /'
            return 1
        fi
    done
}

# ─── 4. Arrancar qudittos ─────────────────────────────────────────────
echo "── arrancando qudittos…"
start_bg quditto-A "$ROOT/target/release/quditto" \
    --listen 127.0.0.1:8081 --r0 10000000 --alpha 0 --distance 0 --max-buffer 1048576 --key-size-bits 1024
start_bg quditto-B "$ROOT/target/release/quditto" \
    --listen 127.0.0.1:8082 --r0 10000000 --alpha 0 --distance 0 --max-buffer 1048576 --key-size-bits 1024

wait_for_http "http://127.0.0.1:8081/healthz" "quditto-A" 15
wait_for_http "http://127.0.0.1:8082/healthz" "quditto-B" 15

# ─── 5. Arrancar QKCs ─────────────────────────────────────────────────
echo "── arrancando QKCs…"
start_bg qkc1 "$ROOT/target/release/qkc" --config "$HERE/qkc1.toml"
start_bg qkc2 "$ROOT/target/release/qkc" --config "$HERE/qkc2.toml"
start_bg qkc3 "$ROOT/target/release/qkc" --config "$HERE/qkc3.toml"

# Esperar a los HTTP admin + peer listeners + local listeners de cada QKC.
wait_for_http "http://127.0.0.1:7201/healthz" "qkc1" 15
wait_for_http "http://127.0.0.1:7202/healthz" "qkc2" 15
wait_for_http "http://127.0.0.1:7203/healthz" "qkc3" 15
wait_for_tcp 127.0.0.1 7001 "qkc1" 5
wait_for_tcp 127.0.0.1 7002 "qkc2" 5
wait_for_tcp 127.0.0.1 7003 "qkc3" 5
wait_for_tcp 127.0.0.1 7101 "qkc1" 5
wait_for_tcp 127.0.0.1 7102 "qkc2" 5
wait_for_tcp 127.0.0.1 7103 "qkc3" 5

# ─── 6. Forwarding tables ─────────────────────────────────────────────
echo "── poblando forwarding tables…"
curl -fsS -X POST http://127.0.0.1:7201/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"updates": {"3": 2}}' | sed 's/^/  qkc1: /'
curl -fsS -X POST http://127.0.0.1:7203/forwarding-table \
    -H 'content-type: application/json' \
    -d '{"updates": {"1": 2}}' | sed 's/^/  qkc3: /'

echo
echo "✓ todo arriba."
echo "  PIDs:  $PIDS"
echo "  Logs:  $LOGS/*.log"
echo
echo "Pruebas:"
echo "  $HERE/status.sh"
echo "  $HERE/send-one.sh"
echo "  $HERE/stress.sh 10000 64"
echo "  $HERE/stop.sh"
