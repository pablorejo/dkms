#!/usr/bin/env bash
# Genera el material TLS que necesita un DKMS para el mTLS ETSI-020 (DKMS<->DKMS)
# y ETSI-014 (SAE->DKMS).
#
#   gen-certs.sh <node_id> <advertise_ip> [out_dir]     # cert de un DKMS
#   gen-certs.sh --sae <sae_id> [out_dir]               # cert de cliente SAE
#
# DOS RAÍCES DE CONFIANZA SEPARADAS (ver docs/SECURITY.md §2):
#   net-ca.crt net-ca.key    -> CA de RED. Firma los certs de NODO (DKMS, y en
#                               fases posteriores SDN/QKC/ORR de control). Es el
#                               trust root de `peer_dkms_ca` y `control_plane_ca`.
#   sae-ca.crt sae-ca.key    -> CA de SAEs. Firma SOLO certs de cliente de SAE.
#                               Es el trust root de `sae_client_ca`.
# Ambas se REUTILIZAN si ya existen. Separarlas evita que un cert de SAE valga
# como cert de DKMS en el plano peer y viceversa (antes colapsaban en un `ca.crt`).
#
# Produce en out_dir (default ./certs):
#   <node_id>.crt <node_id>.key  -> cert de servidor del DKMS (firmado por net-ca),
#                               SAN URI:dkms://<node_id>, IP:<advertise_ip>, DNS:localhost
#   <sae_id>.crt <sae_id>.key    -> (--sae) cert de CLIENTE de un SAE (firmado por
#                               sae-ca), SAN URI:urn:dkms:sae:<sae_id> (de ahí extrae
#                               el DKMS la identidad SAE; ver dkms/src/etsi_http/auth.rs)
#
# MULTI-INSTITUCIÓN: net-ca debe ser COMÚN a la federación (el mTLS entre DKMS
# exige una raíz compartida); sae-ca puede ser por institución (cada DKMS pone
# en `sae_client_ca` la CA de SUS SAEs). Distribuye net-ca.crt a todos; sae-ca
# solo donde haga falta verificar esos SAEs. (Follow-on: bundles per-institución.)
set -euo pipefail

DAYS="${DAYS:-3650}"

# Crea una CA raíz <base>.{crt,key} si no existe, con BasicConstraints CA:TRUE
# explícito (sin él, WebPkiClientVerifier construye un trust set vacío en
# silencio — ver docs/SECURITY.md §5 gotcha 7).
# Algoritmo de firma de los certs (docs/SECURITY.md §Fase 5/6 PQC):
#   rsa (default)  — RSA clásico.
#   ml-dsa-65      — firma post-cuántica ML-DSA (FIPS 204). Requiere openssl
#                    3.5+. La clave se emite en forma **seed-only** (128 B), la
#                    única que carga el runtime Rust (common::tls_pqc).
KEY_ALG="${KEY_ALG:-rsa}"

# Genera una clave privada en el fichero indicado, según KEY_ALG. $2 = bits RSA.
gen_key() {
  local out="$1" bits="${2:-2048}"
  case "$KEY_ALG" in
    ml-dsa-65|ml-dsa|mldsa|mldsa65)
      openssl genpkey -algorithm ML-DSA-65 \
        -provparam ml-dsa.output_formats=seed-only -out "$out" ;;
    *)
      openssl genpkey -algorithm RSA -pkeyopt "rsa_keygen_bits:$bits" -out "$out" ;;
  esac
}

