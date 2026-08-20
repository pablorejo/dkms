#!/usr/bin/env bash
# T42 — reconexión sobre un enlace OCIOSO.
#
# El caso que arregla el commit «qkc: notice a peer that hung up while the link
# was idle». Un enlace ocioso escondía la reconexión entera: el writer solo se
# enteraba de que el socket estaba muerto cuando iba a escribir, y con los
# buffers del DKMS llenos no escribe nunca. `writer_loop` ahora espera en la
# cola O en `stream.readable()`; como el canal hacia el peer es de un solo
# sentido, readable significa EOF o error.
#
# El test consiste en construir exactamente ese estado —buffers llenos, cero
# tráfico— y comprobar que el extremo vivo se entera SIN escribir nada.
#
#   ./t50_idle_reconnect.sh
#   ./t50_idle_reconnect.sh --victim dkms-node-c
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

VICTIM=dkms-node-c
WITNESS=dkms-node-a
while [[ $# -gt 0 ]]; do
    case "$1" in
        --victim)  VICTIM="$2";  shift 2 ;;
        --witness) WITNESS="$2"; shift 2 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done

DIR="$(mkoutdir idle-reconnect)"
seen() { dlogs_since "$1" "$2" "${4:-3m}" | grep -qa "$3"; }

# ─── 1. asegurar el estado ocioso ─────────────────────────────────────
info "── comprobando que el enlace está de verdad ocioso"
info "   (si algún SAE está pidiendo claves, el writer escribe y el test no"
info "    prueba nada: mata primero cualquier sae_load.py)"
for n in "${NODES[@]}"; do
    on "$n" "pkill -f sae_load.py || true" >/dev/null 2>&1 || true
done
sleep 20

# Buffers llenos = no hay refill pendiente = no hay escrituras hacia el peer.
lv=$(dlogs_since "$WITNESS" qkc 60s | grep -a 'keystore.levels' | tail -3 || true)
printf '%s\n' "$lv" >&2
gs=$(dlogs_since "$WITNESS" dkms 30s | grep -a 'generator.state' | tail -2 || true)
printf '%s\n' "$gs" >&2
encmin=$(grep -oa ' enc=[0-9]*' <<<"$gs" | cut -d= -f2 | sort -n | head -1 || echo 0)
check_ge "$WITNESS: buffers DKMS llenos antes de empezar" "${encmin:-0}" 1000

# ─── 2. matar el extremo y NO generar tráfico ─────────────────────────
info "── reiniciando el QKC de $VICTIM sin ninguna carga en el sistema"
t0=$(date +%s)
on "$VICTIM" "docker restart site-qkc-1" >/dev/null

# Lo que se afirma: se entera por EOF, no por un intento de escritura.
wait_for 30 "$WITNESS: detecta el EOF con la cola vacía" \
    seen "$WITNESS" qkc 'peer_hung_up'
wait_for 60 "$WITNESS: relink tras el hangup" \
    seen "$WITNESS" qkc 'qkc.pqc.relink completado'

# ─── 3. el enlace queda operativo sin que nadie lo despierte ─────────
info "── el enlace debe quedar servible SIN que nadie pida una clave"
sleep 30
link_alive() {
    dlogs_since "$VICTIM" qkc 60s | grep -a 'keystore.levels' | tail -3 \
        | grep -qav 'enc=0 dec=0 taken=0'
}
wait_for 120 "$VICTIM: keystore con material tras el relink" link_alive

# Solo AHORA se pide una clave: si hiciera falta pedirla para que el enlace
# despertara, el bug seguiría ahí y este orden lo distingue.
if "$(dirname "${BASH_SOURCE[0]}")/t10_keys_smoke.sh" >/dev/null 2>&1; then
    pass "T10 pasa 6/6 tras la reconexión en ocioso"
else
    fail "T10 falla tras la reconexión en ocioso"
fi
info "recuperación completa en $(( $(date +%s) - t0 )) s"

# ─── 4. el rate-limit de relink ──────────────────────────────────────
#
# `RELINK_MIN_INTERVAL` (5 s) solo debe actuar si las dos reconexiones caen
# dentro de esa ventana. `docker restart` normal tarda ~10 s en el ciclo por el
# stop grace, así que dos reinicios seguidos quedaban a 12 s y DOS relinks eran
# la respuesta correcta — el test fallaba por su propia premisa. Con `-t 0` el
# rebote es inmediato; aun así se mide la separación real y solo se afirma el
# rate-limit cuando la precondición se cumplió de verdad.
info "── RELINK_MIN_INTERVAL: dos rebotes dentro de 5 s → un solo relink"
since2=$(date -u +%Y-%m-%dT%H:%M:%SZ)
on "$VICTIM" "docker restart -t 0 site-qkc-1" >/dev/null
on "$VICTIM" "docker restart -t 0 site-qkc-1" >/dev/null
sleep 45

mapfile -t hangups < <(dlogs_since "$WITNESS" qkc "$since2" \
    | grep -a 'peer_hung_up' | grep -oE '^[0-9-]+T[0-9:]+' || true)
relinks=$(dlogs_since "$WITNESS" qkc "$since2" | grep -ac 'relink completado' || true)

if (( ${#hangups[@]} < 2 )); then
    info "solo se observaron ${#hangups[@]} reconexiones: no se puede evaluar el rate-limit"
else
    gap=$(( $(date -u -d "${hangups[-1]}Z" +%s) - $(date -u -d "${hangups[0]}Z" +%s) ))
    info "separación entre la primera y la última reconexión: ${gap}s"
    if (( gap <= 5 )); then
        if (( relinks <= 1 )); then
            pass "dos rebotes en ${gap}s produjeron $relinks relink(s)"
        else
            fail "dos rebotes en ${gap}s produjeron $relinks relinks: RELINK_MIN_INTERVAL no frena el flapping"
        fi
    else
        info "los rebotes quedaron a ${gap}s (> 5 s): $relinks relinks es lo correcto, no se evalúa el límite"
    fi
fi
if dlogs_since "$WITNESS" qkc "$since2" | grep -qa 'relink omitido'; then
    info "visto además 'qkc.pqc.relink omitido (demasiado seguido)'"
fi

# Dejar el sistema sano antes de irse.
sleep 30
link_alive && pass "el enlace queda sano al terminar" || fail "el enlace queda caído al terminar"

summary
