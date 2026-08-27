#!/usr/bin/env bash
# T10 — una clave extremo a extremo, para los 6 pares ordenados.
# T11 — casos borde (--edges).
#
# El SAE maestro pide enc_keys en SU DKMS; el esclavo recupera esa key_ID con
# dec_keys en el SUYO; se comparan los BYTES.
#
# Por qué la comparación de bytes es el núcleo y no un extra: la clave de
# sesión del SAE va envuelta por OTP con una clave de transporte y viaja por
# ETSI-020 SIN comprobación de integridad propia. Si las claves de transporte
# de los dos extremos divergieran, los dos SAEs se llevarían claves DISTINTAS
# y ningún módulo lo detectaría. Comparar aquí es la única verificación real.
#
#   ./t10_keys_smoke.sh              # los 6 flujos
#   ./t10_keys_smoke.sh --edges      # además, los casos que deben fallar bien
#   ./t10_keys_smoke.sh --pair dkms-node-a dkms-node-c
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need jq ssh

DO_EDGES=0; PAIR=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --edges) DO_EDGES=1; shift ;;
        --pair)  PAIR=("$2" "$3"); shift 3 ;;
        *) log "flag desconocido: $1"; exit 2 ;;
    esac
done

DIR="$(mkoutdir keys-smoke)"

# flujo <host_maestro> <host_esclavo>
flujo() {
    local hm="$1" hs="$2"
    local sm="${NODE_SAE[$hm]}" ss="${NODE_SAE[$hs]}"
    local tag="$sm→$ss"

    # Un 429 no es un fallo funcional: es el control de admisión diciendo
    # "ahora no". Pasa siempre que T10 corre justo detrás de T20, con los
    # buffers aún rellenándose, y hacía a este test flaky sin motivo. Se
    # reintenta con margen; lo que no se tolera es cualquier otro error.
    local resp intento
    for intento in 1 2 3 4 5 6; do
        if resp=$(enc_keys "$hm" "$sm" "$ss" 1 256 2>"$DIR/$sm-$ss.enc.err"); then
            break
        fi
        if ! grep -q '429' "$DIR/$sm-$ss.enc.err"; then
            fail "$tag: enc_keys falló — $(head -c 200 "$DIR/$sm-$ss.enc.err")"
            return 1
        fi
        resp=""
        sleep 5
    done
    if [[ -z "$resp" ]]; then
        fail "$tag: enc_keys sigue devolviendo 429 tras 6 intentos en 30 s"
        return 1
    fi
    printf '%s' "$resp" > "$DIR/$sm-$ss.enc.json"

    local kid kenc
    kid=$(jq -r '.keys[0].key_ID // empty' <<<"$resp")
    kenc=$(jq -r '.keys[0].key     // empty' <<<"$resp")
    if [[ -z "$kid" || -z "$kenc" ]]; then
        fail "$tag: enc_keys sin key_ID/key en la respuesta"
        return 1
    fi

    # El esclavo recupera la clave EN SU PROPIO DKMS, nombrando al maestro.
    local resp2
    if ! resp2=$(dec_keys "$hs" "$ss" "$sm" "$kid" 2>"$DIR/$sm-$ss.dec.err"); then
        fail "$tag: dec_keys falló para key_ID=$kid — $(head -c 200 "$DIR/$sm-$ss.dec.err")"
        return 1
    fi
    printf '%s' "$resp2" > "$DIR/$sm-$ss.dec.json"

    local kdec
    kdec=$(jq -r '.keys[0].key // empty' <<<"$resp2")
    if [[ "$kenc" == "$kdec" && -n "$kdec" ]]; then
        pass "$tag: clave idéntica en ambos extremos (key_ID=${kid:0:8}…)"
    else
        fail "$tag: LOS BYTES NO COINCIDEN — key_ID=$kid"
        log "       maestro: ${kenc:0:32}…"
        log "       esclavo: ${kdec:0:32}…"
        log "       Esto es lo peor que puede salir de esta batería: parar la"
        log "       campaña y guardar los logs de los 3 nodos."
        return 1
    fi
}

