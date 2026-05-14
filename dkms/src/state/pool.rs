//! `BufferPool`: agrupa los buffers por *peer DKMS*.
//!
//! Estructura conceptual:
//!
//! ```text
//!     +--------------------- pool ---------------------+
//!     | peer = dkms-B  -->  enc: SecureKeyBuffer       |
//!     |                     dec: SecureKeyBuffer       |
//!     | peer = dkms-C  -->  enc: SecureKeyBuffer       |
//!     |                     dec: SecureKeyBuffer       |
//!     +------------------------------------------------+
//! ```
//!
//! Los buffers se crean *on-demand* al primer acceso para un peer.
//! Esto evita gestionar manualmente la lista de peers cuando viene un
//! ETSI 020 entrante de un peer no listado en `config.peers`.

use std::sync::Arc;

use dashmap::DashMap;

use super::buffer::SecureKeyBuffer;

/// Par de buffers (ENC para enviar a un peer, DEC para descifrar lo que el
/// peer manda) más capacidad declarada.
pub struct PeerBuffers {
    pub enc: Arc<SecureKeyBuffer>,
    pub dec: Arc<SecureKeyBuffer>,
}

impl PeerBuffers {
    fn new(capacity: usize) -> Self {
        Self {
            enc: Arc::new(SecureKeyBuffer::new(capacity)),
            dec: Arc::new(SecureKeyBuffer::new(capacity)),
        }
    }
}

pub struct BufferPool {
    /// Capacidad aplicada al crear cualquier peer nuevo.
    capacity_per_peer: usize,
    peers: DashMap<String, Arc<PeerBuffers>>,
}

impl BufferPool {
    pub fn new(capacity_per_peer: usize) -> Self {
        Self {
            capacity_per_peer,
            peers: DashMap::new(),
        }
    }

    /// Devuelve los buffers del peer, creándolos si es la primera vez.
    pub fn for_peer(&self, peer: &str) -> Arc<PeerBuffers> {
        if let Some(p) = self.peers.get(peer) {
            return p.clone();
        }
        // race tolerable: si dos hilos crean a la vez, dashmap se queda con
        // uno consistente; el otro se descarta antes de salir de aquí.
        self.peers
            .entry(peer.to_owned())
            .or_insert_with(|| Arc::new(PeerBuffers::new(self.capacity_per_peer)))
            .clone()
    }

    /// Snapshot de cuántas claves hay en ENC/DEC por peer. Útil para
    /// `GetBufferState` y métricas.
    pub fn snapshot(&self) -> Vec<(String, usize, usize)> {
        self.peers
            .iter()
            .map(|e| (e.key().clone(), e.value().enc.len(), e.value().dec.len()))
            .collect()
    }

    /// Forzar borrado de todos los buffers (drain/shutdown).
    pub fn clear_all(&self) {
        for e in self.peers.iter() {
            e.value().enc.clear();
            e.value().dec.clear();
        }
    }
}
