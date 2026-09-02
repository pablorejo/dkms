# IPC

Two transports.

## 1. gRPC (tonic) — control plane

Used between every module pair that isn't on the QKC↔QKC hot path. Schemas
live in [`/proto/`](../proto/):

| Service       | Owner   | Consumers           |
|---------------|---------|---------------------|
| QkcControl    | qkc     | sdn, dkms           |
| OrrControl    | orr     | dkms, peer orrs     |
| SdnControl    | sdn     | qkc, dkms, orr      |
| DkmsControl   | dkms    | orchestrator        |
| QudittoControl| quditto | qkc                 |

Each crate generates client + server stubs into `common::proto::*` via
`common/build.rs`.

### Defaults

`common::ipc::grpc::DialOpts::default()` gives every client the same baseline:

| Option            | Value             |
|-------------------|-------------------|
| connect_timeout   | 2 s               |
| request_timeout   | 10 s              |
| keepalive_interval| 15 s              |
| keepalive_timeout | 5 s               |
| TCP_NODELAY       | true              |
| TLS               | ORR gRPC: mTLS por defecto (2026-08-28); SDN admin: opt-in (`control_tls`) |

### TLS / mTLS

Off by default in dev. To turn on, populate
`common::ipc::grpc::DialOpts.tls` (`ClientTlsConfig`) and configure the
server with `common::tls::server_config(cert, key, ca)`.

## 2. Binary TCP — QKC↔QKC hot path

Plain `tokio::net::TcpStream` with little-endian framing. Confidentiality
is owned by the OTP/quditto layer; integrity is assumed covered by MACsec
or a PQC equivalent on L2.

### Wire format

```
Fixed prefix (10 B):
  MAGIC      4 B  = b"\x51\x4B\x43\x01"   ('Q','K','C', v1)
  FRAME_TYPE 1 B  = 0x01 RECV | 0x02 RELAY
  RESERVED   1 B  = 0x00
  TOTAL_LEN  4 B  u32 LE — bytes remaining

Variable payload:
  SENDER_ID     4 B  u32 LE
  RECEIVER_ID   4 B  u32 LE
  DEST_FINAL    4 B  u32 LE
  KEY_SIZE_BITS 2 B  u16 LE
  N_KEY_IDS     1 B  u8
  KEY_ID_LEN    1 B  u8
  KEY_IDS       N_KEY_IDS * KEY_ID_LEN
  HEADER_LEN    2 B  u16 LE
  HEADER        HEADER_LEN B  (msgpack map)
  PAYLOAD_LEN   4 B  u32 LE
  PAYLOAD       PAYLOAD_LEN B (ciphertext, no base64)
```

Implementation: crate [`wire`](../wire/src/lib.rs) (`common::ipc::binary_tcp` es un re-export de `wire`; el fichero `binary_tcp.rs` no existe).

### Why not gRPC for the hot path too?

Profiling the Python equivalent showed 35-45% overhead from
JSON+pydantic+base64 on this exact path. The frame here is one read + one
write, no parsing per byte. Wire compatibility with the Python prototype
is preserved.

### Versioning

The magic byte includes a version (currently `0x01`). A peer that sees an
unknown version must close the connection (returns `WireError::BadMagic`).
