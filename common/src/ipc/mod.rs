//! Inter-module IPC.
//!
//! Two transports:
//!
//! * [`grpc`] — tonic-based gRPC for the control plane. Schemas live in
//!   `/proto/`, generated types live under `common::proto::*`.
//!
//! * [`binary_tcp`] — custom binary TCP framing used by QKC↔QKC and by
//!   ORR↔QKC. The implementation lives in the [`wire`] crate; we
//!   re-export it here for retrocompatibility with code that addressed
//!   it as `common::ipc::binary_tcp`.

pub mod grpc;

pub mod binary_tcp {
    pub use wire::*;
}
