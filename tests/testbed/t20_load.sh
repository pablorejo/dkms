#!/usr/bin/env bash
# T20 — carga: ¿cuántas claves/s aguanta de verdad?
# T12 — reposo largo (--idle N).
#
# La carga se genera EN cada VM contra su DKMS local (ahí es donde vive un SAE
# real). Mientras corre, un muestreador recoge cada 5 s el estado interno de
# los tres módulos y de la SDN, para poder atribuir el techo a un sitio
# concreto en vez de mirar solo el CSV del cliente.
#
# Tres tramos por punto del barrido, y el que responde la pregunta es el 2º:
#   ráfaga        los buffers están llenos (~4096/peer): claves "gratis"
#   sostenido     vaciado el buffer, manda el refill (rate SDN + 32 tokens/tick)
#   recuperación  parada la carga, cuánto tarda el buffer en rellenarse
#
#   ./t20_load.sh                       # barrido 1,4,16,64 hilos × 300 s
#   ./t20_load.sh --threads 16 --duration 120
#   ./t20_load.sh --idle 3600           # T12: sin carga, solo muestreo
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh scp python3

SWEEP=(1 4 16 64)
DURATION=300
SUSTAIN_FROM=30          # s desde los que se considera "sostenido"
RECOVER=120
IDLE=0
NUMBER=1
SIZE=256

while [[ $# -gt 0 ]]; do
    case "$1" in
        --threads)  SWEEP=("$2"); shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --idle)     IDLE="$2"; shift 2 ;;
        --number)   NUMBER="$2"; shift 2 ;;
        --size)     SIZE="$2"; shift 2 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done

DIR="$(mkoutdir load)"
REMOTE=/tmp/dkms-testbed

# ─── muestreador: estado interno cada 5 s ─────────────────────────────
# Se lanza en background y se para con la marca en $DIR/.sampling
sampler() {
    local tag="$1" secs="$2"
    local end=$(( SECONDS + secs ))
    : > "$DIR/$tag.samples.txt"
    while (( SECONDS < end )) && [[ -f "$DIR/.sampling" ]]; do
        {
            printf '=== t=%s\n' "$(date +%s)"
            for n in "${NODES[@]}"; do
                printf -- '-- %s dkms\n' "$n"
                dlogs "$n" dkms 40 | grep -a 'generator.state' | tail -4 || true
                printf -- '-- %s qkc\n' "$n"
                dlogs "$n" qkc 40 | grep -a 'keystore.levels' | tail -4 || true
            done
            printf -- '-- sdn rates\n'
            for n in "${NODES[@]}"; do
                printf '%s: %s\n' "${NODE_DKMS[$n]}" \
                    "$(sdn_get "/rate/${NODE_DKMS[$n]}" 2>/dev/null || echo '{}')"
            done
            printf -- '-- docker stats\n'
            for n in "${NODES[@]}"; do
                on "$n" "docker stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}'" 2>/dev/null || true
            done
        } >> "$DIR/$tag.samples.txt" 2>&1
        sleep 5
    done
}

start_sampling() { touch "$DIR/.sampling"; sampler "$1" "$2" & SAMPLER_PID=$!; }
stop_sampling()  { rm -f "$DIR/.sampling"; wait "${SAMPLER_PID:-0}" 2>/dev/null || true; }

# ─── T12: reposo largo ────────────────────────────────────────────────
if (( IDLE > 0 )); then
    info "T12 — reposo $IDLE s, sin ninguna carga"
    info "objetivo: ver si el buffer DEC se estabiliza. try_push nunca rechaza"
    info "(capacity es soft-hint) y aquí NO hay SAEs drenando, que es justo la"
    info "pata del argumento de acotación de RAM que no se cumple."
    start_sampling idle "$IDLE"
    sleep "$IDLE"
    stop_sampling
    # dec por peer al principio y al final
    decs=$({ grep -oa ' dec=[0-9]*' "$DIR/idle.samples.txt" || true; } | cut -d= -f2)
    first=$(printf '%s\n' "$decs" | head -1)
    last=$(printf '%s\n' "$decs" | tail -1)
    info "dec: $first → $last en $IDLE s"
    if [[ -n "$first" && -n "$last" ]] && (( last > first * 2 )); then
        fail "el buffer DEC más que dobló en reposo ($first → $last): consumo de RAM sin techo"
    else
        pass "buffer DEC acotado en reposo ($first → $last)"
    fi
    summary
    exit $?
fi

