#!/usr/bin/env bash
# Una configuración de la campaña de escala: <topología> <N> <régimen>.
#
#   topología   ringchords (el `ring` de mesh.sh: anillo + cuerdas)
#               cn         (ciclo puro C_N — edge-transitivo a todo N)
#               bridge2    (dos comunidades ringchords de N/2 + un puente:
#                           regular de facto y máximamente desigual)
#   régimen     ninguna — cero tráfico SAE: mide el camino del GENERADOR solo
#                         (DKMS→ORR→QKC→ORR→DKMS), que es donde vive el sello
#                         extremo a extremo. Al cerrar la observación hace UNA
#                         ronda de `keys` como prueba funcional, ya sin medir.
#               poca   — goteo: una ronda de keys sobre 6 nodos cada ~3 s
#               media  — sae_load con 2 hilos por maestro
#               mucha  — sae_load con 9 hilos por maestro (+ recuperación)
#
# Mide el LLENADO bajo el régimen: la malla arranca con buffers vacíos y la
# carga corre desde el primer segundo. La observación se capa (C_N grande no
# llega al 100 % ni en la noche: la cuota teórica de C70 es 1.3 claves/s), y
# el análisis usa pendientes y bandas, no solo tiempos-a-tope.
#
# Los logs de dkms* y sdn se archivan en DKMS_SCALE_OUT/<topo>-n<N>-<reg>/.
set -uo pipefail

TOPO_FAM=${1:?ringchords|cn|bridge2}
N=${2:?nodos}
REG=${3:?ninguna|poca|media|mucha}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
MESH="$HERE/mesh.sh"
LOADER="$REPO/tests/testbed/sae_load.py"
# Cliente de carga: se prefiere el binario Rust (tests/loadgen). El de Python
# usa el `ssl` del sistema, y con OpenSSL < 3.5 —CESGA tiene 1.1.1g— NO puede
# cargar un cert de cliente ML-DSA: el arm de certs post-cuánticos sería
# imposible de medir. El binario habla mTLS con RSA y con ML-DSA por igual.
LOADER_BIN="$REPO/target/release/sae_load"
OUTBASE="${DKMS_SCALE_OUT:-$REPO/tests/results/scale}"
OUT="$OUTBASE/$TOPO_FAM-n$N-$REG"
mkdir -p "$OUT"

export DKMS_MESH_DIR="${DKMS_MESH_DIR:-${TMPDIR:-/tmp}/dkms-scale-mesh}"
# El tipo de enlace es override-able: la campaña de certificados necesita
# enlaces PQC, porque la firma ML-DSA del handshake solo existe ahí (un enlace
# QKD no negocia ML-KEM, su material lo entrega el KME).
export DKMS_MESH_LINK_TYPE="${DKMS_MESH_LINK_TYPE:-qkd}"
export DKMS_MESH_TOPO=custom

