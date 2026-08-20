#!/usr/bin/env bash
# Genera el material TLS que necesita un DKMS para el mTLS ETSI-020 (DKMS<->DKMS)
# y ETSI-014 (SAE->DKMS).
#
#   gen-certs.sh <node_id> <advertise_ip> [out_dir]     # cert de un DKMS
#   gen-certs.sh --sae <sae_id> [out_dir]               # cert de cliente SAE
#
# Produce en out_dir (default ./certs):
#   ca.crt ca.key            -> CA (se REUTILIZA si ya existe: así una sola CA
#                               firma todos los DKMS de la red)
#   <node_id>.crt <node_id>.key  -> cert de servidor del DKMS, con SAN
#                               URI:dkms://<node_id>, IP:<advertise_ip>, DNS:localhost
#   <sae_id>.crt <sae_id>.key    -> (--sae) cert de CLIENTE de un SAE, con SAN
#                               URI:urn:dkms:sae:<sae_id> (de ahí extrae el DKMS
#                               la identidad SAE; ver dkms/src/etsi_http/auth.rs)
#
# MULTI-INSTITUCIÓN: el mTLS entre DKMS exige una raíz de confianza COMÚN. Opción
# más simple: generar la CA UNA vez (en un sitio) y distribuir ca.crt+ca.key a
# quien emita certs, o que un operador central firme el cert de cada institución.
# (Alternativa avanzada: una CA por institución que se cross-firmen.)
set -euo pipefail

if [[ "${1:-}" == "--sae" ]]; then
  SAE_ID="${2:?usage: gen-certs.sh --sae <sae_id> [out_dir]}"
  OUT="${3:-./certs}"
  DAYS="${DAYS:-3650}"
  mkdir -p "$OUT"
  if [[ ! -f "$OUT/ca.crt" || ! -f "$OUT/ca.key" ]]; then
    echo "[gen-certs] creando CA nueva en $OUT (distribúyela a toda la red)"
    openssl req -x509 -newkey rsa:4096 -nodes -keyout "$OUT/ca.key" -out "$OUT/ca.crt" \
      -days "$DAYS" -subj "/CN=dkms-rust-ca"
  fi
  SAN="URI:urn:dkms:sae:${SAE_ID},DNS:${SAE_ID}"
  echo "[gen-certs] emitiendo cert de cliente SAE ${SAE_ID} (SAN: $SAN)"
  openssl req -newkey rsa:2048 -nodes -keyout "$OUT/${SAE_ID}.key" \
    -out "$OUT/${SAE_ID}.csr" -subj "/CN=${SAE_ID}"
  openssl x509 -req -in "$OUT/${SAE_ID}.csr" -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" \
    -CAcreateserial -days "$DAYS" -out "$OUT/${SAE_ID}.crt" \
    -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=clientAuth\n' "$SAN")
  rm -f "$OUT/${SAE_ID}.csr"
  echo "[gen-certs] listo: $OUT/{ca.crt,${SAE_ID}.crt,${SAE_ID}.key}"
  exit 0
fi

NODE_ID="${1:?usage: gen-certs.sh <node_id> <advertise_ip> [out_dir]}"
IP="${2:?usage: gen-certs.sh <node_id> <advertise_ip> [out_dir]}"
OUT="${3:-./certs}"
DAYS="${DAYS:-3650}"
mkdir -p "$OUT"

if [[ ! -f "$OUT/ca.crt" || ! -f "$OUT/ca.key" ]]; then
  echo "[gen-certs] creando CA nueva en $OUT (distribúyela a toda la red)"
  openssl req -x509 -newkey rsa:4096 -nodes -keyout "$OUT/ca.key" -out "$OUT/ca.crt" \
    -days "$DAYS" -subj "/CN=dkms-rust-ca"
else
  echo "[gen-certs] reutilizando CA existente en $OUT"
fi

# IP:127.0.0.1 además de DNS:localhost: un SAE co-locado con su DKMS apunta a
# `https://127.0.0.1:20005`, y una IP literal en la URL solo casa contra un SAN
# de tipo iPAddress — el DNS:localhost no la cubre. Sin esto el handshake muere
# con "no alternative certificate subject name matches target host name".
SAN="URI:dkms://${NODE_ID},IP:${IP},IP:127.0.0.1,DNS:localhost"
echo "[gen-certs] emitiendo cert de $NODE_ID (SAN: $SAN)"
openssl req -newkey rsa:2048 -nodes -keyout "$OUT/${NODE_ID}.key" \
  -out "$OUT/${NODE_ID}.csr" -subj "/CN=${NODE_ID}"
openssl x509 -req -in "$OUT/${NODE_ID}.csr" -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" \
  -CAcreateserial -days "$DAYS" -out "$OUT/${NODE_ID}.crt" \
  -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$SAN")
rm -f "$OUT/${NODE_ID}.csr"
echo "[gen-certs] listo: $OUT/{ca.crt,${NODE_ID}.crt,${NODE_ID}.key}"
echo "[gen-certs] monta esta carpeta en el DKMS como /config/certs"
