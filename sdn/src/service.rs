//! Cross-cutting service state shared by the gRPC, HTTP, and background
//! workers. This is the orchestration glue between [`TopologyStore`],
//! [`BufferPriorityRegistry`], [`McfSolver`], and the live
//! [`McfSnapshot`].
//!
//! The Python equivalent is the bag of state hanging off `app.state` in
//! `code_dkms/src/SDN/main.py` plus the `Topology.recompute_mcf` method.
//! We split it cleanly: `TopologyStore` keeps only graph/entities;
//! `SdnService` keeps everything that's *driven by* the topology but
//! lives at a different cadence (priorities, last MCF snapshot, the
//! solver, etc.).

use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use common::metrics::Metrics;
use parking_lot::RwLock;
use std::collections::HashMap;
use tracing::{info, warn};

use crate::{
    config::SdnConfig,
    debounce::{estimate_timing, Debouncer},
    error::Result,
    mcf::{Commodity, McfSnapshot, McfSolver},
    priority::{BufferPriorityRegistry, BufferRole, TrafficPriority},
    push::Pushers,
    topology::{Topology, TopologyStore},
};

/// Cached commodity set, valid for a single topology version.
type CommoditiesCache = RwLock<Option<(i64, Arc<Vec<Commodity>>)>>;

#[derive(Clone)]
pub struct SdnService {
    pub cfg: Arc<SdnConfig>,
    pub topology: TopologyStore,
    pub priorities: Arc<BufferPriorityRegistry>,
    pub solver: McfSolver,
    pub mcf_snapshot: Arc<ArcSwap<McfSnapshot>>,
    pub pushers: Arc<Pushers>,
    pub metrics: Metrics,
    /// `(topology_version, commodities)`. Rebuilt whenever the
    /// topology version differs from the cached one.
    commodities: Arc<CommoditiesCache>,
    /// Coalesces bursts of `request_recompute()` calls into one MCF
    /// solve. Built lazily — only constructed when there's a tokio
    /// runtime available (see `attach_debouncer`).
    debouncer: Arc<RwLock<Option<Debouncer>>>,
}

impl SdnService {
    pub async fn new(cfg: SdnConfig, metrics: Metrics) -> Result<Self> {
        let initial = match &cfg.topology_dir {
            Some(p) => match Topology::load_from_folder(std::path::Path::new(p)) {
                Ok(t) => t,
                Err(e) => {
                    warn!(path = %p, error = %e, "failed to load initial topology, starting empty");
                    Topology::default()
                }
            },
            None => Topology::default(),
        };
        let solver = McfSolver::new(cfg.mcf_k_paths);
        let svc = Self {
            cfg: Arc::new(cfg),
            topology: TopologyStore::new(initial),
            priorities: Arc::new(BufferPriorityRegistry::new()),
            solver,
            mcf_snapshot: Arc::new(ArcSwap::from_pointee(McfSnapshot::default())),
            pushers: Arc::new(Pushers::new()),
            metrics,
            commodities: Arc::new(RwLock::new(None)),
            debouncer: Arc::new(RwLock::new(None)),
        };
        // Pre-warm the MCF snapshot so /rate, /forwarding-table, etc.
        // answer something coherent before the first event fires.
        svc.recompute_mcf();
        Ok(svc)
    }

    /// Convenience helper for the HTTP `/rate` endpoints: look up the
    /// keys-per-second rate the MCF assigned to a single buffer.
    pub fn rate_for_buffer(&self, dkms_id: &str, peer_dkms_id: &str, role: BufferRole) -> f64 {
        self.mcf_snapshot
            .load()
            .rate_for_buffer(dkms_id, peer_dkms_id, role)
    }

    /// Re-run the MCF solve end-to-end and publish the resulting
    /// snapshot. Mirrors `Topology.recompute_mcf` in the Python.
    pub fn recompute_mcf(&self) -> Arc<McfSnapshot> {
        recompute_mcf_inner(
            &self.topology,
            &self.priorities,
            self.solver,
            &self.mcf_snapshot,
            &self.commodities,
        )
    }

    /// Force a rebuild of the commodity cache on the next call. Useful
    /// when the topology mutated through paths that don't go via
    /// [`TopologyStore::mutate`] (we shouldn't have any, but it's a
    /// cheap escape hatch).
    pub fn invalidate_commodities(&self) {
        *self.commodities.write() = None;
    }

    // ---------- debouncer wiring -----------------------------------------

