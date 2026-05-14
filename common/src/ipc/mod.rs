//! Inter-module IPC.
//!
//! Two transports:
//!
//! * [`grpc`] — tonic-based gRPC for the control plane. Schemas live in
//!   `/proto/`, generated types live under `common::proto::*`.
//!
//! * [`binary_tcp`] — custom binary TCP framing for the QKC↔QKC hot path.
//!   Faster than gRPC because there's no HTTP/2, no protobuf decode, and
//!   no base64. See `docs/ipc.md` for the wire format.

pub mod binary_tcp;
pub mod grpc;
