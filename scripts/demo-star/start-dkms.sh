#!/usr/bin/env bash
# Arranca 4 DKMSs (uno por hoja) sobre la demo-star de ORR+QKC.
#
# Pre-requisitos:
#   * start.sh (QKC+quditto) corriendo.
#   * start-orrs.sh corriendo.
#   * gen-tls.sh ya ejecutado (idempotente).
#
# Argumentos:
#   $1 = max_hops por defecto (default 1 = PQC E2E). Se sustituye en
#        el template como `southbound.default_max_hops` para todos
#        los DKMSs de esta corrida.

set -euo pipefail
MAX_HOPS=${1:-1}
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/dkms-pids
mkdir -p "$LOGS"
cd "$ROOT"

# Limpieza previa de DKMSs (no toca QKC/ORR/quditto)
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    echo "── encontré $PIDS no vacío. Matando DKMSs previos…"
    while read -r pid name; do
        kill "$pid" 2>/dev/null || true
    done < "$PIDS"
    sleep 0.3
fi
: > "$PIDS"

# Asegura TLS
if [ ! -f "$HERE/tls/ca.crt" ]; then
    echo "── falta TLS; generando…"
    "$HERE/gen-tls.sh"
fi

# Verifica puertos
ALL_PORTS=(8311 8322 8333 8344 8411 8422 8433 8444 \
           8511 8522 8533 8544 8611 8622 8633 8644 \
           9711 9722 9733 9744)
for port in "${ALL_PORTS[@]}"; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port ya está en uso"
        exit 2
    fi
done

echo "── compilando dkms (release)…"
cargo build --release -p dkms --bin dkms 2>&1 | tail -2
test -x "$ROOT/target/release/dkms" || { echo "✗ falta binario dkms"; exit 1; }

# Calcula orr_path para (origen, destino) en la estrella: intermedios
# (los 2 ORRs hoja que no son ni el origen ni el destino) seguidos del
# destino. La SDN cuando exista calculará esto por su lado.
orr_path_for() {
    local origin="$1" dest="$2"
    local mids=""
    for x in 11 22 33 44; do
        [ "$x" = "$origin" ] && continue
        [ "$x" = "$dest" ] && continue
        mids+="orr_$x,"
    done
    echo "${mids}orr_$dest"
}

echo "── arrancando 4 DKMSs (max_hops=$MAX_HOPS)…"
for nn in 11 22 33 44; do
    cfg_dir="$LOGS/dkms-$nn"
    mkdir -p "$cfg_dir"
    p11=$(orr_path_for "$nn" 11)
    p22=$(orr_path_for "$nn" 22)
    p33=$(orr_path_for "$nn" 33)
    p44=$(orr_path_for "$nn" 44)
    sed -e "s|__NN__|$nn|g" \
        -e "s|__ROOT__|$ROOT|g" \
        -e "s|__MAX_HOPS__|$MAX_HOPS|g" \
        -e "s|__ORR_PATH_11__|$p11|g" \
        -e "s|__ORR_PATH_22__|$p22|g" \
        -e "s|__ORR_PATH_33__|$p33|g" \
        -e "s|__ORR_PATH_44__|$p44|g" \
        "$HERE/dkms-template.toml" > "$cfg_dir/default.toml"

    logf="$LOGS/dkms-$nn.log"
    : > "$logf"
    CONFIG_DIR="$cfg_dir" RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}" \
        nohup "$ROOT/target/release/dkms" > "$logf" 2>&1 &
    pid=$!
    echo "$pid dkms-$nn" >> "$PIDS"
    echo "  ▶ dkms-$nn (pid $pid) → https://127.0.0.1:84$nn"
done

# Espera a que los 4 SAE-listeners estén accesibles
for nn in 11 22 33 44; do
    elapsed=0
    while ! (echo > "/dev/tcp/127.0.0.1/84$nn") 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge 100 ]; then
            echo "✗ TIMEOUT dkms-$nn SAE port"
            tail -20 "$LOGS/dkms-$nn.log" | sed 's/^/    /'
            exit 1
        fi
    done
done

# Espera a que el delivery pump conecte con el ORR co-localizado
echo "── esperando que cada DKMS suscriba StreamDeliveries…"
sleep 0.6
ok=$(grep -l "orr deliveries pump connected" "$LOGS"/dkms-*.log 2>/dev/null | wc -l)
elapsed=0
while [ "$ok" -lt 4 ] && [ "$elapsed" -lt 50 ]; do
    sleep 0.2
    elapsed=$((elapsed + 1))
    ok=$(grep -l "orr deliveries pump connected" "$LOGS"/dkms-*.log 2>/dev/null | wc -l)
done
if [ "$ok" -lt 4 ]; then
    echo "✗ solo $ok/4 DKMSs conectados al ORR (modos PQC fallarán)"
    exit 1
fi
echo "✓ 4/4 DKMSs con StreamDeliveries activo"

echo
echo "  PIDs DKMS: $PIDS"
echo "  Logs:      $LOGS/dkms-*.log"
echo
echo "  $HERE/saturate-dkms.sh [count] [size_bits]"
echo "  $HERE/stop-dkms.sh"
