//! Equivalente a `ETSIQKD/ETSI014/ETSI014_KeyRequest.py`.
//!
//! Incluye la normalización del Python para `additional_saes` /
//! `additional_slave_SAE_IDs`: el campo puede llegar como lista, como
//! string CSV/JSON, o desde una cabecera HTTP.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{error::EtsiError, message::EtsiMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014KeyRequest {
    /// Número de claves a devolver (default `1`, `ge=1`).
    #[serde(default = "default_number")]
    pub number: u32,

    /// Tamaño de las claves en bits (default `256`, `gt=0`).
    #[serde(default = "default_size")]
    pub size: u32,

    /// Lista de SAEs adicionales (legacy ETSI 014).
    /// Acepta string CSV/JSON, lista, dict, o ya normalizada via
    /// [`deserialize_sae_list`].
    #[serde(
        default,
        rename = "additional_slave_SAE_IDs",
        deserialize_with = "deserialize_sae_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_slave_sae_ids: Option<Vec<String>>,

    /// Alias simplificado para indicar SAEs adicionales.
    /// Pydantic acepta también `additional-saes` con guion.
    #[serde(
        default,
        rename = "additional_saes",
        alias = "additional-saes",
        deserialize_with = "deserialize_sae_list",
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_saes: Option<Vec<String>>,

    /// Extensiones obligatorias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_mandatory: Option<Vec<serde_json::Map<String, Value>>>,

    /// Extensiones opcionales.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_optional: Option<Vec<serde_json::Map<String, Value>>>,
}

fn default_number() -> u32 {
    1
}
fn default_size() -> u32 {
    256
}

impl Default for Etsi014KeyRequest {
    fn default() -> Self {
        Self {
            number: default_number(),
            size: default_size(),
            additional_slave_sae_ids: None,
            additional_saes: None,
            extension_mandatory: None,
            extension_optional: None,
        }
    }
}

impl Etsi014KeyRequest {
    /// Equivalente al validador del Python (`number >= 1`, `size > 0`).
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.number < 1 {
            return Err(EtsiError::Validation("number must be >= 1".into()));
        }
        if self.size == 0 {
            return Err(EtsiError::Validation("size must be > 0".into()));
        }
        Ok(())
    }

    /// Replica el `_normalize_single_sae` del Python: strip + descarta
    /// vacíos.
    fn normalize_single_sae(value: &Value) -> Option<String> {
        let s = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Null => return None,
            other => other.to_string(),
        };
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }

    /// Replica el `_normalize_sae_list` del Python: acepta lista,
    /// string CSV/JSON, dict con clave conocida; devuelve lista dedup
    /// preservando orden.
    pub fn normalize_sae_list(value: &Value) -> Vec<String> {
        let raw_values: Vec<Value> = match value {
            Value::Null => return vec![],
            Value::Array(arr) => arr.clone(),
            Value::Object(map) => {
                for key in ["additional_saes", "additional_slave_SAE_IDs", "values"] {
                    if let Some(v) = map.get(key) {
                        return Self::normalize_sae_list(v);
                    }
                }
                vec![]
            }
            Value::String(s) => {
                let text = s.trim();
                if text.is_empty() {
                    vec![]
                } else if text.starts_with('[') && text.ends_with(']') {
                    match serde_json::from_str::<Value>(text) {
                        Ok(parsed) => return Self::normalize_sae_list(&parsed),
                        Err(_) => split_csv_like(text),
                    }
                } else {
                    split_csv_like(text)
                }
            }
            other => vec![other.clone()],
        };

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for v in raw_values {
            if let Some(sae) = Self::normalize_single_sae(&v) {
                if seen.insert(sae.clone()) {
                    out.push(sae);
                }
            }
        }
        out
    }

    /// Equivalente a `parse_additional_saes_header` del Python.
    pub fn parse_additional_saes_header(header_value: &Value) -> Vec<String> {
        Self::normalize_sae_list(header_value)
    }

    /// Equivalente a `resolved_additional_slave_sae_ids` del Python:
    /// combina los tres orígenes (campo legacy, alias, cabecera) sin
    /// duplicados y preservando orden de primera aparición.
    pub fn resolved_additional_slave_sae_ids(&self, header_value: Option<&Value>) -> Vec<String> {
        let mut combined = Vec::<String>::new();
        let mut seen = std::collections::HashSet::<String>::new();

        let mut append_unique = |values: Vec<String>| {
            for v in values {
                let t = v.trim().to_string();
                if !t.is_empty() && seen.insert(t.clone()) {
                    combined.push(t);
                }
            }
        };

        if let Some(list) = &self.additional_slave_sae_ids {
            append_unique(list.clone());
        }
        if let Some(list) = &self.additional_saes {
            append_unique(list.clone());
        }
        if let Some(h) = header_value {
            append_unique(Self::parse_additional_saes_header(h));
        }

        combined
    }
}

fn split_csv_like(s: &str) -> Vec<Value> {
    s.split([',', ';', ' ', '\t', '\n', '\r'])
        .filter(|p| !p.is_empty())
        .map(|p| Value::String(p.to_string()))
        .collect()
}

/// Deserializador que aplica `normalize_sae_list` al entrar.
fn deserialize_sae_list<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Vec<String>>, D::Error> {
    let v = Value::deserialize(de)?;
    let normalized = Etsi014KeyRequest::normalize_sae_list(&v);
    Ok(if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    })
}

impl EtsiMessage for Etsi014KeyRequest {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let r = Etsi014KeyRequest::default();
        assert_eq!(r.number, 1);
        assert_eq!(r.size, 256);
    }

    #[test]
    fn normalize_accepts_list() {
        let v = serde_json::json!(["a", "b", "a"]);
        let out = Etsi014KeyRequest::normalize_sae_list(&v);
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn normalize_accepts_csv() {
        let v = Value::String("alice, bob ; charlie".into());
        let out = Etsi014KeyRequest::normalize_sae_list(&v);
        assert_eq!(out, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn normalize_accepts_json_string() {
        let v = Value::String(r#"["x","y"]"#.into());
        let out = Etsi014KeyRequest::normalize_sae_list(&v);
        assert_eq!(out, vec!["x", "y"]);
    }
}