    /// Build and attach the debouncer. Must be called from inside a
    /// tokio runtime (it spawns the worker task). Idempotent: a second
    /// call replaces the previous debouncer and shuts the old one
    /// down.
    ///
    /// Timings are auto-tuned via [`estimate_timing`] from the current
    /// DKMS count, **unless** overridden by `window_override` /
    /// `max_wait_override`.
    pub fn attach_debouncer(
        &self,
        window_override: Option<Duration>,
        max_wait_override: Option<Duration>,
    ) {
        let n = self.topology.load().dkms.len();
        let (_est, auto_window, auto_max_wait) = estimate_timing(n);
        let window = window_override.unwrap_or(auto_window);
        let max_wait = max_wait_override.unwrap_or(auto_max_wait);

        // Capture cloned handles (not `self`) so the closure has no
        // back-reference and the Drop chain doesn't form a cycle.
        let topo = self.topology.clone();
        let prios = self.priorities.clone();
        let solver = self.solver;
        let snap = self.mcf_snapshot.clone();
        let cache = self.commodities.clone();
        let new_deb = Debouncer::new(window, max_wait, move || {
            recompute_mcf_inner(&topo, &prios, solver, &snap, &cache);
        });
        info!(
            n_dkms = n,
            window_ms = window.as_millis() as u64,
            max_wait_ms = max_wait.as_millis() as u64,
            "debouncer attached"
        );

        let mut slot = self.debouncer.write();
        if let Some(old) = slot.take() {
            old.shutdown();
        }
        *slot = Some(new_deb);
    }

    /// Re-tune timings based on the current DKMS count. No-op if the
    /// debouncer isn't attached yet.
    pub fn autotune_debouncer(&self) {
        let Some(d) = self.debouncer.read().clone() else {
            return;
        };
        let n = self.topology.load().dkms.len();
        let (_est, w, mw) = estimate_timing(n);
        d.update_timing(w, mw);
        info!(
            n_dkms = n,
            window_ms = w.as_millis() as u64,
            max_wait_ms = mw.as_millis() as u64,
            "debouncer auto-tuned"
        );
    }

    /// Ask for a recompute. Routes through the debouncer if attached,
    /// otherwise falls back to a synchronous recompute (useful for
    /// tests and CLI tooling).
    pub fn request_recompute(&self) {
        match self.debouncer.read().as_ref() {
            Some(d) => d.request(),
            None => {
                self.recompute_mcf();
            }
        }
    }

    /// Flush any pending recompute. Used on graceful shutdown.
    pub fn flush_debouncer(&self) {
        if let Some(d) = self.debouncer.read().as_ref() {
            d.flush();
        }
    }

    /// Stop the debouncer worker. Pending fires are dropped.
    pub fn shutdown_debouncer(&self) {
        if let Some(d) = self.debouncer.write().take() {
            d.shutdown();
        }
    }

    /// Set a buffer's priority and either fire the recompute right
    /// away or route through the debouncer.
    ///
    /// `Saturated` and `BestEffort` are "the buffer is full, please
    /// slow down" signals — propagating them through a 2-8 s
    /// debouncer means the sender keeps tipping keys into a peer
    /// that doesn't want them during the entire grace period. So
    /// those bypass and recompute immediately. Higher classes can be
    /// coalesced safely.
    pub fn set_priority_and_recompute(
        &self,
        dkms_id: &str,
        peer_dkms_id: &str,
        role: BufferRole,
        priority: TrafficPriority,
    ) -> Arc<McfSnapshot> {
        self.priorities.set(dkms_id, peer_dkms_id, role, priority);
        if priority == TrafficPriority::Saturated || priority == TrafficPriority::BestEffort {
            return self.recompute_mcf();
        }
        self.request_recompute();
        // The recompute may run later; in the meantime callers get the
        // currently-published snapshot.
        self.mcf_snapshot.load_full()
    }

