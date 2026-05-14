//! ETSI GS QKD 020 — Quantum Key Distribution KME-to-KME interface.
//!
//! Mapeo 1:1 desde `ETSIQKD/ETSI020/` del Python:
//!
//! | Python                                | Rust                                |
//! |---------------------------------------|-------------------------------------|
//! | `ETSI020_Status.py` (enum AckStatus)  | [`ack_status::Etsi020AckStatus`]    |
//! | `ETSI020_Version.py`                  | [`version::Etsi020VersionContainer`]|
//! | `ETSI020_Key.py`                      | [`key::Etsi020Key`]                 |
//! | `ETSI020_KeyID.py`                    | [`key_id::Etsi020KeyID`]            |
//! | `ETSI020_Message.py`                  | [`message::Etsi020Message`]         |
//! | `ETSI020_ExtKey.py`                   | [`ext_key::Etsi020ExtKeyContainer`] |
//! | `ETSI020_ExtKeyAck.py`                | [`ext_key_ack::Etsi020ExtKeyAckContainer`] |
//! | `ESTI020_ExtKeyVoid.py`               | [`ext_key_void::Etsi020ExtKeyVoidContainer`] |
//! | `ETSI020_getVersions.py`              | [`get_versions::Etsi020GetVersions`] |
//! | `ETSI020_postExtKeys.py`              | [`post_ext_keys::Etsi020PostExtKeys`] |
//! | `ETSI020_postExtKeysAck.py`           | [`post_ext_keys_ack::Etsi020PostExtKeysAck`] |
//! | `ETSI020_postExtKeysVoid.py`          | [`post_ext_keys_void::Etsi020PostExtKeysVoid`] |
//! | `ETSI020.py` (factory)                | [`factory::Etsi020`]                |

pub mod ack_status;
pub mod ext_key;
pub mod ext_key_ack;
pub mod ext_key_void;
pub mod factory;
pub mod get_versions;
pub mod key;
pub mod key_id;
pub mod message;
pub mod post_ext_keys;
pub mod post_ext_keys_ack;
pub mod post_ext_keys_void;
pub mod version;

pub use ack_status::Etsi020AckStatus;
pub use ext_key::Etsi020ExtKeyContainer;
pub use ext_key_ack::Etsi020ExtKeyAckContainer;
pub use ext_key_void::Etsi020ExtKeyVoidContainer;
pub use get_versions::Etsi020GetVersions;
pub use key::Etsi020Key;
pub use key_id::Etsi020KeyID;
pub use message::Etsi020Message;
pub use post_ext_keys::Etsi020PostExtKeys;
pub use post_ext_keys_ack::Etsi020PostExtKeysAck;
pub use post_ext_keys_void::Etsi020PostExtKeysVoid;
pub use version::Etsi020VersionContainer;
