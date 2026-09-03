//! ETSI GS QKD 014 + ETSI GS QKD 020 message models.
//!
//! Port directo 1:1 del paquete Python `ETSIQKD/`. Cada tipo de aquí
//! corresponde a un fichero del Python; el README incluye la tabla de
//! mapeo Python → Rust.
//!
//! Decisiones de diseño:
//!
//! * **Wire format ETSI tal cual**: campos como `source_KME_ID`,
//!   `master_SAE_ID`, `key_ID` se mantienen exactamente en JSON via
//!   `#[serde(rename = "...")]`, aunque el campo Rust use snake_case.
//! * **Sin transporte**: este crate no monta servidor ni cliente HTTP.
//!   Es solo los modelos + la factoría `from_network`. Los handlers de
//!   axum viven en `dkms/`.
//! * **`endpoint` / `available_access_methods` / `access_method`** que
//!   en pydantic se modelaban como campos `exclude=True` aquí son
//!   constantes asociadas al tipo via el trait [`EtsiMessage`], porque
//!   nunca se serializan al wire.
//! * **`Base64Bytes`** se mapea a [`Base64Bytes`], un newtype con
//!   `Serialize`/`Deserialize` custom que en JSON va como string base64
//!   y en memoria es `Vec<u8>`.
//! * **Factoría `from_network`** devuelve un enum (`Etsi014Built` /
//!   `Etsi020Built`) en lugar del polimorfismo dinámico que el Python
//!   resuelve por herencia. Equivalente semánticamente.

#![forbid(unsafe_code)]
pub mod base64bytes;
pub mod binary;
pub mod error;
pub mod message;

pub mod v014;
pub mod v020;

pub use base64bytes::Base64Bytes;
pub use error::EtsiError;
pub use message::{EtsiMessage, NetworkMessage};

/// Re-export agrupado para callers que quieran `use etsi::prelude::*;`
pub mod prelude {
    pub use crate::{
        base64bytes::Base64Bytes,
        error::EtsiError,
        message::{EtsiMessage, NetworkMessage},
        v014::{
            factory::{Etsi014, Etsi014Built},
            Etsi014Error, Etsi014GetKey, Etsi014GetKeyWithKeyIDs, Etsi014GetStatus, Etsi014Key,
            Etsi014KeyContainer, Etsi014KeyID, Etsi014KeyIDs, Etsi014KeyRequest, Etsi014Status,
        },
        v020::{
            factory::{Etsi020, Etsi020Built},
            Etsi020AckStatus, Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer,
            Etsi020ExtKeyVoidContainer, Etsi020GetVersions, Etsi020Key, Etsi020KeyID,
            Etsi020Message, Etsi020PostExtKeys, Etsi020PostExtKeysAck, Etsi020PostExtKeysVoid,
            Etsi020VersionContainer,
        },
    };
}
