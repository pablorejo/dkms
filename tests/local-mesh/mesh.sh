#!/usr/bin/env bash
# Malla DKMS de N nodos en una sola máquina, con los binarios de release.
#
# Levanta un despliegue completo —SDN + N×(qkc, orr, dkms)— a partir de los
# mismos `node.yml` y el mismo `render_config.py` que usan las imágenes, así
# que ejercita el contrato de despliegue de verdad y no una configuración
# inventada para el test. No sustituye a `tests/testbed/`, que corre contra el
# despliegue real por SSH: esto es lo que se puede repetir sin hardware.
#
#   mesh.sh up [N] [topo]       levanta N nodos (default 3, ring)
#   mesh.sh down               para todo
#   mesh.sh topology           lo que ve la SDN
#   mesh.sh edges              aristas + comprobación de conectividad
#   mesh.sh link <n> [ids...]  reescribe los vecinos del qkc n y lo reinicia
#   mesh.sh keys [nodos...]    intercambio ETSI-014 por par ordenado
#   mesh.sh logs <modulo>      p.ej. `mesh.sh logs dkms3`
#
# Tres topologías, y la elección cambia lo que se mide:
#
#   ring    anillo + cuerdas. Conexa por construcción, grado >=2 para que
#           quitar un nodo no la parta, y con caminos alternativos para que el
#           multipath tenga algo que repartir. Es el caso amable.
#   star    todos colgando del nodo 1. El hub queda con grado N-1 y todo el
#           tráfico que no sea suyo le pasa por encima: es el caso que carga
#           un solo QKC. Ojo, quitar el hub parte la red entera, así que las
#           pruebas de baja de nodo hay que hacerlas sobre una hoja.
#   random  grafo aleatorio CONEXO con semilla (DKMS_MESH_SEED, default 42):
#           árbol de expansión aleatorio + aristas extra hasta grado medio ~3.
#           Reproducible, y es lo que se parece a un despliegue real, donde
#           nadie cablea un anillo perfecto.
#
# Cada enlace lo declara UN solo extremo a propósito: es lo que comprueba que
# la SDN se lo comunica al otro (ver "Peers ride back on the announcement" en
# CLAUDE.md).
#
# **Memoria**: todo va dentro de un scope de systemd con `MemoryMax`. Un
# despliegue local se ha comido una sesión de escritorio antes; ver la sección
# de saturación del CLAUDE.md. No lo quites.
#
# Para medir hace falta poder levantar los dos techos que, con los defaults,
# son constantes nuestras y no límites del sistema:
#
#   DKMS_MESH_TOKENS_PER_TICK   max_tokens_per_peer_per_tick (default 32, que
#                               con tick_ms=100 son 320 claves/s por peer)
#   DKMS_MESH_SAE_MIN_TOKENS    min_capacity_tokens del bucket por SAE, cuyo
#                               refill sale de la rate del SDN — inútil en un
#                               despliegue PQC-only, donde puede valer 0
#
# Se parchea el TOML renderizado y NO se usan variables de entorno del binario:
# config-rs sustituye la sección entera al fijar un campo anidado por env (ver
# CLAUDE.md), así que habría que repetir todos los campos de [generator].
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/release"
RENDER="$REPO/docker/render_config.py"
GENCERTS="$REPO/docker/gen-certs.sh"
# Bajo tests/results/, que está en .gitignore: son artefactos, no fuente.
DIR="${DKMS_MESH_DIR:-$REPO/tests/results/local-mesh}"
SCOPE="dkms-local-mesh"
MEM_MAX="${DKMS_MESH_MEM_MAX:-8G}"
TOKENS_PER_TICK="${DKMS_MESH_TOKENS_PER_TICK:-}"
SAE_MIN_TOKENS="${DKMS_MESH_SAE_MIN_TOKENS:-}"
SDN_HTTP=19002
SDN_GRPC=19000
TOPO="${DKMS_MESH_TOPO:-ring}"
SEED="${DKMS_MESH_SEED:-42}"

