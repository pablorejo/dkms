#!/usr/bin/env bash
# Una celda de la campaña 2026-09: <familia> <N> (familias en topologies.py).
#
# Levanta la malla con enlaces QKD —un quditto por arista, cada uno con la
# distancia de SU arista— y mide TRES cargas sobre la MISMA malla, con un
# muestreo continuo de todos los módulos:
#
#   L0 reposo       0 tráfico SAE. Desde buffers vacíos: el llenado del
#                   generador (DKMS→ORR→QKC→fibra) y la convergencia. Termina
#                   cuando todos los pares están a tope (+60 s) o al cap.
#   L1 media        carga pautada: un flujo por par ordenado con --rate-cap,
#                   dimensionada al L1_FRACTION del techo de fibra de la
#                   topología (acotado a L1_MAX_TOTAL claves/s agregadas).
#   L2 saturación   bucle cerrado, un flujo por par ordenado sin pausa:
#                   demanda ≫ capacidad a todo N. El "sostenido" lo corrige el
#                   análisis con el stock de buffers drenado (Σenc del muestreo).
#   REC             recuperación: el generador rellena tras L2.
#
# Integridad: una ronda `mesh.sh keys` (enc en el maestro, dec en el esclavo,
# bytes comparados) tras L1 y al final; bajo L2 valen `recv_corrupt` y los 503.
#
#   campaign_cell.sh <familia> <N>
#   env: DKMS_CAMPAIGN_OUT (dir de resultados), DKMS_MESH_DIR (scratch),
#        L0_SECS L1_SECS L2_SECS REC_SECS L1_FRACTION L1_MAX_TOTAL SAMPLE_EVERY
set -uo pipefail

FAM=${1:?familia (estrella|anillo|puente|malla|rgg|aleatoria)}
N=${2:?N}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${DKMS_REPO:-$(cd "$HERE/../.." && pwd)}"   # el sbatch corre una COPIA del arnés fuera del repo
MESH="$HERE/mesh.sh"
TOPOGEN="$HERE/topologies.py"
REDUCE="$HERE/campaign_reduce.py"
LOADER="$REPO/target/release/sae_load"
OUT="${DKMS_CAMPAIGN_OUT:-$REPO/tests/results/campaign-2026-09/cells}/$FAM-n$N"

L0_SECS=${L0_SECS:-600}
L1_SECS=${L1_SECS:-300}
L2_SECS=${L2_SECS:-300}
REC_SECS=${REC_SECS:-180}
L1_FRACTION=${L1_FRACTION:-0.5}
L1_MAX_TOTAL=${L1_MAX_TOTAL:-30000}
SAMPLE_EVERY=${SAMPLE_EVERY:-5}
KEYS_NODES_MAX=${KEYS_NODES_MAX:-10}

export DKMS_MESH_DIR="${DKMS_MESH_DIR:-${TMPDIR:-/tmp}/dkms-c9-$FAM-$N}"
export DKMS_MESH_LINK_TYPE=qkd DKMS_MESH_TOPO=custom
export DKMS_MESH_SEED="${DKMS_MESH_SEED:-42}"
MESH_DIR="$DKMS_MESH_DIR"
# Los CSV crudos de carga van al SCRATCH del nodo, no a $OUT (home NFS): a
# N=40 son ~2 GB por celda durante L2 y con varias celdas en paralelo se
# agotó la cuota del home (20 GB) — tres celdas murieron con «0/40 procesos
# vivos» porque la redirección del .err fallaba con EDQUOT (2026-09-03). A
# $OUT solo va lo reducido (y los CSV comprimidos para N≤20).
LOAD_DIR="${DKMS_CELL_LOAD_DIR:-$MESH_DIR-load}"

mkdir -p "$OUT" "$LOAD_DIR"
rm -f "$OUT/DONE" "$OUT/FAILED"
log()  { printf '%s %s\n' "$(date '+%F %T')" "$*" | tee -a "$OUT/cell.log"; }
strip() { sed 's/\x1b\[[0-9;]*m//g'; }
die()  { log "FATAL: $*"; touch "$OUT/FAILED"; stop_sampling; "$MESH" down >/dev/null 2>&1; exit 1; }
sae_port() { echo $(( 20005 + ($1 - 1) * 100 )); }

