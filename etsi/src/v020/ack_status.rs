//! Equivalente a `ETSIQKD/ETSI020/ETSI020_Status.py`.
//!
//! Enum de valores serializados como string igual que el Python
//! (`str, Enum`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Etsi020AckStatus {
    #[serde(rename = "relayed")]
    Relayed,
    #[serde(rename = "voided")]
    Voided,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "key not present")]
    NotPresent,
}

impl Etsi020AckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Relayed => "relayed",
            Self::Voided => "voided",
            Self::Failed => "failed",
            Self::NotPresent => "key not present",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_string() {
        let s = serde_json::to_string(&Etsi020AckStatus::Relayed).unwrap();
        assert_eq!(s, r#""relayed""#);
    }

    #[test]
    fn parses_key_not_present_with_space() {
        let v: Etsi020AckStatus = serde_json::from_str(r#""key not present""#).unwrap();
        assert_eq!(v, Etsi020AckStatus::NotPresent);
    }
}
