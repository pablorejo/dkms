//! Cross-cutting service state shared by the gRPC, HTTP, and background
//! workers. The orchestration glue between [`TopologyStore`], the
//! [`DemandRegistry`], the [`crate::mcmcf::McmcfSolver`], and the
//! live [`McfSnapshot`].

use std::{sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use common::metrics::Metrics;
use parking_lot::RwLock;
use tracing::{info, warn};

use crate::{
    config::SdnConfig,
    debounce::{estimate_timing, Debouncer},
    demand::{DemandRegistry, SharedDemandRegistry},
    error::Result,
    mcf::{BufferRole, McfSnapshot},
    mcmcf::{wcmp_from_topology, McmcfInputs, McmcfSolver},
    metrics::SdnMetrics,
    presence::{Kind, Presence},
    push::Pushers,
    rates_num::{maxmin_allocate, Allocator, NumParams, RateAlloc},
    topology::TopologyStore,
};

#[derive(Clone)]
pub struct SdnService {
    pub cfg: Arc<SdnConfig>,
    pub topology: TopologyStore,
    pub mcf_snapshot: Arc<ArcSwap<McfSnapshot>>,
    pub pushers: Arc<Pushers>,
    pub metrics: Metrics,
    pub sdn_metrics: SdnMetrics,
    /// Per-commodity demand reports POSTed by each DKMS via
    /// `POST /demand`. Read by the MCMCF-λ solver on every recompute.
    pub demand_registry: SharedDemandRegistry,
    /// Coalesces bursts of `request_recompute()` calls into one MCF
    /// solve. Built lazily — only constructed when there's a tokio
    /// runtime available (see `attach_debouncer`).
    debouncer: Arc<RwLock<Option<Debouncer>>>,
    /// Last time each self-registered module announced itself. Drives the
    /// expiry sweep; see [`crate::presence`] for why it lives outside the
    /// topology snapshot.
    pub presence: Arc<Presence>,
    /// Qué asignador produce las rates y, para `num`, su estado de precios.
    pub alloc: RateAlloc,
}

impl SdnService {
    pub async fn new(cfg: SdnConfig, metrics: Metrics) -> Result<Self> {
        // La SDN arranca sin topología, siempre. La construye con lo que los
        // módulos le cuentan al registrarse (`POST /register/{qkc,orr,dkms}`).
        let sdn_metrics = SdnMetrics::register(&metrics);
        let allocator = Allocator::resolve(&cfg.rate_allocator);
        let alloc = RateAlloc::new(
            allocator,
            NumParams {
                alpha: cfg.num_alpha,
                gamma: cfg.num_gamma,
                fill_weight: cfg.num_fill_weight,
            },
        );
        info!(allocator = ?allocator, "asignador de rates seleccionado");
        let svc = Self {
            cfg: Arc::new(cfg),
            topology: TopologyStore::default(),
            mcf_snapshot: Arc::new(ArcSwap::from_pointee(McfSnapshot::default())),
            pushers: Arc::new(Pushers::new()),
            metrics,
            sdn_metrics,
            demand_registry: Arc::new(DemandRegistry::new()),
            debouncer: Arc::new(RwLock::new(None)),
            presence: Arc::new(Presence::new()),
            alloc,
        };
        // 2026-05-20: previously called svc.recompute_mcf() synchronously
        // here. With N=40 (~1560 commodities, ~110 edges) microlp takes
        // 10-60s to solve the first LP, which blocks the runtime startup
        // and the gRPC/HTTP servers never bind. The orchestator then
        // sees "Connection refused" on every SAE provisioning attempt
        // for as long as the LP runs. Spawn it on a background task so
        // the servers come up immediately and /sae endpoints respond
        // with a stale (default) McfSnapshot until the first solve
        // finishes.
        {
            let svc_clone = svc.clone();
            tokio::task::spawn_blocking(move || {
                svc_clone.recompute_mcf();
            });
        }
        Ok(svc)
    }

    /// Convenience helper for the HTTP `/rate` endpoints: look up the
    /// keys-per-second rate the MCF assigned to a single buffer.
    pub fn rate_for_buffer(&self, dkms_id: &str, peer_dkms_id: &str, role: BufferRole) -> f64 {
        self.mcf_snapshot
            .load()
            .rate_for_buffer(dkms_id, peer_dkms_id, role)
    }

    /// Re-run the MCMCF-λ solve end-to-end and publish the resulting
    /// snapshot. Inputs are the topology snapshot + the
    /// [`Self::demand_registry`] reported by DKMSs.
    pub fn recompute_mcf(&self) -> Arc<McfSnapshot> {
        recompute_mcf_inner(
            &self.topology,
            &self.demand_registry,
            &self.mcf_snapshot,
            Some(&self.sdn_metrics.lp_solve_duration_seconds),
            &self.alloc,
        )
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
        let demand = self.demand_registry.clone();
        let snap = self.mcf_snapshot.clone();
        let lp_hist = self.sdn_metrics.lp_solve_duration_seconds.clone();
        let alloc = self.alloc.clone();
        let new_deb = Debouncer::new(window, max_wait, move || {
            recompute_mcf_inner(&topo, &demand, &snap, Some(&lp_hist), &alloc);
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
        // num/maxmin son aritmética de microsegundos: el debouncer existe
        // porque el LP es caro, y aquí solo retrasaría la convergencia de
        // los precios (que quieren cadencia constante, no coalescencia).
        if self.alloc.allocator != Allocator::Lp {
            self.recompute_mcf();
            return;
        }
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

    /// Background loop: attaches the debouncer on the first tick and
    /// then ticks every `mcf_period_ms` as a heartbeat, asking for a
    /// recompute (which the debouncer will coalesce with whatever
    /// just came in over HTTP).
    ///
    /// También spawnea un *version watcher* (200 ms tick) que detecta
    /// cambios en `topology.version` y emite un `TopologyEvent` por el
    /// `Pushers` para invalidar cachés en clientes (ORR, DKMS).
    /// Saca de la topología a los módulos auto-registrados que dejaron de
    /// anunciarse. Sin esto un nodo apagado se queda para siempre y el LP le
    /// sigue asignando caudal que nadie consume.
    ///
    /// Cada entidad caduca por su cuenta, sin cascada: si se cae un QKC pero su
    /// ORR y su DKMS siguen vivos, estos se quedan (el grafo ya tolera ORR/DKMS
    /// colgando de un ancla ausente, y `qkc_of_dkms` devolverá `None`, así que
    /// no se les asigna ruta). Normalmente caen los tres juntos y expiran los
    /// tres.
    fn spawn_presence_sweeper(&self) {
        let ttl_secs = self.cfg.presence_ttl_secs;
        if ttl_secs == 0 {
            info!("presence sweeper disabled (presence_ttl_secs = 0)");
            return;
        }
        let ttl = Duration::from_secs(ttl_secs);
        // Barrer varias veces por TTL para que el retardo de detección sea una
        // fracción de él y no un TTL entero.
        let period = Duration::from_secs((ttl_secs / 3).max(1));
        let presence = self.presence.clone();
        let topology = self.topology.clone();
        // El registro de demanda caduca en el mismo barrido: una entrada que
        // nadie renueva es una comodity fantasma para el solver.
        let demand = self.demand_registry.clone();
        // `Some(0)` = demanda sin expiración (espejo de `presence_ttl_secs`,
        // donde 0 apaga el sweeper): sin este mapeo, 0 evictaba TODOS los
        // informes en cada barrido.
        let demand_ttl_ms = demand_ttl_ms(self.cfg.demand_ttl_secs, ttl_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.tick().await; // el primero es inmediato
            loop {
                tick.tick().await;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let evicted = demand_ttl_ms
                    .map(|ttl| demand.evict_older_than(now_ms, ttl))
                    .unwrap_or(0);
                if evicted > 0 {
                    info!(
                        evicted,
                        ttl_ms = demand_ttl_ms,
                        "demand: informes sin renovar retirados del registro"
                    );
                }
                for (kind, id) in presence.take_expired(ttl) {
                    let res = match kind {
                        // delete_qkc ya hace la cascada del grafo: quita el
                        // nodo y toda arista que lo tocara.
                        Kind::Qkc => topology.delete_qkc(&id),
                        Kind::Orr => topology.delete_orr(&id),
                        Kind::Dkms => topology.delete_dkms(&id),
                    };
                    match res {
                        Ok(()) => warn!(
                            kind = kind.as_str(), id = %id, ttl_secs,
                            "módulo caducado: lleva más de un TTL sin anunciarse, lo saco de la \
                             topología",
                        ),
                        // Ya no estaba (borrado a mano, por ejemplo). No es un
                        // problema: el objetivo era que desapareciera.
                        Err(e) => info!(
                            kind = kind.as_str(), id = %id, error = %e,
                            "módulo caducado que ya no estaba en la topología",
                        ),
                    }
                }
            }
        });
    }

    /// Línea `topology.state` periódica.
    ///
    /// La topología sólo se podía mirar sondeando `GET /topology`, así que un
    /// despliegue que no converge no deja rastro en los logs: cuando alguien
    /// va a mirar por qué un DKMS no recibe claves, lo primero que necesita
    /// saber es si la SDN llegó a ver los módulos, y eso ya no está.
    ///
    /// `declared` es el número de QKC que declaran algún enlace, y va al lado
    /// de `edges` a propósito: la arista existe si la declara **alguno** de
    /// sus dos extremos, así que ver `declared > 0` con `edges = 0` señala
    /// directamente al caso "los vecinos declarados todavía no han
    /// registrado" — el `pending` de siempre — sin tener que ir al `qkc
    /// registered` de cada uno.
    fn spawn_topology_state_logger(&self, every: Duration) {
        let topology = self.topology.clone();
        let presence = self.presence.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let t = topology.load();
                let declared = t.declared.values().filter(|l| !l.is_empty()).count();
                info!(
                    version = t.version,
                    qkcs = t.qkcs.len(),
                    orrs = t.orrs.len(),
                    dkms = t.dkms.len(),
                    saes = t.saes.len(),
                    edges = t.edges.len(),
                    declared,
                    // Módulos con anuncio vivo. La presencia sólo se toca
                    // cuando el anuncio se ACEPTA (http_api.rs), así que un
                    // módulo rechazado o a la espera de su ancla no cuenta;
                    // menos que la suma de los de arriba significa entidades
                    // dadas de alta a mano, que no caducan nunca.
                    announced = presence.len(),
                    "topology.state",
                );
            }
        });
    }

    pub async fn run_background_tasks(self) -> Result<()> {
        info!("sdn: background tasks started");
        // Attach the debouncer now, inside the tokio runtime. Honour
        // any explicit overrides set on the config; otherwise the
        // attach_debouncer call below auto-tunes from the DKMS count.
        let window_override = (self.cfg.push_debounce_ms != 0)
            .then(|| Duration::from_millis(self.cfg.push_debounce_ms));
        self.attach_debouncer(window_override, None);

        self.spawn_presence_sweeper();
        self.spawn_topology_state_logger(Duration::from_secs(5));

        // Version watcher → broadcast TopologyEvent + push de forwarding
        // tables a los QKCs. Dispara en dos eventos:
        //   - cambio en `topology.version` (alta/baja de nodos, enlaces, ...)
        //   - cambio en el `mcf_snapshot` publicado (LP recomputed con
        //     nueva WCMP de la fase 4).
        //
        // Para cada QKC:
        //   1. Si el `McfSnapshot` actual tiene una entrada WCMP en
        //      `wcmp[qkc][dst]`, se POSTea verbatim como
        //      `{dst: [{qkc_id, weight}, ...]}`.
        //   2. Si no, se cae a `topology.next_hop_qkc(qkc, dst)`
        //      (shortest-path) y se POSTea como single-hop
        //      `[{qkc_id: nh, weight: 1}]`.
        //
        // El push inicial se dispara forzando un primer ciclo:
        // arrancamos `last_topo = -1`.
        {
            let topology = self.topology.clone();
            let mcf_snap = self.mcf_snapshot.clone();
            let pushers = self.pushers.clone();
            // El cliente HTTP con el que se empujan las tablas se construye
            // aquí, con la causa a la vista, y no dentro de la task: con
            // `panic = "abort"` un `expect` ahí abortaría el SDN entero sin
            // decir por qué.
            //
            // Con [tls] el push va por https con el cert del SDN como cliente
            // (mismo net-ca que firma el server del QKC) — el esquema es de
            // despliegue, como grpc_tls: el QKC sirve mTLS sii tiene [tls], y
            // un solo lado configurado falla ruidoso en ambos. En claro queda
            // el histórico. `push_scheme` decide; el cliente mTLS no degrada
            // (https_only).
            let http = match &self.cfg.tls {
                Some(t) => common::http::mtls_client(
                    common::http::ClientTls {
                        ca_path: &t.client_ca,
                        cert_path: &t.cert_path,
                        key_path: &t.key_path,
                    },
                    Duration::from_millis(1500),
                )
                .map_err(|e| anyhow::anyhow!("mtls client for forwarding push: {e:#}"))?,
                None => reqwest::Client::builder()
                    .timeout(Duration::from_millis(1500))
                    .build()
                    .map_err(|e| anyhow::anyhow!("reqwest client for forwarding push: {e:#}"))?,
            };
            let push_scheme = push_scheme(self.cfg.tls.as_ref());
            tokio::spawn(async move {
                use crate::mcf::WcmpNextHop;
                use common::proto::sdn::v1::{
                    topology_event, Topology as ProtoTopology, TopologyEvent,
                };
                let mut last_topo: i64 = -1; // fuerza primer push al arrancar
                let mut last_snap_ptr: usize = 0;
                let mut tick = tokio::time::interval(Duration::from_millis(200));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    let snap = topology.load();
                    let mcf = mcf_snap.load();
                    let topo_v = snap.version;
                    let snap_ptr = Arc::as_ptr(&mcf) as usize;
                    if topo_v == last_topo && snap_ptr == last_snap_ptr {
                        continue;
                    }
                    // 1) Forwarding push: una POST por QKC. Concurrent
                    //    via join_all para no serializar 9 QKCs.
                    //
                    // The QKD-only table (`replace_qkd`) is OPT-IN via
                    // `SDN_DUAL_GRADE_TABLES`: pushing a non-empty QKD table
                    // engages the QKC's grade routing, which only behaves
                    // correctly once the DKMS tags pumped frames per grade
                    // (Opt-D). Until then the default (flag off) sends only the
                    // full table, keeping the QKC on its legacy single-table
                    // path. See [[project-qkc-security-levels-design]].
                    let dual_grade = std::env::var("SDN_DUAL_GRADE_TABLES")
                        .map(|v| v != "0" && !v.is_empty())
                        .unwrap_or(false);
                    let mut futures = Vec::new();
                    for (qkc_id, qkc) in &snap.qkcs {
                        let mut table: std::collections::HashMap<String, Vec<WcmpNextHop>> =
                            std::collections::HashMap::new();
                        let from_lp = mcf.wcmp.get(qkc_id);
                        for other in snap.qkcs.keys() {
                            if other == qkc_id {
                                continue;
                            }
                            // Prefer LP-derived WCMP; fall back to
                            // topology shortest-path. The QKC accepts
                            // both wire shapes — single-element vec
                            // with weight=1 is just a degenerate WCMP.
                            if let Some(hops) = from_lp.and_then(|t| t.get(other)) {
                                table.insert(other.clone(), hops.clone());
                            } else if let Some(nh) = snap.next_hop_qkc(qkc_id, other) {
                                if let Ok(qkc_id_num) = nh.parse::<u32>() {
                                    table.insert(
                                        other.clone(),
                                        vec![WcmpNextHop {
                                            qkc_id: qkc_id_num,
                                            weight: 1,
                                        }],
                                    );
                                }
                            }
                        }
                        // QKD-only table (for QKD-grade frames). LP-derived
                        // ONLY — no shortest-path fallback, which could route
                        // over a PQC hop and break the QKD-grade invariant.
                        let qkd_table: std::collections::HashMap<String, Vec<WcmpNextHop>> =
                            mcf.wcmp_qkd.get(qkc_id).cloned().unwrap_or_default();
                        let url = format!(
                            "{push_scheme}://{}:{}/forwarding-table",
                            qkc.host.ip, qkc.host.port
                        );
                        let body = if dual_grade {
                            serde_json::json!({"replace": table, "replace_qkd": qkd_table})
                        } else {
                            serde_json::json!({"replace": table})
                        };
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
                    let (ok, err): (Vec<_>, Vec<_>) = results.into_iter().partition(|r| r.is_ok());
                    info!(
                        topo_from = last_topo,
                        topo_to = topo_v,
                        qkcs_ok = ok.len(),
                        qkcs_err = err.len(),
                        snap_changed = (snap_ptr != last_snap_ptr),
                        "forwarding push done"
                    );
                    for e in err.iter().take(3) {
                        if let Err(s) = e {
                            warn!(error = %s, "forwarding push failed");
                        }
                    }

                    // 2) Broadcast del evento de topology (sólo en
                    //    bumps reales del grafo — el watcher de
                    //    snapshot es interno de la fase 4).
                    if topo_v != last_topo {
                        let ev = TopologyEvent {
                            event: Some(topology_event::Event::Snapshot(ProtoTopology {
                                nodes: Vec::new(),
                                links: Vec::new(),
                                version: topo_v,
                            })),
                            version: topo_v,
                        };
                        pushers.broadcast(ev).await;
                        info!(
                            from = last_topo,
                            to = topo_v,
                            "topology version changed; broadcast"
                        );
                    }

                    // SOLO marcamos como "ya empujado" si TODOS los
                    // QKCs aceptaron. Si alguno falló (typically
                    // connection refused durante el race de arranque)
                    // no avanzamos los `last_*` y reintentamos en el
                    // siguiente tick.
                    if err.is_empty() {
                        last_topo = topo_v;
                        last_snap_ptr = snap_ptr;
                    } else {
                        warn!(
                            qkcs_err = err.len(),
                            "forwarding push partial; will retry next tick"
                        );
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
}

// ---------------- free-function helpers shared with the debouncer closure ----

/// TTL de los informes de demanda en ms: el configurado, o el TTL de
/// presencia si no hay; `Some(0)` configurado = `None` = sin expiración.
fn demand_ttl_ms(cfg: Option<u64>, presence_ttl_secs: u64) -> Option<i64> {
    match cfg.unwrap_or(presence_ttl_secs) {
        0 => None,
        secs => Some(i64::try_from(secs * 1000).unwrap_or(i64::MAX)),
    }
}

/// Esquema del push de forwarding-tables: `https` sii el SDN tiene `[tls]`.
/// Es una decisión de despliegue (como `grpc_tls`): el QKC sirve su admin con
/// mTLS exactamente bajo la misma condición, así que o casan o fallan alto.
fn push_scheme(tls: Option<&crate::config::SdnTlsCfg>) -> &'static str {
    if tls.is_some() {
        "https"
    } else {
        "http"
    }
}

/// Full recompute pipeline — MCMCF-λ LP (phase 3).
///
/// Free function because the debouncer closure captures these
/// references directly — putting the body on `SdnService` would
/// force the closure to capture `self`, creating a reference cycle
/// through `SdnService::debouncer`.
///
/// Steps:
///   1. Snapshot the topology and the demand registry.
///   2. Build [`McmcfInputs`] — every ordered DKMS pair with a valid
///      QKC anchoring becomes a commodity. Unreported commodities
///      get a synthetic `(L=0, B=DEFAULT, δ=0)` so the LP has work
///      at SDN boot.
///   3. Run the LP via [`McmcfSolver`].
///   4. Adapt the solution to the legacy [`McfSnapshot`] shape so
///      the `/rate` endpoint and the forwarding push loop stay
///      unchanged.
///   5. Atomically publish via [`ArcSwap`].
fn recompute_mcf_inner(
    topo_store: &TopologyStore,
    demand_registry: &Arc<DemandRegistry>,
    snap_cell: &Arc<ArcSwap<McfSnapshot>>,
    lp_solve_histogram: Option<&prometheus::Histogram>,
    alloc: &RateAlloc,
) -> Arc<McfSnapshot> {
    let t_start = std::time::Instant::now();
    let topo = topo_store.load();
    let inputs = McmcfInputs::build(&topo, demand_registry);
    let n_commodities = inputs.commodities.len();
    let n_edges = inputs.edge_capacity.len();
    let solution = match alloc.allocator {
        Allocator::Lp => McmcfSolver::new().solve(&inputs),
        // Los dos asignadores nuevos deciden CUÁNTO sobre el mismo routing
        // que los QKCs ejecutan: las tablas WCMP de topología+capacidad.
        // `into_mcf_snapshot` las reconstruye igual para publicarlas — el
        // coste doble es microsegundos y mantiene un único camino de
        // publicación.
        Allocator::Num => {
            let wf = wcmp_from_topology(&topo, None);
            let wq = wcmp_from_topology(&topo, Some(common::security::KeyGrade::Qkd));
            alloc.state.lock().step(&inputs, &wf, &wq, &alloc.params)
        }
        Allocator::Maxmin => {
            let wf = wcmp_from_topology(&topo, None);
            let wq = wcmp_from_topology(&topo, Some(common::security::KeyGrade::Qkd));
            maxmin_allocate(&inputs, &wf, &wq)
        }
    };
    let lambda = solution.lambda;
    let n_flows_positive = solution.rates.values().filter(|r| **r > 0.0).count();
    let snap = solution.into_mcf_snapshot(&topo);

    let arc = Arc::new(snap);
    snap_cell.store(arc.clone());

    let elapsed = t_start.elapsed();
    if let Some(h) = lp_solve_histogram {
        h.observe(elapsed.as_secs_f64());
    }
    info!(
        allocator = ?alloc.allocator,
        n_commodities,
        n_edges,
        lambda,
        flows_with_rate = n_flows_positive,
        registry_len = demand_registry.len(),
        elapsed_ms = elapsed.as_millis() as u64,
        "MCMCF-λ recomputed"
    );

    arc
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn demand_ttl_zero_means_no_expiry() {
        assert_eq!(super::demand_ttl_ms(Some(0), 90), None);
        assert_eq!(super::demand_ttl_ms(None, 90), Some(90_000));
        assert_eq!(super::demand_ttl_ms(Some(30), 90), Some(30_000));
        assert_eq!(super::demand_ttl_ms(None, 0), None);
    }

    #[test]
    fn push_scheme_follows_the_tls_block() {
        assert_eq!(super::push_scheme(None), "http");
        let t = crate::config::SdnTlsCfg {
            cert_path: "/x/sdn.crt".into(),
            key_path: "/x/sdn.key".into(),
            client_ca: "/x/net-ca.crt".into(),
        };
        assert_eq!(super::push_scheme(Some(&t)), "https");
    }

    use super::*;
    use crate::topology::{Dkms, EdgeMeta, HostEndpoint, Orr, Qkc, Topology};

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
                peer_addr: None,
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
                ..Default::default()
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
                ..Default::default()
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
            peer_addr: None,
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(23),
            peer_addr: None,
            tls_id: None,
            orr_id: "o3".into(),
        });
        t.version = 1;
        t
    }

    pub(crate) fn make_service() -> SdnService {
        SdnService {
            cfg: Arc::new(SdnConfig {
                node_id: "test".into(),
                grpc_addr: "0.0.0.0:0".into(),
                http_addr: "0.0.0.0:0".into(),
                metrics_addr: "0.0.0.0:0".into(),
                mcf_period_ms: 60_000,
                push_debounce_ms: 100,
                presence_ttl_secs: 90,
                demand_ttl_secs: None,
                rate_allocator: "lp".into(),
                num_alpha: 1.0,
                num_gamma: 0.2,
                num_fill_weight: 0.1,
                tls: None,
                http_ro_addr: None,
            }),
            topology: TopologyStore::new(small_topo()),
            mcf_snapshot: Arc::new(ArcSwap::from_pointee(McfSnapshot::default())),
            pushers: Arc::new(Pushers::new()),
            metrics: Metrics::new("sdn-test"),
            sdn_metrics: SdnMetrics::register(&Metrics::new("sdn-test-metrics")),
            demand_registry: Arc::new(DemandRegistry::new()),
            debouncer: Arc::new(RwLock::new(None)),
            presence: Arc::new(Presence::new()),
            // Los tests históricos de este módulo ejercitan el pipeline del
            // LP; el asignador de producción tiene los suyos en `rates_num`
            // y un smoke propio más abajo.
            alloc: RateAlloc::new(Allocator::Lp, NumParams::default()),
        }
    }

    #[test]
    fn recompute_with_num_allocator_populates_snapshot() {
        let mut svc = make_service();
        svc.alloc = RateAlloc::new(Allocator::Num, NumParams::default());
        let snap = svc.recompute_mcf();
        // Con precios a cero el primer tick ya publica rates factibles y
        // positivas para el par alcanzable en ambos sentidos.
        assert!(snap.rate_for_flow("dA", "dB") > 0.0);
        assert!(snap.rate_for_flow("dB", "dA") > 0.0);
    }

    #[test]
    fn recompute_with_maxmin_allocator_populates_snapshot() {
        let mut svc = make_service();
        svc.alloc = RateAlloc::new(Allocator::Maxmin, NumParams::default());
        let snap = svc.recompute_mcf();
        assert!(snap.rate_for_flow("dA", "dB") > 0.0);
        assert!(snap.rate_for_flow("dB", "dA") > 0.0);
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

    /// Helper: ingest a single-entry demand report into the service's
    /// registry. Used by the post-MCMCF-λ tests below.
    fn ingest_demand(svc: &SdnService, src: &str, dst: &str, level: f64, cap: f64, drain: f64) {
        use crate::demand::{CommodityDemand, DemandReport};
        svc.demand_registry.ingest(DemandReport {
            dkms_id: src.into(),
            entries: vec![CommodityDemand {
                src_dkms: src.into(),
                dst_dkms: dst.into(),
                level,
                capacity: cap,
                drain_rate: drain,
                timestamp_ms: 1,
                grade: Default::default(),
            }],
        });
    }

    /// A buffer reported as full with zero drain produces r_k = 0
    /// (the LP has nothing to do for that commodity). The opposite
    /// direction still flows.
    #[test]
    fn full_buffer_zero_drain_yields_zero_rate() {
        let svc = make_service();
        // dA→dB: full + no drain (R_k = 0, δ_k = 0).
        // dB→dA: empty buffer (the default synthetic).
        ingest_demand(&svc, "dA", "dB", 4096.0, 4096.0, 0.0);
        let snap = svc.recompute_mcf();
        assert!(snap.rates.contains_key("dA->dB"));
        assert_eq!(snap.rate_for_flow("dA", "dB"), 0.0);
        assert!(snap.rate_for_flow("dB", "dA") > 0.0);
        assert_eq!(
            snap.rates_by_dkms["dA"][&("dB".to_string(), BufferRole::EncKeys)],
            0.0,
        );
        assert_eq!(
            snap.rates_by_dkms["dB"][&("dA".to_string(), BufferRole::DecKeys)],
            0.0,
        );
    }

    /// Full buffer + positive drain → r_k = δ_k (drain compensation
    /// only, no extra fill). Verifies the LP gives "just enough" to
    /// keep level constant.
    #[test]
    fn full_buffer_with_drain_returns_drain_rate() {
        let svc = make_service();
        // dA→dB: full + 25 kps drain. dB→dA: empty + 0 drain.
        ingest_demand(&svc, "dA", "dB", 4096.0, 4096.0, 25.0);
        let snap = svc.recompute_mcf();
        let r_ab = snap.rate_for_flow("dA", "dB");
        assert!(
            (r_ab - 25.0).abs() < 0.5,
            "r_ab should ≈ drain (25), got {r_ab}"
        );
    }

    /// Ingest a new demand report → recompute → snapshot reflects
    /// the new commodity state. Verifies the demand registry ↔ LP
    /// integration end-to-end.
    #[test]
    fn demand_update_then_recompute_changes_rate() {
        let svc = make_service();
        let before = svc.recompute_mcf().rate_for_flow("dA", "dB");
        assert!(before > 0.0);
        // Report the dA→dB buffer as full with no drain. The new
        // recompute must drop its rate to zero.
        ingest_demand(&svc, "dA", "dB", 4096.0, 4096.0, 0.0);
        let after = svc.recompute_mcf();
        assert_eq!(after.rate_for_flow("dA", "dB"), 0.0);
        assert!(after.rate_for_flow("dB", "dA") > 0.0);
    }

    /// The debouncer coalesces a burst of `request_recompute` calls
    /// into a single fire. Verifies via observable side-effect: an
    /// in-flight demand registry change must be picked up by exactly
    /// one (eventual) recompute, not 10 of them.
    #[tokio::test]
    async fn debouncer_coalesces_request_recomputes() {
        let svc = make_service();
        svc.recompute_mcf();
        svc.attach_debouncer(
            Some(Duration::from_millis(40)),
            Some(Duration::from_secs(1)),
        );
        // Mutate the demand registry, then fire a burst of requests.
        ingest_demand(&svc, "dA", "dB", 4096.0, 4096.0, 0.0);
        for _ in 0..10 {
            svc.request_recompute();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Window elapses → exactly one recompute fires, picks up the
        // new demand entry, snapshot reflects r_dA→dB = 0.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut tries = 0;
        while svc.mcf_snapshot.load().rate_for_flow("dA", "dB") != 0.0 && tries < 50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tries += 1;
        }
        assert_eq!(svc.mcf_snapshot.load().rate_for_flow("dA", "dB"), 0.0);
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
