#!/usr/bin/env bash
# T30 — añadir un nodo entero SIN TOCAR los que ya corren.
#
# Es la prueba de la rama `auto_conf_peers`. El nodo D declara un solo enlace
# (hacia el QKC 3) y ningún peer de ORR/DKMS: todo lo demás tiene que llegar
# por la respuesta al anuncio de la SDN, y en a/b/c no se edita ni un fichero.
#
# La aserción que le da sentido a todo lo demás es la última: los sha256 de los
# nueve node.yml de a/b/c idénticos antes y después.
#
#   ./t30_add_node.sh                      # nodo D en la VM de la SDN (~/site4)
#   ./t30_add_node.sh --host dkms-node-d   # nodo D en una VM nueva
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh scp

D_HOST="${D_HOST:-$SDN_HOST}"
D_DIR=site4
while [[ $# -gt 0 ]]; do
    case "$1" in
        --host) D_HOST="$2"; shift 2 ;;
        --dir)  D_DIR="$2";  shift 2 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done
D_IP=$(on "$D_HOST" "hostname -I | awk '{print \$1}'")
info "nodo D en $D_HOST ($D_IP), directorio ~/$D_DIR"

DIR="$(mkoutdir add-node)"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ─── 1. huella de los node.yml de a/b/c ───────────────────────────────
info "── huella de la configuración de los nodos que ya corren"
: > "$DIR/nodeyml.before"
for n in "${NODES[@]}"; do
    on "$n" "sha256sum site/node.*.yml" >> "$DIR/nodeyml.before"
done
cat "$DIR/nodeyml.before" >&2

# ─── 2. estado previo ─────────────────────────────────────────────────
before=$(sdn_get /topology)
v_before=$(jq -r '.version' <<<"$before")
info "topología antes: $before"
for n in "${NODES[@]}"; do
    qkc_get "$n" /forwarding-table > "$DIR/$n.fwd.before.json" 2>/dev/null || true
done
# nivel de keystore por peer antes: ningún enlace preexistente puede caerse
# durante el alta (el bug de laboratorio: un QKC tirando sus propios enlaces).
for n in "${NODES[@]}"; do
    dlogs "$n" qkc 60 | grep -a 'keystore.levels' | tail -4 > "$DIR/$n.levels.before" || true
done

# ─── 3. desplegar D ───────────────────────────────────────────────────
info "── desplegando el nodo D"
on "$D_HOST" "mkdir -p $D_DIR/certs"
for f in node.qkc.yml node.orr.yml node.dkms.yml; do
    sed "s/__ADVERTISE_IP__/$D_IP/g" "$HERE/node-d/$f" > "$DIR/$f"
    scp "${SSH_OPTS[@]}" "$DIR/$f" "$D_HOST:$D_DIR/$f" >/dev/null
done
scp "${SSH_OPTS[@]}" "$REPO_ROOT/docker/compose/site.yml" "$D_HOST:$D_DIR/site.yml" >/dev/null

# .env: mismo TAG que corren los nodos, o la comparación no vale
TAG=$(on "${NODES[0]}" "grep '^TAG=' site/.env | cut -d= -f2" || echo latest)
on "$D_HOST" "printf 'IMAGE_PREFIX=pablopio\nTAG=%s\n' '$TAG' > $D_DIR/.env"
info "usando TAG=$TAG (el mismo que a/b/c)"

# certs: dkms-4 ya está en ~/site4/certs; sae_4 lo pone mint_sae_certs.sh
on "$D_HOST" "ls $D_DIR/certs/dkms-4.crt >/dev/null" || {
    fail "falta $D_HOST:~/$D_DIR/certs/dkms-4.crt — emitirlo con la CA viva antes de seguir"
    summary; exit 1
}

on "$D_HOST" "cd $D_DIR && docker compose -f site.yml up -d"
t_up=$(date +%s)

# ─── 4. convergencia ──────────────────────────────────────────────────
# 3 heartbeats con sdn_announce_secs=15 → 45 s de margen holgado.
info "── esperando convergencia (≤ 60 s)"

