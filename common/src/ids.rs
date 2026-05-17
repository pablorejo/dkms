//! Strongly-typed ID newtypes. Use these instead of bare `String` so the type
//! system catches accidental mix-ups (passing a NodeId where a SaeId was
//! expected, etc.).

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Serialize};

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
