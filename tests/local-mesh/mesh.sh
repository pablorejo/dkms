#!/usr/bin/env bash
# Malla DKMS de N nodos en una sola máquina, con los binarios de release.
#
# Levanta un despliegue completo —SDN + N×(qkc, orr, dkms)— a partir de los
# mismos `node.yml` y el mismo `render_config.py` que usan las imágenes, así
# que ejercita el contrato de despliegue de verdad y no una configuración
# inventada para el test. No sustituye a `tests/testbed/`, que corre contra el
# despliegue real por SSH: esto es lo que se puede repetir sin hardware.
#
#   mesh.sh up [N]              levanta N nodos (default 3)
#   mesh.sh down               para todo
#   mesh.sh topology           lo que ve la SDN
#   mesh.sh edges              aristas + comprobación de conectividad
#   mesh.sh link <n> [ids...]  reescribe los vecinos del qkc n y lo reinicia
#   mesh.sh keys [nodos...]    intercambio ETSI-014 por par ordenado
#   mesh.sh logs <modulo>      p.ej. `mesh.sh logs dkms3`
#
# Topología: anillo + cuerdas. Conexa por construcción, con grado ≥2 para que
# quitar un nodo no la parta y con caminos alternativos para el multipath.
# Cada enlace lo declara UN solo extremo a propósito: es lo que comprueba que
# la SDN se lo comunica al otro (ver "Peers ride back on the announcement" en
# CLAUDE.md).
#
# **Memoria**: todo va dentro de un scope de systemd con `MemoryMax`. Un
# despliegue local se ha comido una sesión de escritorio antes; ver la sección
# de saturación del CLAUDE.md. No lo quites.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/release"
RENDER="$REPO/docker/render_config.py"
GENCERTS="$REPO/docker/gen-certs.sh"
# Bajo tests/results/, que está en .gitignore: son artefactos, no fuente.
DIR="${DKMS_MESH_DIR:-$REPO/tests/results/local-mesh}"
SCOPE="dkms-local-mesh"
MEM_MAX="${DKMS_MESH_MEM_MAX:-8G}"
SDN_HTTP=19002
SDN_GRPC=19000

# Puertos: el nodo n ocupa el rango 20000+(n-1)*100 … +9. Con N≤10 no toca los
# 19xxx de la SDN.
port() { echo $(( $2 + ($1 - 1) * 100 )); }
peer_port()  { port "$1" 20000; }
local_port() { port "$1" 20001; }
admin_port() { port "$1" 20002; }
orr_port()   { port "$1" 20003; }
sae_port()   { port "$1" 20005; }

die() { echo "mesh: $*" >&2; exit 1; }

# ── vecinos declarados por el qkc n ────────────────────────────────────────
# Anillo n→n+1 (y N→1) más una cuerda cada tres nodos hacia el opuesto. Sólo
# lo declara el extremo menor de cada par, para que el otro tenga que
# enterarse por la SDN.
neighbours_of() {
    local n=$1 total=$2 out=()
    (( total >= 2 )) && out+=( $(( n % total + 1 )) )
    if (( total >= 6 && n % 3 == 1 )); then
        local opposite=$(( (n - 1 + total / 2) % total + 1 ))
        (( opposite != n )) && out+=( "$opposite" )
    fi
    echo "${out[@]}"
}

