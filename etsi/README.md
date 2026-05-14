# etsi

Modelos de mensaje y factorías de **ETSI GS QKD 014** y **ETSI GS QKD 020**.
Port directo 1:1 del paquete Python `code_dkms/src/ETSIQKD/`.

Este crate **no implementa servidor ni cliente HTTP**. Solo:

- Los tipos de mensaje, con sus reglas de validación.
- La codificación `base64` automática para los campos `key` (ETSI014) y
  `value` (ETSI020) — pendant del `pydantic.Base64Bytes`.
- Las factorías `Etsi014::from_network` / `Etsi020::from_network` que
  toman un `NetworkMessage` (request o response) y enrutan al subtipo
  apropiado.

El binding HTTP (axum) vive en `dkms/` y consume este crate.

## Mapeo Python → Rust

### ETSI014

| Python                                | Rust                                    |
|---------------------------------------|-----------------------------------------|
| `ETSI014_Error.py`                    | `v014::Etsi014Error`                    |
| `ETSI014_Key.py`                      | `v014::Etsi014Key`                      |
| `ETSI014_KeyContainer.py`             | `v014::Etsi014KeyContainer`             |
| `ETSI014_KeyID.py`                    | `v014::Etsi014KeyID`                    |
| `ETSI014_KeyIDs.py`                   | `v014::Etsi014KeyIDs`                   |
| `ETSI014_KeyRequest.py`               | `v014::Etsi014KeyRequest`               |
| `ETSI014_Status.py`                   | `v014::Etsi014Status`                   |
| `ETSI014_getKey.py`                   | `v014::Etsi014GetKey`                   |
| `ETSI014_getKeyWithKeyIDs.py`         | `v014::Etsi014GetKeyWithKeyIDs`         |
| `ETSI014_getStatus.py`                | `v014::Etsi014GetStatus`                |
| `ETSI014.py` (factory)                | `v014::factory::{Etsi014, Etsi014Built}`|

### ETSI020

| Python                                | Rust                                    |
|---------------------------------------|-----------------------------------------|
| `ETSI020_Status.py` (enum AckStatus)  | `v020::Etsi020AckStatus`                |
| `ETSI020_Version.py`                  | `v020::Etsi020VersionContainer`         |
| `ETSI020_Key.py`                      | `v020::Etsi020Key`                      |
| `ETSI020_KeyID.py`                    | `v020::Etsi020KeyID`                    |
| `ETSI020_Message.py`                  | `v020::Etsi020Message`                  |
| `ETSI020_ExtKey.py`                   | `v020::Etsi020ExtKeyContainer`          |
| `ETSI020_ExtKeyAck.py`                | `v020::Etsi020ExtKeyAckContainer`       |
| `ESTI020_ExtKeyVoid.py`               | `v020::Etsi020ExtKeyVoidContainer`      |
| `ETSI020_getVersions.py`              | `v020::Etsi020GetVersions`              |
| `ETSI020_postExtKeys.py`              | `v020::Etsi020PostExtKeys`              |
| `ETSI020_postExtKeysAck.py`           | `v020::Etsi020PostExtKeysAck`           |
| `ETSI020_postExtKeysVoid.py`          | `v020::Etsi020PostExtKeysVoid`          |
| `ETSI020.py` (factory)                | `v020::factory::{Etsi020, Etsi020Built}`|

> `ESTI020_ExtKeyVoid.py` es typo del Python original — aquí el fichero
> Rust se llama `ext_key_void.rs` (snake_case correcto).

## Convenciones

### Wire vs Rust

Los campos en JSON conservan el nombre del estándar ETSI (`source_KME_ID`,
`master_SAE_ID`, `key_ID`, etc.). En Rust el campo está en snake_case
(`source_kme_id`, ...) y `#[serde(rename = "...")]` hace el mapeo.

### Constantes asociadas

El pydantic modela `endpoint`, `available_access_methods` y `access_method`
como campos `exclude=True`. En Rust son constantes asociadas vía el trait
`EtsiMessage`:

```rust
impl EtsiMessage for Etsi014Status {
    const ENDPOINT: &'static str = "/status";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "GET";
}
```

### `from_network`

Cada subtipo expone su propio `from_network(&NetworkMessage) -> Option<Self>`.
La factoría general (`Etsi014` / `Etsi020`) enruta llamando al correcto a
partir de `endpoint`, `method`, `is_response` y `status_code`.

### Validación

Los validadores pydantic (`ge=0`, `gt=0`, `min_length=1`) se reproducen
como métodos `validate(&self) -> Result<(), EtsiError>`. No se ejecutan
automáticamente al deserializar (pydantic los corre en `__init__`); el
caller decide cuándo llamarlos.

## Uso

```rust
use etsi::prelude::*;

let msg = NetworkMessage {
    method: Some("GET".into()),
    endpoint: Some("status".into()),
    path: "/api/v1/keys/alice/status".into(),
    ..Default::default()
};

if let Some(Etsi014Built::GetStatus(req)) = Etsi014::from_network(&msg) {
    println!("SAE_id = {}", req.sae_id);
}
```
