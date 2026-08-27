#!/usr/bin/env bash
# Una configuración de la campaña de escala: <topología> <N> <régimen>.
#
#   topología   ringchords (el `ring` de mesh.sh: anillo + cuerdas)
#               cn         (ciclo puro C_N — edge-transitivo a todo N)
#               bridge2    (dos comunidades ringchords de N/2 + un puente:
#                           regular de facto y máximamente desigual)
#   régimen     poca   — goteo: una ronda de keys sobre 6 nodos cada ~3 s
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
REG=${3:?poca|media|mucha}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
MESH="$HERE/mesh.sh"
LOADER="$REPO/tests/testbed/sae_load.py"
OUTBASE="${DKMS_SCALE_OUT:-$REPO/tests/results/scale}"
OUT="$OUTBASE/$TOPO_FAM-n$N-$REG"
mkdir -p "$OUT"

export DKMS_MESH_DIR="${DKMS_MESH_DIR:-${TMPDIR:-/tmp}/dkms-scale-mesh}"
export DKMS_MESH_LINK_TYPE=qkd DKMS_MESH_TOPO=custom

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
    poca)
        # 6 nodos repartidos por el anillo; una ronda cada ~3 s.
        local nodes=()
        for i in 0 1 2 3 4 5; do nodes+=( $(( 1 + i * N / 6 )) ); done
        ( while [ -f "$OUT/.loading" ]; do
              "$MESH" keys "${nodes[@]}" >/dev/null 2>&1
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
            python3 -u "$LOADER" \
                --sae "sae_$m" --slaves "$slaves" \
                --certs "$DKMS_MESH_DIR/certs" --host 127.0.0.1 \
                --port $(( 20005 + (m - 1) * 100 )) \
                --threads "$th" --duration "$dur" --number 1 --size 256 \
                --aggregate-throttled --out "$OUT/load.sae_$m.csv" \
                > "$OUT/load.sae_$m.err" 2>&1 &
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
    [ "$REG" = poca ] && return 0
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
    cp "$DKMS_MESH_DIR"/edges.tsv "$OUT/" 2>/dev/null
    n_src=$(ls "$DKMS_MESH_DIR"/logs/dkms*.log 2>/dev/null | wc -l)
    n_dst=$(ls "$OUT"/dkms*.log 2>/dev/null | wc -l)
    [ "$n_dst" -ge "$n_src" ] && [ -s "$OUT/dkms1.log" ] && break
    echo "== archivado incompleto ($n_dst/$n_src, intento $intento); reintento en 30 s"
    sleep 30
done
printf '{"topo":"%s","n":%d,"regimen":"%s","cap_s":%d,"llenos":%d,"total":%d}\n' \
    "$TOPO_FAM" "$N" "$REG" "$CAP" "$OK" "$TOTAL" > "$OUT/meta.json"
"$MESH" down >/dev/null 2>&1
echo "== $TOPO_FAM n=$N $REG: archivado en $OUT"
