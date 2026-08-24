#!/usr/bin/env bash
# Estrés todos-contra-todos sobre la malla local: ¿cuántas claves/s aguanta?
#
# Misma estructura que `tests/testbed/t20_load.sh` —tres tramos, barrido de
# concurrencia, muestreo en vuelo, verificación de integridad— pero con TODOS
# los pares ordenados cargando a la vez en lugar del vecino del anillo, y sin
# SSH porque aquí todo está en la misma máquina.
#
#   ráfaga        buffers llenos (4096/par): mide la ruta de servicio
#                 (mTLS + OTP + ETSI-020) sin el generador de por medio
#   sostenido     vaciado el buffer, manda el refill: mide DKMS→ORR→QKC
#   recuperación  parada la carga, cuánto tarda el buffer en volver a 4096
#
# Dos ramas, porque con los defaults el techo sostenido es una constante
# nuestra y no un límite del sistema:
#
#   --arm A   defaults: 32 tokens/tick × 10 ticks/s = 320 claves/s por peer.
#             Es lo que se lleva un operador de fábrica.
#   --arm B   techo levantado (--tokens) y bucket por SAE fuera de juego, para
#             que aparezca el límite REAL: CPU, relay del ORR, keystore del QKC.
#
#   ./stress.sh --arm A
#   ./stress.sh --arm B --tokens 3200 --threads "4 16" --duration 300
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
MESH="$HERE/mesh.sh"
LOADER="$REPO/tests/testbed/sae_load.py"

ARM=A
NODES=10
SWEEP=(1 4 16)
DURATION=300
TOKENS=""                      # rama B: max_tokens_per_peer_per_tick
SUSTAIN_FROM=60                # segundos desde el inicio en que el buffer ya drenó
SAMPLE_EVERY=5
# Con más de ~24 hilos por proceso el GIL de Python empieza a ser el techo y lo
# que se mediría es el generador de carga. Se reparten los destinos de cada
# maestro en varios procesos hasta bajar de ahí.
MAX_THREADS_PER_PROC=24

while (( $# )); do
    case "$1" in
        --arm)      ARM="$2"; shift 2 ;;
        --nodes)    NODES="$2"; shift 2 ;;
        --threads)  read -ra SWEEP <<< "$2"; shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --tokens)   TOKENS="$2"; shift 2 ;;
        *) echo "opción desconocida: $1" >&2; exit 2 ;;
    esac
done

# Si la carga dura menos que la ventana de ráfaga no hay tramo sostenido que
# medir, y el informe saldría con "0 claves/s" que se lee como una avería.
if (( DURATION <= SUSTAIN_FROM )); then
    SUSTAIN_FROM=$(( DURATION / 3 ))
    echo "aviso: --duration $DURATION es corta; el tramo sostenido empieza en ${SUSTAIN_FROM}s" >&2
fi

STAMP="$(date +%Y%m%d-%H%M%S)"
# Mismo directorio que use mesh.sh, o los logs se buscarían donde no están.
# Importa en un clúster: ahí la malla va al scratch local del nodo y no al home
# por NFS, y sin esto stress.sh miraría en el home.
MESH_DIR="${DKMS_MESH_DIR:-$REPO/tests/results/local-mesh}"
OUT="${DKMS_STRESS_OUT:-$REPO/tests/results}/stress-$ARM-$STAMP"
SDN=http://127.0.0.1:19002
mkdir -p "$OUT"

log()  { printf '%s\n' "$*" | tee -a "$OUT/report.txt"; }
head1() { log ""; log "── $* ──"; }
strip() { sed 's/\x1b\[[0-9;]*m//g'; }
sae_port() { echo $(( 20005 + ($1 - 1) * 100 )); }