topo_is() { [[ "$(sdn_get /topology | jq -r ".$1")" == "$2" ]]; }
# grep sobre los logs recientes de un módulo; se usa como predicado de wait_for
seen() { dlogs_since "$1" "$2" "${4:-5m}" | grep -qa "$3"; }

wait_for 60 "SDN: QKCs 3→4"  topo_is qkcs 4
wait_for 60 "SDN: ORRs 3→4"  topo_is orrs 4
wait_for 60 "SDN: DKMS 3→4"  topo_is dkms 4
wait_for 60 "SDN: SAEs 3→4"  topo_is saes 4
wait_for 60 "SDN: aristas 3→4" topo_is edges 4

after=$(sdn_get /topology)
v_after=$(jq -r '.version' <<<"$after")
if (( v_after > v_before )); then pass "topology.version subió ($v_before → $v_after)"
else fail "topology.version no subió ($v_before → $v_after): la SDN no ha visto el alta"; fi

# ─── 5. el vecino de grafo (node-c) crea el enlace por orden de la SDN ─
info "── node-c es el único vecino de grafo del 4"
c=dkms-node-c
wait_for 60 "node-c: 'enlace nuevo, dicho por la SDN' peer=4" \
    seen "$c" qkc 'enlace nuevo.*peer=4'

# Registrar el vecino en el forwarding NO es opcional: sin eso el enlace existe
# y nunca se enruta, que es idéntico al bug que este código reemplazó.
# La tabla es {"<qkc_destino>": [{"qkc_id": <siguiente salto>, "weight": w}]}.
fwd_has_4() {
    qkc_get "$c" /forwarding-table 2>/dev/null | jq -e 'has("4")' >/dev/null
}
wait_for 60 "node-c: el 4 aparece en su tabla de forwarding" fwd_has_4
qkc_get "$c" /forwarding-table > "$DIR/$c.fwd.after.json" 2>/dev/null || true

wait_for 90 "handshake PQC entre QKC 3 y 4" \
    seen "$c" qkc 'handshake.established.*peer=4'

keystore_4_alive() {
    dlogs_since "$c" qkc 2m | grep -a 'keystore.levels peer=4' | tail -1 \
        | grep -qav 'enc=0 '
}
wait_for 90 "node-c: keystore con el peer 4 por encima de cero" keystore_4_alive

# ─── 6. a y b NO crean enlace QKC (no son vecinos de grafo) ───────────
for n in dkms-node-a dkms-node-b; do
    if dlogs_since "$n" qkc 5m | grep -qa 'enlace nuevo.*peer=4'; then
        fail "$n: creó un enlace QKC al 4 sin ser vecino de grafo"
    else
        pass "$n: sin enlace QKC al 4 (correcto: solo el 3 es su vecino)"
    fi
done

# ─── 7. ORR y DKMS de los tres SÍ aprenden al nuevo (son E2E) ─────────
for n in "${NODES[@]}"; do
    wait_for 60 "$n: ORR arranca bootstrap con orr_4" seen "$n" orr 'par nuevo.*orr_4'
    wait_for 90 "$n: DKMS incorpora dkms-4 como peer"  seen "$n" dkms 'dkms-4'
done

# ─── 8. lo que ya corría sigue corriendo ──────────────────────────────
for n in "${NODES[@]}"; do
    dlogs "$n" qkc 60 | grep -a 'keystore.levels' | tail -4 > "$DIR/$n.levels.after" || true
    dead=$(grep -ac 'enc=0 dec=0 taken=0' "$DIR/$n.levels.after" || true)
    check "$n: ningún enlace preexistente caído durante el alta" "$dead" "0"
    up=$(on "$n" "docker ps --filter 'name=site-' --format '{{.Names}}' | wc -l")
    check "$n: sigue con sus 3 contenedores" "$up" "3"
done

