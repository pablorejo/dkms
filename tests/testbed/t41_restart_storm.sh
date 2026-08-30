#!/usr/bin/env bash
# T43 — tormenta de reinicios: ¿se resincronizan las ventanas de épocas?
#
# Un reinicio suelto lo cubre T40. Lo que este ejercita es lo que rompía el
# enlace de forma permanente (encontrado el 2026-08-03): encadenando varias
# tandas, los dos extremos acababan con ventanas de épocas separadas, cada uno
# cifrando con una que el otro no tenía. Síntoma: `dec_misses == dec_lookups`
# en el `/stats` del QKC, `recv=0` en el DKMS del peer y `expired` subiendo sin
# parar. No se recuperaba solo; hacía falta reiniciar el despliegue entero.
#
# El arreglo: el lado DEC, al descartar claves de una época que no tiene, pide
# resincronizar con la época MÁS ALTA que ha visto, y el iniciador renegocia un
# bloque por encima de las dos ventanas (`SecretStore::request_resync` +
# `relink(peer_epoch)`).
#
#   ./t41_restart_storm.sh              # 6 ciclos
#   ./t41_restart_storm.sh --cycles 10
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

CYCLES=6
while [[ $# -gt 0 ]]; do
    case "$1" in
        --cycles) CYCLES="$2"; shift 2 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done

DIR="$(mkoutdir restart-storm)"

# Tráfico se mide con `dec_lookups` y pérdida con `wait_dec_timeouts` (la clave
# no apareció nunca). Ni `dec_misses` ni `wait_dec_called` sirven de métrica: un
# miss solo dice que la clave aún no estaba materializada y `wait_dec` la
# resuelve enseguida, y en un enlace perfectamente sano `wait_dec` ni siquiera
# se llama — usarlo de denominador hacía que "sano" se leyera como "sin datos".
# En DELTA, no en acumulado: los contadores viven desde el arranque.
misses_snapshot() {
    local out=""
    for n in "${NODES[@]}"; do
        local s
        s=$(qkc_get "$n" /stats 2>/dev/null || echo '{}')
        out+="$n $(jq -rc '[.links // {} | to_entries[] | "\(.key):\(.value.dec_lookups):\(.value.wait_dec_timeouts)"] | join(" ")' <<<"$s" 2>/dev/null || echo '')"$'\n'
    done
    printf '%s' "$out"
}

info "── tormenta: $CYCLES reinicios alternando extremos, sin pausa entre tandas"
misses_snapshot > "$DIR/misses.before"
cat "$DIR/misses.before" >&2

for i in $(seq 1 "$CYCLES"); do
    victim="${NODES[$(( i % ${#NODES[@]} ))]}"
    info "ciclo $i/$CYCLES: reinicio el QKC de $victim"
    on "$victim" "docker restart site-qkc-1" >/dev/null
    sleep 12
done

info "── dejando converger (90 s)"
sleep 90

# ─── ¿se resincronizaron? ─────────────────────────────────────────────
#
# Se mira el delta de los últimos 60 s de tráfico, no el acumulado: durante la
# tormenta es normal perder frames en vuelo. Lo que no vale es que sigan
# fallando DESPUÉS de converger.
#
# Y hace falta TRÁFICO para medir: con los buffers llenos no hay lookups, la
# ventana sale vacía y el test pasaría sin haber comprobado nada. Se drena a
# los SAEs durante la ventana para forzar refill, que es lo que mueve claves
# por el enlace.
info "── midiendo fallos de clave DEC ya convergido, con carga para provocar tráfico"
REMOTE=/tmp/dkms-testbed
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
declare -A SLAVE_OF
for i in "${!NODES[@]}"; do
    nxt=$(( (i + 1) % ${#NODES[@]} ))
    SLAVE_OF["${NODES[$i]}"]="${NODE_SAE[${NODES[$nxt]}]}"
done

# La recuperación es REACTIVA: el lado DEC solo se entera de que las ventanas
# se han separado cuando le llega un frame que no puede descifrar. Un enlace
# ocioso y roto sigue roto hasta que alguien manda algo. Así que primero se
# arranca la carga, se le dan 40 s para que dispare la resincronización y la
# complete, y solo entonces empieza la ventana de medida — si no, se estaría
# midiendo la propia convergencia y saldría rojo con el arreglo funcionando.
for n in "${NODES[@]}"; do
    on_detached "$n" "cd $REMOTE && setsid nohup $LOADER_CMD \
        --sae ${NODE_SAE[$n]} --slave ${SLAVE_OF[$n]} --certs $CERTS_REMOTE \
        --threads 2 --duration 120 --rate-cap 20 --aggregate-throttled \
        --out $REMOTE/storm.csv > $REMOTE/storm.stdout 2>&1 < /dev/null & exit 0" || true
done
info "── 40 s para que el tráfico dispare y complete la resincronización"
sleep 40
misses_snapshot > "$DIR/misses.mid"
sleep 60
misses_snapshot > "$DIR/misses.after"

python3 - "$DIR/misses.mid" "$DIR/misses.after" >&2 <<'PY'
import sys

def parse(path):
    out = {}
    for line in open(path):
        parts = line.split()
        if not parts:
            continue
        node = parts[0]
        for entry in parts[1:]:
            peer, lookups, timeouts = entry.split(":")
            out[(node, peer)] = (int(lookups), int(timeouts))
    return out

mid, after = parse(sys.argv[1]), parse(sys.argv[2])
bad = medidos = 0
for key in sorted(after):
    l1, m1 = mid.get(key, (0, 0))
    l2, m2 = after[key]
    dl, dm = l2 - l1, m2 - m1
    node, peer = key
    if dl <= 0:
        print(f"  {node} → {peer}: sin tráfico en la ventana")
        continue
    medidos += 1
    pct = 100 * dm / dl
    flag = "   ← SIGUE DESINCRONIZADO" if pct > 5 else ""
    print(f"  {node} → {peer}: {dm}/{dl} frames perdidos ({pct:.1f} %){flag}")
    if pct > 5:
        bad += 1
print(f"  ({medidos} enlaces con tráfico medible, {bad} desincronizados)")
# Sin un solo enlace medible el test no ha comprobado nada: eso NO es un
# aprobado, es un test que no ha corrido.
sys.exit(2 if medidos == 0 else (1 if bad else 0))
PY
rc=$?
case $rc in
    0) pass "todos los enlaces descifran tras la tormenta de $CYCLES reinicios" ;;
    2) fail "ningún enlace tuvo tráfico medible: el test no ha comprobado nada" ;;
    *) fail "hay enlaces que siguen tirando lo que reciben: las ventanas de épocas no resincronizan" ;;
esac

# La prueba de fuego es que las claves vuelvan a fluir extremo a extremo.
if "$(dirname "${BASH_SOURCE[0]}")/t10_keys_smoke.sh" >/dev/null 2>&1; then
    pass "T10 pasa 6/6 tras la tormenta"
else
    fail "T10 falla tras la tormenta"
fi

# Y que el iniciador haya dejado constancia de por qué renegoció.
for n in "${NODES[@]}"; do
    if dlogs_since "$n" qkc 10m | grep -qa 'relink.*peer_epoch=[1-9]'; then
        info "$n: renegoció citando una época del peer que no tenía (resync)"
    fi
done

summary
