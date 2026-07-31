//! Contadores del camino de claves DKMS↔DKMS, para diagnosticar dónde se
//! rompe el ciclo emit → deliver → ACK → `buffer_enc`.
//!
//! El ciclo tiene cuatro saltos y hasta ahora sólo el último era visible
//! (`generator.state` sólo contaba los ACK que SÍ llegaban). Con
//! `enc=0 ack_pending=4096 emit_total=0` era imposible distinguir estos
//! casos, que exigen arreglos completamente distintos:
//!
//! | Síntoma | Qué falla |
//! |---|---|
//! | `emitted=0` | el generador no emite: sin rate, sin peers, o el ORR rechaza |
//! | `emitted>0, peer_recv=0` | la clave se pierde en ORR/QKC (ida) |
//! | `peer_recv>0, ack_sent=0` | el peer recibe pero no sabe a dónde acusar recibo |
//! | `ack_sent>0, acked=0` | el ACK no vuelve (TCP 20009) o no casa (`ack_miss_*`) |
//! | `ack_miss_unknown_peer>0` | el `from` del ACK no coincide con el id del peer |
//! | `ack_miss_unknown_key>0` | el ACK llegó tarde: el reaper ya expiró la clave |
//!
//! Los contadores son acumulativos desde el arranque y se vuelcan cada 5 s
//! en la línea `generator.state` junto al estado de los buffers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

/// Contadores de un peer DKMS concreto. Todos monótonos crecientes.
#[derive(Debug, Default)]
pub struct PeerFlow {
    // ── lado emisor (yo genero claves para este peer) ──────────────────
    /// Claves entregadas al ORR sin error.
    pub emitted: AtomicU64,
    /// Emisiones que el ORR rechazó (la clave se retira de `ack_pending`).
    pub emit_failed: AtomicU64,
    /// ACKs que casaron y movieron la clave a `buffer_enc`.
    pub acked: AtomicU64,
    /// ACK con un `from` para el que no hay NADA en `ack_pending`.
    /// Casi siempre un desajuste de identidad (`node_id` del peer ≠ la
    /// clave con la que yo lo tengo configurado).
    pub ack_miss_unknown_peer: AtomicU64,
    /// ACK cuyo peer sí existe pero el `key_id` ya no: llegó después de
    /// que el reaper expirara la entrada (`ack_timeout_ms` corto o
    /// round-trip lento).
    pub ack_miss_unknown_key: AtomicU64,
    /// Entradas que el reaper expiró sin ACK.
    pub expired: AtomicU64,
    /// ACKs descartados porque `buffer_enc` estaba lleno.
    pub enc_full: AtomicU64,

    // ── lado receptor (este peer genera claves para mí) ────────────────
    /// `DKMS_BUFFER` recibidos por ORR desde este peer.
    pub recv: AtomicU64,
    /// ACKs encolados hacia este peer.
    pub ack_enqueued: AtomicU64,
    /// ACKs efectivamente escritos en el socket del peer.
    pub ack_sent: AtomicU64,
    /// Flushes de ACK que fallaron (connect/write). Cuenta CLAVES, no
    /// intentos: un batch de 32 que falla suma 32.
    pub ack_send_failed: AtomicU64,
    /// Claves recibidas sin `ack_endpoint` en la cabecera: imposible
    /// acusar recibo, el emisor las verá expirar.
    pub ack_no_endpoint: AtomicU64,
    /// Claves que llegaron con la huella cambiada: corrupción en tránsito.
    /// Se descartan sin acusar recibo. Cualquier valor > 0 aquí significa
    /// que el enlace QKC hacia ese peer está entregando basura.
    pub recv_corrupt: AtomicU64,
}

macro_rules! bump {
    ($name:ident) => {
        pub fn $name(&self, peer: &str, n: u64) {
            self.peer(peer).$name.fetch_add(n, Ordering::Relaxed);
        }
    };
}

/// Tabla `peer_dkms_id → PeerFlow`, compartida por el generador, el socket
/// de ACK (ambos sentidos) y el pump de deliveries del ORR.
#[derive(Debug, Default)]
pub struct FlowStats {
    peers: DashMap<String, Arc<PeerFlow>>,
    /// Último `ack_endpoint` anunciado por cada peer. Lo registra el pump
    /// de deliveries; sirve para ver a qué dirección estamos mandando los
    /// ACK sin tener que leer la cabecera a mano.
    endpoints: DashMap<String, String>,
}

