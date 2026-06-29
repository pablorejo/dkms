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

use common::security::KeyGrade;
use dashmap::DashMap;

use super::buffer::SecureKeyBuffer;

/// Buffers de un peer. El lado **ENC** está separado por grado
/// (`enc_qkd`/`enc_pqc`) para que una petición `strict_qkd` reciba una clave
/// QKD-grade y `no_worry` consuma PQC-grade conservando las QKD. El lado
/// **DEC** es un único buffer: las claves entrantes se buscan por `key_id`
/// (`take_by_id`), así que el grado no cambia la recuperación.
pub struct PeerBuffers {
    enc_qkd: Arc<SecureKeyBuffer>,
    enc_pqc: Arc<SecureKeyBuffer>,
    pub dec: Arc<SecureKeyBuffer>,
}

impl PeerBuffers {
    fn new(capacity: usize) -> Self {
        Self {
            enc_qkd: Arc::new(SecureKeyBuffer::new(capacity)),
            enc_pqc: Arc::new(SecureKeyBuffer::new(capacity)),
            dec: Arc::new(SecureKeyBuffer::new(capacity)),
        }
    }

    /// Buffer ENC del grado dado.
    pub fn enc(&self, grade: KeyGrade) -> &Arc<SecureKeyBuffer> {
        match grade {
            KeyGrade::Qkd => &self.enc_qkd,
            KeyGrade::Pqc => &self.enc_pqc,
        }
    }

    /// Total de claves ENC en ambos grados (métricas / nivel de demanda).
    pub fn enc_len(&self) -> usize {
        self.enc_qkd.len() + self.enc_pqc.len()
    }

    /// Popea una clave ENC del PRIMER grado con stock según el orden de
    /// preferencia (`SecurityLevel::serve_pref`). `None` si todos vacíos.
    pub fn enc_pop_pref(&self, prefs: &[KeyGrade]) -> Option<super::buffer::TransportKey> {
        prefs.iter().find_map(|&g| self.enc(g).pop_oldest())
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
            .map(|e| (e.key().clone(), e.value().enc_len(), e.value().dec.len()))
            .collect()
    }

    /// Forzar borrado de todos los buffers (drain/shutdown).
    pub fn clear_all(&self) {
        for e in self.peers.iter() {
            e.value().enc_qkd.clear();
            e.value().enc_pqc.clear();
            e.value().dec.clear();
        }
    }
}
