//! Strongly-typed ID newtypes. Use these instead of bare `String` so the type
//! system catches accidental mix-ups (passing a NodeId where a SaeId was
//! expected, etc.).

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};

/// Longitud máxima de un id que entra por la red (URL, cuerpo ETSI, SAN de
/// un cert, respuesta de la SDN).
pub const MAX_ID_LEN: usize = 64;

/// ¿Es un id aceptable viniendo de fuera? No vacío, ≤ [`MAX_ID_LEN`] bytes y
/// solo `[A-Za-z0-9._:@+-]`: sin espacios, saltos de línea, `/` ni `..`, es
/// decir nada que cambie de sentido en una URL, en una línea de log o como
/// clave de un mapa (auditoría 2026-09-03, B-09). Los ids que fabrica el
/// propio proceso (UUID, `dkms-N`, `sae_N`) cumplen de sobra.
pub fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID_LEN
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'+' | b'-')
        })
}

/// Un id que no pasa [`is_valid_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub String);

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "id inválido ({} bytes; se admiten 1..={} de [A-Za-z0-9._:@+-])",
            self.0.len(),
            MAX_ID_LEN
        )
    }
}

impl std::error::Error for InvalidId {}

macro_rules! id_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[inline]
            pub fn new(v: impl Into<String>) -> Self {
                Self(v.into())
            }

            /// Constructor para ids que vienen de FUERA (red, certs, SDN):
            /// rechaza lo que [`is_valid_id`] no admite. El contenido
            /// rechazado no se copia al error (podría llevar saltos de línea
            /// o ANSI): solo su longitud.
            pub fn try_new(v: impl Into<String>) -> Result<Self, InvalidId> {
                let s: String = v.into();
                if is_valid_id(&s) {
                    Ok(Self(s))
                } else {
                    Err(InvalidId(s))
                }
            }

            #[inline]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[inline]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(v: String) -> Self {
                Self(v)
            }
        }

        impl From<&str> for $name {
            fn from(v: &str) -> Self {
                Self(v.to_owned())
            }
        }

        impl<'a> From<Cow<'a, str>> for $name {
            fn from(v: Cow<'a, str>) -> Self {
                Self(v.into_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_newtype!(NodeId, "Identifies a node in the topology graph.");
id_newtype!(SaeId, "Identifies a Secure Application Entity (ETSI SAE).");
id_newtype!(KeyId, "Identifies a single key blob.");
id_newtype!(
    LinkId,
    "Identifies a topology link (directed edge between two NodeIds)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_serde() {
        let n = NodeId::new("node-1");
        let s = serde_json::to_string(&n).unwrap();
        let n2: NodeId = serde_json::from_str(&s).unwrap();
        assert_eq!(n, n2);
    }

    #[test]
    fn distinct_id_types_dont_mix() {
        // Compile-time check: this just has to compile to prove the types
        // don't accidentally coerce into each other.
        let _: NodeId = "x".into();
        let _: SaeId = "y".into();
    }
}
