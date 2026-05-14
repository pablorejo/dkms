#!/usr/bin/env bash
# Generate a local CA + per-module server certs for dev mTLS.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p certs && cd certs

if [ ! -f ca.key ]; then
  openssl genrsa -out ca.key 4096 2>/dev/null
  openssl req -x509 -new -nodes -key ca.key -sha256 -days 3650 \
    -subj "/CN=dkms-rust-dev-ca" -out ca.crt
fi

for svc in qkc orr sdn dkms quditto; do
  if [ ! -f "${svc}.key" ]; then
    openssl genrsa -out "${svc}.key" 2048 2>/dev/null
    openssl req -new -key "${svc}.key" -subj "/CN=${svc}" -out "${svc}.csr"
    openssl x509 -req -in "${svc}.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
      -out "${svc}.crt" -days 365 -sha256 \
      -extfile <(printf "subjectAltName=DNS:%s,DNS:localhost,IP:127.0.0.1" "${svc}")
    rm -f "${svc}.csr"
  fi
done

echo "Certificates in ./certs/"
ls -1 certs 2>/dev/null || true
