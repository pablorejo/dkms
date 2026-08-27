#!/usr/bin/env bash
# Genera material TLS para la demo-star DKMS con DOS raíces separadas
# (docs/SECURITY.md §2):
#   * net-ca  -> firma los 4 certs servidor (dkms-11/22/33/44), SAN dkms://<id>.
#   * sae-ca  -> firma los certs cliente SAE (sae_aa/bb/cc/dd, sae_001..100),
#               SAN sae://<id>.
# Idempotente: si ya existen no regenera. Usa RSA-2048 (PKCS8) para que
# rustls/aws-lc-rs lo cargue sin convertir.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
TLS="$ROOT/tls"
mkdir -p "$TLS"

# ─── Raíces (net-ca para nodos, sae-ca para SAEs) ─────────────────────
ensure_ca() {
    local base="$1" cn="$2"
    [ -f "$TLS/$base.key" ] && return
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$TLS/$base.key" 2>/dev/null
    openssl req -x509 -new -key "$TLS/$base.key" -days 365 -subj "/CN=$cn" -out "$TLS/$base.crt" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
    echo "  ▶ generated CA $base"
}
ensure_ca "net-ca" "dkms-demo-net-ca"
ensure_ca "sae-ca" "dkms-demo-sae-ca"

# ─── Helper: cert firmado por una CA dada con SAN custom ──────────────
gen_cert() {
    local ca="$1"        # base de la CA firmante (net-ca | sae-ca)
    local name="$2"      # archivo base (sin extensión)
    local cn="$3"        # CN del subject
    local san="$4"       # subjectAltName completo (p.ej. URI:dkms://dkms-11,IP:127.0.0.1)
    if [ -f "$TLS/$name.crt" ]; then
        return
    fi
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
        -out "$TLS/$name.key" 2>/dev/null
    openssl req -new -key "$TLS/$name.key" -subj "/CN=$cn" \
        -out "$TLS/$name.csr" 2>/dev/null
    printf "subjectAltName = %s\n" "$san" > "$TLS/$name.ext"
    openssl x509 -req -in "$TLS/$name.csr" -CA "$TLS/$ca.crt" -CAkey "$TLS/$ca.key" \
        -CAcreateserial -out "$TLS/$name.crt" -days 365 \
        -extfile "$TLS/$name.ext" 2>/dev/null
    rm -f "$TLS/$name.csr" "$TLS/$name.ext"
    echo "  ▶ generated $name"
}

# ─── Certs servidor DKMS (net-ca) ─────────────────────────────────────
for id in 11 22 33 44; do
    gen_cert "net-ca" "dkms-$id" "dkms-$id" \
        "URI:dkms://dkms-$id,DNS:localhost,IP:127.0.0.1"
done

# ─── Certs cliente SAE (sae-ca) ───────────────────────────────────────
# Un SAE por DKMS, suficiente para el smoke/saturate. Cada SAE vive en
# la instancia DKMS cuya numeración coincide con su sufijo.
gen_cert "sae-ca" "sae_aa" "sae_aa" "URI:sae://sae_aa"
gen_cert "sae-ca" "sae_bb" "sae_bb" "URI:sae://sae_bb"
gen_cert "sae-ca" "sae_cc" "sae_cc" "URI:sae://sae_cc"
gen_cert "sae-ca" "sae_dd" "sae_dd" "URI:sae://sae_dd"

# ─── 100 SAEs para tests de ramp ──────────────────────────────────────
# Pre-generamos sae_001..sae_100 para los tests `ramp-saes.sh`. La
# asignación SAE→DKMS es config runtime (en el controller), aquí solo
# emitimos el material TLS. Idempotente: ya generados se saltan.
for n in $(seq -f "%03g" 1 100); do
    gen_cert "sae-ca" "sae_$n" "sae_$n" "URI:sae://sae_$n"
done

rm -f "$TLS"/*.srl

echo
echo "TLS material listo en $TLS/"
ls -l "$TLS" | grep -E "\.(crt|key)$" | awk '{print "  "$NF}'
