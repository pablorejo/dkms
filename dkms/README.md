# dkms

**Distributed Key Management Service.** Customer-facing module:

- ETSI QKD 014 over HTTP (mTLS optional) on `:8080`
- ETSI QKD 020 (buffered) on the same port (`/api/v2/...`)
- gRPC orchestrator API on `:50054`
- `/metrics` on `:9103`

## Endpoints

ETSI 014 (`/api/v1/`):
```
GET /api/v1/keys/{slave_sae}/status
GET /api/v1/keys/{slave_sae}/enc_keys?number=1&size=256
GET /api/v1/keys/{master_sae}/dec_keys?key_ID=...
```

ETSI 020 (`/api/v2/`):
```
POST /api/v2/keys/{local_sae}/{remote_sae}/subscribe
GET  /api/v2/keys/{local_sae}/{remote_sae}/keys
```

Orchestrator gRPC: see [`/proto/dkms.proto`](../proto/dkms.proto).

## Build / run

```bash
cargo build --release -p dkms
./scripts/run-dkms.sh
```

## Backend dependencies

- **SDN** (`DKMS_SDN_URL`) for path computation and admission.
- **ORR** (`DKMS_ORR_URL`) for circuit setup.
- **QKC** (`DKMS_QKC_URL`) for key reservations.
- **QRNG / quditto** (`DKMS_QRNG_URL`, optional) for fast local key minting.