    /// Background loop: attaches the debouncer on the first tick and
    /// then ticks every `mcf_period_ms` as a heartbeat, asking for a
    /// recompute (which the debouncer will coalesce with whatever
    /// just came in over HTTP).
    ///
    /// También spawnea un *version watcher* (200 ms tick) que detecta
    /// cambios en `topology.version` y emite un `TopologyEvent` por el
    /// `Pushers` para invalidar cachés en clientes (ORR, DKMS).
    pub async fn run_background_tasks(self) -> Result<()> {
        info!("sdn: background tasks started");
        // Attach the debouncer now, inside the tokio runtime. Honour
        // any explicit overrides set on the config; otherwise the
        // attach_debouncer call below auto-tunes from the DKMS count.
        let window_override = (self.cfg.push_debounce_ms != 0)
            .then(|| Duration::from_millis(self.cfg.push_debounce_ms));
        self.attach_debouncer(window_override, None);

        // Version watcher → broadcast TopologyEvent + push de forwarding
        // tables a los QKCs. Cualquier mutación del grafo dispara:
        //   1. Broadcast a subscribers (DKMS/ORR vacían cachés).
        //   2. POST a `http://<qkc.host>/forwarding-table` con la tabla
        //      `dst_qkc → next_hop` recalculada para cada QKC.
        // El push inicial (al arrancar SDN con `topology_dir` cargada)
        // se dispara forzando un primer ciclo: arrancamos `last = -1`.
        {
            let topology = self.topology.clone();
            let pushers = self.pushers.clone();
            tokio::spawn(async move {
                use common::proto::sdn::v1::{
                    topology_event, Topology as ProtoTopology, TopologyEvent,
                };
                let http = reqwest::Client::builder()
                    .timeout(Duration::from_millis(1500))
                    .build()
                    .expect("reqwest client");
                let mut last: i64 = -1; // fuerza primer push al arrancar
                let mut tick = tokio::time::interval(Duration::from_millis(200));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let snap = topology.load();
                    let now = snap.version;
                    if now != last {
                        // 1) Forwarding push: una POST por QKC. Concurrent
                        //    via join_all para no serializar 9 QKCs.
                        let mut futures = Vec::new();
                        for (qkc_id, qkc) in &snap.qkcs {
                            let mut table = std::collections::HashMap::<String, u32>::new();
                            for other in snap.qkcs.keys() {
                                if other == qkc_id {
                                    continue;
                                }
                                if let Some(next) = snap.next_hop_qkc(qkc_id, other) {
                                    if let Ok(nh) = next.parse::<u32>() {
                                        table.insert(other.clone(), nh);
                                    }
                                }
                            }
                            let url = format!(
                                "http://{}:{}/forwarding-table",
                                qkc.host.ip, qkc.host.port
                            );
                            let body = serde_json::json!({"replace": table});
                            let client = http.clone();
                            let qid = qkc_id.clone();
                            futures.push(async move {
                                match client.post(&url).json(&body).send().await {
                                    Ok(r) if r.status().is_success() => Ok::<String, String>(qid),
                                    Ok(r) => Err(format!("{qid}: HTTP {}", r.status())),
                                    Err(e) => Err(format!("{qid}: {e}")),
                                }
                            });
                        }
                        let results: Vec<std::result::Result<String, String>> =
                            futures::future::join_all(futures).await;
                        let (ok, err): (Vec<_>, Vec<_>) =
                            results.into_iter().partition(|r| r.is_ok());
                        info!(
                            from = last,
                            to = now,
                            qkcs_ok = ok.len(),
                            qkcs_err = err.len(),
                            "forwarding push done"
                        );
                        for e in err.iter().take(3) {
                            if let Err(s) = e {
                                warn!(error = %s, "forwarding push failed");
                            }
                        }

                        // 2) Broadcast del evento (DKMS/ORR invalidan).
                        let ev = TopologyEvent {
                            event: Some(topology_event::Event::Snapshot(ProtoTopology {
                                nodes: Vec::new(),
                                links: Vec::new(),
                                version: now,
                            })),
                            version: now,
                        };
                        pushers.broadcast(ev).await;
                        info!(from = last, to = now, "topology version changed; broadcast");

                        // SOLO marcamos esta versión como "ya empujada"
                        // si TODOS los QKCs aceptaron el POST. Si alguno
                        // falló (típicamente connection refused porque
                        // el QKC todavía no levantó), no actualizamos
                        // `last` para reintentar en el próximo tick.
                        // Esto hace el orden de arranque irrelevante.
                        if err.is_empty() {
                            last = now;
                        } else {
                            warn!(
                                qkcs_err = err.len(),
                                "forwarding push partial; will retry next tick"
                            );
                        }
                    }
                }
            });
        }

