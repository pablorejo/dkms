# etsi — ETSI GS QKD 014 and 020 message models

Message types, JSON encoding and validation rules for **ETSI GS QKD 014**
(the SAE-to-KME key delivery API) and **ETSI GS QKD 020** (the KME-to-KME
interface), and nothing else: this crate opens no socket and knows no HTTP
framework. Every server and client in the workspace serialises through it, so
the wire format is defined once. Each message type derives
`Serialize`/`Deserialize` and implements `EtsiMessage` (`src/message.rs`): constants `ENDPOINT`,
`AVAILABLE_ACCESS_METHODS`, `DEFAULT_ACCESS_METHOD` that never reach the
wire, `to_json()` omitting `None`, `get_endpoint_url(host)` building the full
path. Dependencies: `serde`, `serde_json`, `uuid`, `base64`, `thiserror`.

## Where the transport lives

| Binding | Where |
|---|---|
| ETSI-014 server (SAE plane): `status`, `enc_keys`, `dec_keys` to SAEs over mTLS | `dkms/src/etsi_http/v014.rs` |
| ETSI-020 server (peer plane): `ext_keys`, `ext_keys/ack`, `versions` from peer DKMSs | `dkms/src/etsi_http/v020.rs` |
| ETSI-020 client: pushes `ext_keys` to a peer (the ACK comes back in the response) and posts `ext_keys/ack` | `dkms/src/peer_client.rs` |
| ETSI-014 server: the simulated KME | `quditto/src/server.rs` |
| ETSI-014 client: the QKC pulling link material from a KME, quditto or real | `qkc/src/kme.rs` |

## Types

### `etsi::v014`

| Type | Models |
|---|---|
| `Etsi014GetStatus` | Request `GET /api/v1/keys/{SAE_id}/status`. |
| `Etsi014Status` | Its response: `source_KME_ID`, `target_KME_ID`, `master_SAE_ID`, `slave_SAE_ID`, `key_size`, `stored_key_count`, `max_key_count`, `max_key_per_request`, `max_key_size`, `min_key_size`, `max_SAE_ID_count`, `status_extension`. |
| `Etsi014GetKey` | Request for `enc_keys`: `SAE_id` from the path plus an `Etsi014KeyRequest`, from a POST body or from `GET ?number=&size=`. |
| `Etsi014KeyRequest` | The "Key request" object: `number` (default 1), `size` (default 256), `additional_slave_SAE_IDs` (also accepted as `additional_saes`, as a list, a CSV string or a JSON string), `extension_mandatory`, `extension_optional`. The DKMS reads the requested security grade from the extensions (`dkms/src/security_level.rs`). |
| `Etsi014GetKeyWithKeyIDs` | Request for `dec_keys`: `SAE_id` plus `Etsi014KeyIDs`, from a POST body or from `GET ?key_ID=`. |
| `Etsi014KeyIDs`, `Etsi014KeyID` | The "Key IDs" object: `key_IDs: [{key_ID, key_ID_extension}]`. |
| `Etsi014KeyContainer`, `Etsi014Key` | The response of `enc_keys` and `dec_keys`: `keys: [{key_ID, key, key_ID_extension, key_extension}]`, `key_container_extension`. |
| `Etsi014Error` | Error body: `message` and optional `details` (a list of objects or of strings). |

### `etsi::v020`

The `Post…` types flatten their container, so their JSON is the container's.