impl FlowStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn peer(&self, peer: &str) -> Arc<PeerFlow> {
        if let Some(e) = self.peers.get(peer) {
            return e.value().clone();
        }
        self.peers
            .entry(peer.to_owned())
            .or_insert_with(|| Arc::new(PeerFlow::default()))
            .clone()
    }

    bump!(emitted);
    bump!(emit_failed);
    bump!(acked);
    bump!(ack_miss_unknown_peer);
    bump!(ack_miss_unknown_key);
    bump!(expired);
    bump!(enc_full);
    bump!(recv);
    bump!(ack_enqueued);
    bump!(ack_sent);
    bump!(ack_send_failed);
    bump!(ack_no_endpoint);
    bump!(recv_corrupt);

    /// Registra el `ack_endpoint` que anuncia un peer. Devuelve `true` si
    /// es la primera vez que lo vemos o si cambió — el caller lo usa para
    /// loguear una sola vez en vez de por cada clave.
    pub fn note_endpoint(&self, peer: &str, endpoint: &str) -> bool {
        // El guard de `get` se suelta al terminar ESTE statement. Hacer el
        // `insert` dentro de un `match` sobre `get` bloquea el shard
        // consigo mismo — y como esto corre en el pump de deliveries del
        // ORR, congelaría la recepción de claves en la primera que llegue.
        let same = self
            .endpoints
            .get(peer)
            .is_some_and(|e| e.value() == endpoint);
        if same {
            return false;
        }
        self.endpoints.insert(peer.to_owned(), endpoint.to_owned());
        true
    }

    pub fn endpoint_of(&self, peer: &str) -> Option<String> {
        self.endpoints.get(peer).map(|e| e.value().clone())
    }

    /// Ids de todos los peers con algún contador. El log de estado une
    /// esto con los peers del `PeerRegistry` para que un peer del que sólo
    /// recibimos (y que no está en nuestra config) también salga.
    pub fn peer_ids(&self) -> Vec<String> {
        self.peers.iter().map(|e| e.key().clone()).collect()
    }
}

/// Motivo por el que un `ack_endpoint` anunciado no sirve para que los
/// peers nos alcancen. Ver [`classify_endpoint`].
#[derive(Debug, PartialEq, Eq)]
pub enum EndpointVerdict {
    Routable,
    /// `0.0.0.0` / `[::]` — al conectar, Linux lo reinterpreta como
    /// localhost, así que el ACK del peer va a su PROPIO socket y se
    /// pierde en silencio. Es el fallo más traicionero de todos.
    Wildcard,
    /// `127.0.0.1` / `localhost` / `[::1]` — el peer se acusaría recibo a
    /// sí mismo. Sólo es válido si todos los DKMS comparten máquina.
    Loopback,
}

/// Clasifica el `ack_endpoint` que vamos a anunciar. Sin resolución DNS:
/// un hostname se asume enrutable (en K8s el Service lo es).
pub fn classify_endpoint(endpoint: &str) -> EndpointVerdict {
    let host = match endpoint.rsplit_once(':') {
        // `[::1]:9000` → quitamos los corchetes.
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => endpoint,
    };
    match host {
        "0.0.0.0" | "::" | "" => EndpointVerdict::Wildcard,
        "127.0.0.1" | "::1" | "localhost" => EndpointVerdict::Loopback,
        h if h.starts_with("127.") => EndpointVerdict::Loopback,
        _ => EndpointVerdict::Routable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_per_peer() {
        let s = FlowStats::new();
        s.emitted("dkms-2", 3);
        s.emitted("dkms-3", 1);
        s.acked("dkms-2", 2);
        assert_eq!(s.peer("dkms-2").emitted.load(Ordering::Relaxed), 3);
        assert_eq!(s.peer("dkms-2").acked.load(Ordering::Relaxed), 2);
        assert_eq!(s.peer("dkms-3").emitted.load(Ordering::Relaxed), 1);
        assert_eq!(s.peer("dkms-3").acked.load(Ordering::Relaxed), 0);
        let mut ids = s.peer_ids();
        ids.sort();
        assert_eq!(ids, vec!["dkms-2".to_string(), "dkms-3".to_string()]);
    }

    #[test]
    fn note_endpoint_only_fires_on_change() {
        let s = FlowStats::new();
        assert!(s.note_endpoint("dkms-2", "10.0.0.2:20009"));
        assert!(!s.note_endpoint("dkms-2", "10.0.0.2:20009"));
        assert!(s.note_endpoint("dkms-2", "10.0.0.9:20009"));
        assert_eq!(s.endpoint_of("dkms-2").as_deref(), Some("10.0.0.9:20009"));
    }

    #[test]
    fn wildcard_and_loopback_endpoints_are_flagged() {
        assert_eq!(
            classify_endpoint("0.0.0.0:20009"),
            EndpointVerdict::Wildcard
        );
        assert_eq!(classify_endpoint("[::]:20009"), EndpointVerdict::Wildcard);
        assert_eq!(
            classify_endpoint("127.0.0.1:20009"),
            EndpointVerdict::Loopback
        );
        assert_eq!(
            classify_endpoint("localhost:20009"),
            EndpointVerdict::Loopback
        );
        assert_eq!(
            classify_endpoint("192.168.50.202:20009"),
            EndpointVerdict::Routable
        );
        assert_eq!(classify_endpoint("dkms-2:20009"), EndpointVerdict::Routable);
    }
}
