#!/usr/bin/env bash
# Malla DKMS de N nodos en una sola máquina, con los binarios de release.
#
# Levanta un despliegue completo —SDN + N×(qkc, orr, dkms)— a partir de los
# mismos `node.yml` y el mismo `render_config.py` que usan las imágenes, así
# que ejercita el contrato de despliegue de verdad y no una configuración
# inventada para el test. No sustituye a `tests/testbed/`, que corre contra el
# despliegue real por SSH: esto es lo que se puede repetir sin hardware.
#
#   mesh.sh up [N] [topo]       levanta N nodos (default 3, ring; custom lee
#                              las aristas de DKMS_MESH_EDGES="1-2 2-3 ...")
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
#   DKMS_MESH_ACK_TRANSPORT     etsi020 (default del binario desde 2026-09-03:
#                              ACK por mTLS, listener del socket apagado) |
#                              socket: el TCP plano heredado, brazo de comparación
#   DKMS_MESH_PQC_REKEY_SECS    pqc_rekey_secs de los enlaces PQC (default 3600)
#   DKMS_MESH_E2E_REKEY_SECS    transport_e2e.rekey_secs del DKMS (default 3600)
#   DKMS_MESH_ROTATION_MS       rotation_period_ms del ORR (default 1 h)
#   DKMS_MESH_MAX_HOPS          default_max_hops del DKMS (0 = relé; 1 = cebolla e2e)
#   DKMS_MESH_BOOTSTRAP_TRUST   strict (default del binario desde 2026-09-03: el
#                              ORR exige anuncios de pubkey firmados con el cert
#                              de nodo, sin verify keys) | tofu: brazo de comparación
#   DKMS_MESH_LINK_TYPE         pqc (default) o qkd. En modo qkd se levanta un
#                               quditto POR ARISTA —los dos QKC del enlace
#                               apuntan al mismo, uno pide enc_keys y el otro
#                               dec_keys con esos ID— y los enlaces se declaran
#                               en AMBOS extremos, porque la SDN no puede
#                               inventarse el kme_url. Con esto la capacidad de
#                               la arista pasa a significar algo:
#                               cap = R0 · 10^(-alpha·d/10), en vez del
#                               centinela de 1e9 de las aristas pqc.
#   DKMS_MESH_R0                R0 del quditto en claves/s (default 2000)
#   DKMS_MESH_ALPHA             atenuación dB/km (default 0.2)
#   DKMS_MESH_DIST_KM           distancia del enlace (default 5)
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
# key id de un cert: `Subject` (el suyo) o `Authority` (el de quien lo firmó).
# Sirve para saber si una hoja cuelga de una CA sin verificar la firma, que
# con ML-DSA no puede hacer un openssl viejo. Acepta el formato de 1.1.1
# (`keyid:XX:..`) y el de 3.x (`XX:..` a secas).
cert_keyid() {
    openssl x509 -in "$1" -noout -text 2>/dev/null \
        | grep -A1 "$2 Key Identifier" | tail -1 | tr -d ' ' | sed 's/^keyid://'
}
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
LINK_TYPE="${DKMS_MESH_LINK_TYPE:-pqc}"
# DKMS_MESH_PQC_AUTH=sign firma con ML-DSA el handshake del QKC (enlaces PQC) y
# el anuncio de pubkey del ORR. Con `off` (default) esos dos planos van sin
# firmar, como han ido siempre. Las identidades las genera `pqc_keygen` en
# $DIR/signkeys y son efímeras como la malla.
PQC_AUTH="${DKMS_MESH_PQC_AUTH:-off}"
# DKMS_MESH_FRAME_AUTH=prefer|require pone MAC a los frames de DATOS del enlace
# QKC<->QKC (integridad + autenticacion de origen + anti-replay). Necesita una
# `link_psk` identica en los dos extremos: se deriva del indice de arista, que
# es el mismo mirado desde cualquiera de los dos lados.
FRAME_AUTH="${DKMS_MESH_FRAME_AUTH:-off}"
# El gRPC del ORR va con mTLS (lo que le habla su DKMS —material de
# transporte— y lo que le hablan los ORR pares), como en producción: es el
# default del binario. DKMS_MESH_GRPC_TLS=0 lo apaga, que es el brazo de
# comparación. Necesita un cert de nodo por ORR (orr_N, CA de red), que se
# genera aquí o viene en DKMS_MESH_CERTS_SRC.
GRPC_TLS="${DKMS_MESH_GRPC_TLS:-1}"
# DKMS_MESH_CONTROL_TLS=0 deja el plano de control en claro (el brazo
# histórico). Con 1 (default) la SDN lleva [tls] y sirve su HTTP admin Y su
# gRPC con mTLS, todos los anuncios van https con el cert de nodo, y el push
# de forwarding viaja https — y en el QKC solo lo autoriza el cert `sdn`
# (B1b). La razón de ser del brazo: el QKC lleva su [tls], que desde la
# Fase 10 enciende POR DEFECTO el handshake firmado con cert (`sign`) y el
# sello por-frame (`require`) en los DOS tipos de enlace (A3/A4), sin
# declarar nada por par. Necesita certs `qkc-N` y `sdn` además de los de
# siempre — los juegos pre-generados de CESGA tienen que crecer con ellos.
# Nota: con CONTROL_TLS=1 el `sdn_url` gRPC del ORR queda https, y su
# GetOrrPath (solo se usa con max_hops>=2 sin orr_path) no monta TLS de
# cliente: para probar cebolla multi-salto, pasa orr_path o CONTROL_TLS=0.
CONTROL_TLS="${DKMS_MESH_CONTROL_TLS:-1}"
SDN_SCHEME=$([ "$CONTROL_TLS" = 1 ] && echo https || echo http)
# Algoritmo de los certificados: ML-DSA-65 (PQC) por defecto, como gen-certs.
# Donde el openssl no sabe generarlos (CESGA, 1.1.1g) se siembran solos de
# certs-pregen/<alg> si existe, sin tener que pedirlo.
KEY_ALG="${KEY_ALG:-ml-dsa-65}"; export KEY_ALG
if [ "${1:-}" = up ] && [ -z "${DKMS_MESH_CERTS_SRC:-}" ] && [ "$KEY_ALG" != rsa ] \
   && ! openssl list -public-key-algorithms 2>/dev/null | grep -qi "ML-DSA"; then
    if [ -d "$REPO/certs-pregen/$KEY_ALG" ]; then
        DKMS_MESH_CERTS_SRC="$REPO/certs-pregen/$KEY_ALG"
        echo "mesh: $(openssl version) no genera $KEY_ALG; certs pre-generados de $DKMS_MESH_CERTS_SRC"
    else
        echo "mesh: FATAL: $(openssl version) no genera $KEY_ALG y no hay $REPO/certs-pregen/$KEY_ALG (KEY_ALG=rsa para clásico, que no es PQC)" >&2
        exit 1
    fi