# ─── 8b. el nodo nuevo intercambia material con TODOS, sin corrupción ─
#
# La comprobación que hacía falta: el 2026-08-02 el par 1↔4 entregaba el 100 %
# de las claves corruptas —dos ORRs con master_secret distinto— y desde fuera
# solo se veía que la clave e2e no salía. Mirando `recv_corrupt` y `acked` por
# peer se ve exactamente qué par está roto y en qué sentido.
info "── el nodo nuevo debe intercambiar material con los tres, sin corrupción"
sleep 30
d_state=$(on "$D_HOST" "docker logs --tail 40 site4-dkms-1 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
          | grep -a generator.state | tail -3" || true)
printf '%s\n' "$d_state" | cut -c1-200 >&2
for peer in dkms-1 dkms-2 dkms-3; do
    line=$(printf '%s\n' "$d_state" | grep -a "peer=$peer" | tail -1 || true)
    if [[ -z "$line" ]]; then
        fail "nodo D: sin generator.state para $peer"
        continue
    fi
    corrupt=$(printf '%s\n' "$line" | max_field 'recv_corrupt=[0-9]*')
    check "nodo D → $peer: recv_corrupt" "${corrupt:-?}" "0"
    enc=$(printf '%s\n' "$line" | max_field ' enc=[0-9]*')
    check_ge "nodo D → $peer: buffer enc con material" "${enc:-0}" 1
done
# Y el otro sentido: ningún nodo existente debe estar descartando lo del nuevo.
for n in "${NODES[@]}"; do
    line=$(dlogs_since "$n" dkms 2m | grep -a "generator.state peer=dkms-4" | tail -1 || true)
    [[ -z "$line" ]] && { fail "$n: sin generator.state para dkms-4"; continue; }
    corrupt=$(printf '%s\n' "$line" | max_field 'recv_corrupt=[0-9]*')
    check "$n → dkms-4: recv_corrupt" "${corrupt:-?}" "0"
done

# ─── 9. e2e con el nodo nuevo: sae_4 → sae_1, multi-hop 4→3→1 ────────
info "── clave extremo a extremo con el nodo nuevo (ejercita el forwarding 4→3→1)"
sleep 20   # margen para que dkms-4 tenga buffer con dkms-1
CERTS_D="/home/debian/$D_DIR/certs"
resp=$(on "$D_HOST" "curl -sf --max-time 30 \
    --cacert $CERTS_D/ca.crt --cert $CERTS_D/sae_4.crt --key $CERTS_D/sae_4.key \
    -H 'Content-Type: application/json' -d '{\"number\":1,\"size\":256}' \
    https://127.0.0.1:$DKMS_SAE/api/v1/keys/sae_1/enc_keys" 2>/dev/null || true)
kid=$(jq -r '.keys[0].key_ID // empty' <<<"${resp:-{\}}")
kenc=$(jq -r '.keys[0].key // empty'   <<<"${resp:-{\}}")
if [[ -z "$kid" ]]; then
    fail "sae_4 → sae_1: enc_keys no devolvió clave (¿buffer de dkms-4 aún vacío? reintentar en 60 s)"
else
    kdec=$(dec_keys dkms-node-a sae_1 sae_4 "$kid" 2>/dev/null | jq -r '.keys[0].key // empty' || true)
    if [[ -n "$kdec" && "$kenc" == "$kdec" ]]; then
        pass "sae_4 → sae_1: clave idéntica en ambos extremos por el camino 4→3→1"
    else
        fail "sae_4 → sae_1: la clave no llegó igual (enc=${kenc:0:16}… dec=${kdec:0:16}…)"
    fi
fi

# ─── 10. la aserción que da sentido al resto ─────────────────────────
info "── ¿se ha tocado la configuración de los nodos que ya corrían?"
: > "$DIR/nodeyml.after"
for n in "${NODES[@]}"; do
    on "$n" "sha256sum site/node.*.yml" >> "$DIR/nodeyml.after"
done
if diff -q "$DIR/nodeyml.before" "$DIR/nodeyml.after" >/dev/null; then
    pass "los 9 node.yml de a/b/c intactos: añadir una institución no toca ningún nodo vivo"
else
    fail "los node.yml de a/b/c CAMBIARON durante el alta:"
    diff "$DIR/nodeyml.before" "$DIR/nodeyml.after" >&2 || true
fi

info "el nodo D queda arriba; T31 lo retira"
summary
