//! SDN — the network controller. One per deployment, run by the operator.
//!
//! It owns the in-memory topology graph ([`topology`]) and everything
//! derived from it, but it does **not** read a topology file: the graph is
//! built from what the modules announce over its HTTP admin API
//! ([`http_api`]) — a QKC declares itself and its links, an ORR the QKC it
//! hangs off, a DKMS its ORR and its SAEs — and the announce loop doubles
//! as a heartbeat ([`presence`]), so a module that goes quiet is dropped
//! with its edges. Each announce response carries the peer set the module
//! should hold, which is how new nodes become reachable without touching
//! the running ones.
//!
//! From the graph and the per-link capacities it produces two things,
//! deliberately decoupled:
//!
//! * **Routes** — WCMP forwarding tables per QKC (`mcmcf::wcmp_from_topology`),
//!   pushed to each QKC's `POST /forwarding-table` whenever the topology
//!   version or the published snapshot changes ([`service`]). Routes change
//!   at topology cadence, never from instantaneous buffer levels.
//! * **Rates** — how fast each DKMS pair may fill its transport buffers,
//!   computed every `mcf_period_ms` from the demand the DKMSs report
//!   ([`demand`]) by the allocator selected in config ([`rates_num`]:
//!   proportional-fair `num` by default, `maxmin`, or the MCMCF-λ linear
//!   program in [`mcmcf`] as reference) and served back on `GET /rate`.
//!
//! Link capacity is the quditto formula for QKD links, overridden by the
//! rate the QKCs measure in situ, and the declared capacity for PQC links
//! — one decision point, `EdgeMeta::capacity_keys_per_second`.
//!
//! The gRPC surface ([`grpc_server`]) is the small remainder: SAE binding
//! lookups, ORR-level paths and a topology event stream. With `[tls]`
//! configured both planes run under mTLS and the mutating routes require a
//! network-CA certificate whose identity matches the announced id ([`mtls`]).

#![forbid(unsafe_code)]
// SdnError wraps io::Error, serde_json::Error and anyhow::Error (each
// ~176 B), so the Err-variant is unavoidably large. Boxing every #[from]
// variant would be cosmetic churn — control-plane fns don't run in hot
// paths and Result<(), SdnError> isn't passed around in tight loops.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod debounce;
pub mod demand;
pub mod error;
pub mod grpc_server;
pub mod http_api;
pub mod link_admission;
pub mod mcf;
pub mod mcmcf;
pub mod metrics;
pub mod mtls;
pub mod presence;
pub mod push;
pub mod rates_num;
pub mod routing;
pub mod service;
pub mod topology;