fi
SIGNDIR=""
R0="${DKMS_MESH_R0:-2000}"
ALPHA="${DKMS_MESH_ALPHA:-0.2}"
DIST_KM="${DKMS_MESH_DIST_KM:-5}"
# Un quditto por arista, en 30xxx: los módulos del nodo n usan 20000+(n-1)*100
# (hasta 29909 con N=100), así que los quditto empiezan justo encima.
# …+9, así que con N=70 llegan a 26909 — el antiguo 21000+idx CHOCABA con los
# puertos del nodo 11 en adelante (el modo qkd nunca había corrido con N>10).
qd_port() { echo $(( 30000 + $1 )); }

# PSK del enlace (a,b), en base64. Determinista y simetrica: `edge_idx` ordena
# los extremos, asi que los dos lados calculan el mismo valor sin coordinarse.
# Es material de prueba, efimero como la malla -- en produccion lo reparte el
# operador fuera de banda.
link_psk() {
    printf 'dkms-mesh-link-psk/%s/%s' "$SEED" "$(edge_idx "$1" "$2")" \
        | sha256sum | cut -d" " -f1 | xxd -r -p | base64 -w0
}

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
    python3 - "$1" "$TOPO" "$SEED" "$DIR/dist.tsv" <<'TOPO_PY' > "$DIR/topology.tsv"
import os, random, sys
n, topo, seed, dist_path = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
edges = set()
# Distancia en km por arista, solo la custom la declara ("a-b:12.5"); el resto
# hereda DKMS_MESH_DIST_KM al construir edges.tsv.
dist = {}

if topo == "custom":
    # Lista explicita de aristas en DKMS_MESH_EDGES: "1-2 2-3 3-10 ...".
    # Para medir topologias concretas (Petersen, C_n puro, contraejemplos)
    # sin inventar un generador por cada una.
    for e in os.environ["DKMS_MESH_EDGES"].split():
        ab, _, d = e.partition(":")
        a, b = sorted(int(x) for x in ab.split("-"))
        assert 1 <= a < b <= n, f"arista fuera de rango: {e}"
        edges.add((a, b))
        if d:
            dist[(a, b)] = float(d)
elif topo == "star":
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
with open(dist_path, "w") as fh:
    for (a, b), d in sorted(dist.items()):
        fh.write("%d\t%d\t%s\n" % (a, b, d))
