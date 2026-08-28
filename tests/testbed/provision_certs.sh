#!/usr/bin/env bash
# Provisiona TODO el material TLS del testbed y lo reparte por las VMs.
#
# Por qué existe y no basta con emitir certs de SAE: la CA que firmó los certs
# desplegados el 31/07 **perdió su clave privada** (en `dkms-sdn:~/site4/certs`
# quedó una `ca.key` de una CA anterior y una `ca.crt` de la buena, con
# timestamps distintos; la clave de la buena no está en ninguna VM ni en el
# portátil). Sin clave no se puede firmar un cert de SAE nuevo, así que la
# única salida es reemitir el PKI entero. Como el redespliegue reinicia los
# contenedores de todas formas, no cuesta nada extra.
#
# Emite, reutilizando las CAs de $WORK si ya existen (docs/SECURITY.md §2):
#   net-ca.crt/key           CA de RED: firma los certs de NODO (DKMS). Trust
#                            root de peer_dkms_ca y control_plane_ca.
#   sae-ca.crt/key           CA de SAEs: firma los certs de cliente de SAE.
#                            Trust root de sae_client_ca.
#   dkms-N.crt/key           cert de servidor de cada DKMS (SAN URI:dkms://id + IP)
#   sae_N.crt/key            cert de cliente de cada SAE (SAN URI:urn:dkms:sae:id)
#   rogue_ca.crt, sae_rogue  CA ajena, para el caso negativo de T11 (plano SAE);
#                            además un SAE legítimo presentado en el plano peer
#                            (:8444, trust net-ca) debe rechazarse — cross-plane.
#
# y reparte a cada VM: net-ca.crt + sae-ca.crt + su dkms-N.{crt,key} + TODOS los sae_*.
# Los scripts necesitan actuar como cualquier SAE desde cualquier VM (el
# destino de un dec_keys vive en otra máquina), de ahí que vayan todos.
#
#   ./provision_certs.sh              # emite lo que falte y reparte
#   ./provision_certs.sh --force      # tira el PKI y lo rehace desde cero
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
need openssl ssh scp

WORK="${CERT_WORKDIR:-$HOME/.dkms-testbed/certs}"
GEN="$REPO_ROOT/docker/gen-certs.sh"
FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

# nodo D vive en la VM de la SDN (~/site4); su DKMS es dkms-4
D_HOST="${D_HOST:-$SDN_HOST}"
D_IP="$SDN_IP"

mkdir -p "$WORK"; chmod 700 "$WORK"
info "material TLS en $WORK (contiene la clave de la CA — fuera del repo)"

