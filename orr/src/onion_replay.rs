//! Frescura extremo a extremo de la cebolla: una ventana anti-replay por ORR
//! de origen.
//!
//! El MAC de enlace del QKC ya impide que alguien reinyecte un frame **en el
//! cable**, pero es salto a salto: cada QKC lo recalcula, así que un QKC del
//! camino puede capturar una capa válida y volver a mandarla. El tag AEAD de la
//! capa sigue siendo correcto —la clave no ha cambiado— y el destino la
//! entregaría otra vez.
//!
//! Lo que lo cierra es el par `(session, counter)` que el ORR **de origen**
//! pone en su cabecera y que entra en el AAD de todas las capas de ese mensaje.
//! Un ORR que reenvía tiene que copiarlos tal cual: si los cambia, el AAD que
//! calcula el siguiente salto deja de cuadrar y el `peel` falla. Así que el
//! contador que llega es el que puso el origen, y basta con no aceptarlo dos
//! veces.
//!
//! La ventana se indexa por `header.from`, que es el ORR de origen y no cambia
//! salto a salto — a diferencia de `next_orr_id`.

use common::crypto::frame_mac::{ReplayError, ReplayWindow};
use dashmap::DashMap;
use parking_lot::Mutex;

/// Anchura de la ventana, en unidades de contador.
///
/// **No es "cuántos mensajes hacia atrás toleramos", y esa diferencia es la
/// razón de este número.** El contador es UNO por ORR de origen, compartido
/// entre todos sus destinos, así que el destino sólo ve una de cada `d`
/// posiciones si el origen habla con `d` peers: la ventana se consume al ritmo
/// TOTAL de emisión del origen, no al de lo que llega aquí.
///
/// Con los 13 000 mensajes/s por nodo medidos en CESGA (n=10, régimen alto),
/// los 1024 del default darían 79 ms de tolerancia a reordenación — y a N=30,
/// menos. Cualquier hipo de red o de planificación por encima de eso se
/// contaría como replay de un mensaje legítimo. 65 536 dan ~5 s a ese ritmo, y
/// cuestan 8 KB de bitmap por origen (menos de 250 KB con 30 nodos).
///
/// La alternativa —un contador por destino, que haría la secuencia densa— se
/// descartó: en un camino multi-salto el ORR intermedio vería las secuencias de
/// varios destinos entrelazadas y con los mismos números, y las colisiones
/// serían falsos replays.
const ONION_WINDOW: u64 = 65_536;

/// Ventanas anti-replay, una por ORR de origen visto.
#[derive(Debug, Default)]
pub struct OnionReplay {
    by_origin: DashMap<String, Mutex<ReplayWindow>>,
}

impl OnionReplay {
    pub fn new() -> Self {
        Self {
            by_origin: DashMap::new(),
        }
    }

    /// Acepta `(session, counter)` de `origin` si es fresco, y lo marca.
    ///
    /// **Llamar sólo después de que el `peel` haya salido bien**: el AAD es lo
    /// que ata estos dos números al mensaje, así que antes de comprobar el tag
    /// no son de fiar y cualquiera podría tirar la ventana con una `session`
    /// inventada.
    pub fn check(&self, origin: &str, session: u64, counter: u64) -> Result<(), ReplayError> {
        // `entry` para no perder la carrera entre dos frames del mismo origen
        // que lleguen a la vez y creen cada uno su ventana.
        let w = self
            .by_origin
            .entry(origin.to_string())
            .or_insert_with(|| Mutex::new(ReplayWindow::new(ONION_WINDOW)));
        let mut g = w.lock();
        g.check_and_set(session, counter)
    }

    /// Cuántos orígenes distintos se están siguiendo. Para la línea `orr.state`.
    pub fn tracked_origins(&self) -> usize {
        self.by_origin.len()
    }

    /// Olvida un origen. Lo llama la baja de un peer: si vuelve, volverá con
    /// una `session` nueva de todos modos.
    pub fn forget(&self, origin: &str) {
        self.by_origin.remove(origin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_is_accepted_and_the_repeat_is_not() {
        let r = OnionReplay::new();
        assert!(r.check("orr_1", 7, 1).is_ok());
        assert!(matches!(
            r.check("orr_1", 7, 1),
            Err(ReplayError::Replayed { .. })
        ));
    }

    #[test]
    fn origins_do_not_share_a_window() {
        // Dos orígenes pueden ir por el mismo contador sin estorbarse.
        let r = OnionReplay::new();
        assert!(r.check("orr_1", 7, 1).is_ok());
        assert!(r.check("orr_2", 7, 1).is_ok());
        assert_eq!(r.tracked_origins(), 2);
    }

    #[test]
    fn out_of_order_within_the_window_is_fine() {
        let r = OnionReplay::new();
        assert!(r.check("orr_1", 7, 10).is_ok());
        for c in 1..10 {
            assert!(r.check("orr_1", 7, c).is_ok(), "rezagado {c}");
        }
        assert!(r.check("orr_1", 7, 5).is_err());
    }

    #[test]
    fn a_restarted_origin_starts_over() {
        let r = OnionReplay::new();
        assert!(r.check("orr_1", 7, 5).is_ok());
        // Sesión nueva: el origen reinició y vuelve a contar desde 1.
        assert!(r.check("orr_1", 8, 1).is_ok());
        // Lo CAPTURADO de la sesión vieja (contador ya visto) no cuela...
        assert!(matches!(
            r.check("orr_1", 7, 5),
            Err(ReplayError::Replayed { .. })
        ));
        // ...pero un frame rezagado y fresco de esa sesión sí, una vez (C-08:
        // la sesión anterior conserva su ventana; y no tira la nueva).
        assert!(r.check("orr_1", 7, 6).is_ok());
        assert!(r.check("orr_1", 7, 6).is_err());
        assert!(r.check("orr_1", 8, 2).is_ok());
        // Dos reinicios más tarde, la 7 está retirada del todo.
        assert!(r.check("orr_1", 9, 1).is_ok());
        assert!(matches!(
            r.check("orr_1", 7, 7),
            Err(ReplayError::RetiredSession { .. })
        ));
    }

    #[test]
    fn the_window_survives_a_sparse_counter() {
        // El caso real: el origen habla con 9 peers, así que a ESTE destino le
        // llega una de cada ~9 posiciones del contador. Con la ventana estrecha
        // del default esto empezaría a dar `TooOld` en cuanto hubiera algo de
        // reordenación; con `ONION_WINDOW` hay margen de sobra.
        let r = OnionReplay::new();
        let peers = 9u64;
        for i in 1..=2000u64 {
            assert!(r.check("orr_1", 7, i * peers).is_ok(), "mensaje {i}");
        }
        // Y un rezagado de hace 500 mensajes (4500 posiciones de contador)
        // sigue entrando: es reordenación, no un replay.
        assert!(r.check("orr_1", 7, 1500 * peers - 1).is_ok());
    }

    #[test]
    fn forget_drops_the_window() {
        let r = OnionReplay::new();
        assert!(r.check("orr_1", 7, 1).is_ok());
        r.forget("orr_1");
        assert_eq!(r.tracked_origins(), 0);
        assert!(r.check("orr_1", 7, 1).is_ok());
    }
}