TOPO_PY
    # Lista plana de aristas con un índice estable: es lo que da su puerto a
    # cada quditto y lo que permite que los dos extremos apunten al mismo.
    # Cuarta columna: la distancia en km de la arista (la suya si la declaró
    # la topología custom, DKMS_MESH_DIST_KM si no) — es lo que recibe su
    # quditto y lo que el QKC anuncia a la SDN para dimensionarla.
    python3 - "$DIR/topology.tsv" "$DIR/dist.tsv" "$DIST_KM" <<'EDGES_PY' > "$DIR/edges.tsv"
import sys
topo, distf, default = sys.argv[1], sys.argv[2], sys.argv[3]
dist = {}
for line in open(distf):
    a, b, d = line.split()
    dist[(int(a), int(b))] = d
idx = 0
for line in open(topo):
    parts = line.rstrip("\n").split("\t")
    a = int(parts[0])
    for v in (parts[1].split() if len(parts) > 1 else []):
        b = int(v)
        print("%d %d %d %s" % (idx, a, b, dist.get((min(a, b), max(a, b)), default)))
        idx += 1
EDGES_PY
}

# Aristas que tocan al nodo n, en las dos direcciones -> "idx vecino".
#
# En qkd las declaran los DOS extremos: el `kme_url` no lo puede suministrar la
# SDN, así que un enlace declarado por un solo lado quedaría a medias — el otro
# QKC lo vería ofrecido por la SDN, no tendría con qué levantarlo, y lo avisaría
# por log. En pqc se sigue declarando por un solo extremo a propósito.
incident_of() {
    awk -v n="$1" '$2 == n { print $1, $3 } $3 == n { print $1, $2 }' "$DIR/edges.tsv"
}

neighbours_of() {   # neighbours_of <n>
    awk -F'\t' -v n="$1" '$1 == n { print $2 }' "$DIR/topology.tsv"
}

# Índice de la arista {a,b} en edges.tsv, que es lo que le da su puerto al
# quditto compartido por los dos extremos.
edge_idx() {
    awk -v a="$1" -v b="$2" \
        '($2 == a && $3 == b) || ($2 == b && $3 == a) { print $1; exit }' "$DIR/edges.tsv"
}

# Distancia en km de la arista {a,b} (cuarta columna de edges.tsv).
edge_dist() {
    awk -v a="$1" -v b="$2" \
        '($2 == a && $3 == b) || ($2 == b && $3 == a) { print $4; exit }' "$DIR/edges.tsv"
}

