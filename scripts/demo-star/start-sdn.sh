#!/usr/bin/env bash
# Arranca la SDN sobre la demo-star.
#
# Genera una carpeta `topology/` con la topología estrella en formato
# JSON (QKC/*.json, ORR/*.json, DKMS/*.json, SAE/*.json) y se la pasa
# al SDN como `topology_dir`. El SDN la carga al arrancar.
#
# Tras esto:
#   * DKMS con `sdn_endpoint` configurado resuelve SAEs vía SDN
#     (la cache TTL hace que sólo la primera consulta pegue a la red).
#   * ORR con `sdn_url` configurado pide `GetOrrPath` la primera vez
#     que necesita un path para un destino, y cachea localmente.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/sdn-pids
TOPO=$LOGS/topology
CFG=$LOGS/sdn-cfg
mkdir -p "$LOGS"
cd "$ROOT"

# Limpieza previa
if [ -f "$PIDS" ] && [ -s "$PIDS" ]; then
    echo "── encontré $PIDS no vacío. Matando SDN previo…"
    while read -r pid name; do
        kill "$pid" 2>/dev/null || true
    done < "$PIDS"
    sleep 0.3
fi
: > "$PIDS"

# Verifica puertos SDN
for port in 50053 50055 9102; do
    if (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
        echo "✗ puerto 127.0.0.1:$port ya está en uso"
        exit 2
    fi
done

# ─── Generar la topología estrella en JSON ────────────────────────────
echo "── generando topología estrella en $TOPO/"
rm -rf "$TOPO" "$CFG"
mkdir -p "$TOPO/QKC" "$TOPO/ORR" "$TOPO/DKMS" "$TOPO/SAE" "$CFG"

# Parámetros del modelo de quditto que la SDN usa para calcular capacidades.
# Idealmente coinciden con los que pasaste a `start.sh` para el quditto real;
# si no, los rates que devuelve `GET /rate` no reflejan la realidad.
SDN_QD_R0=${SDN_QD_R0:-${QD_R0:-100000000}}
SDN_QD_ALPHA=${SDN_QD_ALPHA:-${QD_ALPHA:-0}}
SDN_QD_BUFFER=${SDN_QD_BUFFER:-${QD_BUFFER:-1048576}}
echo "── SDN model: R0=$SDN_QD_R0 alpha=$SDN_QD_ALPHA buffer=$SDN_QD_BUFFER"

# 9 QKCs: hub (0), 4 intermedios (1-4), 4 hojas (11/22/33/44).
# Cada QKC tiene su host {id, ip, port} y opcionalmente `kmes` (canales).
emit_qkc() {
    local id="$1" host_id="$2" port="$3"; shift 3
    local nei=""
    local first=1
    for n in "$@"; do
        if [ $first -eq 1 ]; then first=0; else nei+=","; fi
        nei+="$(printf '{"neighbor_qkc_id":"%s","channel":{"distance":1,"quditto_rate_r0":%s,"quditto_rate_alpha":%s,"quditto_max_buffer_size":%s}}' "$n" "$SDN_QD_R0" "$SDN_QD_ALPHA" "$SDN_QD_BUFFER")"
    done
    cat > "$TOPO/QKC/qkc-$id.json" <<EOF
{
  "id": "$id",
  "host": {"id": $host_id, "ip": "127.0.0.1", "port": $port},
  "kmes": [$nei]
}
EOF
}
# El hub (0) conecta con los 4 intermedios; cada intermedio (1-4) con
# el hub y con su hoja respectiva (1↔11, 2↔22, 3↔33, 4↔44). Las hojas
# tienen un único enlace hacia su intermedio. SDN ve el grafo no dirigido.
emit_qkc 0  100 7200 1 2 3 4
emit_qkc 1  101 7201 0 11
emit_qkc 2  102 7202 0 22
emit_qkc 3  103 7203 0 33
emit_qkc 4  104 7204 0 44
emit_qkc 11 111 7211 1
emit_qkc 22 122 7222 2
emit_qkc 33 133 7233 3
emit_qkc 44 144 7244 4

# 4 ORRs (uno por hoja), cada uno co-localizado con su QKC hoja.
for nn in 11 22 33 44; do
    cat > "$TOPO/ORR/orr_$nn.json" <<EOF
{
  "id": "orr_$nn",
  "qkc_id": "$nn",
  "host": {"id": $((200 + nn)), "ip": "127.0.0.1", "port": $((50500 + nn))}
}
EOF
done

# 4 DKMSs, cada uno apuntando al ORR co-localizado.
for nn in 11 22 33 44; do
    cat > "$TOPO/DKMS/dkms-$nn.json" <<EOF
{
  "id": "dkms-$nn",
  "orr_id": "orr_$nn",
  "host": {"id": $((300 + nn)), "ip": "127.0.0.1", "port": $((8400 + nn))}
}
EOF
done

# 4 SAEs (uno por DKMS): sae_aa→dkms-11, sae_bb→22, sae_cc→33, sae_dd→44.
for pair in "aa:11" "bb:22" "cc:33" "dd:44"; do
    sae=${pair%%:*}
    dk=${pair##*:}
    cat > "$TOPO/SAE/sae_$sae.json" <<EOF
{
  "id": "sae_$sae",
  "dkms_id": "dkms-$dk"
}
EOF
done

# ─── Config del SDN ───────────────────────────────────────────────────
cat > "$CFG/default.toml" <<EOF
node_id  = "sdn-demo"
grpc_addr     = "0.0.0.0:50053"
http_addr     = "0.0.0.0:50055"
metrics_addr  = "0.0.0.0:9102"
topology_dir  = "$TOPO"
default_policy = "shortest_hops"
mcf_period_ms  = 5000
push_debounce_ms = 100
mcf_k_paths = 3
EOF

# ─── Compilar + arrancar ──────────────────────────────────────────────
echo "── compilando sdn (release)…"
cargo build --release -p sdn --bin sdn 2>&1 | tail -2
test -x "$ROOT/target/release/sdn" || { echo "✗ falta binario sdn"; exit 1; }

logf="$LOGS/sdn.log"
: > "$logf"
CONFIG_DIR="$CFG" RUST_LOG="${RUST_LOG:-info,tonic=warn,h2=warn}" \
    nohup "$ROOT/target/release/sdn" > "$logf" 2>&1 &
pid=$!
echo "$pid sdn" >> "$PIDS"
echo "  ▶ sdn (pid $pid)  → gRPC :50053  HTTP :50055"

# Espera a que ambos puertos respondan
for port in 50053 50055; do
    elapsed=0
    while ! (echo > "/dev/tcp/127.0.0.1/$port") 2>/dev/null; do
        sleep 0.1
        elapsed=$((elapsed + 1))
        if [ "$elapsed" -ge 100 ]; then
            echo "✗ TIMEOUT SDN :$port"
            tail -20 "$logf" | sed 's/^/    /'
            exit 1
        fi
    done
done

# Confirma que cargó la topología
loaded=$(curl -fsS http://127.0.0.1:50055/topology 2>/dev/null || echo '{}')
echo "  topología: $loaded"

echo
echo "  Logs: $logf"
echo "  $HERE/stop-sdn.sh"
