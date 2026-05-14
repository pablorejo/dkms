//! Newtype `Base64Bytes` equivalente a `pydantic.Base64Bytes`.
//!
//! En JSON va como una cadena base64. En memoria es `Vec<u8>` con los
//! bytes crudos. Esto reproduce el comportamiento del Python sin obligar
//! a los callers a hacer base64 manualmente.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64Bytes(pub Vec<u8>);

impl Base64Bytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn to_b64_string(&self) -> String {
        STANDARD.encode(&self.0)
    }
}

impl From<Vec<u8>> for Base64Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<Base64Bytes> for Vec<u8> {
    fn from(v: Base64Bytes) -> Self {
        v.0
    }
}

impl AsRef<[u8]> for Base64Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for Base64Bytes {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_b64_string())
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        let raw = STANDARD
            .decode(s.as_bytes())
            .map_err(|e| serde::de::Error::custom(format!("base64 decode: {e}")))?;
        Ok(Self(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_json() {
        let v = Base64Bytes::new(b"hello world".to_vec());
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(j, r#""aGVsbG8gd29ybGQ=""#);
        let back: Base64Bytes = serde_json::from_str(&j).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn rejects_garbage() {
        let r: Result<Base64Bytes, _> = serde_json::from_str(r#""not valid base64!!!""#);
        assert!(r.is_err());
    }
}
