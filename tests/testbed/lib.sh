#!/usr/bin/env bash
# Común a los scripts del testbed Proxmox. No se ejecuta directamente.
#
# Convenciones del testbed (ver README.md):
#   dkms-sdn    192.168.50.201  SDN central (+ ~/site4 preparado para el nodo D)
#   dkms-node-a 192.168.50.202  qkc_id 1 / orr_1 / dkms-1 / sae_1
#   dkms-node-b 192.168.50.203  qkc_id 2 / orr_2 / dkms-2 / sae_2
#   dkms-node-c 192.168.50.204  qkc_id 3 / orr_3 / dkms-3 / sae_3
#
# Los certs de SAE viven en la VM (~/site/certs) porque el SAE se ejecuta
# donde vive un SAE de verdad: junto a su DKMS.

set -euo pipefail

# ─── topología del testbed ────────────────────────────────────────────
NODES=(dkms-node-a dkms-node-b dkms-node-c)
declare -gA NODE_IP=(
    [dkms-node-a]=192.168.50.202
    [dkms-node-b]=192.168.50.203
    [dkms-node-c]=192.168.50.204
)
declare -gA NODE_QKC=( [dkms-node-a]=1 [dkms-node-b]=2 [dkms-node-c]=3 )
declare -gA NODE_ORR=( [dkms-node-a]=orr_1 [dkms-node-b]=orr_2 [dkms-node-c]=orr_3 )
declare -gA NODE_DKMS=( [dkms-node-a]=dkms-1 [dkms-node-b]=dkms-2 [dkms-node-c]=dkms-3 )
declare -gA NODE_SAE=( [dkms-node-a]=sae_1 [dkms-node-b]=sae_2 [dkms-node-c]=sae_3 )

SDN_HOST=dkms-sdn
SDN_IP=192.168.50.201
SDN_HTTP=19002          # admin HTTP de la SDN
QKC_ADMIN=20002         # HTTP admin del QKC
DKMS_SAE=20005          # ETSI-014 (mTLS) del DKMS
CERTS_REMOTE=/home/debian/site/certs

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CAMPAIGN="${CAMPAIGN:-testbed-$(date +%Y%m%d)}"
OUTDIR="${OUTDIR:-$REPO_ROOT/tests/results/$CAMPAIGN}"

# ControlMaster: el muestreador de T20 abre una docena de conexiones cada 5 s
# (logs de 3 módulos × 3 nodos, docker stats, rates de la SDN). Sin multiplexar,
# el handshake SSH domina el ciclo y el muestreo se retrasa hasta perder de
# vista lo que quiere medir. La primera conexión a cada host abre el socket y
# las demás lo reutilizan; ControlPersist lo mantiene entre scripts.
SSH_MUX_DIR="${SSH_MUX_DIR:-${TMPDIR:-/tmp}/dkms-testbed-ssh}"
mkdir -p "$SSH_MUX_DIR"; chmod 700 "$SSH_MUX_DIR"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new
          -o ControlMaster=auto -o "ControlPath=$SSH_MUX_DIR/%r@%h:%p" -o ControlPersist=120)

# ─── salida ───────────────────────────────────────────────────────────
_PASS=0; _FAIL=0
C_OK=$'\033[32m'; C_BAD=$'\033[31m'; C_DIM=$'\033[2m'; C_OFF=$'\033[0m'

log()  { printf '%s\n' "$*" >&2; }
info() { printf '%s──%s %s\n' "$C_DIM" "$C_OFF" "$*" >&2; }
pass() { _PASS=$((_PASS+1)); printf '%sPASS%s %s\n' "$C_OK"  "$C_OFF" "$*" >&2; }
fail() { _FAIL=$((_FAIL+1)); printf '%sFAIL%s %s\n' "$C_BAD" "$C_OFF" "$*" >&2; }

# check <descripción> <valor_obtenido> <valor_esperado>
check() {
    local what="$1" got="$2" want="$3"
    if [[ "$got" == "$want" ]]; then pass "$what (= $got)"
    else fail "$what: obtenido '$got', esperado '$want'"; fi
}

# check_ge <descripción> <valor> <mínimo>
check_ge() {
    local what="$1" got="$2" min="$3"
    if [[ "$got" =~ ^[0-9]+$ ]] && (( got >= min )); then pass "$what (= $got ≥ $min)"
    else fail "$what: obtenido '$got', esperado ≥ $min"; fi
}

summary() {
    log ""
    if (( _FAIL == 0 )); then
        printf '%s%d PASS, 0 FAIL%s\n' "$C_OK" "$_PASS" "$C_OFF" >&2
    else
        printf '%s%d PASS, %d FAIL%s\n' "$C_BAD" "$_PASS" "$_FAIL" "$C_OFF" >&2
    fi
    (( _FAIL == 0 ))
}

# ─── acceso remoto ────────────────────────────────────────────────────
# `-n` no es opcional: sin él, un `on` dentro de un `while read` se bebe el
# resto de la entrada del bucle y este da UNA vuelta en vez de N. Así el
# muestreo de integridad de T20 decía "1 claves muestreadas" cuando pedía 20.
on() { local host="$1"; shift; ssh -n "${SSH_OPTS[@]}" "$host" "$@"; }