# ─── muestreo en vuelo ─────────────────────────────────────────────────────
# La diferencia importante con t20_load.sh: aquí se recoge también la rate que
# la SDN asigna a cada DKMS. Sin ella los 429 son inatribuibles — el bucket por
# SAE se dimensiona con `link_capacity / N_SAEs`, y en un despliegue PQC-only
# esa señal no significa nada y puede caer a 0. Ver CLAUDE.md.
sampler() {
    local tag=$1
    : > "$OUT/$tag.samples.txt"
    while [ -f "$OUT/.sampling" ]; do
        {
            printf '=== t=%s\n' "$(date +%s)"
            for n in $(seq 1 "$NODES"); do
                printf -- '-- dkms%s\n' "$n"
                strip < "$MESH_DIR/logs/dkms$n.log" | grep -a generator.state | tail -"$NODES"
                printf -- '-- orr%s\n' "$n"
                strip < "$MESH_DIR/logs/orr$n.log" | grep -a 'orr.state' | tail -1
                # El keystore del QKC: `misses` y `wenc_to` son lo que dice si
                # el enlace se queda sin material. Con enlaces QKD es la
                # evidencia directa de que el límite es la fibra y no el
                # generador del DKMS — sin esto hay que deducirlo de los
                # agregados.
                printf -- '-- qkc%s\n' "$n"
                strip < "$MESH_DIR/logs/qkc$n.log" | grep -a 'keystore.levels' | tail -3
                printf -- '-- rate dkms-%s: %s\n' "$n" \
                    "$(curl -s --max-time 3 "$SDN/rate/dkms-$n" || echo '{}')"
            done
            printf -- '-- topology: %s\n' "$(curl -s --max-time 3 "$SDN/topology")"
            printf -- '-- cpu\n'
            ps -eo pcpu,rss,comm --sort=-pcpu | head -12
        } >> "$OUT/$tag.samples.txt" 2>&1
        sleep "$SAMPLE_EVERY"
    done
}
start_sampling() { touch "$OUT/.sampling"; sampler "$1" & SAMPLER_PID=$!; }
stop_sampling()  { rm -f "$OUT/.sampling"; wait "${SAMPLER_PID:-0}" 2>/dev/null || true; }

# ─── espera a que la malla tenga los buffers llenos ────────────────────────
wait_buffers_full() {
    local deadline=$(( SECONDS + ${1:-300} )) want=$(( NODES - 1 ))
    while (( SECONDS < deadline )); do
        local full
        full=$(strip < "$MESH_DIR/logs/dkms1.log" | grep -a generator.state | tail -"$want" \
               | grep -c 'enc=4[0-9][0-9][0-9]')
        (( full >= want )) && return 0
        sleep 5
    done
    return 1
}

