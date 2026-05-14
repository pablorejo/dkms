//! Wire-level message types for QKC.
//!
//! These are the strongly-typed counterparts of the Python `message_models`
//! classes. The binary TCP transport uses `Frame` (in
//! `common::ipc::binary_tcp`); the higher-level QKC semantics ride on top.

use serde::{Deserialize, Serialize};

/// Decrypted/plaintext payload exchanged once OTP is peeled off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub sender:    String,
    pub receiver:  String,
    pub final_dst: String,
    pub key_size_bits: u32,
    pub key_ids:   Vec<String>,
    pub header:    serde_json::Value,
    pub payload:   Vec<u8>,
}

/// Auxiliary metadata accepted in the msgpack header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageData {
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub created_at_ms:  Option<i64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Wrapped-up message ready for the wire.
#[derive(Debug, Clone)]
pub struct MessageEncrypted {
    pub sender:    String,
    pub receiver:  String,
    pub final_dst: String,
    pub key_ids:   Vec<String>,
    pub header_mp: Vec<u8>,
    pub payload:   Vec<u8>,
}
