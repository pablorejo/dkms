#!/usr/bin/env bash
# T40/T41 — reinicio de un extremo del enlace PQC.
#
# Requiere la FASE 0 hecha: las imágenes desplegadas (diag-v7 / auto-peers-v1)
# son anteriores a los commits que arreglan justo esto, así que contra ellas
# este test mide binarios viejos y su resultado no dice nada del código.
#
# El iniciador del handshake es SIEMPRE el QKC lex-menor. Un responder que se
# reinicia no puede pedirse un epoch nuevo mandando INIT: el iniciador se
# quedaría con su secreto viejo mientras el responder adopta el nuevo, y el OTP
# del enlace no lleva MAC para detectar la divergencia. Por eso el disparador
# es local: la reconexión TCP.
#
#   ./t40_restart_pqc.sh --greater   # reinicia el 3 (responder) → relink en 1 y 2
#   ./t40_restart_pqc.sh --smaller   # reinicia el 1 (iniciador) → re-encapsulación
#   ./t40_restart_pqc.sh --both
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

MODE=greater
case "${1:---greater}" in
    --greater) MODE=greater ;;
    --smaller) MODE=smaller ;;
    --both)    MODE=both ;;
    *) log "uso: $0 [--greater|--smaller|--both]"; exit 2 ;;
esac

DIR="$(mkoutdir restart-pqc)"
seen() { dlogs_since "$1" "$2" "${4:-3m}" | grep -qa "$3"; }

# epochs_of <host> <peer>  →  epochs establecidos con ESE peer, en orden de log
#
# Por peer y no agregado: con `pqc_rekey_keys` el epoch avanza solo cada pocos
# segundos bajo tráfico, así que un máximo global sube igual aunque el enlace
# que nos importa esté muerto. La línea es
#   qkc.pqc.handshake.established me=1 peer=2 epoch=48 replaced=false
epochs_of() {
    # Los dos grep necesitan su `|| true`: con `set -o pipefail`, un nodo que
    # todavía no tiene handshakes en su ventana de log tumbaba el script
    # entero, y en silencio — el segundo grep se quedaba sin entrada y salía 1.
    { dlogs "$1" qkc 2000 | grep -a "handshake.established .*peer=$2 " || true; } \
        | { grep -oa 'epoch=[0-9]*' || true; } | cut -d= -f2
}

# Un mismo número de epoch establecido dos veces con el mismo peer es la
# situación que nada aguas abajo puede detectar: el OTP del enlace no lleva
# MAC, así que dos secretos distintos bajo el mismo número se traducen en
# claves divergentes y en silencio.
count_repeated_epochs() {
    epochs_of "$1" "$2" | sort -n | uniq -d | wc -l
}

