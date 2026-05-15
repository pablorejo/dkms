#!/usr/bin/env bash
# Genera material TLS para la demo-star DKMS:
#   * Una CA raíz self-signed.
#   * 4 certs servidor (dkms-11/22/33/44) con SAN URI=dkms://<id>, IP=127.0.0.1.
#   * 4 certs cliente SAE (sae_aa/bb/cc/dd) con SAN URI=sae://<id>.
# Idempotente: si ya existen no regenera. Usa RSA-2048 (PKCS8) para que
# rustls/aws-lc-rs lo cargue sin convertir.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
TLS="$ROOT/tls"
mkdir -p "$TLS"

# ─── CA raíz ──────────────────────────────────────────────────────────
if [ ! -f "$TLS/ca.key" ]; then
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$TLS/ca.key" 2>/dev/null
    openssl req -x509 -new -key "$TLS/ca.key" -days 365 \
        -subj "/CN=dkms-demo-ca" -out "$TLS/ca.crt" 2>/dev/null
    echo "  ▶ generated CA"
fi

# ─── Helper: cert firmado por la CA con SAN custom ────────────────────
gen_cert() {
    local name="$1"      # archivo base (sin extensión)
    local cn="$2"        # CN del subject
    local san="$3"       # subjectAltName completo (p.ej. URI:dkms://dkms-11,IP:127.0.0.1)
    if [ -f "$TLS/$name.crt" ]; then
        return
    fi
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
        -out "$TLS/$name.key" 2>/dev/null
    openssl req -new -key "$TLS/$name.key" -subj "/CN=$cn" \
        -out "$TLS/$name.csr" 2>/dev/null
    printf "subjectAltName = %s\n" "$san" > "$TLS/$name.ext"
    openssl x509 -req -in "$TLS/$name.csr" -CA "$TLS/ca.crt" -CAkey "$TLS/ca.key" \
        -CAcreateserial -out "$TLS/$name.crt" -days 365 \
        -extfile "$TLS/$name.ext" 2>/dev/null
    rm -f "$TLS/$name.csr" "$TLS/$name.ext"
    echo "  ▶ generated $name"
}

# ─── Certs servidor DKMS ──────────────────────────────────────────────
for id in 11 22 33 44; do
    gen_cert "dkms-$id" "dkms-$id" \
        "URI:dkms://dkms-$id,DNS:localhost,IP:127.0.0.1"
done

# ─── Certs cliente SAE ────────────────────────────────────────────────
# Un SAE por DKMS, suficiente para el smoke/saturate. Cada SAE vive en
# la instancia DKMS cuya numeración coincide con su sufijo.
gen_cert "sae_aa" "sae_aa" "URI:sae://sae_aa"
gen_cert "sae_bb" "sae_bb" "URI:sae://sae_bb"
gen_cert "sae_cc" "sae_cc" "URI:sae://sae_cc"
gen_cert "sae_dd" "sae_dd" "URI:sae://sae_dd"

rm -f "$TLS"/*.srl

echo
echo "TLS material listo en $TLS/"
ls -l "$TLS" | grep -E "\.(crt|key)$" | awk '{print "  "$NF}'
