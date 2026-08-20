#!/usr/bin/env bash
# T31 — quitar el nodo D y comprobar que el suelo local aguanta.
#
# La aserción central es una asimetría: node-c debe tirar el enlace al 4 —el
# que le dio la SDN— y CONSERVAR los suyos al 1 y al 2, que están en su
# node.yml. Ese es el invariante «node.yml es un suelo, no una foto»: sin él,
# cada vez que la SDN va por detrás se destruyen enlaces vivos y material de
# clave vivo.
#
#   ./t31_remove_node.sh
#   ./t31_remove_node.sh --host dkms-node-d --ttl 90
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

D_HOST="${D_HOST:-$SDN_HOST}"
D_DIR=site4
TTL=90            # presence_ttl_secs por defecto de la SDN
while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) D_HOST="$2"; shift 2 ;;
        --dir)  D_DIR="$2";  shift 2 ;;
        --ttl)  TTL="$2";    shift 2 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done

DIR="$(mkoutdir remove-node)"
c=dkms-node-c

before=$(sdn_get /topology)
v_before=$(jq -r '.version' <<<"$before")
info "topología antes: $before"

# Si el nodo ya no estaba (p. ej. se relanza el test), no hay baja que
# registrar y la versión NO debe subir: un anuncio que no cambia nada no la
# toca, que es justo el invariante de idempotencia. Sin esta distinción el
# test se contradice a sí mismo al repetirlo.
HABIA_NODO_D=1
[[ "$(jq -r '.qkcs' <<<"$before")" == "3" ]] && HABIA_NODO_D=0
(( HABIA_NODO_D )) || info "el nodo D ya no estaba: se comprueba el estado final, no la transición"

# uptime de los contenedores, para detectar reinicios provocados por la baja
for n in "${NODES[@]}"; do
    on "$n" "docker ps --filter 'name=site-' --format '{{.Names}} {{.RunningFor}}'" \
        > "$DIR/$n.uptime.before"
done

info "── parando el nodo D"
on "$D_HOST" "cd $D_DIR && docker compose -f site.yml down"

info "── esperando a que expire la presencia (ttl $TTL s + margen)"
topo_is() { [[ "$(sdn_get /topology | jq -r ".$1")" == "$2" ]]; }
seen()    { dlogs_since "$1" "$2" "${4:-5m}" | grep -qa "$3"; }

wait_for $(( TTL + 60 )) "SDN: QKCs 4→3"    topo_is qkcs 3
wait_for 60               "SDN: ORRs 4→3"    topo_is orrs 3
wait_for 60               "SDN: DKMS 4→3"    topo_is dkms 3
wait_for 60               "SDN: aristas 4→3" topo_is edges 3

v_after=$(sdn_get /topology | jq -r '.version')
if (( HABIA_NODO_D )); then
    if (( v_after > v_before )); then
        pass "topology.version subió con la baja ($v_before → $v_after)"
    else
        fail "topology.version no subió: la SDN no ha registrado la baja"
    fi
elif (( v_after == v_before )); then
    pass "topology.version no se movió sin cambios reales ($v_before): anuncios idempotentes"
else
    fail "topology.version subió ($v_before → $v_after) sin que cambiara nada — cada bump re-empuja forwarding y re-corre el LP"
fi

# ─── la asimetría ─────────────────────────────────────────────────────
#
# Se consulta el admin del QKC, no los logs: `keystore.levels` sigue apareciendo
# en la ventana de `docker logs --since` un rato después de que el enlace se
# haya ido, y eso daba un falso negativo. `/stats` es el estado, no su eco.
info "── node-c: pierde el enlace que le dio la SDN, conserva los suyos"
links_of() { qkc_get "$1" /stats | jq -r '.links | keys[]' 2>/dev/null | sort | tr '\n' ' '; }
info "node-c enlaces ahora: $(links_of "$c")"

for p in 1 2; do
    if [[ " $(links_of "$c") " == *" $p "* ]]; then
        pass "node-c: conserva su enlace declarado al $p"
    else
        fail "node-c: PERDIÓ el enlace al $p, que está en su node.yml — el suelo local no se respeta"
    fi
done

peer4_gone() { [[ " $(links_of "$c") " != *" 4 "* ]]; }
wait_for 120 "node-c: el enlace al 4 desaparece de sus links" peer4_gone

fwd4_gone() { ! qkc_get "$c" /forwarding-table | jq -e 'has("4")' >/dev/null 2>&1; }
wait_for 60 "node-c: el 4 desaparece de su tabla de forwarding" fwd4_gone

for n in "${NODES[@]}"; do
    wait_for 120 "$n: ORR retira orr_4" seen "$n" orr 'par retirado.*orr_4'
done

# ─── nada se ha reiniciado ────────────────────────────────────────────
for n in "${NODES[@]}"; do
    on "$n" "docker ps --filter 'name=site-' --format '{{.Names}} {{.RunningFor}}'" \
        > "$DIR/$n.uptime.after"
    restarts=$(on "$n" "docker ps --filter 'name=site-' --format '{{.Names}}' | \
        xargs -r docker inspect -f '{{.RestartCount}}' | awk '{s+=\$1} END{print s+0}'")
    check "$n: contenedores sin reiniciar tras la baja" "$restarts" "0"
    panics=$(dlogs "$n" qkc 200 | grep -ac 'panicked at' || true)
    check "$n: sin panics en el QKC" "$panics" "0"
done

# ─── y la red de 3 sigue funcionando ─────────────────────────────────
info "── el triángulo original debe seguir entregando claves"
if "$(dirname "${BASH_SOURCE[0]}")/t10_keys_smoke.sh" >/dev/null 2>&1; then
    pass "T10 pasa después de la baja (6/6 flujos)"
else
    fail "T10 falla después de la baja: la salida de D se ha llevado algo por delante"
fi

summary