# ─── caso <host_reiniciado> <id_qkc> <hosts_testigo...> ───────────────
#
# Qué se afirma depende de QUIÉN se reinició, porque el papel no es simétrico:
#
# * Reinicia el lex-MAYOR (responder): los testigos son los iniciadores y son
#   los únicos que pueden pedir epoch nuevo. Deben detectar la reconexión y
#   llamar a `relink`.
# * Reinicia el lex-MENOR (iniciador): los testigos son responders y por diseño
#   **no** mandan INIT. La recuperación la conduce el que arrancó, mandando
#   INIT con pubkey fresca; el responder re-encapsula en `handle_init`. Exigir
#   aquí un `relink` de los testigos es pedirles justo lo que no deben hacer.
caso() {
    local victim="$1" vid="$2"; shift 2
    local witnesses=("$@")
    info "══ reiniciando el QKC de $victim (id $vid); testigos: ${witnesses[*]}"

    for w in "${witnesses[@]}"; do
        epochs_of "$w" "$vid" > "$DIR/$w.epochs.before"
        info "$w: epochs con el peer $vid antes → $(tail -3 "$DIR/$w.epochs.before" | tr '\n' ' ')"
    done

    # Marca de tiempo para acotar los logs a ESTE caso: con una ventana fija
    # de minutos, el segundo caso encontraba los `relink` del primero y daba
    # un verde falso (o un rojo falso, según a quién mirase).
    local since
    since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    on "$victim" "docker restart site-qkc-1" >/dev/null
    local t0=$SECONDS
    seen_since() { dlogs_since "$1" "$2" "$since" | grep -qa "$3"; }

    if (( vid > 1 )); then
        for w in "${witnesses[@]}"; do
            wait_for 60 "$w: detecta la reconexión y lanza relink" \
                seen_since "$w" qkc 'qkc.pqc.relink: renegocio el enlace'
            wait_for 60 "$w: relink completado" seen_since "$w" qkc 'qkc.pqc.relink completado'
        done
    else
        # El que se reinició es el iniciador: debe rehacer el handshake con
        # cada vecino por su cuenta.
        for w in "${witnesses[@]}"; do
            local wid="${NODE_QKC[$w]}"
            wait_for 90 "$victim: re-establece el handshake con el peer $wid" \
                seen_since "$victim" qkc "handshake.established .*peer=$wid"
        done
        for w in "${witnesses[@]}"; do
            if seen_since "$w" qkc 'qkc.pqc.relink: renegocio el enlace'; then
                fail "$w: un responder ha lanzado relink — solo el lex-menor puede pedir epoch nuevo"
            else
                pass "$w: no manda INIT siendo responder (correcto)"
            fi
        done
    fi

    # Qué se comprueba de los epochs, y por qué no el máximo histórico:
    #
    # Cada QKC reinicia su contador de epochs al arrancar (visto: recién
    # levantado empieza en epoch=1 aunque su vecino guarde epochs mucho más
    # altos). Comparar el máximo que aparece en el log contra el de antes
    # mezcla vidas distintas del proceso y da falsos rojos en cuanto el propio
    # testigo se ha reiniciado en un caso anterior. Lo que sí es propiedad del
    # relink es que **negocia epochs nuevos por encima de la base que anuncia**
    # (`base=N` en su propia línea) y poda por debajo.
    sleep 5
    for w in "${witnesses[@]}"; do
        local nuevos
        nuevos=$(dlogs_since "$w" qkc "$since" \
                 | grep -ac "handshake.established .*peer=$vid " || true)
        if (( vid > 1 )); then
            check_ge "$w: epochs nuevos con el peer $vid tras el relink" "$nuevos" 1
            local base_line
            base_line=$(dlogs_since "$w" qkc "$since" | grep -a 'relink completado' | tail -1 || true)
            [[ -n "$base_line" ]] && info "$w: $(grep -oE 'base=[0-9]+ epocas_descartadas=[0-9]+' <<<"$base_line")"
        else
            info "$w: $nuevos handshakes nuevos con el peer $vid (el iniciador reinició su contador)"
        fi
        # Dentro de la ventana de este caso, un número no puede salir dos veces.
        local dup
        dup=$(dlogs_since "$w" qkc "$since" | grep -a "handshake.established .*peer=$vid " \
              | grep -oa 'epoch=[0-9]*' | sort | uniq -d | wc -l)
        check "$w: epochs repetidos con el peer $vid en esta ventana" "$dup" "0"
    done

    # El enlace vuelve a llevar material.
    keystore_alive() {
        dlogs_since "$victim" qkc 90s | grep -a 'keystore.levels' | tail -3 \
            | grep -qav 'enc=0 dec=0 taken=0'
    }
    wait_for 120 "$victim: el keystore vuelve a llenarse" keystore_alive

    # La firma de la regresión, por si acaso.
    local stuck
    stuck=$(dlogs_since "$victim" qkc 3m | grep -ac 'timeout waiting pqc-secret' || true)
    if (( stuck > 0 )); then
        fail "$victim: $stuck × 'timeout waiting pqc-secret' — el enlace NO se recuperó"
        log "       (firma completa: keystore.levels enc=0 dec=0 taken=0 + esta línea cada 10 s,"
        log "        con handshake.established sano en ambos lados y la SDN viendo todo bien)"
    else
        pass "$victim: sin 'timeout waiting pqc-secret' tras la recuperación"
    fi

    info "recuperado en ~$(( SECONDS - t0 )) s; comprobando entrega de claves"
    if "$(dirname "${BASH_SOURCE[0]}")/t10_keys_smoke.sh" >/dev/null 2>&1; then
        pass "T10 pasa 6/6 tras el reinicio de $victim"
    else
        fail "T10 falla tras el reinicio de $victim"
    fi
}

if [[ "$MODE" == greater || "$MODE" == both ]]; then
    # El 3 es lex-mayor en sus dos enlaces: los testigos son 1 y 2, que son los
    # iniciadores y los que tienen que llamar a relink().
    caso dkms-node-c 3 dkms-node-a dkms-node-b
fi

if [[ "$MODE" == smaller || "$MODE" == both ]]; then
    info "══ caso espejo: se reinicia el lex-MENOR (el iniciador)"
    info "   aquí el que arranca manda INIT con pubkey nueva, y el responder"
    info "   tiene que RE-ENCAPSULAR en vez de replicar su ciphertext cacheado."
    info "   Si lo replicara, los dos extremos divergen en silencio y el único"
    info "   síntoma sería que T10 empieza a devolver bytes distintos."
    caso dkms-node-a 1 dkms-node-b dkms-node-c
fi

summary
