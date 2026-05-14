//! Orchestrator-facing helpers. Mirrors the Python `agent_controller.py`
//! but moves the surface onto gRPC (see `grpc_server`); this module just
//! holds the business logic for register/deregister/drain so it can be
//! tested without spinning up tonic.

use crate::{error::Result, service::DkmsService};

pub struct RegisterArgs {
    pub sae_id: String,
    pub rate_keys_per_sec: u64,
    pub burst_keys: u64,
}

pub async fn register_sae(svc: &DkmsService, args: RegisterArgs) -> Result<String> {
    svc.buckets.set_rate(&args.sae_id, args.rate_keys_per_sec, args.burst_keys);
    Ok(uuid::Uuid::new_v4().to_string())
}

pub async fn deregister_sae(_svc: &DkmsService, _sae_id: &str) -> Result<()> {
    // TODO: remove bucket + drain pending buffers.
    Ok(())
}

pub async fn drain(_svc: &DkmsService, _grace_seconds: u32) -> Result<()> {
    // TODO: stop accepting new requests, flush pending.
    Ok(())
}