# ─── preparar el cliente en cada VM ───────────────────────────────────
# Cliente de carga: el binario Rust (tests/loadgen) si está compilado para las
# VMs (target-bookworm; DKMS_TESTBED_LOADER lo pisa). El DKMS negocia SOLO
# X25519MLKEM768 y presenta certs ML-DSA, que el `ssl` de Python solo tiene con
# OpenSSL >= 3.5; el binario habla con ambos. Misma CLI y mismo CSV.
LOADER_BIN="${DKMS_TESTBED_LOADER:-$(dirname "${BASH_SOURCE[0]}")/../../target-bookworm/release/sae_load}"
if [ -x "$LOADER_BIN" ]; then LOADER_CMD="./sae_load"; else LOADER_CMD="python3 -u sae_load.py"; fi
ship_loader() {   # ship_loader <nodo>
    if [ -x "$LOADER_BIN" ]; then
        scp "${SSH_OPTS[@]}" "$LOADER_BIN" "$1:$REMOTE/sae_load" >/dev/null
    else
        scp "${SSH_OPTS[@]}" "$(dirname "${BASH_SOURCE[0]}")/sae_load.py" "$1:$REMOTE/" >/dev/null
    fi
}
for n in "${NODES[@]}"; do
    on "$n" "mkdir -p $REMOTE"
    ship_loader "$n"
done

