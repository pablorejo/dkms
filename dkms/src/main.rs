//! Binario DKMS.
//!
//! Cuatro tareas concurrentes:
//!
//! * Plano norte (ETSI 014, SAE-facing, mTLS).
//! * Plano este/oeste (ETSI 020, DKMS↔DKMS, mTLS).
//! * Plano de gestión gRPC (`DkmsControl`).
//! * Endpoint Prometheus (`/metrics`).
//! * Tareas de fondo (sweeper de pending, refill de buffers — TODO QKC).

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;
use tracing::{info, warn};

use common::{logging, metrics::Metrics};
use dkms::{
    config::DkmsConfig,
    control::{
        ack_pending::AckPendingStore, ack_socket, AckClient, BatchedAckClient, Generator,
        SaeBufferBuckets,
    },
    etsi_http, grpc_server,
    peer_client::PeerHttpClient,
    sae_binding::{SaeBindingCache, SaeResolver, SdnSaeResolver, StaticSaeResolver},
    service::DkmsService,
    southbound::{OrrClient, QkcClient, SdnClient, SdnHttpClient},
    state::{BufferPool, PendingStore},
    token_bucket::SaeBuckets,
};

#[derive(Parser, Debug)]
#[command(name = "dkms", version, about = "DKMS (ETSI 014/020 key delivery)")]
struct Cli {
    /// Directorio de configuración (sobrescribe `CONFIG_DIR`).
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

/// El cliente SDN gRPC apunta a un `http://host:port_grpc`. El endpoint
/// HTTP-admin del SDN está en otro puerto (50055 en la demo-star).
/// Como `SouthboundCfg` solo tiene el gRPC, derivamos a partir de él
/// asumiendo la convención `puerto_grpc + 2 = puerto_http`. Si la demo
/// usa otros valores, conviene leerlo de config; por simplicidad asumimos
/// este offset igual al de la demo-star.
fn derive_sdn_http_url(grpc_url: &str) -> String {
    // Quitar prefijo http(s)://
    let trimmed = grpc_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (host, port_str) = trimmed.rsplit_once(':').unwrap_or((trimmed, "50053"));
    let port: u16 = port_str
        .split('/')
        .next()
        .unwrap_or("50053")
        .parse()
        .unwrap_or(50053);
    let http_port = if port == 50053 { 50055 } else { port + 2 };
    format!("http://{host}:{http_port}")
}

/// `ClientTlsConfig` para el gRPC dkms→SDN si `sdn_endpoint` es https:
/// presenta el cert de este DKMS y verifica al SDN con `control_plane_ca`
/// (fallback a `peer_dkms_ca`, la misma CA de red). `None` si es http (claro).
fn build_control_plane_tls(cfg: &DkmsConfig) -> Result<Option<tonic::transport::ClientTlsConfig>> {
    client_tls_for(cfg, &cfg.southbound.sdn_endpoint)
}

/// `ClientTlsConfig` para el gRPC dkms→ORR. Va con mTLS por defecto
/// (`southbound.orr_tls`, que sube el esquema a `https://` al cargar la
/// config): el DKMS presenta su certificado de nodo y verifica el del ORR con
/// `control_plane_ca`. Sólo con `orr_tls = false` y `http://` va en claro.
fn build_orr_tls(cfg: &DkmsConfig) -> Result<Option<tonic::transport::ClientTlsConfig>> {
    match cfg.southbound.orr_endpoint.as_deref() {
        Some(ep) => client_tls_for(cfg, ep),
        None => Ok(None),
    }
}

fn client_tls_for(
    cfg: &DkmsConfig,
    url: &str,
) -> Result<Option<tonic::transport::ClientTlsConfig>> {
    if !url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        return Ok(None);
    }
    use tonic::transport::{Certificate, ClientTlsConfig, Identity};
    let ca_path = cfg
        .tls
        .control_plane_ca
        .as_deref()
        .unwrap_or(&cfg.tls.peer_dkms_ca);
    let ca =
        std::fs::read(ca_path).with_context(|| format!("read control_plane_ca {ca_path:?}"))?;
    let cert = std::fs::read(&cfg.tls.cert_path).context("read tls.cert_path")?;
    let key = std::fs::read(&cfg.tls.key_path).context("read tls.key_path")?;
    Ok(Some(
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca))
            .identity(Identity::from_pem(cert, key)),
    ))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("dkms");
    let cli = Cli::parse();
    if let Some(d) = cli.config_dir.as_deref() {
        std::env::set_var("CONFIG_DIR", d);
    }

    // Provider PQC (ML-DSA + clásicos) como default del proceso: así reqwest
    // (peer_client, announcers) y tonic pueden cargar/verificar certs ML-DSA,
    // no solo `common::tls`. Idempotente. Ver common::tls_pqc.
    let _ = common::tls_pqc::install_process_default();

    let mut cfg: DkmsConfig = common::config::load_config("dkms")?;
    // El gRPC hacia el ORR va con mTLS por defecto (`southbound.orr_tls`):
    // el esquema del endpoint se sube a https aquí, una vez, y todo lo que
    // venga detrás (cliente, TLS de cliente) se guía por él.
    if cfg.southbound.orr_tls {
        if let Some(ep) = cfg.southbound.orr_endpoint.as_mut() {
            let t = ep.trim();
            if t.len() >= 7 && t[..7].eq_ignore_ascii_case("http://") {
                *ep = format!("https://{}", &t[7..]);
            }
        }
    }
    info!(node_id = %cfg.node_id, "dkms starting");

    // Los peers del node.yml son la semilla; a partir de aquí manda la SDN
    // (ver `dkms::peers`). Se crea aquí porque lo leen el anunciador, el
    // servicio y el generador.
    let peers = Arc::new(dkms::peers::PeerRegistry::from_config(cfg.peers.clone()));

    // ─── Métricas ─────────────────────────────────────────────────────
    let metrics = Metrics::new("dkms");
    metrics
        .serve(cfg.listen.metrics_addr.to_string())
        .await
        .context("metrics listener")?;

    // ─── Anuncio a la SDN ─────────────────────────────────────────────
    // En su propia task: si la SDN no está, o falta `sdn_http_url`, el DKMS
    // sirve claves igual. Solo deja de aparecer en la topología.
    if let Some(announcer) =
        dkms::southbound::sdn_announce::SdnAnnouncer::from_config(&cfg, peers.clone())
    {
        tokio::spawn(announcer.run());
    }

    // ─── Estado ───────────────────────────────────────────────────────
    let cfg = Arc::new(cfg);
    let pool = Arc::new(BufferPool::new(cfg.buffer.capacity_per_peer));
    let pending = Arc::new(PendingStore::new(cfg.pending.default_ttl_secs));
    let buckets = Arc::new(SaeBuckets::new(
        cfg.sae.default_rate_keys_per_sec,
        cfg.sae.default_burst_keys,
    ));

    // ─── Cliente DKMS↔DKMS (HTTP/2 ETSI 020) ──────────────────────────
    // Construido sólo si algún peer usa `transport = "http"`. En modo
    // ORR-only el PeerHttpClient no se invoca, así que evitamos pagar
    // el coste (y los problemas de TLS backend) cuando nadie lo necesita.
    // SAE-key delivery DKMS↔DKMS siempre va por HTTP/2 ETSI 020 con
    // session_key cifrada OTP usando `buffer_enc[peer]`. El ORR queda
    // dedicado al llenado de los buffers (generator), no al servicio
    // de SAEs. Por eso el peer_client SIEMPRE se construye —
    // independientemente de `cfg.peers[*].transport`.
    let peer_client = Some(Arc::new(
        PeerHttpClient::build(
            &cfg.tls.cert_path,
            &cfg.tls.key_path,
            &cfg.tls.peer_dkms_ca,
            cfg.request.clone(),
        )
        .context("peer http client")?,
    ));

    // ─── Clientes sur (SDN / QKC / ORR) ──────────────────────────────
    // Cada uno intenta conectar; si falla, se loguea y se sigue con
    // `None`. El DKMS arranca aunque sus vecinos no estén listos —
    // útil en bring-up donde los pods se inician en cualquier orden.
    //
    // Para SDN aplicamos el mismo patrón de "retry con backoff" que
    // QKC/ORR (ver abajo). En despliegues K8s donde DKMS y SDN arrancan
    // en paralelo, un fallo transitorio en el primer intento hace que
    // `sdn = None` permanentemente: ni `SdnSaeResolver` ni el bucle de
    // `stream_topology` se cablean, y el DKMS responde 404 para todo
    // `enc_keys` con peer SAE remoto hasta que lo reinicies.
    // mTLS del plano de control gRPC si `sdn_endpoint` es https: presentamos
    // el cert de este DKMS y verificamos al SDN con control_plane_ca (fallback
    // a peer_dkms_ca, la misma CA de red). docs/SECURITY.md §Fase 3.
    let sdn_grpc_tls = build_control_plane_tls(&cfg)?;
    let sdn = {
        let mut last_err: Option<String> = None;
        let mut connected: Option<std::sync::Arc<SdnClient>> = None;
        for attempt in 0..30 {
            match SdnClient::connect(&cfg.southbound, sdn_grpc_tls.clone()).await {
                Ok(c) => {
                    info!(
                        endpoint = %cfg.southbound.sdn_endpoint,
                        attempt = attempt + 1,
                        "sdn client connected"
                    );
                    connected = Some(std::sync::Arc::new(c));
                    break;
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
        }
        if connected.is_none() {
            warn!(
                error = %last_err.unwrap_or_else(|| "unknown".into()),
                endpoint = %cfg.southbound.sdn_endpoint,
                "sdn unreachable after 30 retries; continuing without it (404s expected on remote SAE lookups)"
            );
        }
        connected
    };

    // ─── SAE binding resolver ─────────────────────────────────────────
    // Si la SDN está disponible, usamos un `SdnSaeResolver` que consulta
    // `GetSaeBinding` por gRPC. La `SaeBindingCache` envuelve cualquier
    // resolver y cachea con TTL — "primera vez SDN, siguientes en
    // memoria" sale gratis. El mapa estático `[sae_bindings]` del config
    // sólo se usa cuando la SDN no está cableada (local-dev/CI).
    let resolver: Arc<dyn SaeResolver> = if let Some(sdn_client) = &sdn {
        info!("using SdnSaeResolver (SDN-backed)");
        Arc::new(SdnSaeResolver::new(sdn_client.clone()))
    } else {
        let static_resolver = Arc::new(StaticSaeResolver::new());
        for (sae, node) in &cfg.sae_bindings {
            static_resolver.insert(
                common::ids::SaeId::new(sae.clone()),
                common::ids::NodeId::new(node.clone()),
            );
        }
        if !cfg.sae_bindings.is_empty() {
            info!(
                n = cfg.sae_bindings.len(),
                "loaded static sae bindings from config (SDN unreachable)"
            );
        }
        static_resolver
    };
    let sae_binding = Arc::new(SaeBindingCache::new(
        resolver,
        cfg.sae_binding.ttl_secs,
        cfg.sae_binding.max_entries,
    ));

    // Suscripción a `StreamTopology`: cuando la SDN avisa de un
    // cambio, invalidamos la cache de SAE bindings. La siguiente
    // request volverá a preguntar a la SDN y la cache se rellenará.
    // Reconnect con backoff exponencial si el stream cae.
    if let Some(sdn_client) = sdn.clone() {
        let cache = sae_binding.clone();
        tokio::spawn(async move {
            let mut backoff_ms: u64 = 250;
            loop {
                match sdn_client.stream_topology().await {
                    Ok(mut stream) => {
                        info!("dkms.topology_subscriber connected");
                        backoff_ms = 250;
                        while let Some(item) = stream.message().await.transpose() {
                            match item {
                                Ok(ev) => {
                                    cache.invalidate_all().await;
                                    info!(
                                        version = ev.version,
                                        "dkms.sae_binding_cache invalidated"
                                    );
                                }
                                Err(s) => {
                                    warn!(status = %s, "dkms.topology stream broken");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "dkms.topology subscribe failed; retrying"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
        });
    }
    // Same K8s parallel-startup race as ORR below: QKC sidecar may not
    // be ready when DKMS hits this. Retry up to 20×1s.
    let qkc = {
        let mut last_err: Option<String> = None;
        let mut connected: Option<std::sync::Arc<QkcClient>> = None;
        for attempt in 0..20 {
            match QkcClient::connect(&cfg.southbound, None).await {
                Ok(c) => {
                    info!(endpoint = %cfg.southbound.qkc_endpoint, attempt = attempt + 1, "qkc client connected");
                    connected = Some(std::sync::Arc::new(c));
                    break;
                }
                Err(e) => {
                    last_err = Some(e.to_string());
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
        }
        if let (None, Some(err)) = (&connected, &last_err) {
            warn!(error = %err, endpoint = %cfg.southbound.qkc_endpoint, "qkc unreachable after 20 retries; continuing without it");
        }
        connected
    };
    // ORR es opcional por config: si `southbound.orr_endpoint` está
    // vacío/ausente, `connect_opt` devuelve `Ok(None)` sin loguear.
    // En despliegues K8s con sidecars ORR/QKC en el mismo Pod, los
    // 3 containers arrancan en paralelo y el DKMS puede llegar a
    // `connect_opt` antes de que el ORR haya bindado :50052. Hacemos
    // hasta 20 retries × 1s para absorber la race; si pasada esta
    // ventana sigue down, lo damos por permanentemente caído.
    let orr = {
        let mut last_err: Option<String> = None;
        let mut connected: Option<std::sync::Arc<OrrClient>> = None;
        for attempt in 0..20 {
            match OrrClient::connect_opt(&cfg.southbound, build_orr_tls(&cfg)?).await {
                Ok(Some(c)) => {
                    info!(
                        endpoint = %cfg.southbound.orr_endpoint.as_deref().unwrap_or(""),
                        attempt = attempt + 1,
                        "orr client connected",
                    );
                    connected = Some(std::sync::Arc::new(c));
                    break;
                }
                Ok(None) => break, // not configured
                Err(e) => {
                    last_err = Some(e.to_string());
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
        }
        if let (None, Some(err)) = (&connected, &last_err) {
            warn!(error = %err, "orr unreachable after 20 retries; continuing without it");
        }
        connected
    };

    let mut svc = DkmsService::new(
        cfg.clone(),
        metrics.clone(),
        pool.clone(),
        pending,
        buckets,
        sae_binding,
        peers.clone(),
        sdn,
        qkc,
        orr.clone(),
        peer_client,
    );

    // Rotación por tiempo de las épocas e2e con cada peer (`crate::e2e`).
    svc.e2e.clone().spawn_rekey_loop();

    // ─── Control plane: Generator + AckSocket ────────────────────────
    // Solo arranca si tenemos ORR conectado (para enviar) y al menos un
    // peer con transport=orr. El Generator pollerá rates al SDN HTTP
    // (http_admin del SDN expone GET /rate/{dkms_id}) y rellenará los
    // buffers ENC compartidos contra cada peer.
    let mut ack_socket_addr: Option<std::net::SocketAddr> = cfg.generator.ack_socket_addr;
    let (generator_arc, ack_client_arc, sae_buf_buckets_arc) = if let (Some(orr_client), true) =
        (orr.as_ref(), cfg.generator.enabled)
    {
        // Derivar dirección de ACK socket: si no está explícita, usar
        // (peer_addr.host, peer_addr.port+1000) — convención local-dev.
        if ack_socket_addr.is_none() {
            let mut a = cfg.listen.peer_addr;
            a.set_port(a.port().wrapping_add(1000));
            ack_socket_addr = Some(a);
        }
        let sdn_http_url = derive_sdn_http_url(&cfg.southbound.sdn_endpoint);
        let ctrl_ca = cfg
            .tls
            .control_plane_ca
            .as_deref()
            .unwrap_or(&cfg.tls.peer_dkms_ca);
        let sdn_http = Arc::new(
            SdnHttpClient::new_with_tls(
                sdn_http_url,
                std::time::Duration::from_millis(cfg.southbound.rpc_timeout_ms),
                Some(common::http::ClientTls {
                    ca_path: ctrl_ca,
                    cert_path: &cfg.tls.cert_path,
                    key_path: &cfg.tls.key_path,
                }),
            )
            .context("sdn http client")?,
        );
        let mut cfg_with_addr = (*cfg).clone();
        cfg_with_addr.generator.ack_socket_addr = ack_socket_addr;
        let ack_pending = Arc::new(AckPendingStore::new());
        let gen = Generator::new(
            &cfg_with_addr,
            peers.clone(),
            orr_client.clone(),
            sdn_http,
            svc.e2e.clone(),
            pool.clone(),
            ack_pending.clone(),
            svc.demand_tracker.clone(),
            svc.flow.clone(),
        );
        let gen_arc = gen.spawn_background();

        // ACK client batched (envía ACKs hacia los `ack_endpoint` que
        // viajan en el header de los DKMS_BUFFER entrantes).
        let ack_client = AckClient::new(cfg.node_id.clone());
        // ACK autenticado si el operador lo pide Y hay cliente mTLS. En un
        // despliegue ORR-only `peer_client` es None: ahí no hay por dónde,
        // y se avisa en vez de callar y seguir con el socket como si nada.
        let quiere_etsi020 = cfg.generator.ack_transport.eq_ignore_ascii_case("etsi020");
        let ack_etsi020 = match (quiere_etsi020, svc.peer_client.as_ref()) {
            (true, Some(pc)) => {
                info!("dkms.ack: los ACK salen autenticados por ETSI-020 (mTLS)");
                Some(dkms::control::Etsi020AckTransport {
                    client: pc.clone(),
                    peers: svc.peers.clone(),
                })
            }
            (true, None) => {
                warn!(
                        "dkms.ack: ack_transport = etsi020 pero no hay peer_client                          (¿todos los peers por ORR?); los ACK siguen por el socket SIN AUTENTICAR"
                    );
                None
            }
            (false, _) => None,
        };
        let batched = Arc::new(BatchedAckClient::with_transport(
            ack_client,
            32,
            std::time::Duration::from_millis(50),
            svc.flow.clone(),
            ack_etsi020,
        ));

        // Servidor TCP de ACKs entrantes.
        if let Some(addr) = ack_socket_addr {
            let gen_for_socket = gen_arc.clone();
            tokio::spawn(async move {
                if let Err(e) = ack_socket::serve(gen_for_socket, addr).await {
                    warn!(error = %e, "ack_socket server exited");
                }
            });
            info!(%addr, "dkms.ack_socket configured");
        }

        // SAE buffer buckets dinámicos: comparten el handle de rates SDN
        // con el Generator para calcular refill = link_capacity / N_SAEs.
        let sae_buf_buckets = Arc::new(SaeBufferBuckets::new(
            pool.clone(),
            gen_arc.rates_handle(),
            cfg.sae.observation_window_secs,
            cfg.sae.min_capacity_tokens,
        ));

        (Some(gen_arc), Some(batched), Some(sae_buf_buckets))
    } else {
        info!("generator disabled (no ORR or generator.enabled=false)");
        (None, None, None)
    };
    svc.set_control(generator_arc, ack_client_arc, sae_buf_buckets_arc);
    let _ = ack_socket_addr; // silencia warning si no se usa

    // ─── TLS servidor ─────────────────────────────────────────────────
    let sae_tls = common::tls::server_config(
        &cfg.tls.cert_path,
        &cfg.tls.key_path,
        Some(cfg.tls.sae_client_ca.as_path()),
    )
    .context("sae tls")?;
    let peer_tls = common::tls::server_config(
        &cfg.tls.cert_path,
        &cfg.tls.key_path,
        Some(cfg.tls.peer_dkms_ca.as_path()),
    )
    .context("peer dkms tls")?;

    // ─── Listeners ────────────────────────────────────────────────────
    let http_task = tokio::spawn({
        let svc = svc.clone();
        let listen = cfg.listen.clone();
        async move { etsi_http::serve(svc, &listen, sae_tls, peer_tls).await }
    });
    let grpc_task = tokio::spawn({
        let svc = svc.clone();
        let addr = cfg.listen.grpc_addr;
        async move { grpc_server::serve(svc, addr).await }
    });
    let bg_task = tokio::spawn({
        let svc = svc.clone();
        async move {
            svc.run_background_tasks()
                .await
                .map_err(anyhow::Error::from)
        }
    });

    let _ = cli; // (silencia warning si no se usa)
    let _: PathBuf = cfg.tls.cert_path.clone(); // (mantener type para futuras checks)

    tokio::select! {
        r = http_task => r??,
        r = grpc_task => r??,
        r = bg_task   => r??,
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