ensure_ca() {
  local out="$1" base="$2" cn="$3"
  if [[ -f "$out/$base.crt" && -f "$out/$base.key" ]]; then
    echo "[gen-certs] reutilizando CA existente $out/$base.crt"
    return 0
  fi
  echo "[gen-certs] creando CA '$cn' ($KEY_ALG) en $out/$base.crt"
  gen_key "$out/$base.key" 4096
  # Las extensiones van en un fichero de config, NO con `-addext`.
  #
  # Con `-x509`, openssl aplica además la sección `v3_ca` de su openssl.cnf, y
  # en OpenSSL 1.1.1 (CESGA tiene 1.1.1g) `-addext` no la sustituye: la SUMA.
  # El resultado es un cert con DOS `basicConstraints`, que RFC 5280 prohíbe
  # («a certificate MUST NOT include more than one instance of a particular
  # extension»); openssl 1.1.1 deja de reconocerlo como emisor válido y toda
  # la cadena falla con «unable to get local issuer certificate» — medido en
  # CESGA 2026-08-27. Con `x509_extensions` en `[req]` mandamos nosotros, y
  # sale una sola vez tanto en 1.1.1 como en 3.x.
  local cfg="$out/.$base.cnf"
  cat > "$cfg" <<CFG
[req]
distinguished_name = dn
prompt = no
x509_extensions = ca_ext
[dn]
CN = $cn
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
CFG
  openssl req -x509 -key "$out/$base.key" -out "$out/$base.crt" \
    -days "$DAYS" -config "$cfg"
  rm -f "$cfg"
}

# Firma un CSR con una CA dada, aplicando SAN + EKU.
sign_with() {
  local out="$1" ca_base="$2" name="$3" san="$4" eku="$5"
  openssl x509 -req -in "$out/$name.csr" -CA "$out/$ca_base.crt" -CAkey "$out/$ca_base.key" \
    -CAcreateserial -days "$DAYS" -out "$out/$name.crt" \
    -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=%s\n' "$san" "$eku")
  rm -f "$out/$name.csr"
}

if [[ "${1:-}" == "--sae" ]]; then
  SAE_ID="${2:?usage: gen-certs.sh --sae <sae_id> [out_dir]}"
  OUT="${3:-./certs}"
  mkdir -p "$OUT"
  ensure_ca "$OUT" "sae-ca" "dkms-rust-sae-ca"
  SAN="URI:urn:dkms:sae:${SAE_ID},DNS:${SAE_ID}"
  echo "[gen-certs] emitiendo cert de cliente SAE ${SAE_ID} (SAN: $SAN)"
  gen_key "$OUT/${SAE_ID}.key" 2048
  openssl req -new -key "$OUT/${SAE_ID}.key" -out "$OUT/${SAE_ID}.csr" -subj "/CN=${SAE_ID}"
  sign_with "$OUT" "sae-ca" "$SAE_ID" "$SAN" "clientAuth"
  echo "[gen-certs] listo: $OUT/{sae-ca.crt,${SAE_ID}.crt,${SAE_ID}.key}"
  exit 0
fi

NODE_ID="${1:?usage: gen-certs.sh <node_id> <advertise_ip> [out_dir]}"
IP="${2:?usage: gen-certs.sh <node_id> <advertise_ip> [out_dir]}"
OUT="${3:-./certs}"
mkdir -p "$OUT"

ensure_ca "$OUT" "net-ca" "dkms-rust-net-ca"

# IP:127.0.0.1 además de DNS:localhost: un SAE co-locado con su DKMS apunta a
# `https://127.0.0.1:20005`, y una IP literal en la URL solo casa contra un SAN
# de tipo iPAddress — el DNS:localhost no la cubre. Sin esto el handshake muere
# con "no alternative certificate subject name matches target host name".
SAN="URI:dkms://${NODE_ID},IP:${IP},IP:127.0.0.1,DNS:localhost"
echo "[gen-certs] emitiendo cert de $NODE_ID (SAN: $SAN)"
gen_key "$OUT/${NODE_ID}.key" 2048
openssl req -new -key "$OUT/${NODE_ID}.key" -out "$OUT/${NODE_ID}.csr" -subj "/CN=${NODE_ID}"
sign_with "$OUT" "net-ca" "$NODE_ID" "$SAN" "serverAuth,clientAuth"
echo "[gen-certs] listo: $OUT/{net-ca.crt,${NODE_ID}.crt,${NODE_ID}.key}"
echo "[gen-certs] monta esta carpeta en el DKMS como /config/certs"
