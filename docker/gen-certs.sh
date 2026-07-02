#!/usr/bin/env bash
# Genera el material TLS que necesita un DKMS para el mTLS ETSI-020 (DKMS<->DKMS)
# y ETSI-014 (SAE->DKMS).
#
#   gen-certs.sh <node_id> <advertise_ip> [out_dir]
#
# Produce en out_dir (default ./certs):
#   ca.crt ca.key            -> CA (se REUTILIZA si ya existe: así una sola CA
#                               firma todos los DKMS de la red)
#   <node_id>.crt <node_id>.key  -> cert de servidor del DKMS, con SAN
#                               URI:dkms://<node_id>, IP:<advertise_ip>, DNS:localhost
#
# MULTI-INSTITUCIÓN: el mTLS entre DKMS exige una raíz de confianza COMÚN. Opción
# más simple: generar la CA UNA vez (en un sitio) y distribuir ca.crt+ca.key a
# quien emita certs, o que un operador central firme el cert de cada institución.
# (Alternativa avanzada: una CA por institución que se cross-firmen.)
set -euo pipefail
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

SAN="URI:dkms://${NODE_ID},IP:${IP},DNS:localhost"
echo "[gen-certs] emitiendo cert de $NODE_ID (SAN: $SAN)"
openssl req -newkey rsa:2048 -nodes -keyout "$OUT/${NODE_ID}.key" \
  -out "$OUT/${NODE_ID}.csr" -subj "/CN=${NODE_ID}"
openssl x509 -req -in "$OUT/${NODE_ID}.csr" -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" \
  -CAcreateserial -days "$DAYS" -out "$OUT/${NODE_ID}.crt" \
  -extfile <(printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$SAN")
rm -f "$OUT/${NODE_ID}.csr"
echo "[gen-certs] listo: $OUT/{ca.crt,${NODE_ID}.crt,${NODE_ID}.key}"
echo "[gen-certs] monta esta carpeta en el DKMS como /config/certs"
