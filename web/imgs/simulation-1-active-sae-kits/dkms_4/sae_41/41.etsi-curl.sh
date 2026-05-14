#!/usr/bin/env bash
set -euo pipefail

RUNTIME_BASE_URL="https://api.pablopiorejoiglesias.es/api/sim/1/dkms/43"
CALLER_SAE_ID="41"
TARGET_SAE_ID="${1:-peer-sae-id}"

CERT_FILE="41.client.crt.pem"
KEY_FILE="41.client.key.pem"

echo "Requesting enc_keys from ${RUNTIME_BASE_URL} for target SAE=${TARGET_SAE_ID}"
curl --silent --show-error --fail \
  --cert "${CERT_FILE}" \
  --key "${KEY_FILE}" \
  -H "Content-Type: application/json" \
  -X POST \
  -d '{"number":1,"size":32}' \
  "${RUNTIME_BASE_URL}/api/v1/keys/${TARGET_SAE_ID}/enc_keys"

# Note: 41.ca.crt.pem is the SAE/runtime CA bundle, not the public HTTPS server CA.
# Optional: add --cacert <server-ca.pem> only if your runtime HTTPS cert is not publicly trusted.
# Example dec_keys (replace KEY_ID with value returned by enc_keys):
# curl --silent --show-error --fail \
#   --cert "${CERT_FILE}" \
#   --key "${KEY_FILE}" \
#   "${RUNTIME_BASE_URL}/api/v1/keys/${CALLER_SAE_ID}/dec_keys?key_id=KEY_ID"
