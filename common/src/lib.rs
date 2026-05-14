//! Shared library for every dkms_rust module.
//!
//! Anything that crosses crate boundaries (IDs, config loading, logging,
//! gRPC clients/servers, TLS) lives here. Each module's own business logic
//! does NOT belong here.

pub mod config;
pub mod crypto;
pub mod error;
pub mod ids;
pub mod ipc;
pub mod logging;
pub mod metrics;
pub mod tls;

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
    };
}
