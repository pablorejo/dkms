//! QKC — Quantum Key Channel.
//!
//! Hace de **enrutador hop-by-hop** entre nodos QKC. Cifra el payload
//! con claves OTP de 256 bits obtenidas del **quditto compartido** de
//! cada enlace (un quditto distinto por enlace QKC↔QKC) y deja la
//! cabecera en claro.
//!
//! Tres listeners:
//!
//! * **TCP peer** — frames `FRAME_RECV` / `FRAME_RELAY` desde / hacia
//!   otros QKCs. Es el hot path.
//! * **TCP local** — frames `FRAME_LOCAL_SEND` / `FRAME_LOCAL_DELIVER`
//!   entre este QKC y el ORR co-localizado. Mismo wire binario, sin
//!   cifrado (el plaintext se entrega al ORR tal cual y el ORR le da
//!   los plaintext al QKC para que él los cifre).
//! * **HTTP admin** — `POST /forwarding-table` (SDN o pruebas) y
//!   `GET /healthz`.
//!
//! Diferencias con el QKC Python (todas en favor de velocidad):
//!
//! * Sin GIL: cifrado XOR puro inline, sin process pool ni bridge
//!   sync/async.
//! * Forwarding table en `ArcSwap` — lecturas lock-free.
//! * `dec_keys` siempre en **batch** (un POST por mensaje, no uno por
//!   chunk).
//! * Cliente HTTP a quditto con keep-alive y `reqwest::Client` único.
//! * Cola de envío por peer con `crossbeam::ArrayQueue` lock-free.

pub mod config;
pub mod crypto;
pub mod error;
pub mod http_admin;
pub mod keystore;
pub mod kme;
pub mod relay;
pub mod routing;
pub mod service;
pub mod transport;
