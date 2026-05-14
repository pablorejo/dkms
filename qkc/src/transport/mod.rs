//! Capa de transporte.
//!
//! Tres piezas:
//!
//! * [`peer_server`] — listener TCP que recibe frames de **otros QKCs**
//!   y los despacha al módulo [`crate::relay`].
//! * [`peer_client`] — pool de salida hacia otros QKCs. Una conexión
//!   persistente por peer con cola lock-free y writer task dedicado.
//! * [`local`] — listener TCP local para conexiones del ORR. Mismo
//!   wire binario.

pub mod local;
pub mod peer_client;
pub mod peer_server;