# ── aristas de la familia a este N ─────────────────────────────────────────
edges_of() {
    python3 - "$TOPO_FAM" "$N" <<'PY'
import sys
fam, n = sys.argv[1], int(sys.argv[2])
def ringchords(base, n):
    # Réplica del generador `ring` de mesh.sh: anillo + cuerda cada 3 nodos
    # hacia el opuesto. `base` desplaza los ids (para las dos comunidades).
    es = set()
    for k in range(1, n + 1):
        a, b = k, k % n + 1
        es.add((base + min(a, b), base + max(a, b)))
    for k in range(1, n + 1, 3):
        opp = (k - 1 + n // 2) % n + 1
        if n >= 6 and opp != k:
            es.add((base + min(k, opp), base + max(k, opp)))
    return es
if fam == "cn":
    es = {(k, k % n + 1) for k in range(1, n + 1)}
    es = {(min(a, b), max(a, b)) for a, b in es}
elif fam == "ringchords":
    es = ringchords(0, n)
elif fam == "bridge2":
    h = n // 2
    es = ringchords(0, h) | ringchords(h, h) | {(1, h + 1)}
else:
    raise SystemExit(f"familia desconocida: {fam}")
print(" ".join(f"{a}-{b}" for a, b in sorted(es)))
PY
}
DKMS_MESH_EDGES=$(edges_of)
export DKMS_MESH_EDGES
echo "== $TOPO_FAM n=$N $REG: $(echo "$DKMS_MESH_EDGES" | wc -w) aristas"
# El algoritmo de firma de los certs lo hereda mesh.sh -> gen-certs.sh por env.
echo "== certs: KEY_ALG=${KEY_ALG:-ml-dsa-65} (loader: $([ -x "$LOADER_BIN" ] && echo rust || echo python))"

# ── caps de observación ────────────────────────────────────────────────────
CAP=600
[ "$N" -ge 40 ] && CAP=900
[ "$TOPO_FAM" = cn ] && [ "$N" -ge 40 ] && CAP=1200
# Para ensayos: DKMS_SCALE_CAP pisa el cap de observación.
[ -n "${DKMS_SCALE_CAP:-}" ] && CAP=$DKMS_SCALE_CAP
RECOVERY_CAP=300

# ── carga por régimen ──────────────────────────────────────────────────────
LOAD_PIDS=()
start_load() {
    case "$REG" in
    ninguna)
        # A propósito, nada. El generador llena los buffers sin que ningún SAE
        # pida: es el único régimen en el que lo que se mide es SOLO la cadena
        # de transporte, sin la ruta de servicio compitiendo por la CPU.
        ;;
    poca)
        # 6 nodos repartidos por el anillo; una ronda cada ~3 s.
        local nodes=()
        for i in 0 1 2 3 4 5; do nodes+=( $(( 1 + i * N / 6 )) ); done
        # La salida se ARCHIVA, no se tira: `mesh.sh keys` es lo único de toda
        # la campaña que comprueba que los dos extremos de un intercambio
        # ETSI-014 se llevan la MISMA clave, byte a byte. Mandarla a /dev/null
        # dejaba la prueba funcional más fuerte sin registrar, y el job sólo
        # reportaba "N/90 llenos", que dice que hay material, no que sea el
        # correcto.
        ( while [ -f "$OUT/.loading" ]; do
              "$MESH" keys "${nodes[@]}" >> "$OUT/keys.log" 2>&1
              sleep 3
          done ) &
        LOAD_PIDS+=($!)
        ;;
    media|mucha)
        local th=2; [ "$REG" = mucha ] && th=9
        local dur=$(( CAP + 120 ))
        for m in $(seq 1 "$N"); do
            local slaves=""
            for s2 in $(seq 1 "$N"); do
                [ "$s2" != "$m" ] && slaves="$slaves,sae_$s2"
            done
            slaves=${slaves#,}
            if [ -x "$LOADER_BIN" ]; then
                "$LOADER_BIN" \
                    --sae "sae_$m" --slaves "$slaves" \
                    --certs "$DKMS_MESH_DIR/certs" --host 127.0.0.1 \
                    --port $(( 20005 + (m - 1) * 100 )) \
                    --threads "$th" --duration "$dur" --number 1 --size 256 \
                    --aggregate-throttled --out "$OUT/load.sae_$m.csv" \
                    > "$OUT/load.sae_$m.err" 2>&1 &
            else
                python3 -u "$LOADER" \
                    --sae "sae_$m" --slaves "$slaves" \
                    --certs "$DKMS_MESH_DIR/certs" --host 127.0.0.1 \
                    --port $(( 20005 + (m - 1) * 100 )) \
                    --threads "$th" --duration "$dur" --number 1 --size 256 \
                    --aggregate-throttled --out "$OUT/load.sae_$m.csv" \
                    > "$OUT/load.sae_$m.err" 2>&1 &
            fi
            LOAD_PIDS+=($!)
        done
        ;;
    esac
}
# La carga tiene que ARRANCAR, y hay que comprobarlo: el 2026-08-26 cuatro
# configuraciones se archivaron enteras con los .err llenos de SyntaxError
# (sae_load.py pedía Python>=3.7 y el job hermético usa el 3.6 del sistema),
# y sus curvas pasaron por "medidas bajo carga" siendo llenados en vacío.
# Mejor morir aquí que producir un dato que miente.
check_load() {
    case "$REG" in ninguna|poca) return 0 ;; esac
    sleep 20
    local vivos=0 p
    for p in "${LOAD_PIDS[@]:-}"; do kill -0 "$p" 2>/dev/null && vivos=$((vivos+1)); done
    local errs
    errs=$(grep -lE "SyntaxError|Traceback|error:" "$OUT"/load.sae_*.err 2>/dev/null | wc -l)
    echo "== carga: $vivos/${#LOAD_PIDS[@]} procesos vivos, $errs con error"
    if [ "$vivos" -lt $(( ${#LOAD_PIDS[@]} / 2 )) ] || [ "$errs" -gt 0 ]; then
        echo "== FATAL: la carga no arrancó (ver $OUT/load.sae_*.err)"
        head -4 "$OUT"/load.sae_1.err 2>/dev/null
        stop_load; "$MESH" down >/dev/null 2>&1
        exit 2
    fi
}

stop_load() {
    rm -f "$OUT/.loading"
    for p in "${LOAD_PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
    wait 2>/dev/null
}

full_count() {
    local ok=0 c
    for i in $(seq 1 "$N"); do
        c=$(sed 's/\x1b\[[0-9;]*m//g' "$DKMS_MESH_DIR/logs/dkms$i.log" 2>/dev/null \
            | grep -a generator.state | tail -$(( N - 1 )) | grep -c 'enc=4096')
        ok=$(( ok + c ))
    done
    echo "$ok"
}

# ── la medición ────────────────────────────────────────────────────────────
"$MESH" down >/dev/null 2>&1
T_UP0=$SECONDS
"$MESH" up "$N" custom > "$OUT/up.log" 2>&1 || { echo "== FALLO al subir"; cat "$OUT/up.log" | tail -5; exit 1; }
echo "== malla arriba en $(( SECONDS - T_UP0 ))s"
touch "$OUT/.loading"
start_load
check_load
TOTAL=$(( N * (N - 1) ))
DEADLINE=$(( SECONDS + CAP ))
OK=0
while (( SECONDS < DEADLINE )); do
    OK=$(full_count)
    (( OK >= TOTAL )) && break
    sleep 15
done
echo "== observación cerrada: $OK/$TOTAL llenos"
stop_load
# Sin tráfico durante la medida, la comprobación de que el material entregado
# es el BUENO tiene que hacerse en algún momento: una ronda al final, ya fuera
# de la ventana. Sin esto el arm sólo diría "llenó", que no es lo mismo que
# "los dos extremos se llevan la misma clave".
if [ "$REG" = ninguna ]; then
    nodes=()
    for i in 0 1 2 3 4 5; do nodes+=( $(( 1 + i * N / 6 )) ); done
    echo "== prueba funcional final: mesh.sh keys ${nodes[*]}"
    "$MESH" keys "${nodes[@]}" > "$OUT/keys.log" 2>&1
    tail -1 "$OUT/keys.log"
fi
if [ "$REG" = mucha ]; then
    DEADLINE=$(( SECONDS + RECOVERY_CAP ))
    while (( SECONDS < DEADLINE )); do
        OK=$(full_count)
        (( OK >= TOTAL )) && break
        sleep 15
    done
    echo "== recuperación: $OK/$TOTAL"
fi
# La cuota del NetApp parpadea tras la avería del 2026-08-26 (EDQUOT
# intermitente estando al 48 %): archivar con reintentos, que un parpadeo no
# le robe el resultado a un job de 40 minutos.
for intento in $(seq 1 12); do
    cp "$DKMS_MESH_DIR"/logs/dkms*.log "$DKMS_MESH_DIR"/logs/sdn.log "$OUT/" 2>/dev/null
    # qkc y orr también: son los que registran el handshake firmado y el
    # bootstrap. Sin ellos, un fallo de firma solo se ve como "no llenó".
    cp "$DKMS_MESH_DIR"/logs/qkc1.log "$DKMS_MESH_DIR"/logs/qkc2.log \
       "$DKMS_MESH_DIR"/logs/orr1.log "$DKMS_MESH_DIR"/logs/orr2.log "$OUT/" 2>/dev/null
    cp "$DKMS_MESH_DIR"/edges.tsv "$OUT/" 2>/dev/null
    n_src=$(ls "$DKMS_MESH_DIR"/logs/dkms*.log 2>/dev/null | wc -l)
    n_dst=$(ls "$OUT"/dkms*.log 2>/dev/null | wc -l)
    [ "$n_dst" -ge "$n_src" ] && [ -s "$OUT/dkms1.log" ] && break
    echo "== archivado incompleto ($n_dst/$n_src, intento $intento); reintento en 30 s"
    sleep 30
done
# `key_alg` y `loader` son la atribución del arm: sin ellos, dos celdas con el
# mismo topo/n/régimen son indistinguibles y la comparación RSA vs ML-DSA no
# se puede reconstruir a posteriori. `cert_sig` se lee del cert emitido, que
# es la verdad sobre el terreno (no lo que se pidió por env).
# Contadores del sello extremo a extremo (dkms/src/e2e.rs), agregados sobre el
# ÚLTIMO `generator.state` de cada par: son la prueba de que el camino nuevo no
# está descartando material en silencio. `epochs_none` sostenido significa que
# el acuerdo de clave no cuaja; los otros tres deberían ser 0.
e2e_sum() {
    local field=$1 total=0 v
    for i in $(seq 1 "$N"); do
        v=$(sed 's/\x1b\[[0-9;]*m//g' "$DKMS_MESH_DIR/logs/dkms$i.log" 2>/dev/null \
            | grep -a generator.state | tail -$(( N - 1 )) \
            | grep -o "$field=[0-9]*" | cut -d= -f2 | awk '{t+=$1} END{print t+0}')
        total=$(( total + ${v:-0} ))
    done
    echo "$total"
}
E2E_CORRUPT=$(e2e_sum recv_corrupt); E2E_NOEPOCH=$(e2e_sum recv_no_epoch); E2E_REPLAY=$(e2e_sum recv_replayed)
E2E_NONE=0
for i in $(seq 1 "$N"); do
    c=$(sed 's/\x1b\[[0-9;]*m//g' "$DKMS_MESH_DIR/logs/dkms$i.log" 2>/dev/null \
        | grep -a generator.state | tail -$(( N - 1 )) | grep -c 'e2e_epoch="none"')
    E2E_NONE=$(( E2E_NONE + c ))
done
echo "== e2e: recv_corrupt=$E2E_CORRUPT recv_no_epoch=$E2E_NOEPOCH recv_replayed=$E2E_REPLAY pares_sin_epoca=$E2E_NONE"

CERT_SIG=$(openssl x509 -in "$DKMS_MESH_DIR/certs/dkms-1.crt" -noout -text 2>/dev/null \
    | grep -m1 'Signature Algorithm' | sed 's/.*: *//' | tr -d ' ')
LOADER_USED=$([ -x "$LOADER_BIN" ] && echo rust || echo python)
printf '{"topo":"%s","n":%d,"regimen":"%s","cap_s":%d,"llenos":%d,"total":%d,"key_alg":"%s","cert_sig":"%s","loader":"%s","e2e":{"recv_corrupt":%d,"recv_no_epoch":%d,"recv_replayed":%d,"pares_sin_epoca":%d}}\n' \
    "$TOPO_FAM" "$N" "$REG" "$CAP" "$OK" "$TOTAL" \
    "${KEY_ALG:-ml-dsa-65}" "${CERT_SIG:-desconocido}" "$LOADER_USED" \
    "$E2E_CORRUPT" "$E2E_NOEPOCH" "$E2E_REPLAY" "$E2E_NONE" > "$OUT/meta.json"
"$MESH" down >/dev/null 2>&1
echo "== $TOPO_FAM n=$N $REG: archivado en $OUT"