# destino de cada nodo: el siguiente en el anillo
declare -A SLAVE_OF
for i in "${!NODES[@]}"; do
    nxt=$(( (i + 1) % ${#NODES[@]} ))
    SLAVE_OF["${NODES[$i]}"]="${NODE_SAE[${NODES[$nxt]}]}"
done

# ─── barrido ──────────────────────────────────────────────────────────
for TH in "${SWEEP[@]}"; do
    info "── punto: $TH hilos/nodo, $DURATION s"
    tag="th$TH"
    start_sampling "$tag" $(( DURATION + RECOVER ))

    # Solo se espera a los ssh de arranque: un `wait` pelado se quedaría
    # también con el muestreador, que corre todo el punto del barrido.
    launch_pids=()
    for n in "${NODES[@]}"; do
        on_detached "$n" "cd $REMOTE && setsid nohup $LOADER_CMD \
            --sae ${NODE_SAE[$n]} --slave ${SLAVE_OF[$n]} \
            --certs $CERTS_REMOTE --threads $TH --duration $DURATION \
            --number $NUMBER --size $SIZE --aggregate-throttled \
            --out $REMOTE/$tag.csv --record-keys $REMOTE/$tag.keys \
            > $REMOTE/$tag.stdout 2>&1 < /dev/null & exit 0" &
        launch_pids+=($!)
    done
    # Un lanzamiento fallido no debe tumbar el barrido entero: se anota y el
    # punto se evalúa con los nodos que sí arrancaron.
    wait "${launch_pids[@]}" || info "$tag: algún lanzamiento remoto devolvió error"

    sleep $(( DURATION + 10 ))
    info "carga terminada; $RECOVER s de recuperación"
    sleep "$RECOVER"
    stop_sampling

    # Los tres a la vez, comprimiendo y por conexión propia: un CSV de carga
    # sostenida son >100 MB de texto casi idéntico (comprime ~20×), y sacarlo
    # por el socket multiplexado lo serializa con el muestreador — medido en
    # 10 min por punto, más que la carga misma.
    for n in "${NODES[@]}"; do
        (
            ssh -n "${SSH_OPTS[@]}" -o ControlMaster=no -o ControlPath=none "$n" \
                "gzip -c $REMOTE/$tag.csv" > "$DIR/$n.$tag.csv.gz" 2>/dev/null || true
            gunzip -f "$DIR/$n.$tag.csv.gz" 2>/dev/null || true
            scp "${SSH_OPTS[@]}" "$n:$REMOTE/$tag.keys" "$DIR/$n.$tag.keys" >/dev/null 2>&1 || true
        ) &
    done
    wait

    # ── resumen del punto ────────────────────────────────────────────
    python3 - "$DIR" "$tag" "$SUSTAIN_FROM" <<'PY'
import csv, glob, statistics, sys
d, tag, sustain_from = sys.argv[1], sys.argv[2], float(sys.argv[3])
# Dos pasadas en streaming, sin acumular las filas: a 16 hilos cada CSV trae
# más de un millón de líneas y guardarlas como dicts se come varios GB — en un
# portátil de 16 GB eso acaba en OOM, que es justo lo que CLAUDE.md avisa de no
# provocar. Solo se retienen las latencias de las respuestas OK, que son las
# que hacen falta para los percentiles.
files = glob.glob(f"{d}/*.{tag}.csv")

t0 = None
for f in files:                                     # 1ª pasada: origen de tiempos
    with open(f) as fh:
        for r in csv.DictReader(fh):
            t = float(r["t_unix"])
            t0 = t if t0 is None else min(t0, t)
if t0 is None:
    print("  (sin filas)"); raise SystemExit

# Las filas agregadas (--aggregate-throttled) llevan thread=-1 y en key_id un
# "xN" con cuántas peticiones representan; una fila normal cuenta como una.
def weight(r):
    kid = r.get("key_id") or ""
    return int(kid[1:]) if r["thread"] == "-1" and kid.startswith("x") else 1

bad, lat = {}, []
n_ok = keys_burst = keys_sus = 0
sus_min = sus_max = None
for f in files:                                     # 2ª pasada: agregados
    with open(f) as fh:
        for r in csv.DictReader(fh):
            if r["status"] != "200":
                bad[r["status"]] = bad.get(r["status"], 0) + weight(r)
                continue
            n_ok += 1
            lat.append(float(r["latency_ms"]))
            t = float(r["t_unix"])
            n = int(r["n_keys"])
            if t - t0 >= sustain_from:
                keys_sus += n
                sus_min = t if sus_min is None else min(sus_min, t)
                sus_max = t if sus_max is None else max(sus_max, t)
            else:
                keys_burst += n

span_sus = (sus_max - sus_min) if (sus_min is not None and sus_max > sus_min) else 0
lat.sort()
def pct(p):
    return lat[min(int(len(lat)*p), len(lat)-1)] if lat else float("nan")
print(f"  peticiones      : {n_ok + sum(bad.values())}  ok={n_ok}  err={sum(bad.values())}")
print(f"  ráfaga (<{sustain_from:.0f}s): {keys_burst} claves")
print(f"  SOSTENIDO       : {keys_sus} claves en {span_sus:.1f}s = "
      f"{keys_sus/span_sus if span_sus else 0:.1f} claves/s agregadas")
print(f"  latencia ok     : p50={pct(.50):.1f}ms p95={pct(.95):.1f}ms p99={pct(.99):.1f}ms")
if bad:
    print(f"  respuestas ≠200 : {bad}")
    faults = {s: n for s, n in bad.items() if s in ("500", "502", "504")}
    if faults:
        print(f"  ¡FALLOS DE SERVIDOR! {faults} — 429/503 son degradación, esto no")
else:
    print("  respuestas ≠200 : ninguna")
PY

    # ── centinelas del sistema, no del cliente ───────────────────────
    corrupt=$(grep -oa 'recv_corrupt=[0-9]*' "$DIR/$tag.samples.txt" | cut -d= -f2 | sort -rn | head -1 || echo 0)
    check "$tag: recv_corrupt durante la carga" "${corrupt:-0}" "0"
    # 429 y 503 son degradación correcta, no fallos: el 429 es el control de
    # admisión haciendo su trabajo y el 503 es el "no key available" de
    # ETSI-014 cuando el buffer se vacía más rápido de lo que rellena el
    # generator. Lo que no puede aparecer es un 500 (fallo interno) ni un
    # 502/504 (el peer o el ORR caídos): eso sí es que algo se ha roto.
    faults=$(awk -F, 'NR>1 && ($3=="500" || $3=="502" || $3=="504")' "$DIR"/*."$tag".csv 2>/dev/null | wc -l)
    check "$tag: respuestas 500/502/504" "$faults" "0"
    degraded=$(awk -F, 'NR>1 && $3=="503"' "$DIR"/*."$tag".csv 2>/dev/null | wc -l)
    info "$tag: 503 (buffer sin material) = $degraded — esperable al drenar más rápido que el refill"
    maxack=$(grep -oa 'ack_pending=[0-9]*' "$DIR/$tag.samples.txt" | cut -d= -f2 | sort -rn | head -1 || echo 0)
    info "$tag: ack_pending máximo = ${maxack:-0}"
    panics=$(for n in "${NODES[@]}"; do dlogs "$n" dkms 200; dlogs "$n" qkc 200; done \
             | grep -ac 'panicked at' || true)
    check "$tag: panics durante la carga" "$panics" "0"

    # ── integridad por muestreo: 20 claves de las emitidas ───────────
    # Si dos claves de transporte divergieran, esto es lo único que lo vería.
    for n in "${NODES[@]}"; do
        kf="$DIR/$n.$tag.keys"
        [[ -s "$kf" ]] || continue
        slave="${SLAVE_OF[$n]}"
        # host del esclavo
        shost=""
        for m in "${NODES[@]}"; do [[ "${NODE_SAE[$m]}" == "$slave" ]] && shost="$m"; done
        [[ -n "$shost" ]] || continue
        okc=0; badc=0
        while IFS=, read -r kid digest; do
            [[ -n "$kid" ]] || continue
            got=$(dec_keys "$shost" "$slave" "${NODE_SAE[$n]}" "$kid" 2>/dev/null \
                  | jq -r '.keys[0].key // empty' || true)
            [[ -z "$got" ]] && continue          # ya consumida o no entregada: no es el objeto del test
            gd=$(printf '%s' "$got" | sha256sum | cut -d' ' -f1)
            if [[ "$gd" == "$digest" ]]; then okc=$((okc+1)); else badc=$((badc+1)); fi
        done < <(shuf -n 20 "$kf" 2>/dev/null || head -20 "$kf")
        if (( badc == 0 )); then pass "$n→$slave: $okc claves muestreadas, bytes idénticos"
        else fail "$n→$slave: $badc de $((okc+badc)) claves con BYTES DISTINTOS bajo carga"; fi
    done
done

# ── ¿escala con la concurrencia? ──────────────────────────────────────
if (( ${#SWEEP[@]} > 1 )); then
    info "── comparativa del barrido (la tasa sostenida NO debe caer al subir hilos)"
    info "   una caída de 16→64 hilos apunta a falta de backpressure, que es"
    info "   exactamente el patrón del bug spawn-then-acquire ya arreglado en el QKC"
fi

info "artefactos en $DIR"
summary