| Type | Models |
|---|---|
| `Etsi020GetVersions`, `Etsi020VersionContainer` | `GET …/versions` and its response `{versions: [...]}`. |
| `Etsi020PostExtKeys`, `Etsi020ExtKeyContainer`, `Etsi020Key` | `POST …/ext_keys`: `keys: [{key_id, value, extension}]`, `initiator_sae_id`, `target_sae_ids`, `ack_callback_url`, `extension_mandatory`, `extension_optional`. |
| `Etsi020PostExtKeysAck`, `Etsi020ExtKeyAckContainer`, `Etsi020KeyID`, `Etsi020AckStatus` | `POST …/ext_keys/ack`: `key_ids`, `ack_status` (`relayed`, `voided`, `failed`, `key not present`), `initiator_sae_id`, `target_sae_id`, `message`. |
| `Etsi020PostExtKeysVoid`, `Etsi020ExtKeyVoidContainer` | `POST …/ext_keys/void`: `keys` (UUIDs to invalidate), `initiator_sae_id`, `target_sae_ids`, `ack_callback_url`; the `all_confirmation` query flag lives on the struct and is never serialised. |
| `Etsi020Message` | Error body: `message` and optional `details`. |

## Conventions

**Wire-format fidelity.** JSON keeps the standards' field names and their
capitalisation (`source_KME_ID`, `master_SAE_ID`, `key_ID`, `key_IDs`,
`SAE_id`, `max_SAE_ID_count`); Rust fields are snake_case and
`#[serde(rename = "...")]` maps between the two. Optional fields are omitted
when `None`; `*_extension` fields are free JSON objects passed through as is.

**`Base64Bytes`** (`src/base64bytes.rs`): a `Vec<u8>` newtype that serialises
as a standard, padded base64 string and rejects anything that does not
decode. It types `key` in 014 and `value` in 020, so callers handle raw bytes.

**Validation** is explicit, `validate(&self) -> Result<(), EtsiError>`, and
deserialising does not run it: `number ≥ 1`, `size > 0`, a key of at least
one byte, non-empty `keys` and `target_sae_ids`, `min_key_size ≤
max_key_size`, `stored_key_count ≤ max_key_count`.

**`from_network` factories.** `NetworkMessage` (`src/message.rs`) is a
transport-neutral view of a request or response (`is_response`, `method`,
`endpoint`, `path`, `status_code`, `url_parameters`, `headers`, `data`).
`Etsi014::from_network` returns `Option<Etsi014Built>`, one variant per
message (`GetStatus`, `GetKey`, `GetKeyWithKeyIDs`, `Status`, `KeyContainer`,
`Error` for status 400, 501, 503); `Etsi020::from_network` covers
`GetVersions`, `PostExtKeysVoid`, `VersionContainer` and `Error` (400, 401,
408, 503, 555) — `ext_keys` and `ack` bodies deserialise directly into their
containers. Unrecognised input gives `None`. The DKMS handlers are not on
this path: `axum::Json` deserialises bodies straight into the types.

**Binary encoding** (`src/binary.rs`): an alternative to JSON + base64 for
`enc_keys`/`dec_keys`, outside the standard but negotiated with
`Accept: application/octet-stream` (and `Content-Type` on the POST body):
magic `QKDB`, version `0x01`, `key_size_bits` (u16 LE), a count (u32 LE), then
raw 16-byte UUIDs each followed by raw key bytes (`pack_keys`/`unpack_keys`);
the `dec_keys` request body (`pack_key_ids`/`unpack_key_ids`) has the same
header without `key_size_bits` and carries only the UUIDs. quditto serves it;
no client in the workspace requests it — the QKC speaks JSON so that real
KMEs work unchanged.

## Further reading

- [docs/architecture.md § 4](../docs/architecture.md#4-flow-2-a-sae-asks-for-a-key) — the request these messages carry, end to end, from `enc_keys` to `dec_keys`.
- [dkms/README.md](../dkms/README.md) — the ETSI-014 and ETSI-020 planes of the DKMS, certificates, the session-key flow.
- [quditto/README.md](../quditto/README.md) — the simulated KME and its endpoints.
- [docs/ipc.md](../docs/ipc.md) — the transports between modules.
- ETSI GS QKD 014 V1.1.1 (the version `dkms/src/etsi_http/v014.rs` cites) and
  ETSI GS QKD 020 (no version pinned; `GET /kmapi/v1/versions` answers `["1.0"]`).
