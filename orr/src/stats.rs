//! Contadores del ORR y la línea de estado periódica.
//!
//! El ORR era el único de los cinco módulos sin nada periódico que mirar: el
//! QKC saca `keystore.levels` cada 5 s y el DKMS `generator.state`, pero aquí
//! sólo había eventos sueltos del bootstrap. Cuando un par empezaba a
//! entregar material corrupto, sus logs no decían absolutamente nada — había
//! que deducirlo del `recv_corrupt` del DKMS del otro extremo y bajar desde
//! ahí. Esta línea existe para que ese diagnóstico se vea de un vistazo.
//!
//! Lo que hay que mirar cuando algo va mal:
//!
//! * `dropped_no_secret` subiendo ⇒ llegan frames cifrados con una época que
//!   este extremo no tiene. Es la firma del re-bootstrap pendiente: el peer se
//!   reinició y regeneró su identidad mientras nosotros conservábamos la
//!   vieja. Dispara el re-handshake pasivo, así que un pico que se estabiliza
//!   es recuperación; uno que no para es el fallo.
//! * `peel_failed` subiendo ⇒ el secreto existe pero no descifra, o el frame
//!   viene mal formado. La capa onion no lleva MAC, así que esto sólo caza lo
//!   estructural: un secreto divergente entrega basura sin error, y quien lo
//!   detecta es el `key_digest` del DKMS.
//! * un peer con `master=no` de forma persistente ⇒ el bootstrap no ha
//!   convergido con él, y todo lo que se le mande será indescifrable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::peers::PeerRegistry;

/// Contadores acumulados desde el arranque. Todos `Relaxed`: se leen para
/// mirarlos, no para coordinar nada.
#[derive(Debug, Default)]
pub struct OrrStats {
    /// Mensajes que este ORR ha aceptado enviar (tras elegir modo).
    pub sent: AtomicU64,
    /// ...y los que fallaron antes de salir.
    pub send_failed: AtomicU64,
    /// Frames recibidos del QKC, sea cual sea su destino.
    pub recv: AtomicU64,
    /// Entregados al DKMS local.
    pub delivered: AtomicU64,
    /// Reenviados a otro ORR (sólo cebolla multi-hop).
    pub relayed: AtomicU64,
    /// Descartados por no tener `master_secret` de la época que traían.
    pub dropped_no_secret: AtomicU64,
    /// `onion::peel` devolvió error.
    pub peel_failed: AtomicU64,
}

impl OrrStats {
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// Arranca la tarea que saca `orr.state` y una `orr.peer` por par.
///
/// Se separan a propósito: la agregada es la que se sigue en el tiempo, y las
/// de par son las que dicen con quién no ha convergido el bootstrap. Repetir
/// los contadores globales en cada par sólo sería ruido.
pub fn spawn_state_logger(stats: Arc<OrrStats>, peers: Arc<PeerRegistry>, every: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
            let snapshot = peers.snapshot();
            let with_bootstrap = snapshot.keys().filter(|p| peers.has_bootstrap(p)).count();
            let with_master = snapshot
                .keys()
                .filter(|p| peers.has_master_secret(p))
                .count();
            info!(
                me = %peers.local_orr_id(),
                qkc = peers.local_qkc_id(),
                peers = snapshot.len(),
                with_bootstrap,
                with_master,
                sent = g(&stats.sent),
                send_failed = g(&stats.send_failed),
                recv = g(&stats.recv),
                delivered = g(&stats.delivered),
                relayed = g(&stats.relayed),
                // Los dos que hay que vigilar: ver el módulo.
                dropped_no_secret = g(&stats.dropped_no_secret),
                peel_failed = g(&stats.peel_failed),
                "orr.state",
            );
            // Orden estable: si no, dos vueltas seguidas parecen distintas y
            // no se pueden comparar de un vistazo.
            let mut ids: Vec<&String> = snapshot.keys().collect();
            ids.sort();
            for peer in ids {
                info!(
                    me = %peers.local_orr_id(),
                    peer = %peer,
                    qkc = snapshot[peer],
                    bootstrap = peers.has_bootstrap(peer),
                    master = peers.has_master_secret(peer),
                    epoch = ?peers.latest_epoch_for(peer),
                    send_epoch = ?peers.current_send_epoch(peer),
                    // Sin dirección no hay re-bootstrap posible: el peer sería
                    // inalcanzable justo cuando hay que rehacer el handshake.
                    addr = ?peers.grpc_addr(peer),
                    "orr.peer",
                );
            }
        }
    });
}