[ -x "$LOADER" ] || die "falta $LOADER (cargo build --release)"
ulimit -n "$(ulimit -Hn)" 2>/dev/null || ulimit -n 65536 2>/dev/null || true

# ── topología ──────────────────────────────────────────────────────────────
python3 "$TOPOGEN" "$FAM" "$N" --both > "$OUT/topology.json" || die "topologies.py falló"
DKMS_MESH_EDGES=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["edges_env"])' "$OUT/topology.json")
export DKMS_MESH_EDGES
read -r EDGES PAIRS FIBRA CUELLO HOPS < <(python3 -c '
import json,sys; m=json.load(open(sys.argv[1]))
print(m["edges"], m["pairs"], m["techo_fibra_keys_per_s"], m["techo_cuello_keys_per_s"], m["mean_hops"])' "$OUT/topology.json")
log "== $FAM N=$N: $EDGES aristas, $PAIRS pares, saltos medios $HOPS, techo fibra $FIBRA claves/s, cuello $CUELLO"

# ── muestreo continuo ──────────────────────────────────────────────────────
# Una línea agregada por módulo y muestra (no las 99 líneas por DKMS de N=100,
# que serían 1 GB): sumas y extremos de generator.state / keystore.levels /
# orr.state, la última mcmcf de la SDN, CPU y RSS por tipo de proceso. Las
# líneas crudas se conservan solo para dkms1/qkc1 (curva de llenado par a par).
SAMPLER_PID=""
sampler() {
    local n last
    while [ -f "$OUT/.sampling" ]; do
        {
            printf '=== t=%s phase=%s up=%s\n' "$(date +%s)" "$(cat "$OUT/.phase" 2>/dev/null || echo '?')" "$SECONDS"
            for n in $(seq 1 "$N"); do
                tail -c 900000 "$MESH_DIR/logs/dkms$n.log" 2>/dev/null | strip | grep -a 'generator.state' \
                    | tail -$(( N - 1 )) | awk -v node="$n" '
                    { delete kv; for (i = 1; i <= NF; i++) { p = index($i, "="); if (p) { k = substr($i, 1, p-1); v = substr($i, p+1); gsub(/"/, "", v); kv[k] = v } }
                      if (!("peer" in kv)) next
                      if (kv["peer"] in seen) next; seen[kv["peer"]] = 1
                      pairs++; enc += kv["enc"]; dec += kv["dec"]; ackp += kv["ack_pending"]; expd += kv["expired"]
                      recv += kv["recv"]; corrupt += kv["recv_corrupt"]; noep += kv["recv_no_epoch"]; repl += kv["recv_replayed"]
                      ddrop += kv["dec_dropped"]; emitted += kv["emitted"]; acked += kv["acked"]; efail += kv["emit_failed"]
                      ackfail += kv["ack_send_failed"]; obs += kv["observed_keys_per_s"]; sdnr += kv["sdn_rate_keys_per_s"]
                      if (kv["sdn_rate_keys_per_s"] + 0 == 0) sdnz++
                      if (kv["enc"] + 0 == 0) ez++; if (kv["enc"] + 0 >= 4096) ef++
                      if (kv["e2e_epoch"] == "none") e2n++
                      if (encmin == "" || kv["enc"] + 0 < encmin) encmin = kv["enc"] + 0 }
                    END { printf "D n=%s pairs=%d enc=%d dec=%d enc_min=%s enc_zero=%d enc_full=%d ackp=%d expired=%d recv=%d corrupt=%d noepoch=%d replayed=%d decdrop=%d emitted=%d acked=%d emit_failed=%d ack_send_failed=%d obs=%.1f sdn_rate=%.1f sdn_zero=%d e2e_none=%d\n",
                        node, pairs, enc, dec, encmin, ez, ef, ackp, expd, recv, corrupt, noep, repl, ddrop, emitted, acked, efail, ackfail, obs, sdnr, sdnz, e2n }'
                tail -c 400000 "$MESH_DIR/logs/qkc$n.log" 2>/dev/null | strip | grep -a 'keystore.levels' \
                    | tail -40 | awk -v node="$n" '
                    { delete kv; for (i = 1; i <= NF; i++) { p = index($i, "="); if (p) { k = substr($i, 1, p-1); v = substr($i, p+1); gsub(/"/, "", v); kv[k] = v } }
                      if (!("peer" in kv)) next
                      L[kv["peer"]] = $0; line[kv["peer"]] = 1
                      enc[kv["peer"]] = kv["enc"]; dec[kv["peer"]] = kv["dec"]; taken[kv["peer"]] = kv["taken"]; miss[kv["peer"]] = kv["misses"]
                      wto[kv["peer"]] = kv["wenc_to"]; rf[kv["peer"]] = kv["refill_fail"]; nd[kv["peer"]] = kv["notify_drop"]
                      rate[kv["peer"]] = ("rate" in kv) ? kv["rate"] : ""; rq[kv["peer"]] = ("rate_q" in kv) ? kv["rate_q"] : "" }
                    END { for (p in line) { links++; E += enc[p]; Dd += dec[p]; T += taken[p]; M += miss[p]; W += wto[p]; RF += rf[p]; ND += nd[p]
                            if (rate[p] != "") { nr++; R += rate[p]; q[rq[p]]++ } }
                          qs = ""; for (k in q) qs = qs k ":" q[k] ","
                          printf "Q n=%s links=%d enc=%d dec=%d taken=%d misses=%d wenc_to=%d refill_fail=%d notify_drop=%d rate_n=%d rate_sum=%.1f rate_q=%s\n", node, links, E, Dd, T, M, W, RF, ND, nr, R, qs }'
                tail -c 100000 "$MESH_DIR/logs/orr$n.log" 2>/dev/null | strip | grep -a 'orr.state' | tail -1 \
                    | awk -v node="$n" '{ printf "O n=%s %s\n", node, $0 }' | sed -E 's/^(O n=[0-9]+) .*orr\.state /\1 /'
            done
            # Curva par a par de un nodo (dkms1), para ver el llenado de cada peer.
            tail -c 900000 "$MESH_DIR/logs/dkms1.log" 2>/dev/null | strip | grep -a 'generator.state' | tail -$(( N - 1 )) \
                | grep -oE 'peer=[^ ]+ enc=[0-9]+ dec=[0-9]+' | awk '{ printf "R1 %s %s %s\n", $1, $2, $3 }'
            # Stock de cada KME (quditto, HTTP plano en 30000+idx): el tercer
            # almacén que drena la saturación; sin él el sostenido de L2 no se
            # puede corregir del todo (8192 claves por arista de fábrica).
            while read -r idx _a _b _d; do
                [ -n "$idx" ] || continue
                curl -s --max-time 2 "http://127.0.0.1:$(( 30000 + idx ))/api/v1/keys/probe/status" 2>/dev/null \
                    | python3 -c 'import sys,json
try:
    d=json.load(sys.stdin); print("K idx=%s stored=%d max=%d" % (sys.argv[1], d.get("stored_key_count",0), d.get("max_key_count",0)))
except Exception:
    pass' "$idx"
            done < "$MESH_DIR/edges.tsv"
            tail -c 200000 "$MESH_DIR/logs/sdn.log" 2>/dev/null | strip | grep -a 'mcmcf' | tail -2 | sed 's/^/S /'
            ps -eo rss=,pcpu=,comm= | awk '
                $3 ~ /^(sdn|qkc|orr|dkms|quditto|sae_load)$/ { n[$3]++; rss[$3] += $1; cpu[$3] += $2 }
                END { for (c in n) printf "P comm=%s n=%d rss_mb=%.0f cpu=%.0f\n", c, n[c], rss[c] / 1024, cpu[c] }'
            printf 'P loadavg=%s\n' "$(cut -d' ' -f1-3 /proc/loadavg)"
        } >> "$OUT/samples.txt" 2>/dev/null
        sleep "$SAMPLE_EVERY"
    done
}
start_sampling() { touch "$OUT/.sampling"; sampler & SAMPLER_PID=$!; }
stop_sampling()  { rm -f "$OUT/.sampling"; [ -n "$SAMPLER_PID" ] && wait "$SAMPLER_PID" 2>/dev/null; SAMPLER_PID=""; }
phase() { echo "$1" > "$OUT/.phase"; log "== fase $1 (t+${SECONDS}s)"; }

full_count() {
    local ok=0 c n
    for n in $(seq 1 "$N"); do
        c=$(tail -c 900000 "$MESH_DIR/logs/dkms$n.log" 2>/dev/null | strip | grep -a generator.state \
            | tail -$(( N - 1 )) | grep -c 'enc=4096')
        ok=$(( ok + c ))
    done
    echo "$ok"
}

# ── carga ──────────────────────────────────────────────────────────────────
LOAD_PIDS=()
start_load() {   # start_load <tag> <segundos> <rate-cap por flujo (0 = bucle cerrado)> [backoff-ms]
    local tag=$1 dur=$2 cap=$3 backoff=${4:-0} m slaves s
    LOAD_PIDS=()
    for m in $(seq 1 "$N"); do
        slaves=""
        for s in $(seq 1 "$N"); do [ "$s" != "$m" ] && slaves="$slaves,sae_$s"; done
        slaves=${slaves#,}
        SAE_LOAD_WORKERS=$(( N >= 50 ? 4 : 2 )) "$LOADER" \
            --sae "sae_$m" --slaves "$slaves" \
            --certs "$MESH_DIR/certs" --host 127.0.0.1 --port "$(sae_port "$m")" \
            --threads $(( N - 1 )) --duration "$dur" --number 1 --size 256 \
            --rate-cap "$cap" --backoff-ms "$backoff" --aggregate-throttled --out "$LOAD_DIR/$tag.sae_$m.csv" \
            > "$LOAD_DIR/$tag.sae_$m.err" 2>&1 &
        LOAD_PIDS+=($!)
    done
    log "   $tag: ${#LOAD_PIDS[@]} procesos de carga, $(( N * (N - 1) )) flujos, rate-cap $cap req/s por flujo, backoff ${backoff}ms, ${dur}s"
}
check_load() {   # la carga tiene que ARRANCAR (ver scale_one.sh, 2026-08-26)
    local tag=$1 vivos=0 p errs
    sleep 20
    for p in "${LOAD_PIDS[@]}"; do kill -0 "$p" 2>/dev/null && vivos=$(( vivos + 1 )); done
    errs=$(grep -lE "SyntaxError|Traceback|^Error|error:" "$LOAD_DIR"/"$tag".sae_*.err 2>/dev/null | wc -l)
    log "   $tag: $vivos/${#LOAD_PIDS[@]} procesos vivos a los 20 s, $errs con error"
    if [ "$vivos" -lt $(( ${#LOAD_PIDS[@]} / 2 )) ] || [ "$errs" -gt 0 ]; then
        head -3 "$LOAD_DIR/$tag.sae_1.err" 2>/dev/null | tee -a "$OUT/cell.log"
        ls "$LOAD_DIR" | head -3 | tee -a "$OUT/cell.log"
        df -h "$OUT" | tail -1 | tee -a "$OUT/cell.log"
        die "la carga $tag no arrancó"
    fi
}
wait_load() { local p; for p in "${LOAD_PIDS[@]}"; do wait "$p" 2>/dev/null; done; }

keys_round() {   # keys_round <tag>: ronda ETSI-014 enc/dec con bytes comparados
    local nodes=() k
    if (( N <= KEYS_NODES_MAX )); then
        for k in $(seq 1 "$N"); do nodes+=("$k"); done
    else
        for k in $(seq 0 $(( KEYS_NODES_MAX - 1 ))); do nodes+=( $(( 1 + k * N / KEYS_NODES_MAX )) ); done
    fi
    "$MESH" keys "${nodes[@]}" > "$OUT/keys_$1.log" 2>&1
    log "   integridad $1 (${#nodes[@]} nodos): $(tail -1 "$OUT/keys_$1.log")"
}

# ── la celda ───────────────────────────────────────────────────────────────
log "############ $FAM N=$N · $(uname -n) · $(nproc) cpus · $(date '+%F %T') ############"
"$MESH" down >/dev/null 2>&1
T_UP0=$SECONDS
if ! "$MESH" up "$N" custom > "$OUT/up.log" 2>&1; then
    tail -5 "$OUT/up.log" | tee -a "$OUT/cell.log"
    # Los logs de arranque viven en el scratch del nodo, que muere con el job:
    # sin esto un fallo de bring-up no se puede diagnosticar a posteriori.
    mkdir -p "$OUT/boot-logs"
    for f in sdn qkc1 orr1 dkms1 quditto0; do
        [ -f "$MESH_DIR/logs/$f.log" ] && tail -c 200000 "$MESH_DIR/logs/$f.log" > "$OUT/boot-logs/$f.log"
    done
    cp "$MESH_DIR/cfg/sdn/default.toml" "$OUT/boot-logs/sdn.toml" 2>/dev/null
    die "mesh.sh up falló"
fi
T_UP=$(( SECONDS - T_UP0 ))
log "== malla arriba en ${T_UP}s ($(grep -c . "$MESH_DIR/edges.tsv") aristas, $(grep -c 'registrados' "$OUT/up.log")/1 registro completo)"
grep -E 'módulos registrados|AVISO|capacidad' "$OUT/up.log" | tee -a "$OUT/cell.log"
trap 'stop_sampling; "$MESH" down >/dev/null 2>&1' EXIT

# L0: reposo / llenado
start_sampling
phase L0
TOTAL=$PAIRS
DEADLINE=$(( SECONDS + L0_SECS ))
T_FULL=""
OK=0
while (( SECONDS < DEADLINE )); do
    OK=$(full_count)
    if (( OK >= TOTAL )) && [ -z "$T_FULL" ]; then
        T_FULL=$(( SECONDS - T_UP0 ))
        log "   todos los pares a tope a los ${T_FULL}s desde el arranque; +60 s de reposo"
        DEADLINE=$(( SECONDS + 60 ))
    fi
    sleep 10
done
OK=$(full_count)
log "== L0 cerrado: $OK/$TOTAL pares a tope$( [ -n "$T_FULL" ] && echo " (tope a los ${T_FULL}s)" || echo " (NO llegó a tope en ${L0_SECS}s)")"
python3 "$HERE/bootstrap_times.py" "$MESH_DIR" > "$OUT/bootstrap_times.txt" 2>&1 || true

# L1: carga pautada al L1_FRACTION del techo de fibra
R1_TOTAL=$(python3 -c "print(min(float('$FIBRA'), float('$L1_MAX_TOTAL')) * float('$L1_FRACTION'))")
R1_FLOW=$(python3 -c "print('%.6f' % (float('$R1_TOTAL') / $PAIRS))")
phase L1
log "   oferta L1: $R1_TOTAL claves/s agregadas = $R1_FLOW por par ordenado"
start_load L1 "$L1_SECS" "$R1_FLOW"
check_load L1
wait_load
keys_round L1

# L2: saturación (bucle cerrado; 100 ms de pausa tras cada 429/503 para que
# 9900 flujos rechazados no midan la ruta de rechazo del DKMS)
phase L2
start_load L2 "$L2_SECS" 0 "${L2_BACKOFF_MS:-100}"
check_load L2
wait_load

# REC: recuperación
phase REC
DEADLINE=$(( SECONDS + REC_SECS ))
T_REC0=$SECONDS
T_REC=""
while (( SECONDS < DEADLINE )); do
    OK=$(full_count)
    if (( OK >= TOTAL )); then T_REC=$(( SECONDS - T_REC0 )); log "   buffers de nuevo a tope en ${T_REC}s"; break; fi
    sleep 10
done
OK=$(full_count)
# Si el último recuento ya está a tope, recuperó justo al límite: no decir lo contrario.
[ -z "$T_REC" ] && (( OK >= TOTAL )) && T_REC=$REC_SECS
log "== REC cerrado: $OK/$TOTAL pares a tope$( [ -n "$T_REC" ] && echo " (en ${T_REC}s)" || echo " (NO recuperó en ${REC_SECS}s)")"
keys_round final
stop_sampling

# ── salud ──────────────────────────────────────────────────────────────────
dead=0; for p in "$MESH_DIR"/logs/*.pid; do kill -0 "$(cat "$p")" 2>/dev/null || dead=$(( dead + 1 )); done
panics=$(grep -al 'panicked at' "$MESH_DIR"/logs/*.log 2>/dev/null | wc -l)
corrupt=0; noepoch=0; replayed=0; expired=0
for n in $(seq 1 "$N"); do
    v=$(tail -c 900000 "$MESH_DIR/logs/dkms$n.log" | strip | grep -a generator.state | tail -$(( N - 1 )) \
        | grep -oE 'recv_corrupt=[0-9]+|recv_no_epoch=[0-9]+|recv_replayed=[0-9]+|expired=[0-9]+' \
        | awk -F= '/recv_corrupt/{a+=$2} /recv_no_epoch/{b+=$2} /recv_replayed/{c+=$2} /^expired/{d+=$2} END{print a+0, b+0, c+0, d+0}')
    set -- $v; corrupt=$(( corrupt + $1 )); noepoch=$(( noepoch + $2 )); replayed=$(( replayed + $3 )); expired=$(( expired + $4 ))
done
peel=0; nosec=0
for n in $(seq 1 "$N"); do
    v=$(tail -c 100000 "$MESH_DIR/logs/orr$n.log" | strip | grep -a 'orr.state' | tail -1 \
        | grep -oE 'peel_failed=[0-9]+|dropped_no_secret=[0-9]+' | awk -F= '/peel/{a=$2} /dropped/{b=$2} END{print a+0, b+0}')
    set -- $v; peel=$(( peel + $1 )); nosec=$(( nosec + $2 ))
done
# Sello por-frame: la ÚLTIMA línea `qkc.frame_auth` de cada (me, peer) lleva
# los contadores acumulados; se suman bad_mac + replayed + plain_rej.
bad_mac=$(for n in $(seq 1 "$N"); do tail -c 400000 "$MESH_DIR/logs/qkc$n.log" | strip | grep -a 'qkc.frame_auth me=' | tail -40; done \
    | awk '{ for (i = 1; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] } k = v["me"] "-" v["peer"]; last[k] = v["bad_mac"] + v["replayed"] + v["plain_rej"] }
           END { for (k in last) t += last[k]; print t + 0 }')
intake_drop=$(cat "$MESH_DIR"/logs/qkc*.log | strip | grep -a 'intake_full' | grep -oE 'total=[0-9]+' | cut -d= -f2 | sort -n | tail -1); intake_drop=${intake_drop:-0}
orr_send_failed=0
for n in $(seq 1 "$N"); do
    v=$(tail -c 100000 "$MESH_DIR/logs/orr$n.log" | strip | grep -a 'orr.state' | tail -1 | grep -oE 'send_failed=[0-9]+' | cut -d= -f2)
    orr_send_failed=$(( orr_send_failed + ${v:-0} ))
done
log "== salud: procesos muertos=$dead panics=$panics recv_corrupt=$corrupt recv_no_epoch=$noepoch recv_replayed=$replayed expired=$expired peel_failed=$peel dropped_no_secret=$nosec orr_send_failed=$orr_send_failed frame_auth_rechazos=$bad_mac intake_dropped(líneas)=$intake_drop"

# ── reducción de los CSV de carga (en el scratch; a $OUT solo lo reducido) ──
for tag in L1 L2; do
    python3 "$REDUCE" "$LOAD_DIR" "$tag" > "$OUT/$tag.reduce.log" 2>&1 || log "   AVISO: reduce $tag falló: $(tail -1 "$OUT/$tag.reduce.log")"
    cp "$LOAD_DIR/$tag.persec.csv" "$LOAD_DIR/$tag.pairs.csv" "$LOAD_DIR/$tag.summary.json" "$OUT/" 2>/dev/null
    cp "$LOAD_DIR"/$tag.sae_*.err "$OUT/" 2>/dev/null
    log "   $tag: $(python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); print("ok_keys=%d req_ok=%d n429=%d n503=%d other=%d p50=%.1fms p99=%.1fms pares_sin_clave=%d" % (s["ok_keys"], s["ok_req"], s["n429"], s["n503"], s["n_other"], s["lat_p50_ms"], s["lat_p99_ms"], s["pairs_zero"]))' "$OUT/$tag.summary.json" 2>/dev/null || echo 'sin resumen')"
    if (( N <= 20 )); then
        for f in "$LOAD_DIR"/$tag.sae_*.csv; do gzip -c "$f" > "$OUT/$(basename "$f").gz" 2>/dev/null; done
    fi
    rm -f "$LOAD_DIR"/$tag.sae_*.csv
done

# ── archivo ────────────────────────────────────────────────────────────────
cp "$MESH_DIR/edges.tsv" "$MESH_DIR/topology.tsv" "$MESH_DIR/logs/starts.tsv" "$OUT/" 2>/dev/null
for f in sdn dkms1 dkms2 qkc1 qkc2 orr1 orr2 quditto0; do
    [ -f "$MESH_DIR/logs/$f.log" ] && gzip -c "$MESH_DIR/logs/$f.log" > "$OUT/$f.log.gz"
done
gzip -f "$OUT/samples.txt"
du -sh "$MESH_DIR/logs" 2>/dev/null | awk '{print "   logs de la malla en scratch: " $1}' | tee -a "$OUT/cell.log"
python3 - "$OUT" "$FAM" "$N" "$T_UP" "${T_FULL:-}" "${T_REC:-}" "$R1_TOTAL" "$R1_FLOW" "$L0_SECS" "$L1_SECS" "$L2_SECS" "$REC_SECS" \
    "$dead" "$panics" "$corrupt" "$noepoch" "$replayed" "$expired" "$peel" "$nosec" "$bad_mac" "$intake_drop" "$OK" "$TOTAL" "$orr_send_failed" <<'PY'
import json, os, socket, sys
a = sys.argv
meta = {
    "family": a[2], "n": int(a[3]), "t_up_s": int(a[4]),
    "t_full_s": int(a[5]) if a[5] else None, "t_recover_s": int(a[6]) if a[6] else None,
    "l1_offered_total": float(a[7]), "l1_offered_per_flow": float(a[8]),
    "l0_secs": int(a[9]), "l1_secs": int(a[10]), "l2_secs": int(a[11]), "rec_secs": int(a[12]),
    "health": {"dead_processes": int(a[13]), "panics": int(a[14]), "recv_corrupt": int(a[15]),
               "recv_no_epoch": int(a[16]), "recv_replayed": int(a[17]), "expired": int(a[18]),
               "peel_failed": int(a[19]), "dropped_no_secret": int(a[20]),
               "frame_auth_rejects": int(a[21]), "intake_dropped_lines": int(a[22]),
               "orr_send_failed": int(a[25])},
    "final_full_pairs": int(a[23]), "pairs": int(a[24]),
    "host": socket.gethostname(), "cpus": os.cpu_count(),
    "slurm_job": os.environ.get("SLURM_JOB_ID"), "git_sha": open(os.path.join(os.path.dirname(a[1]), "..", "GIT_SHA")).read().strip() if os.path.exists(os.path.join(os.path.dirname(a[1]), "..", "GIT_SHA")) else None,
}
json.dump(meta, open(os.path.join(a[1], "meta.json"), "w"), indent=1, sort_keys=True)
PY
"$MESH" down >/dev/null 2>&1
trap - EXIT
touch "$OUT/DONE"
log "############ fin $FAM N=$N · $(date '+%F %T') · $(( SECONDS / 60 )) min ############"