write_qkc_yml() {   # write_qkc_yml <n> [vecinos...]
    local n=$1; shift
    {
        echo "qkc_id: $n"
        echo 'sdn_url: "127.0.0.1"'
        echo 'advertise_ip: "127.0.0.1"'
        echo "sdn_announce_secs: 5"
        echo "ports: {peer: $(peer_port "$n"), local: $(local_port "$n"), admin: $(admin_port "$n")}"
        if (( $# > 0 )); then
            echo "links:"
            for nb in "$@"; do echo "  - {neighbor_id: $nb, type: pqc}"; done
        fi
    } > "$DIR/yml/node$n.qkc.yml"
    python3 "$RENDER" qkc "$DIR/yml/node$n.qkc.yml" "$DIR/cfg/qkc$n" >/dev/null
}

generate() {        # generate <N>
    local total=$1
    rm -rf "$DIR"; mkdir -p "$DIR"/{yml,cfg/sdn,logs,certs}
    printf 'listen_ip: "0.0.0.0"\npresence_ttl_secs: 90\n' > "$DIR/yml/node.sdn.yml"
    python3 "$RENDER" sdn "$DIR/yml/node.sdn.yml" "$DIR/cfg/sdn" >/dev/null

    for n in $(seq 1 "$total"); do
        mkdir -p "$DIR/cfg/qkc$n" "$DIR/cfg/orr$n" "$DIR/cfg/dkms$n"
        # shellcheck disable=SC2046
        write_qkc_yml "$n" $(neighbours_of "$n" "$total")

        cat > "$DIR/yml/node$n.orr.yml" <<EOF
orr_id: "orr_$n"
qkc_id: $n
qkc_addr: "127.0.0.1:$(local_port "$n")"
sdn_url: "http://127.0.0.1:$SDN_GRPC"
advertise_ip: "127.0.0.1"
sdn_announce_secs: 5
ports: {grpc: $(orr_port "$n"), metrics: $(port "$n" 20004)}
EOF
        python3 "$RENDER" orr "$DIR/yml/node$n.orr.yml" "$DIR/cfg/orr$n" >/dev/null

        cat > "$DIR/yml/node$n.dkms.yml" <<EOF
node_id: "dkms-$n"
advertise_ip: "127.0.0.1"
orr_addr: "127.0.0.1:$(orr_port "$n")"
sdn_endpoint: "http://127.0.0.1:$SDN_GRPC"
orr_id: "orr_$n"
sdn_announce_secs: 5
certs_dir: "$DIR/certs"
ports: {sae: $(sae_port "$n"), peer: $(port "$n" 20006), grpc: $(port "$n" 20007), metrics: $(port "$n" 20008), ack: $(port "$n" 20009)}
sae_bindings:
  sae_$n: dkms-$n
EOF
        python3 "$RENDER" dkms "$DIR/yml/node$n.dkms.yml" "$DIR/cfg/dkms$n" >/dev/null

        # Una sola CA para toda la malla: el mTLS entre DKMS exige raíz común.
        bash "$GENCERTS" "dkms-$n" 127.0.0.1 "$DIR/certs" >/dev/null 2>&1
        bash "$GENCERTS" --sae "sae_$n" "$DIR/certs" >/dev/null 2>&1
    done
    echo "$total" > "$DIR/N"
}

write_boot() {      # el proceso principal del scope: mientras viva, vive el cgroup
    cat > "$DIR/boot.sh" <<EOF
#!/usr/bin/env bash
export RUST_LOG=\${RUST_LOG:-info}
CONFIG_DIR="$DIR/cfg/sdn" nohup "$BIN/sdn" > "$DIR/logs/sdn.log" 2>&1 &
echo \$! > "$DIR/logs/sdn.pid"
for n in \$(seq 1 $1); do
  CONFIG_DIR="$DIR/cfg/qkc\$n" nohup "$BIN/qkc" --config "$DIR/cfg/qkc\$n/qkc.toml" \\
      > "$DIR/logs/qkc\$n.log" 2>&1 &
  echo \$! > "$DIR/logs/qkc\$n.pid"
done
for r in orr dkms; do
  for n in \$(seq 1 $1); do
    CONFIG_DIR="$DIR/cfg/\$r\$n" nohup "$BIN/\$r" > "$DIR/logs/\$r\$n.log" 2>&1 &
    echo \$! > "$DIR/logs/\$r\$n.pid"
  done
done
wait
EOF
    chmod +x "$DIR/boot.sh"
}

wait_for() {        # wait_for <segundos> <expresión que debe dar "true">
    local deadline=$(( SECONDS + $1 )); shift
    while (( SECONDS < deadline )); do
        [ "$(eval "$*")" = "true" ] && return 0
        sleep 1
    done
    return 1
}

topology_json() { curl -s --max-time 5 "http://127.0.0.1:$SDN_HTTP/topology"; }

cmd_up() {
    local total=${1:-3}
    [ -x "$BIN/sdn" ] || die "faltan los binarios: cargo build --release"
    (( total >= 2 && total <= 10 )) || die "N entre 2 y 10 (los puertos son 20000+100n)"
    echo "mesh: generando $total nodos en $DIR"
    generate "$total"
    write_boot "$total"
    echo "mesh: arrancando $(( 1 + total * 3 )) procesos con MemoryMax=$MEM_MAX"
    systemd-run --user --scope --unit="$SCOPE" \
        -p MemoryMax="$MEM_MAX" -p MemorySwapMax=2G "$DIR/boot.sh" >/dev/null 2>&1 &
    curl -s --retry 40 --retry-delay 1 --retry-connrefused \
        "http://127.0.0.1:$SDN_HTTP/healthz" >/dev/null || die "la SDN no arrancó"
    local want=$(( total * 3 ))
    if wait_for 180 "topology_json | python3 -c \"import sys,json;t=json.load(sys.stdin);print(str(t['qkcs']+t['orrs']+t['dkms']==$want).lower())\""; then
        echo "mesh: $want módulos registrados"
    else
        echo "mesh: AVISO — la topología no convergió a $want módulos" >&2
    fi
    cmd_edges
}

cmd_down() {
    systemctl --user stop "$SCOPE.scope" >/dev/null 2>&1
    for p in "$DIR"/logs/*.pid; do
        [ -f "$p" ] || continue
        kill "$(cat "$p")" 2>/dev/null
        rm -f "$p"
    done
    sleep 1
    echo "mesh: parado"
}

cmd_topology() { topology_json | python3 -m json.tool; }

cmd_edges() {
    curl -s --max-time 5 "http://127.0.0.1:$SDN_HTTP/links" | python3 -c '
import sys, json, collections
E = [(l["a"], l["b"]) for l in json.load(sys.stdin)]
print(f"  {len(E)} aristas:", sorted((int(a), int(b)) for a, b in E))
if not E:
    sys.exit(0)
g = collections.defaultdict(set)
for a, b in E:
    g[a].add(b); g[b].add(a)
start = next(iter(g)); seen = {start}; stack = [start]
while stack:
    for x in g[stack.pop()]:
        if x not in seen:
            seen.add(x); stack.append(x)
print(f"  {len(seen)}/{len(g)} nodos alcanzables ->",
      "CONEXO" if len(seen) == len(g) else "PARTIDO")
print("  grados:", {k: len(v) for k, v in sorted(g.items(), key=lambda kv: int(kv[0]))})'
}

# Reescribe los vecinos declarados por un qkc y lo reinicia. Con esto se
# crean, modifican y eliminan enlaces: el anuncio es autoritativo sobre lo que
# ese QKC declara, así que dejar de nombrar a un vecino retira la arista.
cmd_link() {
    local n=$1; shift
    [ -f "$DIR/N" ] || die "no hay malla levantada"
    write_qkc_yml "$n" "$@"
    kill "$(cat "$DIR/logs/qkc$n.pid")" 2>/dev/null
    cat "$DIR/logs/qkc$n.log" >> "$DIR/logs/qkc$n.hist.log" 2>/dev/null
    CONFIG_DIR="$DIR/cfg/qkc$n" RUST_LOG="${RUST_LOG:-info}" nohup \
        "$BIN/qkc" --config "$DIR/cfg/qkc$n/qkc.toml" > "$DIR/logs/qkc$n.log" 2>&1 &
    echo $! > "$DIR/logs/qkc$n.pid"
    echo "mesh: qkc$n declara ahora [${*:-ninguno}]"
}

cmd_logs() { sed 's/\x1b\[[0-9;]*m//g' "$DIR/logs/$1.log"; }

case "${1:-}" in
    up)       shift; cmd_up "$@" ;;
    down)     cmd_down ;;
    topology) cmd_topology ;;
    edges)    cmd_edges ;;
    link)     shift; cmd_link "$@" ;;
    keys)     shift; DKMS_MESH_DIR="$DIR" exec "$(dirname "${BASH_SOURCE[0]}")/keys_smoke.sh" "$@" ;;
    logs)     shift; cmd_logs "$@" ;;
    *)        sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 2 ;;
esac