write_qkc_yml() {   # write_qkc_yml <n> [vecinos...]
    local n=$1; shift
    {
        echo "qkc_id: $n"
        if [ "$CONTROL_TLS" = 1 ]; then
            # https hereda en el anuncio y el push vuelve mTLS; [tls] además
            # enciende los defaults de la Fase 10 (sign + require, A3/A4).
            echo 'sdn_url: "https://127.0.0.1"'
            echo 'control_tls: true'
            echo "certs_dir: \"$DIR/certs\""
        else
            # El renderer pone control_tls (y https) POR DEFECTO desde
            # 2026-09-03: el brazo en claro tiene que pedirlo por escrito.
            echo 'sdn_url: "127.0.0.1"'
            echo 'control_tls: false'
        fi
        echo 'advertise_ip: "127.0.0.1"'
        echo "sdn_announce_secs: 5"
        echo "ports: {peer: $(peer_port "$n"), local: $(local_port "$n"), admin: $(admin_port "$n")}"
        # Firma ML-DSA del handshake PQC (DKMS_MESH_PQC_AUTH=sign): la semilla
        # de este QKC. La clave pública del vecino va en cada enlace, abajo.
        if [ "$PQC_AUTH" = sign ]; then
            echo "sign_secret_seed: \"$(cat "$SIGNDIR/qkc-$n.seed")\""
        fi
        if (( $# > 0 )); then
            echo "links:"
            for nb in "$@"; do
                if [ "$LINK_TYPE" = qkd ]; then
                    # Los dos extremos apuntan al MISMO quditto: uno pedirá
                    # enc_keys y el otro recuperará esas mismas por dec_keys.
                    # r0/alpha/distance no los usa el QKC — se los pasa a la
                    # SDN, que dimensiona la arista con r0·10^(-alpha·d/10).
                    printf '  - {neighbor_id: %s, neighbor_addr: "127.0.0.1:%s", type: qkd, ' \
                        "$nb" "$(peer_port "$nb")"
                    printf 'kme_url: "http://127.0.0.1:%s", r0: %s, alpha: %s, distance_km: %s' \
                        "$(qd_port "$(edge_idx "$n" "$nb")")" "$R0" "$ALPHA" "$(edge_dist "$n" "$nb")"
                    if [ "$FRAME_AUTH" != off ]; then
                        printf ', link_psk: "%s", frame_auth: %s' \
                            "$(link_psk "$n" "$nb")" "$FRAME_AUTH"
                    fi
                    printf '}\n'
                else
                    # Firma ML-DSA del handshake (solo enlaces PQC: los QKD no
                    # negocian ML-KEM, su material lo da el KME). Cada extremo
                    # firma con SU semilla y verifica con la clave pública del
                    # vecino, así que solo se comparten claves públicas.
                    local auth=""
                    if [ "$PQC_AUTH" = sign ]; then
                        auth=", pqc_auth: sign, peer_verify_key: \"$(cat "$SIGNDIR/qkc-$nb.vk")\""
                    fi
                    if [ -n "${DKMS_MESH_PQC_REKEY_SECS:-}" ]; then
                        auth="$auth, pqc_rekey_secs: $DKMS_MESH_PQC_REKEY_SECS"
                    fi
                    if [ "$FRAME_AUTH" != off ]; then
                        auth="$auth, link_psk: \"$(link_psk "$n" "$nb")\", frame_auth: $FRAME_AUTH"
                    fi
                    if [ -n "${DKMS_MESH_PQC_CAP:-}" ]; then
                        # Capacidad PQC declarada; sin la variable se prueba el
                        # default de la SDN (10 000 claves/s).
                        echo "  - {neighbor_id: $nb, type: pqc, capacity_keys_per_s: $DKMS_MESH_PQC_CAP$auth}"
                    else
                        echo "  - {neighbor_id: $nb, type: pqc$auth}"
                    fi
                fi
            done
        fi
    } > "$DIR/yml/node$n.qkc.yml"
    python3 "$RENDER" qkc "$DIR/yml/node$n.qkc.yml" "$DIR/cfg/qkc$n" >/dev/null
}

# Levanta los topes de medida sobre el TOML ya renderizado.
#
# Las claves de secciones que el render YA emite ([generator], [southbound],
# [sae]) se INSERTAN tras su cabecera, no se anexan al final: en TOML una
# línea suelta al final caería dentro de la última sección del fichero, y una
# cabecera repetida es un error de parseo ("Cannot declare twice") que mata al
# DKMS al cargar — pasó con [sae], que el render emite siempre desde que
# enforce_authorization es explícito. Solo una tabla que el render NO escribe
# ([transport_e2e]) puede añadirse entera al final.
patch_dkms_toml() {
    local toml=$1
    if [ -n "$TOKENS_PER_TICK" ]; then
        sed -i "/^\[generator\]/a max_tokens_per_peer_per_tick = $TOKENS_PER_TICK" "$toml"
    fi
    # Brazo cebolla: max_hops != 0 mete el bootstrap ORR↔ORR y sus épocas en
    # el camino de datos, que es lo que hay que ejercitar para ver la
    # rotación del master_secret con tráfico real encima.
    if [ -n "${DKMS_MESH_MAX_HOPS:-}" ]; then
        sed -i "/^\[southbound\]/a default_max_hops = $DKMS_MESH_MAX_HOPS" "$toml"
    fi
    # Rotación de la época e2e DKMS↔DKMS (default 3600 s). Tabla nueva al final:
    # es una cabecera, así que no cae dentro de la anterior.
    if [ -n "${DKMS_MESH_E2E_REKEY_SECS:-}" ]; then
        printf '\n[transport_e2e]\nrekey_secs = %s\n' "$DKMS_MESH_E2E_REKEY_SECS" >> "$toml"
    fi
    if [ -n "$SAE_MIN_TOKENS" ]; then
        sed -i "/^\[sae\]/a min_capacity_tokens = $SAE_MIN_TOKENS" "$toml"
    fi
}

generate() {        # generate <N>
    local total=$1
    rm -rf "$DIR"; mkdir -p "$DIR"/{yml,cfg/sdn,logs,certs}
    # Identidades de firma ML-DSA (una por QKC y una por ORR) antes de escribir
    # ningún node.yml: las semillas y las claves públicas se referencian ahí.
    SIGNDIR="$DIR/signkeys"
    if [ "$PQC_AUTH" = sign ]; then
        local kg="$REPO/target/release/pqc_keygen"
        [ -x "$kg" ] || kg="$REPO/target/debug/pqc_keygen"
        [ -x "$kg" ] || { echo "mesh: FATAL: falta pqc_keygen (cargo build --release)" >&2; exit 1; }
        "$kg" --out "$SIGNDIR" --nodes "$total" >/dev/null || {
            echo "mesh: FATAL: pqc_keygen falló" >&2; exit 1; }
        echo "mesh: firma ML-DSA activa (handshake QKC + bootstrap ORR)"
    fi
    build_topology "$total"
    printf 'listen_ip: "0.0.0.0"\npresence_ttl_secs: 90\n' > "$DIR/yml/node.sdn.yml"
    if [ "$CONTROL_TLS" = 1 ]; then
        printf 'control_tls: true\ncerts_dir: "%s"\n' "$DIR/certs" >> "$DIR/yml/node.sdn.yml"
    else
        # Default del renderer desde 2026-09-03: el brazo en claro lo pide.
        printf 'control_tls: false\n' >> "$DIR/yml/node.sdn.yml"
    fi
    python3 "$RENDER" sdn "$DIR/yml/node.sdn.yml" "$DIR/cfg/sdn" >/dev/null

    for n in $(seq 1 "$total"); do
        mkdir -p "$DIR/cfg/qkc$n" "$DIR/cfg/orr$n" "$DIR/cfg/dkms$n"
        # shellcheck disable=SC2046
        if [ "$LINK_TYPE" = qkd ] || [ "$PQC_AUTH" = sign ] || [ "$FRAME_AUTH" != off ]; then
            # Con firma ML-DSA los declaran los DOS extremos, por la misma razón
            # que en qkd: la SDN no puede suministrar la clave de verificación
            # del vecino (no la conoce, y no debe repartir material de
            # identidad), así que un enlace declarado por un solo lado dejaría
            # al otro extremo sin con qué verificar — medido: 423 descartes por
            # "sin peer_verify_key" y 436 por "firma inválida" (el enlace que
            # crea la SDN heredaba la clave de OTRO vecino).
            #
            # `frame_auth` está en el mismo saco y por el mismo motivo: la raíz
            # del MAC es la `link_psk`, que es config local. Declarado por un
            # solo lado, el extremo que recibe el enlace de la SDN se queda sin
            # PSK y descarta todo lo que llega — medido aquí el 2026-08-28, el
            # nodo 4 tirando todos los frames del 1 y del 3 con "frame sin MAC
            # en un enlace autenticado".
            write_qkc_yml "$n" $(incident_of "$n" | awk '{print $2}')
        else
            write_qkc_yml "$n" $(neighbours_of "$n")
        fi

        cat > "$DIR/yml/node$n.orr.yml" <<EOF
orr_id: "orr_$n"
qkc_id: $n
qkc_addr: "127.0.0.1:$(local_port "$n")"
sdn_url: "$SDN_SCHEME://127.0.0.1:$SDN_GRPC"
advertise_ip: "127.0.0.1"
sdn_announce_secs: 5
ports: {grpc: $(orr_port "$n"), metrics: $(port "$n" 20004)}
EOF
        if [ "$GRPC_TLS" = 1 ]; then
            printf 'grpc_tls: true\ncerts_dir: "%s"\n' "$DIR/certs" >> "$DIR/yml/node$n.orr.yml"
        else
            printf 'grpc_tls: false\n' >> "$DIR/yml/node$n.orr.yml"
        fi
        # Firma ML-DSA del bootstrap: el ORR firma la pubkey ML-KEM que anuncia
        # en GetPublicKey, y sus pares la verifican con la clave pública de
        # aquí. Cierra el MITM del bootstrap aunque la identidad ML-KEM siga
        # siendo efímera (se regenera en cada arranque).
        if [ "$PQC_AUTH" = sign ]; then
            {
                echo "sign_secret_seed: \"$(cat "$SIGNDIR/orr_$n.seed")\""
                echo "bootstrap_trust: strict"
                echo "peer_verify_keys:"
                for m in $(seq 1 "$total"); do
                    [ "$m" = "$n" ] && continue
                    echo "  orr_$m: \"$(cat "$SIGNDIR/orr_$m.vk")\""
                done
            } >> "$DIR/yml/node$n.orr.yml"
        fi
        # Ancla de confianza del bootstrap sin repartir claves: con
        # DKMS_MESH_BOOTSTRAP_TRUST=strict el ORR exige anuncios firmados con
        # el cert de nodo (cadena hasta net-ca + SAN), que es lo que firma por
        # defecto con [tls]. Es la prueba de que strict funciona sin config
        # por par. (El brazo `sign` ya lo pone junto a sus verify keys.)
        if [ "$PQC_AUTH" != sign ] && [ -n "${DKMS_MESH_BOOTSTRAP_TRUST:-}" ]; then
            printf 'bootstrap_trust: %s\n' "$DKMS_MESH_BOOTSTRAP_TRUST" >> "$DIR/yml/node$n.orr.yml"
        fi
        python3 "$RENDER" orr "$DIR/yml/node$n.orr.yml" "$DIR/cfg/orr$n" >/dev/null
        # Periodo de rotación del master_secret (default 1 h): para ver varias
        # rotaciones en una sesión corta. Se inserta tras `orr_id`, arriba del
        # todo — el fichero acaba en [tls] y una clave suelta al final caería
        # dentro de esa tabla.
        if [ -n "${DKMS_MESH_ROTATION_MS:-}" ]; then
            sed -i "/^orr_id = /a rotation_period_ms = $DKMS_MESH_ROTATION_MS" "$DIR/cfg/orr$n/default.toml"
        fi

        cat > "$DIR/yml/node$n.dkms.yml" <<EOF
node_id: "dkms-$n"
advertise_ip: "127.0.0.1"
orr_addr: "127.0.0.1:$(orr_port "$n")"
sdn_endpoint: "$SDN_SCHEME://127.0.0.1:$SDN_GRPC"
orr_id: "orr_$n"
orr_tls: $([ "$GRPC_TLS" = 1 ] && echo true || echo false)
sdn_announce_secs: 5
certs_dir: "$DIR/certs"
ports: {sae: $(sae_port "$n"), peer: $(port "$n" 20006), grpc: $(port "$n" 20007), metrics: $(port "$n" 20008), ack: $(port "$n" 20009)}
sae_bindings:
  sae_$n: dkms-$n
EOF
        # ACK autenticados por ETSI-020 y, con todos los nodos así, el socket
        # TCP plano ni se escucha: el brazo que decide el flip de defaults.
        if [ -n "${DKMS_MESH_ACK_TRANSPORT:-}" ]; then
            printf 'ack_transport: %s\n' "$DKMS_MESH_ACK_TRANSPORT" >> "$DIR/yml/node$n.dkms.yml"
            if [ "$DKMS_MESH_ACK_TRANSPORT" = etsi020 ]; then
                printf 'ack_socket_listen: false\n' >> "$DIR/yml/node$n.dkms.yml"
            fi
        fi
        python3 "$RENDER" dkms "$DIR/yml/node$n.dkms.yml" "$DIR/cfg/dkms$n" >/dev/null
        patch_dkms_toml "$DIR/cfg/dkms$n/default.toml"

        # Una sola CA para toda la malla: el mTLS entre DKMS exige raíz común.
        # DKMS_MESH_CERT_IP: IP extra en el SAN del cert de servidor (gen-certs
        # añade siempre IP:127.0.0.1). Para clientes SAE externos a la máquina,
        # p.ej. contenedores strongSwan que llegan por el gateway del bridge.
        #
        # DKMS_MESH_CERTS_SRC salta la generación y copia un juego ya hecho
        # (ver más abajo): en máquinas con OpenSSL < 3.5 —CESGA tiene 1.1.1g—
        # `genpkey -algorithm ML-DSA-65` no existe, así que los certs
        # post-cuánticos se generan fuera y se traen con el job.
        if [ -z "${DKMS_MESH_CERTS_SRC:-}" ]; then
            bash "$GENCERTS" "dkms-$n" "${DKMS_MESH_CERT_IP:-127.0.0.1}" "$DIR/certs" >/dev/null 2>&1
            bash "$GENCERTS" --sae "sae_$n" "$DIR/certs" >/dev/null 2>&1
            [ "$GRPC_TLS" = 1 ] && bash "$GENCERTS" "orr_$n" "${DKMS_MESH_CERT_IP:-127.0.0.1}" "$DIR/certs" >/dev/null 2>&1
            # Fase 10: el [tls] del QKC es lo que enciende sign+require por
            # defecto; su SAN dkms://qkc-N es lo que verifica el handshake.
            [ "$CONTROL_TLS" = 1 ] && bash "$GENCERTS" "qkc-$n" "${DKMS_MESH_CERT_IP:-127.0.0.1}" "$DIR/certs" >/dev/null 2>&1
        fi
    done
    # El cert `sdn` autoriza el push de forwarding en el admin mTLS del QKC
    # (B1b) y sirve el HTTP/gRPC de la SDN.
    if [ -z "${DKMS_MESH_CERTS_SRC:-}" ] && [ "$CONTROL_TLS" = 1 ]; then
        bash "$GENCERTS" sdn "${DKMS_MESH_CERT_IP:-127.0.0.1}" "$DIR/certs" >/dev/null 2>&1
    fi
    # Certs pre-generados: se copian enteros (CAs + hojas). Se valida que estén
    # los de TODOS los nodos, porque una malla a la que le falte un cert
    # arranca igual y falla mucho después, en el primer handshake.
    if [ -n "${DKMS_MESH_CERTS_SRC:-}" ]; then
        cp "$DKMS_MESH_CERTS_SRC"/*.crt "$DKMS_MESH_CERTS_SRC"/*.key "$DIR/certs/" 2>/dev/null
        local faltan=0 n2
        for n2 in $(seq 1 "$total"); do
            [ -s "$DIR/certs/dkms-$n2.crt" ] || faltan=$((faltan+1))
            [ -s "$DIR/certs/sae_$n2.crt" ] || faltan=$((faltan+1))
            if [ "$GRPC_TLS" = 1 ]; then
                [ -s "$DIR/certs/orr_$n2.crt" ] || faltan=$((faltan+1))
            fi
            if [ "$CONTROL_TLS" = 1 ]; then
                [ -s "$DIR/certs/qkc-$n2.crt" ] || faltan=$((faltan+1))
            fi
        done
        if [ "$CONTROL_TLS" = 1 ]; then
            [ -s "$DIR/certs/sdn.crt" ] || faltan=$((faltan+1))
        fi
        if [ "$faltan" -gt 0 ] || [ ! -s "$DIR/certs/net-ca.crt" ] || [ ! -s "$DIR/certs/sae-ca.crt" ]; then
            echo "mesh: FATAL: DKMS_MESH_CERTS_SRC=$DKMS_MESH_CERTS_SRC no cubre N=$total ($faltan ficheros de DKMS/ORR/SAE ausentes)" >&2
            exit 1
        fi
        # Y que las hojas cuelguen DE ESTA CA y no de otra copia con el mismo
        # nombre: openssl 1.1.1 no verifica firmas ML-DSA, pero sí imprime los
        # key ids. Pasó (CESGA 2026-08-28): orr_N firmados por la net-ca local
        # sobre un juego cuya net-ca era otra, y el DKMS no validó al ORR — sin
        # un solo error que lo dijera, sólo "transport error" y 0/90 llenos.
        local pair leaf ca
        for pair in dkms-1:net-ca sae_1:sae-ca $([ "$GRPC_TLS" = 1 ] && echo orr_1:net-ca) \
                $([ "$CONTROL_TLS" = 1 ] && echo qkc-1:net-ca sdn:net-ca); do
            leaf=${pair%%:*}; ca=${pair##*:}
            if [ "$(cert_keyid "$DIR/certs/$leaf.crt" Authority)" != "$(cert_keyid "$DIR/certs/$ca.crt" Subject)" ]; then
                echo "mesh: FATAL: $leaf.crt no está firmado por el $ca.crt de $DKMS_MESH_CERTS_SRC (es otra copia de la CA): regenera el juego entero" >&2
                exit 1
            fi
        done
        echo "mesh: certs pre-generados desde $DKMS_MESH_CERTS_SRC ($(/bin/ls "$DIR"/certs/*.crt | wc -l) certs, firma: $(openssl x509 -in "$DIR/certs/dkms-1.crt" -noout -text 2>/dev/null | grep -m1 'Signature Algorithm' | sed 's/.*: *//'))"
    fi
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
# Descriptores: con N=100 un DKMS tiene ~100 peers + ~100 SAEs conectados y
# la SDN 300 anunciantes; el 1024 por defecto no llega.
ulimit -n "\$(ulimit -Hn)" 2>/dev/null || ulimit -n 65536 2>/dev/null || true
: > "$DIR/logs/starts.tsv"
mark() { printf '%s\\t%s\\n' "\$1" "\$(date +%s.%N)" >> "$DIR/logs/starts.tsv"; }
mark sdn
CONFIG_DIR="$DIR/cfg/sdn" nohup "$BIN/sdn" > "$DIR/logs/sdn.log" 2>&1 &
echo \$! > "$DIR/logs/sdn.pid"
# Un quditto por arista, y antes que los QKC: si el KME no está escuchando
# cuando el QKC intenta su primer refill, el enlace arranca con errores que
# luego hay que distinguir de los de verdad.
QD_LINES='__QD_LINES__'
if [ -n "\$QD_LINES" ]; then
  while read -r idx a b dist; do
    [ -n "\$idx" ] || continue
    mark "quditto\$idx"
    # QUDITTO_TLS=off explícito: aquí quditto y sus dos QKCs comparten
    # 127.0.0.1 (el caso sidecar documentado); el default del binario es mTLS.
    # La distancia es la de ESA arista (edges.tsv), no una global.
    QUDITTO_TLS=off nohup "$BIN/quditto" --listen "127.0.0.1:\$((30000 + idx))" \
        --r0 __R0__ --alpha __ALPHA__ --distance "\${dist:-__DIST__}" \
        > "$DIR/logs/quditto\$idx.log" 2>&1 &
    echo \$! > "$DIR/logs/quditto\$idx.pid"
  done <<< "\$QD_LINES"
  sleep 2
fi
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
    if [ "$LINK_TYPE" = qkd ]; then
        python3 - "$DIR/boot.sh" "$DIR/edges.tsv" "$R0" "$ALPHA" "$DIST_KM" <<'PATCH_PY'
import io, sys
boot, edges, r0, alpha, dist = sys.argv[1:6]
lines = io.open(edges).read().strip()
s = io.open(boot).read()
s = s.replace("__QD_LINES__", lines).replace("__R0__", r0)
s = s.replace("__ALPHA__", alpha).replace("__DIST__", dist)
io.open(boot, "w").write(s)
PATCH_PY
    else
        sed -i "s/QD_LINES='__QD_LINES__'/QD_LINES=''/" "$DIR/boot.sh"
    fi
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

# curl al HTTP admin de la SDN respetando CONTROL_TLS: con el plano de
# control en mTLS hay que presentar un cert de la net-ca (vale el de la
# propia SDN, que la malla ya tiene en $DIR/certs).
sdn_curl() { # sdn_curl <path> [curl args...]
    local path=$1; shift
    if [ "$CONTROL_TLS" = 1 ]; then
        curl -s --cacert "$DIR/certs/net-ca.crt" --cert "$DIR/certs/sdn.crt" \
             --key "$DIR/certs/sdn.key" "$@" "https://127.0.0.1:$SDN_HTTP$path"
    else
        curl -s "$@" "http://127.0.0.1:$SDN_HTTP$path"
    fi
}
topology_json() { sdn_curl /topology --max-time 5; }

cmd_up() {
    local total=${1:-3}
    [ -n "${2:-}" ] && TOPO="$2"
    case "$TOPO" in ring|star|random) ;;
                    custom) [ -n "${DKMS_MESH_EDGES:-}" ] || die "topología custom sin DKMS_MESH_EDGES" ;;
                    *) die "topología desconocida: $TOPO (ring|star|random|custom)" ;; esac
    case "$LINK_TYPE" in pqc|qkd) ;; *) die "DKMS_MESH_LINK_TYPE debe ser pqc o qkd" ;; esac
    # Los cinco, no sólo la SDN: faltando uno la malla arranca igual y lo que
    # se mide son once millones de ConnectionRefused.
    local needed=(sdn qkc orr dkms)
    [ "$LINK_TYPE" = qkd ] && needed+=(quditto)
    for b in "${needed[@]}"; do
        [ -x "$BIN/$b" ] || die "falta target/release/$b — cargo build --release"
    done
    # Tope 100: los puertos de nodo (20000+100(n-1)+9 ≤ 29909) deben quedar
    # bajo los 30xxx de los quditto. El límite de verdad lo pone la memoria.
    (( total >= 2 && total <= 100 )) || die "N entre 2 y 100 (puertos 20000+100n < 30000)"
    echo "mesh: generando $total nodos en $DIR  (topología: $TOPO$([ "$TOPO" = random ] && echo ", semilla $SEED"), enlaces $LINK_TYPE)"
    if [ "$LINK_TYPE" = qkd ]; then
        # cap = R0·10^(-alpha·d/10) es lo que la SDN usará como capacidad de
        # arista. Conviene tenerlo delante: NO es R0, y confundirlos ha costado
        # más de un "déficit contra el teórico" que no lo era.
        local cap
        cap=$(python3 -c "print(f'{$R0 * 10 ** (-$ALPHA * $DIST_KM / 10):.1f}')")
        echo "mesh: R0=$R0 alpha=$ALPHA d=${DIST_KM}km  ->  capacidad por arista $cap claves/s"
    fi
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
    sdn_curl /healthz --retry 40 --retry-delay 1 --retry-connrefused \
        >/dev/null || die "la SDN no arrancó"
    local want=$(( total * 3 ))
    if wait_for $(( 120 + 6 * total )) "topology_json | python3 -c \"import sys,json;t=json.load(sys.stdin);print(str(t['qkcs']+t['orrs']+t['dkms']==$want).lower())\""; then
        echo "mesh: $want módulos registrados"
    else
        echo "mesh: AVISO — la topología no convergió a $want módulos" >&2
    fi
    cmd_edges
}

cmd_down() {
    systemctl --user stop "$SCOPE.scope" >/dev/null 2>&1
    pkill -f "$BIN/quditto --listen 127.0.0.1:30" 2>/dev/null
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
    sdn_curl /links --max-time 5 | python3 -c '
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