        let mut tick = tokio::time::interval(Duration::from_millis(self.cfg.mcf_period_ms));
        loop {
            tick.tick().await;
            self.request_recompute();
            // TODO (DiffPusher milestone): diff the published snapshot
            // and push deltas to DKMS/QKC.
        }
    }

    /// Resolve the cached commodity set for the given topology
    /// snapshot, rebuilding if the version has bumped. Exposed for
    /// debug endpoints / tests that want to introspect what the
    /// solver is operating on.
    pub fn commodities_for(&self, topo: &Topology) -> Arc<Vec<Commodity>> {
        get_or_build_commodities(&self.commodities, self.solver, topo)
    }
}

// ---------------- free-function helpers shared with the debouncer closure ----

/// Look up commodities for `topo`, rebuilding the cache if its
/// version has bumped. Free function so the debouncer closure can
/// call it with just the cache + solver references it captures.
fn get_or_build_commodities(
    cache: &CommoditiesCache,
    solver: McfSolver,
    topo: &Topology,
) -> Arc<Vec<Commodity>> {
    // Fast path: read lock, hope the cache matches the version.
    {
        let r = cache.read();
        if let Some((v, c)) = r.as_ref() {
            if *v == topo.version {
                return c.clone();
            }
        }
    }
    // Slow path: take the write lock, double-check, build.
    let mut w = cache.write();
    if let Some((v, c)) = w.as_ref() {
        if *v == topo.version {
            return c.clone();
        }
    }
    let built = Arc::new(solver.build_commodities(topo));
    *w = Some((topo.version, built.clone()));
    built
}

