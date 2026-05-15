#!/usr/bin/env bash
# Ramp test: arranca 10 SAEs en t=0, +10 cada STEP_SECONDS hasta N_MAX.
# Cada SAE corre `sae_sim.py` contra su DKMS asignado aleatoriamente
# (semilla reproducible) y empuja enc_keys hasta agotar el duración.
#
# Variables de entorno:
#   N_INITIAL          (default 10)  - SAEs activos al comienzo
#   N_MAX              (default 100) - SAEs totales al final del ramp
#   STEP               (default 10)  - SAEs añadidos por step
#   STEP_SECONDS       (default 10)  - espera entre steps
#   SUSTAIN_SECONDS    (default 30)  - tiempo extra tras llegar a N_MAX
#   SEED               (default 42)  - semilla para la asignación SAE→DKMS
#   SIZE_BITS          (default 256) - tamaño de cada key pedida
#   NUMBER             (default 1)   - claves por petición
#   RATE_CAP           (default 0)   - cap keys/s por SAE (0=ilimitado)
#
# Salida:
#   /tmp/dkms-star-demo/ramp/sae_NNN.log  por cada SAE (timestamp status)
#   /tmp/dkms-star-demo/ramp/assignment.csv  sae,dkms,slave,start_t
#   /tmp/dkms-star-demo/ramp/start_unix.txt  timestamp t0 (para offset en plots)

set -euo pipefail
N_INITIAL=${N_INITIAL:-10}
N_MAX=${N_MAX:-100}
STEP=${STEP:-10}
STEP_SECONDS=${STEP_SECONDS:-10}
SUSTAIN_SECONDS=${SUSTAIN_SECONDS:-30}
SEED=${SEED:-42}
SIZE_BITS=${SIZE_BITS:-256}
NUMBER=${NUMBER:-1}
RATE_CAP=${RATE_CAP:-0}

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$(cd "$(dirname "$0")" && pwd)"
TLS="$HERE/tls"
LOGS=/tmp/dkms-star-demo/ramp
PIDS=$LOGS/sae-pids
mkdir -p "$LOGS"
: > "$PIDS"

# Asignación reproducible SAE → DKMS y slave SAE.
# Generamos la matriz con Python (mejor que awk/bash para semilla).
python3 - <<PYEOF > "$LOGS/assignment.csv"
import random
random.seed($SEED)
dkms_ids = [11, 22, 33, 44]
n_max = $N_MAX
print("sae_id,home_dkms,slave_sae,dkms_url,sae_port")
saes = [f"sae_{i:03d}" for i in range(1, n_max + 1)]
for sae in saes:
    home = random.choice(dkms_ids)
    # slave = otro SAE distinto al propio, también seeded
    others = [s for s in saes if s != sae]
    slave = random.choice(others)
    port = 8400 + home  # 8411, 8422, 8433, 8444
    print(f"{sae},dkms-{home},{slave},https://127.0.0.1:{port},{port}")
PYEOF

echo "── ramp test: $N_INITIAL → $N_MAX SAEs, step=$STEP/$STEP_SECONDS s, sustain=${SUSTAIN_SECONDS}s, seed=$SEED"
echo "   asignación SAE→DKMS en $LOGS/assignment.csv"
echo

# Total duración de cada SAE: tiempo desde su arranque hasta el final
# del experimento. Los SAEs que arrancan primero corren más tiempo.
TOTAL_RAMP=$(( (N_MAX - N_INITIAL) / STEP * STEP_SECONDS ))
TOTAL_DURATION=$(( TOTAL_RAMP + SUSTAIN_SECONDS ))
echo "── duración total: ${TOTAL_DURATION}s ($TOTAL_RAMP ramp + $SUSTAIN_SECONDS sustain)"

# Lee assignment.csv y arranca en orden por SAE id (sae_001 primero).
START_UNIX=$(date +%s.%N)
echo "$START_UNIX" > "$LOGS/start_unix.txt"
i=0
while IFS=, read -r sae_id home_dkms slave_sae dkms_url sae_port; do
    if [ "$sae_id" = "sae_id" ]; then continue; fi
    i=$((i + 1))
    # Calcular en qué step debe arrancar este SAE.
    if [ "$i" -le "$N_INITIAL" ]; then
        delay_from_start=0
    else
        steps_after_initial=$(( (i - N_INITIAL + STEP - 1) / STEP ))
        delay_from_start=$(( steps_after_initial * STEP_SECONDS ))
    fi
    # Espera hasta delay_from_start desde START_UNIX
    target_t=$(awk "BEGIN{printf \"%.3f\", $START_UNIX + $delay_from_start}")
    now=$(date +%s.%N)
    sleep_for=$(awk "BEGIN{d=$target_t - $now; if (d<0) d=0; printf \"%.3f\", d}")
    if awk "BEGIN{exit !($sleep_for > 0.01)}"; then
        sleep "$sleep_for"
    fi
    # Cuánto tiempo le queda al SAE de aquí al final.
    remaining=$(awk "BEGIN{r=$START_UNIX + $TOTAL_DURATION - $(date +%s.%N); if (r<1) r=1; printf \"%.0f\", r}")
    logf="$LOGS/${sae_id}.log"
    : > "$logf"
    nohup python3 "$HERE/sae_sim.py" \
        --sae-id "$sae_id" \
        --slave "$slave_sae" \
        --url "$dkms_url" \
        --cert "$TLS/${sae_id}.crt" \
        --key  "$TLS/${sae_id}.key" \
        --ca   "$TLS/ca.crt" \
        --log  "$logf" \
        --duration "$remaining" \
        --size-bits "$SIZE_BITS" \
        --number "$NUMBER" \
        --rate-cap "$RATE_CAP" \
        > /dev/null 2>&1 &
    echo "$! $sae_id" >> "$PIDS"
    if [ "$((i % 10))" -eq 0 ] || [ "$i" -le "$N_INITIAL" ]; then
        t=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) - $START_UNIX}")
        echo "  [t=${t}s] arrancado $sae_id → $home_dkms (slave $slave_sae) — total activos $i"
    fi
done < "$LOGS/assignment.csv"

echo
echo "── todos los $N_MAX SAEs arrancados. Esperando que terminen…"
echo "   logs en $LOGS/sae_*.log"
echo "   PIDs en $PIDS"

# Esperar a que todos terminen
while read -r pid sae; do
    wait "$pid" 2>/dev/null || true
done < "$PIDS"

END_UNIX=$(date +%s.%N)
TOTAL=$(awk "BEGIN{printf \"%.1f\", $END_UNIX - $START_UNIX}")
echo "── todos los SAEs finalizados. wall-time total = ${TOTAL}s"
echo
echo "  python3 $HERE/plot_ramp.py  # genera gráficas"
