#!/usr/bin/env bash
# Arranca 4 ORRs (uno por hoja) sobre la topología estrella QKC.
# Asume que start.sh (QKC + quditto) ya está corriendo.
#
# Cada ORR tiene `peer_grpc_addrs` apuntando a los otros 3, y al
# arrancar pide sus pubkeys ML-KEM vía GetPublicKey con backoff
# exponencial — los modos 1 / -1 / >=2 (PQC) funcionan automáticamente.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/orr-pids
mkdir -p "$LOGS"
cd "$ROOT"

# Limpieza previa de ORRs (no toca los QKCs/qudittos)
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    echo "── encontré $PIDS no vacío. Matando ORRs previos…"
    while read -r pid name; do
        kill "$pid" 2>/dev/null || true
    done < "$PIDS"
    sleep 0.3
fi
: > "$PIDS"

# Verifica puertos gRPC + metrics
for port in 50511 50522 50533 50544 9511 9522 9533 9544; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port ya está en uso"
        exit 2
    fi
done

# Compila si hace falta
echo "── compilando orr (release)…"
cargo build --release -p orr --bin orr --bin orr-test-client 2>&1 | tail -2
test -x "$ROOT/target/release/orr" || { echo "✗ falta binario orr"; exit 1; }
test -x "$ROOT/target/release/orr-test-client" || { echo "✗ falta orr-test-client"; exit 1; }

start_bg() {
    local name=$1 leaf=$2
    local logf="$LOGS/$name.log"
    : > "$logf"
    CONFIG_DIR="$HERE/$name" RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}" \
        nohup "$ROOT/target/release/orr" > "$logf" 2>&1 &
    local pid=$!
    echo "$pid $name" >> "$PIDS"
    echo "  ▶ $name (pid $pid)  → http://127.0.0.1:505$leaf"
}

echo "── arrancando 4 ORRs (uno por hoja)…"
start_bg orr-11 11
start_bg orr-22 22
start_bg orr-33 33
start_bg orr-44 44

# Espera a que los 4 gRPC estén accesibles
for port in 50511 50522 50533 50544; do
    elapsed=0
    while ! (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge 100 ]; then
            echo "✗ TIMEOUT gRPC :$port"
            tail -10 "$LOGS/orr-$((port % 100)).log" | sed 's/^/    /'
            exit 1
        fi
    done
done

# Espera al bootstrap de pubkeys: cada ORR necesita las 3 ajenas
echo "── esperando bootstrap de pubkeys cross…"
sleep 1.0

n_bootstrap=$(grep -h "orr.peer_pubkey bootstrap ok" "$LOGS"/orr-*.log 2>/dev/null | wc -l)
expected=12  # 4 ORRs × 3 peers = 12 intercambios
elapsed=0
while [ "$n_bootstrap" -lt "$expected" ] && [ "$elapsed" -lt 50 ]; do
    sleep 0.2
    elapsed=$((elapsed + 1))
    n_bootstrap=$(grep -h "orr.peer_pubkey bootstrap ok" "$LOGS"/orr-*.log 2>/dev/null | wc -l)
done

if [ "$n_bootstrap" -lt "$expected" ]; then
    echo "✗ solo $n_bootstrap/$expected bootstraps completados — los modos PQC fallarán"
else
    echo "✓ $n_bootstrap/$expected pubkeys intercambiadas vía GetPublicKey"
fi

echo
echo "  PIDs ORR: $PIDS"
echo "  Logs:     $LOGS/orr-*.log"
echo
echo "  $HERE/saturate-orr.sh [count] [bytes] [max_hops]"
echo "  $HERE/stop-orrs.sh"