/// Full recompute pipeline. Free function because the debouncer
/// closure captures these references directly — putting the body on
/// `SdnService` would force the closure to capture `self`, creating
/// a reference cycle through `SdnService::debouncer`.
///
/// Steps:
///   1. Snapshot the topology + commodities (cached per version).
///   2. Read per-flow weight from the priority registry, applying
///      `min(ENC(src,dst), DEC(dst,src))`. Either endpoint being
///      saturated drops the weight to 0 and excludes the flow.
///   3. Run the solver on the *active* commodities only.
///   4. Stitch explicit `rate=0` entries for the excluded flows so
///      `/rate` returns 0 instead of "not found".
///   5. Atomically publish the new snapshot via [`ArcSwap`].
fn recompute_mcf_inner(
    topo_store: &TopologyStore,
    priorities: &Arc<BufferPriorityRegistry>,
    solver: McfSolver,
    snap_cell: &Arc<ArcSwap<McfSnapshot>>,
    cache: &Arc<CommoditiesCache>,
) -> Arc<McfSnapshot> {
    let topo = topo_store.load();
    let commodities = get_or_build_commodities(cache, solver, &topo);

    // ---- weights -----------------------------------------------------
    let mut weights: HashMap<String, f64> = HashMap::with_capacity(commodities.len());
    for c in commodities.iter() {
        let enc = priorities.get(&c.src_dkms, &c.dst_dkms, BufferRole::EncKeys);
        let dec = priorities.get(&c.dst_dkms, &c.src_dkms, BufferRole::DecKeys);
        weights.insert(c.flow_id(), enc.weight().min(dec.weight()));
    }

    // ---- partition active / excluded ---------------------------------
    let active: Vec<Commodity> = commodities
        .iter()
        .filter(|c| weights.get(&c.flow_id()).copied().unwrap_or(0.0) > 0.0)
        .cloned()
        .collect();
    let active_weights: HashMap<String, f64> = active
        .iter()
        .map(|c| (c.flow_id(), weights[&c.flow_id()]))
        .collect();
    let excluded_count = commodities.len() - active.len();

    // ---- solve --------------------------------------------------------
    let caps = solver.capacities(&topo);
    let mut snap = solver.solve(&active, &caps, &active_weights);

    // ---- pin rate=0 for excluded flows --------------------------------
    for c in commodities.iter() {
        if weights.get(&c.flow_id()).copied().unwrap_or(0.0) > 0.0 {
            continue;
        }
        snap.rates.entry(c.flow_id()).or_insert(0.0);
        snap.rates_by_dkms
            .entry(c.src_dkms.clone())
            .or_default()
            .insert((c.dst_dkms.clone(), BufferRole::EncKeys), 0.0);
        snap.rates_by_dkms
            .entry(c.dst_dkms.clone())
            .or_default()
            .insert((c.src_dkms.clone(), BufferRole::DecKeys), 0.0);
    }

    // ---- publish ------------------------------------------------------
    let arc = Arc::new(snap);
    snap_cell.store(arc.clone());

    info!(
        commodities = commodities.len(),
        excluded = excluded_count,
        flows_with_rate = arc.rates.values().filter(|r| **r > 0.0).count(),
        "mcf recomputed"
    );

    arc
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Dkms, EdgeMeta, HostEndpoint, Orr, Qkc};

    fn host(id: i64) -> HostEndpoint {
        HostEndpoint {
            id,
            ip: format!("10.0.0.{id}"),
            port: 9000 + id as u16,
        }
    }

    fn small_topo() -> Topology {
        let mut t = Topology::default();
        for q in ["1", "2", "3"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 100.0,
                alpha: 0.2,
                max_buffer_size: 10,
            },
        );
        t.add_edge(
            "2",
            "3",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 100.0,
                alpha: 0.2,
                max_buffer_size: 10,
            },
        );
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: host(11),
            qkc_id: "1".into(),
        });
        t.upsert_orr(Orr {
            id: "o3".into(),
            host: host(13),
            qkc_id: "3".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dA".into(),
            host: host(21),
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(23),
            tls_id: None,
            orr_id: "o3".into(),
        });
        t.version = 1;
        t
    }

    fn make_service() -> SdnService {
        SdnService {
            cfg: Arc::new(SdnConfig {
                node_id: "test".into(),
                grpc_addr: "0.0.0.0:0".into(),
                http_addr: "0.0.0.0:0".into(),
                metrics_addr: "0.0.0.0:0".into(),
                topology_dir: None,
                default_policy: "shortest_hops".into(),
                mcf_period_ms: 60_000,
                push_debounce_ms: 100,
                mcf_k_paths: 3,
            }),
            topology: TopologyStore::new(small_topo()),
            priorities: Arc::new(BufferPriorityRegistry::new()),
            solver: McfSolver::new(3),
            mcf_snapshot: Arc::new(ArcSwap::from_pointee(McfSnapshot::default())),
            pushers: Arc::new(Pushers::new()),
            metrics: Metrics::new("sdn-test"),
            commodities: Arc::new(RwLock::new(None)),
            debouncer: Arc::new(RwLock::new(None)),
        }
    }

    #[test]
    fn recompute_populates_snapshot() {
        let svc = make_service();
        let snap = svc.recompute_mcf();
        // dA→dB and dB→dA both reachable.
        assert!(snap.rate_for_flow("dA", "dB") > 0.0);
        assert!(snap.rate_for_flow("dB", "dA") > 0.0);
        // ArcSwap was actually published — load() returns same data.
        let live = svc.mcf_snapshot.load();
        assert!((live.rate_for_flow("dA", "dB") - snap.rate_for_flow("dA", "dB")).abs() < 1e-9);
    }

    #[test]
    fn saturated_flow_is_pinned_to_zero() {
        let svc = make_service();
        // Mark the ENC side of dA→dB as Saturated. Weight collapses to 0.
        svc.priorities
            .set("dA", "dB", BufferRole::EncKeys, TrafficPriority::Saturated);
        let snap = svc.recompute_mcf();
        // The excluded flow appears in the snapshot at rate 0.
        assert!(snap.rates.contains_key("dA->dB"));
        assert_eq!(snap.rate_for_flow("dA", "dB"), 0.0);
        // The opposite direction still flows.
        assert!(snap.rate_for_flow("dB", "dA") > 0.0);
        // Buffer view also pinned.
        assert_eq!(
            snap.rates_by_dkms["dA"][&("dB".to_string(), BufferRole::EncKeys)],
            0.0,
        );
        assert_eq!(
            snap.rates_by_dkms["dB"][&("dA".to_string(), BufferRole::DecKeys)],
            0.0,
        );
    }

    #[test]
    fn dec_side_saturation_also_excludes_flow() {
        // The min(ENC, DEC) rule: saturating *either* endpoint should
        // exclude the flow, not just the ENC side.
        let svc = make_service();
        svc.priorities
            .set("dB", "dA", BufferRole::DecKeys, TrafficPriority::Saturated);
        let snap = svc.recompute_mcf();
        assert_eq!(snap.rate_for_flow("dA", "dB"), 0.0);
    }

    #[test]
    fn commodities_cache_reuses_until_version_bumps() {
        let svc = make_service();
        let topo = svc.topology.load();
        let v_before = topo.version;
        let c1 = svc.commodities_for(&topo);
        let c2 = svc.commodities_for(&topo);
        assert!(Arc::ptr_eq(&c1, &c2), "same Arc returned within version");

        // Mutate the topology — version bumps.
        svc.topology.mutate(|t| {
            t.upsert_qkc(Qkc {
                id: "99".into(),
                host: host(99),
                kme_host: None,
            });
            true
        });
        let topo2 = svc.topology.load();
        assert!(topo2.version > v_before);
        let c3 = svc.commodities_for(&topo2);
        assert!(!Arc::ptr_eq(&c1, &c3), "rebuilt after version bump");
    }

    #[test]
    fn set_priority_and_recompute_returns_fresh_snapshot() {
        let svc = make_service();
        let _ = svc.recompute_mcf();
        let before = svc.mcf_snapshot.load().rate_for_flow("dA", "dB");
        assert!(before > 0.0);
        let after_snap = svc.set_priority_and_recompute(
            "dA",
            "dB",
            BufferRole::EncKeys,
            TrafficPriority::Saturated,
        );
        assert_eq!(after_snap.rate_for_flow("dA", "dB"), 0.0);
    }

    #[tokio::test]
    async fn debouncer_attaches_and_coalesces_requests() {
        let svc = make_service();
        // Pre-warm so the initial snapshot exists, then attach a fast
        // debouncer for the test.
        svc.recompute_mcf();
        svc.attach_debouncer(
            Some(Duration::from_millis(40)),
            Some(Duration::from_secs(1)),
        );
        // Pollute the registry, then fire a burst of requests. The
        // debouncer should coalesce them into a single recompute.
        svc.priorities
            .set("dA", "dB", BufferRole::EncKeys, TrafficPriority::Saturated);
        for _ in 0..10 {
            svc.request_recompute();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Allow the window to elapse + a generous safety margin.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // The debounced fire is async (spawn_blocking) — give it room.
        let mut tries = 0;
        while svc.mcf_snapshot.load().rate_for_flow("dA", "dB") != 0.0 && tries < 50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tries += 1;
        }
        assert_eq!(svc.mcf_snapshot.load().rate_for_flow("dA", "dB"), 0.0);
        svc.shutdown_debouncer();
    }

    #[tokio::test]
    async fn saturated_signal_bypasses_debouncer() {
        let svc = make_service();
        svc.recompute_mcf();
        svc.attach_debouncer(
            Some(Duration::from_secs(60)), // huge window
            Some(Duration::from_secs(600)),
        );
        // Even with a 60s window, the saturated signal should fire
        // recompute synchronously and the snapshot should reflect the
        // change immediately.
        let snap = svc.set_priority_and_recompute(
            "dA",
            "dB",
            BufferRole::EncKeys,
            TrafficPriority::Saturated,
        );
        assert_eq!(snap.rate_for_flow("dA", "dB"), 0.0);
        assert_eq!(svc.mcf_snapshot.load().rate_for_flow("dA", "dB"), 0.0);
        svc.shutdown_debouncer();
    }

    #[tokio::test]
    async fn priority_change_coalesces_through_debouncer() {
        let svc = make_service();
        svc.recompute_mcf();
        svc.attach_debouncer(
            Some(Duration::from_millis(40)),
            Some(Duration::from_secs(1)),
        );
        // Setting Important is non-urgent → goes through the debouncer.
        // The snapshot returned by the call is the *currently* published
        // one (pre-change), since the recompute hasn't run yet.
        let before = svc.mcf_snapshot.load().rate_for_flow("dA", "dB");
        let returned = svc.set_priority_and_recompute(
            "dA",
            "dB",
            BufferRole::EncKeys,
            TrafficPriority::Important,
        );
        assert_eq!(returned.rate_for_flow("dA", "dB"), before);
        // After the window elapses, the new weights have been applied.
        // The fire is async (spawn_blocking) — give the snapshot room
        // to settle.
        let mut after = before;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            after = svc.mcf_snapshot.load().rate_for_flow("dA", "dB");
            if (after - before).abs() > f64::EPSILON {
                break;
            }
        }
        assert!(
            (after - before).abs() > f64::EPSILON,
            "rate should have changed after the debouncer fired (before={before}, after={after})",
        );
        svc.shutdown_debouncer();
    }

    #[tokio::test]
    async fn autotune_uses_dkms_count() {
        let svc = make_service();
        svc.attach_debouncer(None, None);
        // The test topology has 2 DKMSs → tiny estimate → window/max_wait
        // hit their floors.
        let d = svc.debouncer.read().clone().expect("debouncer attached");
        assert!(d.max_wait() >= Duration::from_secs(2));
        assert!(d.window() >= Duration::from_millis(500));
        svc.shutdown_debouncer();
    }
}