# Puertos: el nodo n ocupa el rango 20000+(n-1)*100 … +9. Con N≤10 no toca los
# 19xxx de la SDN.
port() { echo $(( $2 + ($1 - 1) * 100 )); }
peer_port()  { port "$1" 20000; }
local_port() { port "$1" 20001; }
admin_port() { port "$1" 20002; }
orr_port()   { port "$1" 20003; }
sae_port()   { port "$1" 20005; }

die() { echo "mesh: $*" >&2; exit 1; }

# ── vecinos declarados por cada qkc, según la topología ────────────────────
#
# Se calcula una sola vez y se cachea en $DIR/topology.tsv: la aleatoria tiene
# que salir igual para todos los nodos, así que no puede recalcularse por
# separado en cada llamada.
#
# En las tres, cada arista la declara SOLO el extremo de id menor. Es
# deliberado: obliga a que el otro extremo se entere por la SDN, que es la
# propiedad que sostiene todo el diseño de auto-configuración.
build_topology() {  # build_topology <N>
    python3 - "$1" "$TOPO" "$SEED" <<'TOPO_PY' > "$DIR/topology.tsv"
import random, sys
n, topo, seed = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
edges = set()

if topo == "star":
    edges = {(1, k) for k in range(2, n + 1)}
elif topo == "random":
    rnd = random.Random(seed)
    # Primero un árbol de expansión: garantiza que sale conexa. Un G(n,p) a
    # secas puede salir partido, y una malla partida no mide lo que se quiere
    # medir, mide otra cosa.
    order = list(range(1, n + 1))
    rnd.shuffle(order)
    for i in range(1, len(order)):
        a, b = order[i], order[rnd.randrange(i)]
        edges.add((min(a, b), max(a, b)))
    # Y aristas extra hasta un grado medio de ~3, que es el orden de los
    # despliegues que se han medido en este proyecto.
    target = max(n - 1, (3 * n) // 2)
    todos = [(a, b) for a in range(1, n + 1) for b in range(a + 1, n + 1)]
    rnd.shuffle(todos)
    for e in todos:
        if len(edges) >= target:
            break
        edges.add(e)
else:  # ring
    if n >= 2:
        edges = {(min(k, k % n + 1), max(k, k % n + 1)) for k in range(1, n + 1)}
    for k in range(1, n + 1, 3):
        opposite = (k - 1 + n // 2) % n + 1
        if n >= 6 and opposite != k:
            edges.add((min(k, opposite), max(k, opposite)))

# Una línea por nodo: el declarante y sus vecinos declarados.
declared = {k: [] for k in range(1, n + 1)}
for a, b in sorted(edges):
    declared[a].append(b)
for k in range(1, n + 1):
    print("%d\t%s" % (k, " ".join(str(x) for x in declared[k])))
TOPO_PY
}

neighbours_of() {   # neighbours_of <n>
    awk -F'\t' -v n="$1" '$1 == n { print $2 }' "$DIR/topology.tsv"
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

# Levanta los topes de medida sobre el TOML ya renderizado.
#
# `max_tokens_per_peer_per_tick` se INSERTA tras la cabecera [generator], no se
# anexa al final: el fichero termina en [sae_bindings], y una línea suelta al
# final caería dentro de esa sección. [sae] no lo escribe el render, así que
# ahí sí se puede añadir la sección entera.
patch_dkms_toml() {
    local toml=$1
    if [ -n "$TOKENS_PER_TICK" ]; then
        sed -i "/^\[generator\]/a max_tokens_per_peer_per_tick = $TOKENS_PER_TICK" "$toml"
    fi
    if [ -n "$SAE_MIN_TOKENS" ]; then
        printf '\n[sae]\nmin_capacity_tokens = %s\n' "$SAE_MIN_TOKENS" >> "$toml"
    fi
}

generate() {        # generate <N>
    local total=$1
    rm -rf "$DIR"; mkdir -p "$DIR"/{yml,cfg/sdn,logs,certs}
    build_topology "$total"
    printf 'listen_ip: "0.0.0.0"\npresence_ttl_secs: 90\n' > "$DIR/yml/node.sdn.yml"
    python3 "$RENDER" sdn "$DIR/yml/node.sdn.yml" "$DIR/cfg/sdn" >/dev/null

    for n in $(seq 1 "$total"); do
        mkdir -p "$DIR/cfg/qkc$n" "$DIR/cfg/orr$n" "$DIR/cfg/dkms$n"
        # shellcheck disable=SC2046
        write_qkc_yml "$n" $(neighbours_of "$n")

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
        patch_dkms_toml "$DIR/cfg/dkms$n/default.toml"

        # Una sola CA para toda la malla: el mTLS entre DKMS exige raíz común.
        bash "$GENCERTS" "dkms-$n" 127.0.0.1 "$DIR/certs" >/dev/null 2>&1
        bash "$GENCERTS" --sae "sae_$n" "$DIR/certs" >/dev/null 2>&1
    done
    echo "$total" > "$DIR/N"
}

write_boot() {      # el proceso principal del scope: mientras viva, vive el cgroup
    # `starts.tsv` guarda el instante en que se lanza cada proceso. Es el t0 de
    # los tiempos de convergencia: el primer log del módulo ya llega tarde por
    # lo que tarde en inicializarse, así que medir desde ahí escondería
    # justamente parte de lo que se quiere medir.
    cat > "$DIR/boot.sh" <<EOF
#!/usr/bin/env bash
export RUST_LOG=\${RUST_LOG:-info}
: > "$DIR/logs/starts.tsv"
mark() { printf '%s\\t%s\\n' "\$1" "\$(date +%s.%N)" >> "$DIR/logs/starts.tsv"; }
mark sdn
CONFIG_DIR="$DIR/cfg/sdn" nohup "$BIN/sdn" > "$DIR/logs/sdn.log" 2>&1 &
echo \$! > "$DIR/logs/sdn.pid"
for n in \$(seq 1 $1); do
  mark "qkc\$n"
  CONFIG_DIR="$DIR/cfg/qkc\$n" nohup "$BIN/qkc" --config "$DIR/cfg/qkc\$n/qkc.toml" \\
      > "$DIR/logs/qkc\$n.log" 2>&1 &
  echo \$! > "$DIR/logs/qkc\$n.pid"
done
for r in orr dkms; do
  for n in \$(seq 1 $1); do
    mark "\$r\$n"
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
    [ -n "${2:-}" ] && TOPO="$2"
    case "$TOPO" in ring|star|random) ;; *) die "topología desconocida: $TOPO (ring|star|random)" ;; esac
    [ -x "$BIN/sdn" ] || die "faltan los binarios: cargo build --release"
    (( total >= 2 && total <= 10 )) || die "N entre 2 y 10 (los puertos son 20000+100n)"
    echo "mesh: generando $total nodos en $DIR  (topología: $TOPO$([ "$TOPO" = random ] && echo ", semilla $SEED"))"
    generate "$total"
    write_boot "$total"
    # Se prueba a ejecutarlo, no sólo a que exista el binario: dentro de un
    # trabajo de SLURM `systemd-run` está pero no hay bus de sesión de usuario,
    # así que fallaría al arrancar y la malla se quedaría sin levantar.
    if systemd-run --user --scope --quiet true >/dev/null 2>&1; then
        echo "mesh: arrancando $(( 1 + total * 3 )) procesos con MemoryMax=$MEM_MAX"
        systemd-run --user --scope --unit="$SCOPE" \
            -p MemoryMax="$MEM_MAX" -p MemorySwapMax=2G "$DIR/boot.sh" >/dev/null 2>&1 &
    else
        # Sin scope propio el tope lo pone quien nos haya lanzado. Bajo SLURM
        # eso es el `--mem` del trabajo, que es un cgroup igual de real; en una
        # sesión normal significa que NO hay tope, y eso hay que decirlo.
        echo "mesh: systemd-run no utilizable aquí; arranco sin scope propio" >&2
        if [ -n "${SLURM_JOB_ID:-}" ]; then
            echo "mesh: el tope lo pone SLURM (job $SLURM_JOB_ID, --mem=${SLURM_MEM_PER_NODE:-?} MB)" >&2
        else
            echo "mesh: AVISO — sin límite de memoria; ver la sección de saturación de CLAUDE.md" >&2
        fi
        "$DIR/boot.sh" >/dev/null 2>&1 &
    fi
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
