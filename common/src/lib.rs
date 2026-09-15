//! Shared library for every dkms_rust module.
//!
//! Anything that crosses crate boundaries lives here; each module's own
//! business logic does not. The boundary *between* modules is the protobuf
//! schema in `/proto/`, compiled into [`proto`] — internal types are never
//! re-exported across crates.
//!
//! What is in here, by concern:
//!
//! * **Plumbing** — [`config`] (`default.toml` ← `local.toml` ← env, with
//!   `SecretString` for anything that must not reach a log), [`logging`],
//!   [`metrics`], [`ids`] (validated newtypes for node/SAE/key ids),
//!   [`error`], [`log_throttle`], [`net`].
//! * **Transport** — [`ipc`] (the gRPC dial defaults; the binary TCP wire
//!   is the `wire` crate) and [`http`] (the mTLS-aware client the announce
//!   loops share).
//! * **Identity and channels** — [`tls`] and [`tls_pqc`] (rustls with
//!   ML-DSA-65 certificates and hybrid X25519+ML-KEM key exchange only,
//!   self-checked at boot), [`cert_identity`] (who a node is, from the SAN
//!   of its certificate).
//! * **Primitives** — [`crypto`]: ML-KEM ([`crypto::pqc`]), ML-DSA
//!   ([`crypto::pqc_sign`]), AES-256-GCM ([`crypto::aead`]), the per-frame
//!   link MAC with its anti-replay window ([`crypto::frame_mac`]), the
//!   handshake HMAC ([`crypto::link_mac`]) and OTP ([`crypto::otp`]).
//! * **Policy** — [`security`] (key grades and the security level a SAE
//!   may request), [`hardening`] (keys out of swap and core dumps).
//! * [`test_support`] — helpers for the crates' tests only.

// `unsafe` solo en hardening.rs (mlockall / RLIMIT_CORE), con allow local.
#![deny(unsafe_code)]
// `CommonError` (y los Status de tonic en el código GENERADO por prost)
// superan el umbral de `result_large_err` del clippy moderno. Boxear el
// error cruzaría todas las firmas públicas del workspace y el código
// generado ni siquiera es nuestro, así que el lint se permite a nivel de
// crate — práctica habitual en ecosistemas tonic.
#![allow(clippy::result_large_err)]

pub mod cert_identity;
pub mod config;
pub mod crypto;
pub mod error;
pub mod hardening;
pub mod http;
pub mod ids;
pub mod ipc;
pub mod log_throttle;
pub mod logging;
pub mod metrics;
pub mod net;
pub mod security;
pub mod test_support;
pub mod tls;
pub mod tls_pqc;

/// Auto-generated protobuf types and gRPC stubs.
///
/// Modules are namespaced exactly like the `package` declaration in each
/// .proto file: `common::v1`, `qkc::v1`, `orr::v1`, `sdn::v1`, etc.
pub mod proto {
    pub mod common {
        pub mod v1 {
            tonic::include_proto!("dkms.common.v1");
        }
    }
    pub mod qkc {
        pub mod v1 {
            tonic::include_proto!("dkms.qkc.v1");
        }
    }
    pub mod orr {
        pub mod v1 {
            tonic::include_proto!("dkms.orr.v1");
        }
    }
    pub mod sdn {
        pub mod v1 {
            tonic::include_proto!("dkms.sdn.v1");
        }
    }
    pub mod dkms {
        pub mod v1 {
            tonic::include_proto!("dkms.dkms.v1");
        }
    }
    pub mod quditto {
        pub mod v1 {
            tonic::include_proto!("dkms.quditto.v1");
        }
    }
}

/// Re-export commonly used items so callers can `use common::prelude::*;`.
pub mod prelude {
    pub use crate::{
        config::{load_config, ConfigError},
        error::CommonError,
        ids::{KeyId, LinkId, NodeId, SaeId},
        logging,
        security::{GradeResolution, KeyGrade, SecurityLevel},
    };
}