# ─── 1. estado del PKI ────────────────────────────────────────────────
# Las dos CAs (net-ca, sae-ca) las crea y reutiliza el propio gen-certs.sh en
# la primera emisión de nodo / de SAE. Aquí solo gestionamos el wipe --force.
if (( FORCE )); then
    info "--force: rehaciendo el PKI entero (net-ca + sae-ca + hojas)"
    rm -f "$WORK"/*.crt "$WORK"/*.key "$WORK"/*.csr "$WORK"/*.srl
fi

# ─── 2. certs de servidor de los DKMS ─────────────────────────────────
# El SAN lleva la IP anunciada: si no coincide con la real, el peer rechaza la
# conexión ETSI-020 y el síntoma aparece lejos de aquí.
issue_dkms() {
    local id="$1" ip="$2"
    [[ -f "$WORK/$id.crt" ]] && return 0
    DAYS=3650 bash "$GEN" "$id" "$ip" "$WORK" >/dev/null
    info "emitido $id (IP $ip)"
}
for n in "${NODES[@]}"; do
    issue_dkms "${NODE_DKMS[$n]}" "${NODE_IP[$n]}"
done
issue_dkms dkms-4 "$D_IP"

# ─── 2b. certs de nodo de los ORR ─────────────────────────────────────
# El gRPC del ORR va con mTLS por defecto (grpc_tls): presenta su cert de
# nodo y exige uno de net-ca a su DKMS y a los ORR de los demás nodos. Mismo
# generador, mismo SAN con la IP anunciable; el id sigue la convención
# `orr_<n>` que la SDN y los DKMS derivan.
orr_of() { echo "orr_${1#dkms-}"; }
for n in "${NODES[@]}"; do
    issue_dkms "$(orr_of "${NODE_DKMS[$n]}")" "${NODE_IP[$n]}"
done
issue_dkms orr_4 "$D_IP"
pass "net-ca: $(openssl x509 -in "$WORK/net-ca.crt" -noout -fingerprint -sha256 | cut -d= -f2)"

# ─── 3. certs de cliente de los SAE ───────────────────────────────────
issue_sae() {
    local id="$1"
    [[ -f "$WORK/$id.crt" ]] && return 0
    DAYS=3650 bash "$GEN" --sae "$id" "$WORK" >/dev/null
    info "emitido $id"
}
SAES=()
for n in "${NODES[@]}"; do SAES+=("${NODE_SAE[$n]}"); done
SAES+=(sae_4)
for s in "${SAES[@]}"; do issue_sae "$s"; done
pass "sae-ca: $(openssl x509 -in "$WORK/sae-ca.crt" -noout -fingerprint -sha256 | cut -d= -f2)"

# ─── 4. CA ajena para el caso negativo de T11 ─────────────────────────
if [[ ! -f "$WORK/sae_rogue.crt" ]]; then
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$WORK/rogue_ca.key" \
        -out "$WORK/rogue_ca.crt" -days 3650 -subj "/CN=rogue-ca" 2>/dev/null
    openssl req -newkey rsa:2048 -nodes -keyout "$WORK/sae_rogue.key" \
        -out "$WORK/sae_rogue.csr" -subj "/CN=sae_rogue" 2>/dev/null
    openssl x509 -req -in "$WORK/sae_rogue.csr" -CA "$WORK/rogue_ca.crt" \
        -CAkey "$WORK/rogue_ca.key" -CAcreateserial -days 3650 \
        -out "$WORK/sae_rogue.crt" \
        -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=clientAuth\n' \
                   "URI:urn:dkms:sae:sae_rogue") 2>/dev/null
    rm -f "$WORK/sae_rogue.csr"
    info "emitido sae_rogue con una CA ajena (caso negativo de T11)"
fi

# ─── 5. repartir ──────────────────────────────────────────────────────
sae_files() {
    local out=()
    for s in "${SAES[@]}" sae_rogue; do
        [[ -f "$WORK/$s.crt" ]] && out+=("$WORK/$s.crt" "$WORK/$s.key")
    done
    printf '%s\n' "${out[@]}"
}

for n in "${NODES[@]}"; do
    id="${NODE_DKMS[$n]}"; oid="$(orr_of "$id")"
    mapfile -t files < <(sae_files)
    scp "${SSH_OPTS[@]}" "$WORK/net-ca.crt" "$WORK/sae-ca.crt" "$WORK/rogue_ca.crt" \
        "$WORK/$id.crt" "$WORK/$id.key" "$WORK/$oid.crt" "$WORK/$oid.key" \
        "${files[@]}" "$n:$CERTS_REMOTE/" >/dev/null
    on "$n" "chmod 600 $CERTS_REMOTE/*.key"
    pass "$n: net-ca + sae-ca + $id + $oid + ${#SAES[@]} SAEs"
done

# nodo D (~/site4 en la VM de la SDN)
on "$D_HOST" "mkdir -p site4/certs"
mapfile -t files < <(sae_files)
scp "${SSH_OPTS[@]}" "$WORK/net-ca.crt" "$WORK/sae-ca.crt" "$WORK/rogue_ca.crt" \
    "$WORK/dkms-4.crt" "$WORK/dkms-4.key" "$WORK/orr_4.crt" "$WORK/orr_4.key" \
    "${files[@]}" "$D_HOST:site4/certs/" >/dev/null
on "$D_HOST" "chmod 600 site4/certs/*.key"
pass "$D_HOST:~/site4/certs: net-ca + sae-ca + dkms-4 + orr_4 + ${#SAES[@]} SAEs"

# Ninguna VM debe tener claves privadas de CA — solo los certs públicos.
# Retiramos cualquier *-ca.key/.srl (y la ca.key huérfana histórica de site4).
on "$D_HOST" "rm -f site4/certs/*-ca.key site4/certs/ca.key site4/certs/*.srl" || true
info "retiradas claves privadas de CA de site4 (las VMs solo llevan certs públicos)"

log ""
info "DKMS y ORR leen los certs al arrancar: hay que reiniciarlos para que los tomen"
info "  for h in ${NODES[*]}; do ssh \$h 'cd site && docker compose -f site.yml restart dkms orr'; done"
summary