# ─── T10: pares ───────────────────────────────────────────────────────
if (( ${#PAIR[@]} == 2 )); then
    flujo "${PAIR[0]}" "${PAIR[1]}" || true
else
    for a in "${NODES[@]}"; do
        for b in "${NODES[@]}"; do
            [[ "$a" == "$b" ]] && continue
            flujo "$a" "$b" || true
        done
    done
fi

# ─── T11: bordes ──────────────────────────────────────────────────────
if (( DO_EDGES )); then
    info "── T11: casos que deben fallar bien"
    HM=dkms-node-a; HS=dkms-node-b
    SM="${NODE_SAE[$HM]}"; SS="${NODE_SAE[$HS]}"

    # /status: de aquí salen los límites que se prueban justo debajo
    st=$(etsi_status "$HM" "$SM" "$SS" 2>/dev/null || echo '{}')
    printf '%s' "$st" > "$DIR/status.json"
    maxreq=$(jq -r '.max_key_per_request // 0' <<<"$st")
    maxsize=$(jq -r '.max_key_size // 0' <<<"$st")
    info "status: max_key_per_request=$maxreq max_key_size=$maxsize stored=$(jq -r '.stored_key_count // "?"' <<<"$st")"
    # El SAE que pregunta es el maestro de la consulta y su identidad viene del
    # cert de cliente, así que /status debe devolverla. Salía vacía.
    check "master_SAE_ID en /status" "$(jq -r '.master_SAE_ID // ""' <<<"$st")" "$SM"

    # code_of <host> <sae> <verbo curl...> → código HTTP, o 000 si no hubo respuesta
    # curl ya escribe 000 cuando la conexión ni llega a hablar HTTP (rechazo
    # TLS), así que el `|| echo 000` de más pegaba un segundo 000 al primero y
    # salía "000000", que no casaba con ningún patrón: un mTLS que funcionaba
    # se leía como roto. Se queda solo el fallback para cuando curl no imprime
    # nada en absoluto.
    raw_code() {
        local host="$1"; shift
        local out
        out=$(on "$host" "$* -o /dev/null -w '%{http_code}' 2>/dev/null" || true)
        printf '%s' "${out:-000}"
    }

    C="curl -s --max-time 15 --cacert $CERTS_REMOTE/net-ca.crt"

    # 1. key_ID inventada → error ETSI, nunca 5xx
    code=$(raw_code "$HS" "$C --cert $CERTS_REMOTE/$SS.crt --key $CERTS_REMOTE/$SS.key \
        -H 'Content-Type: application/json' \
        -d '{\"key_IDs\":[{\"key_ID\":\"00000000-0000-0000-0000-000000000000\"}]}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$SM/dec_keys")
    if [[ "$code" =~ ^4 ]]; then pass "key_ID inexistente → $code (4xx)"
    else fail "key_ID inexistente → $code (se esperaba 4xx; un 5xx es un bug)"; fi

    # 2. la misma key_ID dos veces: la segunda no debe entregar clave
    resp=$(enc_keys "$HM" "$SM" "$SS" 1 256 2>/dev/null || echo '{}')
    kid=$(jq -r '.keys[0].key_ID // empty' <<<"$resp")
    if [[ -n "$kid" ]]; then
        dec_keys "$HS" "$SS" "$SM" "$kid" >/dev/null 2>&1 || true
        second=$(dec_keys "$HS" "$SS" "$SM" "$kid" 2>/dev/null | jq -r '.keys[0].key // empty' || true)
        if [[ -z "$second" ]]; then pass "dec_keys repetido no reentrega la clave"
        else fail "dec_keys repetido REENTREGA la clave (key_ID=$kid) — una clave de un solo uso no debe salir dos veces"; fi
    else
        fail "no pude obtener una key_ID para el test de reentrega"
    fi

    # 3. slave_sae inexistente
    code=$(raw_code "$HM" "$C --cert $CERTS_REMOTE/$SM.crt --key $CERTS_REMOTE/$SM.key \
        -H 'Content-Type: application/json' -d '{\"number\":1,\"size\":256}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/sae_noexiste/enc_keys")
    if [[ "$code" =~ ^4 ]]; then pass "slave_sae inexistente → $code (4xx)"
    else fail "slave_sae inexistente → $code (se esperaba 4xx)"; fi

    # 4. sin cert de cliente → el TLS se cae (curl devuelve 000)
    code=$(raw_code "$HM" "$C -H 'Content-Type: application/json' \
        -d '{\"number\":1,\"size\":256}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$SS/enc_keys")
    if [[ "$code" == "000" || "$code" =~ ^4 ]]; then pass "sin cert de cliente → rechazado ($code)"
    else fail "sin cert de cliente → $code: el mTLS NO está exigiendo cert"; fi

    # 5. cert de otra CA
    code=$(raw_code "$HM" "$C --cert $CERTS_REMOTE/sae_rogue.crt --key $CERTS_REMOTE/sae_rogue.key \
        -H 'Content-Type: application/json' -d '{\"number\":1,\"size\":256}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$SS/enc_keys")
    if [[ "$code" == "000" || "$code" =~ ^4 ]]; then pass "cert de CA ajena → rechazado ($code)"
    else fail "cert de CA ajena → $code: el DKMS acepta certs que no firma su CA"; fi

    # 6. number por encima del máximo anunciado
    if (( maxreq > 0 )); then
        over=$(( maxreq + 1 ))
        code=$(raw_code "$HM" "$C --cert $CERTS_REMOTE/$SM.crt --key $CERTS_REMOTE/$SM.key \
            -H 'Content-Type: application/json' -d '{\"number\":$over,\"size\":256}' \
            https://127.0.0.1:$DKMS_SAE/api/v1/keys/$SS/enc_keys")
        if [[ "$code" =~ ^4 ]]; then pass "number=$over > max_key_per_request → $code (4xx)"
        else fail "number=$over → $code: o lo trunca en silencio o no valida"; fi
    fi

    # 7. tamaño de clave no soportado
    code=$(raw_code "$HM" "$C --cert $CERTS_REMOTE/$SM.crt --key $CERTS_REMOTE/$SM.key \
        -H 'Content-Type: application/json' -d '{\"number\":1,\"size\":7}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$SS/enc_keys")
    if [[ "$code" =~ ^4 ]]; then pass "size=7 bits → $code (4xx)"
    else fail "size=7 bits → $code (se esperaba 4xx)"; fi
fi

summary