# ─── una tanda del barrido ─────────────────────────────────────────────────
run_point() {
    local th=$1 tag="t$th"
    head1 "concurrencia: $th hilos por par ordenado ($(( th * NODES * (NODES-1) )) en total)"

    log "   esperando buffers llenos…"
    wait_buffers_full 300 || log "   AVISO: los buffers no llegaron a tope; la ráfaga saldrá corta"

    start_sampling "$tag"
    local pids=()
    for m in $(seq 1 "$NODES"); do
        # Destinos de este maestro, repartidos en grupos para no pasar de
        # MAX_THREADS_PER_PROC hilos por proceso.
        local slaves=()
        for s in $(seq 1 "$NODES"); do [ "$s" != "$m" ] && slaves+=("sae_$s"); done
        local groups=$(( (th * ${#slaves[@]} + MAX_THREADS_PER_PROC - 1) / MAX_THREADS_PER_PROC ))
        (( groups < 1 )) && groups=1
        (( groups > ${#slaves[@]} )) && groups=${#slaves[@]}
        local per=$(( (${#slaves[@]} + groups - 1) / groups ))
        local g=0 i=0
        while (( i < ${#slaves[@]} )); do
            local chunk=("${slaves[@]:i:per}")
            local list; list=$(IFS=,; echo "${chunk[*]}")
            python3 -u "$LOADER" \
                --sae "sae_$m" --slaves "$list" \
                --certs "$MESH_DIR/certs" --host 127.0.0.1 --port "$(sae_port "$m")" \
                --threads $(( th * ${#chunk[@]} )) --duration "$DURATION" \
                --number 1 --size 256 --aggregate-throttled \
                --out "$OUT/$tag.sae_$m.g$g.csv" \
                --record-keys "$OUT/$tag.sae_$m.g$g" \
                > "$OUT/$tag.sae_$m.g$g.err" 2>&1 &
            pids+=($!)
            i=$(( i + per )); g=$(( g + 1 ))
        done
    done
    log "   ${#pids[@]} procesos de carga, $DURATION s"
    for p in "${pids[@]}"; do wait "$p" 2>/dev/null; done
    stop_sampling

    # ─── recuperación: cuánto tarda el buffer en volver a tope ───────────
    local t_rec=$SECONDS
    if wait_buffers_full 300; then
        log "   recuperación de buffers: $(( SECONDS - t_rec ))s"
    else
        log "   recuperación de buffers: NO alcanzada en 300s"
    fi

    analyse "$tag" "$th"
    attribute "$tag"
}

# Con qué señal de la SDN y con qué buffers se encontró la carga. Es lo que
# permite decir si un 429 es contrapresión legítima —el buffer ENC vacío, que
# es lo que pasa cuando la demanda supera al refill— o el ruido de lambda, que
# en un despliegue PQC-only puede caer a 0 y dejar el bucket sin refill.
attribute() {
    local tag=$1
    python3 - "$OUT/$tag.samples.txt" <<'ATTR' | tee -a "$OUT/report.txt"
import re, sys
rates, enc_zero, enc_tot = [], 0, 0
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    if line.startswith("-- rate "):
        rates += [float(x) for x in re.findall(r'"enc":([0-9.]+)', line)]
    elif "generator.state" in line:
        m = re.search(r"\benc=(\d+)", line)
        if m:
            enc_tot += 1
            enc_zero += (m.group(1) == "0")
if rates:
    nz = [r for r in rates if r > 0]
    print("   rate del SDN durante la carga: min=%.0f max=%.0f keys/s   (%d/%d muestras a cero)"
          % (min(rates), max(rates), len(rates) - len(nz), len(rates)))
if enc_tot:
    print("   buffer ENC vacio en %d/%d muestras (%.0f%%)"
          % (enc_zero, enc_tot, 100.0 * enc_zero / enc_tot))
    print("     ENC vacio => los 429 son contrapresion: la demanda supera al refill")
ATTR
}

analyse() {
    local tag=$1 th=$2
    python3 - "$OUT" "$tag" "$SUSTAIN_FROM" "$th" "$NODES" <<'PY' | tee -a "$OUT/report.txt"
import csv, glob, sys
out, tag, sustain_from, th, nodes = sys.argv[1], sys.argv[2], float(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
files = glob.glob(f"{out}/{tag}.*.csv")
# Dos pasadas en streaming: a 16 hilos por par cada CSV trae millones de filas y
# retenerlas como dicts se come varios GB. Solo se guardan las latencias OK.
t0 = None
for f in files:
    with open(f) as fh:
        for r in csv.DictReader(fh):
            t = float(r["t_unix"]); t0 = t if t0 is None else min(t0, t)
if t0 is None:
    print("   (sin filas)"); raise SystemExit

def weight(r):
    kid = r.get("key_id") or ""
    return int(kid[1:]) if r["thread"] == "-1" and kid.startswith("x") else 1

bad, lat = {}, []
n_ok = burst = sus = 0
smin = smax = None
for f in files:
    with open(f) as fh:
        for r in csv.DictReader(fh):
            if r["status"] != "200":
                bad[r["status"]] = bad.get(r["status"], 0) + weight(r); continue
            n_ok += 1
            lat.append(float(r["latency_ms"]))
            t = float(r["t_unix"]); n = int(r["n_keys"])
            if t - t0 >= sustain_from:
                sus += n
                smin = t if smin is None else min(smin, t)
                smax = t if smax is None else max(smax, t)
            else:
                burst += n
span = (smax - smin) if (smin is not None and smax and smax > smin) else 0
lat.sort()
def pct(p):
    return lat[min(int(len(lat) * p), len(lat) - 1)] if lat else float("nan")

pairs = nodes * (nodes - 1)
rate = sus / span if span else 0.0
print(f"   claves en ráfaga (primeros {sustain_from:.0f}s): {burst}")
print(f"   claves sostenidas: {sus} en {span:.0f}s  =  {rate:.0f} claves/s agregadas")
print(f"                      {rate/pairs:.1f} claves/s por par ordenado  (techo defaults: 320)")
print(f"   latencia OK  p50={pct(.50):.1f}ms  p95={pct(.95):.1f}ms  p99={pct(.99):.1f}ms  n={len(lat)}")
if bad:
    tot = sum(bad.values())
    desglose = "  ".join(f"{k}={v}" for k, v in sorted(bad.items(), key=lambda kv: -kv[1]))
    print(f"   respuestas no-200: {tot}   {desglose}")
    print("     429 = bucket por SAE (cruzar con la rate del SDN del muestreo)")
    print("     503 = clave de transporte que el peer no reconoce")
else:
    print("   respuestas no-200: ninguna")
PY
}

# ─── integridad bajo carga ─────────────────────────────────────────────────
# Lo único que detecta que dos claves de transporte se han desincronizado: la
# clave de sesión del SAE no lleva integridad propia, así que si el material
# estuviera desalineado los dos extremos se llevarían claves distintas sin que
# nada fallase.
verify_integrity() {
    head1 "integridad bajo carga (muestreo de dec_keys)"
    local ok=0 bad=0 checked=0
    for kf in "$OUT"/*.sae_*.keys; do
        [ -s "$kf" ] || continue
        local base slave m
        base=$(basename "$kf")
        slave=$(sed -E 's/.*\.(sae_[0-9]+)\.keys/\1/' <<< "$base")
        m=$(sed -E 's/.*\.sae_([0-9]+)\.g[0-9]+\..*/\1/' <<< "$base")
        [[ "$slave" =~ ^sae_[0-9]+$ ]] || continue
        local sn="${slave#sae_}"
        while IFS=, read -r kid digest; do
            [ -n "$kid" ] || continue
            local got
            got=$(curl -sS --max-time 10 --cacert "$MESH_DIR/certs/ca.crt" \
                  --cert "$MESH_DIR/certs/$slave.crt" --key "$MESH_DIR/certs/$slave.key" \
                  -H 'Content-Type: application/json' \
                  -d "{\"key_IDs\":[{\"key_ID\":\"$kid\"}]}" \
                  "https://127.0.0.1:$(sae_port "$sn")/api/v1/keys/sae_$m/dec_keys" \
                  2>/dev/null | jq -r '.keys[0].key // empty')
            # Vacía = ya consumida o nunca entregada; no es el objeto del test.
            [ -z "$got" ] && continue
            checked=$((checked+1))
            if [ "$(printf '%s' "$got" | sha256sum | cut -d' ' -f1)" = "$digest" ]; then
                ok=$((ok+1))
            else
                bad=$((bad+1))
                log "   ✗ BYTES DISTINTOS  sae_$m→$slave  key_ID=$kid"
            fi
        done < <(shuf -n 10 "$kf" 2>/dev/null || head -10 "$kf")
    done
    if (( checked == 0 )); then
        log "   (no quedaron claves sin consumir que muestrear)"
    elif (( bad == 0 )); then
        log "   $ok claves muestreadas, bytes idénticos en todas"
    else
        log "   $bad de $checked con BYTES DISTINTOS — esto importa más que toda la curva"
    fi
}

# ─── corrupción y salud de los ORR ─────────────────────────────────────────
health() {
    head1 "salud tras la campaña"
    local corrupt=0
    for n in $(seq 1 "$NODES"); do
        local c
        c=$(strip < "$MESH_DIR/logs/dkms$n.log" | grep -a generator.state \
            | grep -oE 'recv_corrupt=[0-9]+' | cut -d= -f2 | sort -n | tail -1)
        corrupt=$(( corrupt + ${c:-0} ))
    done
    log "   recv_corrupt acumulado en la malla: $corrupt"
    for n in $(seq 1 "$NODES"); do
        strip < "$MESH_DIR/logs/orr$n.log" | grep -a 'orr.state' | tail -1 \
            | grep -oE 'me=[^ ]+|dropped_no_secret=[0-9]+|peel_failed=[0-9]+' | tr '\n' ' '
        echo
    done | tee -a "$OUT/report.txt"
}

# ─── campaña ───────────────────────────────────────────────────────────────
log "############ estrés rama $ARM · $NODES nodos · $(date '+%F %T') ############"
log "   host: $(hostname)  cpus: $(nproc)"
[ "$ARM" = B ] && [ -z "$TOKENS" ] && TOKENS=3200
if [ "$ARM" = B ]; then
    log "   techo levantado: max_tokens_per_peer_per_tick=$TOKENS y bucket por SAE fuera de juego"
    export DKMS_MESH_TOKENS_PER_TICK="$TOKENS" DKMS_MESH_SAE_MIN_TOKENS=100000000
else
    log "   defaults de fábrica: 32 tokens/tick = 320 claves/s por peer"
    unset DKMS_MESH_TOKENS_PER_TICK DKMS_MESH_SAE_MIN_TOKENS
fi

"$MESH" up "$NODES" 2>&1 | tee -a "$OUT/report.txt"
trap '"$MESH" down >/dev/null 2>&1' EXIT

head1 "tiempos de convergencia"
sleep 90
"$HERE/bootstrap_times.py" "$MESH_DIR" 2>&1 | tee -a "$OUT/report.txt"

for th in "${SWEEP[@]}"; do run_point "$th"; done
verify_integrity
health

log ""
log "   resultados en $OUT"
log "############ fin $(date '+%F %T') ############"
