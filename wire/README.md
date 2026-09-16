# wire — the binary TCP frame

The one frame format of the data plane, shared by two connections: QKC to
QKC (port 20000, every key-bearing frame between neighbours) and ORR to QKC
(port 20001, the local hand-off in both directions). Same parser, same
`Frame`, only the `kind` differs. `common::ipc::binary_tcp` re-exports this
crate (kept for compatibility; nothing in the workspace uses that path
today). Where the wire sits among the three planes, and why the data plane
is binary at all, is in [docs/ipc.md](../docs/ipc.md); the hop-by-hop flow
a frame takes, and which layer owns which field of it, in
[docs/architecture.md](../docs/architecture.md#3-flow-1-filling-the-transport-buffers);
the API (`Frame`, `read_frame`, `read_frame_body_timeout`, `write_frame`, the
trailer and NOTIFY helpers) is in the crate doc: `make doc-open`.

## The layers

A frame carries one payload and one cleartext header per layer above the
QKC. Each layer writes and reads only its own:

| Field | Written by | Read by | The QKC |
|-------|-----------|---------|---------|
| `sender_id`, `receiver_id`, `key_size_bits`, `key_ids` | the sending QKC of each hop | the next QKC | rewrites them hop by hop (`key_ids` are the OTP keys spent on that link) |
| `dest_final`, `grade` | the ORR, in `FRAME_LOCAL_SEND` | every QKC on the path | propagates; `grade` (`0` QKD, `1` PQC, the prefix's RESERVED byte) picks the QKD-only or the full forwarding table |
| `epoch_id` | the origin ORR: which `master_secret` epoch its onion layer uses, `0` = none | the destination ORR | propagates byte for byte — a relay that reset it to 0 silently broke rotation once ([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas), "The QKC must propagate `frame.epoch_id`") |
| `header_orr` | the origin ORR; rewritten by each ORR that peels a layer | ORRs | propagates byte for byte |
| `header_dkms` | the origin DKMS (key id, the sender's id, key size, `incarnation`, e2e epoch, counter and tag, `ack_endpoint`), handed to the ORR as the `app_header` map of its gRPC | the destination DKMS only | propagates byte for byte; the ORR serialises the map it was given and re-encodes the same map unchanged at every ORR hop |
| payload | the origin DKMS: 32 bytes under its e2e seal, then the ORR's optional onion layers, then the link OTP | peeled in reverse | the only field that spends QKD key material; the link MAC trailer is added after OTP encryption and removed before decryption |

Everything that is not secret rides in a header, because on a `qkd` link
each `key_size_bits / 8` block of payload costs one QKD key
([ipc.md § Why binary](../docs/ipc.md#why-binary); the measured cost of
getting it wrong is in the [engineering
notes](../docs/engineering-notes.md#active-known-issues--gotchas), "On an
OTP link, every byte added to the payload").

## Byte layout

Little-endian, no padding, except `EPOCH_ID` (big-endian, to match the HMAC
input of the ORR rotation). `read_frame` returns a `Frame` with `kind` and
`grade` filled from the prefix.

```text
MAGIC 4 B ('Q','K','C',0x03)  KIND 1 B  GRADE 1 B  TOTAL_LEN u32 LE (bytes that follow)
SENDER_ID u32  RECEIVER_ID u32  DEST_FINAL u32  KEY_SIZE_BITS u16 (0 = plaintext)
EPOCH_ID u32 BE  N_KEY_IDS u8  KEY_ID_LEN u8  KEY_IDS N×LEN
HDR_ORR_LEN u16 + msgpack  HDR_DKMS_LEN u16 + msgpack  PAYLOAD_LEN u32 + payload
```

## Frame kinds

| Constant | Hex | Direction | Payload |
|----------|-----|-----------|---------|
| `FRAME_RECV` | `0x01` | QKC → QKC | data for this receiver; OTP ciphertext |
| `FRAME_RELAY` | `0x02` | QKC → QKC | data in transit; decrypted and re-encrypted per link, `dest_final` kept |
| `FRAME_ACK` | `0x03` | — | defined, emitted by no module (the DKMS ACK travels over ETSI-020) |
| `FRAME_RECV_AUTH` | `0x04` | QKC → QKC | `0x01` with the link MAC trailer |
| `FRAME_RELAY_AUTH` | `0x05` | QKC → QKC | `0x02` with the link MAC trailer |
| `FRAME_LOCAL_SEND` | `0x10` | ORR → QKC | plaintext to encrypt and route to `dest_final` |
| `FRAME_LOCAL_DELIVER` | `0x11` | QKC → ORR | plaintext that arrived for this node |
| `FRAME_KEY_IDS_NOTIFY` | `0x20` | QKC → QKC, same link | `count` u32 LE + `count × 16` raw UUIDs the sender just took from the link's KME |
| `FRAME_PQC_KEM_INIT` | `0x21` | initiator (lower `qkc_id`) → responder | `epoch_be(4) ‖ ML-KEM public key` |
| `FRAME_PQC_KEM_RESP` | `0x22` | responder → initiator | `epoch_be(4) ‖ ML-KEM ciphertext` |
| `FRAME_PQC_KEM_INIT_AUTH` | `0x23` | as `0x21` | payload ‖ 32-byte HMAC keyed by `link_psk` (`pqc_auth = prefer\|require`) |
| `FRAME_PQC_KEM_RESP_AUTH` | `0x24` | as `0x22` | same |
| `FRAME_KEY_IDS_NOTIFY_AUTH` | `0x25` | as `0x20` | with the link MAC trailer, so the NOTIFY shares the counter and replay window of the data frames |
| `FRAME_PQC_KEM_INIT_SIGNED` | `0x26` | as `0x21` | `epoch_be(4) ‖ certificate chain ‖ ML-KEM public key ‖ ML-DSA-65 signature` (`pqc_auth = sign`); the chain is `u16 count ‖ (u32 len ‖ DER)*`, the sender's node certificate verified against the network CA, empty with the legacy `sign_secret_seed` |
| `FRAME_PQC_KEM_RESP_SIGNED` | `0x27` | as `0x22` | same layout with the ciphertext in place of the public key |
| `FRAME_PQC_RESYNC_REQ` | `0x28` | responder → initiator | `epoch_be(4)`, its highest epoch: asks the only end allowed to send INIT to relink above both windows |
| `FRAME_PQC_RESYNC_REQ_AUTH` | `0x29` | as `0x28` | HMAC variant |
| `FRAME_PQC_RESYNC_REQ_SIGNED` | `0x2A` | as `0x28` | signed variant |

The link MAC trailer (`AUTH_TRAILER_LEN` = 48: `session(8) ‖ counter(8) ‖
tag(32)`, appended to the payload) is what `is_auth_kind`, `auth_kind_for`,
`base_kind_of`, `append_auth_trailer` and `split_auth_trailer` handle; the
HMAC itself lives in `common::crypto::frame_mac`. A receiver ignores kinds
it does not know, which is what makes the `off | prefer | require` rollout
of `pqc_auth` and `frame_auth` possible.

## Size caps

`MAX_TOTAL_LEN` is 1 MiB, checked on the announced length before anything
is allocated, and the body is read in 16 KiB chunks: an unauthenticated
10-byte prefix cannot make the reader reserve more than what actually
arrives. The worst legal frame is about five times smaller (up to 255 key ids
of 255 bytes, two headers of up to 65 535 bytes, a 32-byte data payload, a
NOTIFY of 4 + 128 × 16 bytes, a handshake of a few KB). `write_frame` calls
`validate_for_encode` first: a field longer than its `u8`/`u16` prefix is an
error, not a silent truncation that would ship a well-formed corrupt frame.
`read_frame_body_timeout` bounds the body once the prefix is in; the prefix
itself may take as long as the link is idle.

## Versioning

The last byte of `MAGIC` is the wire version. `v3` added `EPOCH_ID` for the
ORR's `master_secret` rotation and is not bit-compatible with `v2`: a `v3`
reader answers a `v2` frame with `WireError::BadMagic` and the connection
handler closes, and vice versa. The incompatibility is deliberate, so that a
mixed deployment fails loudly instead of misparsing. The crate is
`#![forbid(unsafe_code)]`.
