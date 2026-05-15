//! Plano de control del DKMS: scheduling de generación de claves para
//! buffers compartidos entre DKMSs vecinos, según la rate que dicte el
//! SDN.
//!
//! El [`Generator`] mantiene N token buckets (uno por peer DKMS), los
//! recarga al ritmo que devuelve `GET /rate/{dkms_id}` del SDN, y emite
//! claves vía ORR hacia cada peer.
//!
//! Las claves emitidas viven temporalmente en `ack_pending` (en la propia
//! tabla del Generator) y solo pasan al `BufferPool.enc[peer]` cuando el
//! peer confirma con un ACK por socket TCP plano (ver
//! [`crate::control::ack_socket`]).

pub mod ack_pending;
pub mod ack_socket;
pub mod generator;
pub mod priority;
pub mod sae_buffer_bucket;

pub use ack_pending::{AckPendingStore, AckPendingEntry};
pub use ack_socket::{AckClient, AckFrame, BatchedAckClient};
pub use generator::Generator;
pub use priority::{classify, BufferQos};
pub use sae_buffer_bucket::{AdmitFailure, SaeBufferBuckets};