# Igual que `on`, pero SIN multiplexar y desligando la entrada.
#
# Para lanzar un proceso remoto en segundo plano: sobre una conexión
# multiplexada, el ssh que arranca el trabajo se queda esperando a que el canal
# cierre y no vuelve nunca —observado colgando 17 min en el punto de 16 hilos
# de T20, con la carga ya terminada—. Con `ControlMaster=no` cada lanzamiento
# usa su propia conexión y vuelve en cuanto el shell remoto termina.
on_detached() {
    local host="$1"; shift
    ssh -n "${SSH_OPTS[@]}" -o ControlMaster=no -o ControlPath=none "$host" "$@"
}

# sdn_get <ruta>   →  cuerpo de la respuesta del admin HTTP de la SDN
sdn_get() { on "$SDN_HOST" "curl -sSf --max-time 10 localhost:$SDN_HTTP$1"; }

# qkc_get <host> <ruta>  →  admin HTTP del QKC de ese nodo
qkc_get() { on "$1" "curl -sSf --max-time 10 localhost:$QKC_ADMIN$2"; }

# dlogs <host> <qkc|orr|dkms> [líneas]  →  logs del contenedor, sin ANSI
dlogs() {
    local host="$1" role="$2" n="${3:-200}"
    on "$host" "docker logs --tail $n site-$role-1 2>&1 | sed 's/\x1b\[[0-9;]*m//g'"
}

# dlogs_since <host> <rol> <duración docker, p.ej. 60s>
dlogs_since() {
    local host="$1" role="$2" since="$3"
    on "$host" "docker logs --since $since site-$role-1 2>&1 | sed 's/\x1b\[[0-9;]*m//g'"
}

# ─── ETSI-014 (se ejecuta EN la VM, con sus certs) ────────────────────
# enc_keys <host> <sae_maestro> <sae_esclavo> [number] [size]
#   → JSON del KeyContainer por stdout
enc_keys() {
    local host="$1" master="$2" slave="$3" number="${4:-1}" size="${5:-256}"
    on "$host" "curl -sSf --max-time 20 \
        --cacert $CERTS_REMOTE/net-ca.crt \
        --cert   $CERTS_REMOTE/$master.crt \
        --key    $CERTS_REMOTE/$master.key \
        -H 'Content-Type: application/json' \
        -d '{\"number\":$number,\"size\":$size}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$slave/enc_keys"
}

# dec_keys <host> <sae_esclavo> <sae_maestro> <key_id>
dec_keys() {
    local host="$1" slave="$2" master="$3" key_id="$4"
    on "$host" "curl -sSf --max-time 20 \
        --cacert $CERTS_REMOTE/net-ca.crt \
        --cert   $CERTS_REMOTE/$slave.crt \
        --key    $CERTS_REMOTE/$slave.key \
        -H 'Content-Type: application/json' \
        -d '{\"key_IDs\":[{\"key_ID\":\"$key_id\"}]}' \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$master/dec_keys"
}

# etsi_status <host> <sae_propio> <sae_destino>
etsi_status() {
    local host="$1" me="$2" other="$3"
    on "$host" "curl -sSf --max-time 10 \
        --cacert $CERTS_REMOTE/net-ca.crt \
        --cert   $CERTS_REMOTE/$me.crt \
        --key    $CERTS_REMOTE/$me.key \
        https://127.0.0.1:$DKMS_SAE/api/v1/keys/$other/status"
}

# ─── espera activa ────────────────────────────────────────────────────
# wait_for <segundos> <descripción> <comando...>   (reintenta cada 2 s)
wait_for() {
    local timeout="$1" what="$2"; shift 2
    local deadline=$(( SECONDS + timeout ))
    while (( SECONDS < deadline )); do
        if "$@" >/dev/null 2>&1; then
            pass "$what (tras $(( timeout - (deadline - SECONDS) )) s)"
            return 0
        fi
        sleep 2
    done
    fail "$what: no ocurrió en $timeout s"
    return 1
}

# ─── extracción de logs ───────────────────────────────────────────────
# Con `set -o pipefail`, un grep sin coincidencias tumba el script entero: "no
# hay pánicos" no es un error. Estos dos envoltorios devuelven un valor vacío o
# cero en ese caso, que es lo que queremos afirmar.

# count_matches <patrón> <fichero...>   → total de líneas que casan (0 si ninguna)
count_matches() {
    local pat="$1"; shift
    { grep -ac "$pat" "$@" || true; } | awk -F: '{ s += ($NF + 0) } END { print s + 0 }'
}

# max_field <patrón grep -o>  (lee stdin)  → mayor valor tras '=', vacío si no hay
max_field() { { grep -oa "$1" || true; } | cut -d= -f2 | sort -rn | head -1; }

need() {
    for c in "$@"; do
        command -v "$c" >/dev/null || { log "falta el comando '$c'"; exit 2; }
    done
}

mkoutdir() { mkdir -p "$OUTDIR/$1"; echo "$OUTDIR/$1"; }
