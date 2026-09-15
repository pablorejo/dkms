//! QKC — Quantum Key Channel.
//!
//! Enrutador **salto a salto** entre nodos: cada enlace QKC↔QKC cifra el
//! payload con OTP (una clave de `key_size_bits` por bloque, [`crypto`]) y
//! deja las cabeceras en claro. El material de cada enlace sale de una de
//! dos fuentes, elegida por `link_type` en la config ([`config`]):
//!
//! * **`qkd`** — un KME ETSI GS QKD 014 ([`kme`]): el simulador `quditto`
//!   en pruebas o el hardware real en producción (`quditto_url` en el TOML,
//!   `kme_url` en el `node.yml` que lo genera). Los dos
//!   extremos hablan con el mismo KME; el emisor pide `enc_keys` y avisa
//!   de los `key_ID` con `FRAME_KEY_IDS_NOTIFY`, el receptor los recupera
//!   con `dec_keys` ([`keystore`]). La tasa real del enlace se mide in situ
//!   ([`rate_estimator`]) y se anuncia a la SDN.
//! * **`pqc`** — sin hardware: un secreto ML-KEM por época negociado sobre
//!   el propio canal TCP ([`pqc_handshake`]), del que se deriva el material
//!   OTP de forma determinista en los dos extremos ([`pqc_source`]).
//!
//! Sobre cualquiera de las dos, cada frame de datos lleva un MAC de enlace
//! con ventana anti-replay ([`frame_auth`]), verificado en el lector de la
//! conexión y en orden de llegada.
//!
//! Listeners ([`transport`]):
//!
//! * **TCP peer** (`peer_listen`, 20000) — frames `FRAME_RECV` /
//!   `FRAME_RELAY` de otros QKCs, más handshake PQC y NOTIFY. El hot path.
//! * **TCP local** (`local_listen`, 20001) — `FRAME_LOCAL_SEND` /
//!   `FRAME_LOCAL_DELIVER` con el ORR co-localizado: mismo wire, en claro,
//!   el QKC cifra lo que sale y descifra lo que entra.
//! * **HTTP admin** (`admin_http`, 20002; mTLS cuando hay `[tls]`,
//!   [`mtls_admin`]) — `POST /forwarding-table`, que sólo acepta a la SDN,
//!   y `GET /healthz` / `/stats` ([`http_admin`]).
//!
//! No hay topología configurada: el QKC se **anuncia** a la SDN en bucle
//! ([`sdn_client`]) con sus enlaces y su tasa medida, y la respuesta trae
//! el conjunto de vecinos, que puede añadir o retirar enlaces PQC en
//! caliente. El reenvío es multipath WCMP sobre la tabla que la SDN empuja
//! ([`routing`], [`relay`]).
//!
//! El hot path no hace red ni HTTP: forwarding table en `ArcSwap`, buffers
//! de claves en memoria rellenados por workers de fondo, una cola lock-free
//! por peer con su writer.

#![forbid(unsafe_code)]
pub mod config;
pub mod crypto;
pub mod error;
pub mod frame_auth;
pub mod http_admin;
pub mod keystore;
pub mod kme;
pub mod mtls_admin;
pub mod pqc_handshake;
pub mod pqc_source;
pub mod rate_estimator;
pub mod relay;
pub mod routing;
pub mod sdn_client;
pub mod service;
pub mod transport;
