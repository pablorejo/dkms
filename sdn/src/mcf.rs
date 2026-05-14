//! Min-cost flow refresher.
//!
//! Periodically recomputes the optimal allocation of (src,dst) demands
//! across the topology and pushes the resulting per-link weights into the
//! routing module so subsequent path queries use up-to-date costs.
//!
//! TODO: port the actual MCF solver from the Python `SDN/mcf.py`. For now
//! this is a stub.

use crate::{error::Result, topology::TopologyStore};

pub struct Demand {
    pub src: String,
    pub dst: String,
    pub bps: u64,
}

pub fn recompute(_topo: &TopologyStore, _demands: &[Demand]) -> Result<()> {
    // TODO: solve MCF, write per-link cost into topology attributes.
    Ok(())
}
