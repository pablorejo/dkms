//! Niveles de seguridad de las peticiones de clave y "grado" del material.
//!
//! Con enlaces QKD y PQC mezclados, una clave servida tiene un **grado**
//! según el camino por el que se bombeó su clave de transporte:
//!
//! * [`KeyGrade::Qkd`] — todos los saltos del camino fueron QKD (seguridad
//!   incondicional por enlace, + la capa onion PQC E2E del ORR encima).
//! * [`KeyGrade::Pqc`] — algún salto fue PQC (seguridad computacional ahí).
//!
//! Cada petición ETSI 014 declara un [`SecurityLevel`] que decide de qué grado
//! se le sirve. El default por-DKMS es [`SecurityLevel::QkdPrefer`].
//!
//! La política exacta vive en [`SecurityLevel::resolve_grade`] para que los tres
//! consumidores (resolución de demanda en el SDN, llenado del generador y
//! servicio de la petición en el DKMS) compartan una única fuente de verdad.

use serde::{Deserialize, Serialize};

/// Grado de seguridad de una clave (de transporte y, por herencia OTP, de la
/// clave de sesión que envuelve).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeyGrade {
    /// Camino solo-QKD: seguridad incondicional por enlace. Default: una clave
    /// sin grado explícito (p.ej. reporte de demanda antiguo) se trata como QKD.
    #[default]
    Qkd,
    /// Camino con ≥1 enlace PQC: seguridad computacional en ese salto.
    Pqc,
}

impl KeyGrade {
    /// Token estable usado en logs y claves de mapa.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyGrade::Qkd => "qkd",
            KeyGrade::Pqc => "pqc",
        }
    }

    /// Byte que viaja en el frame `wire` (debe coincidir con
    /// `wire::GRADE_QKD` = 0 / `wire::GRADE_PQC` = 1).
    pub fn wire_byte(self) -> u8 {
        match self {
            KeyGrade::Qkd => 0,
            KeyGrade::Pqc => 1,
        }
    }
}

impl std::fmt::Display for KeyGrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Nivel de seguridad que una petición exige al camino de la clave.
///
/// Semántica **"solo conectividad"** para [`Self::QkdPrefer`]: si existe
/// cualquier camino solo-QKD entre origen y destino se usa QKD (aunque sea más
/// lento); PQC solo cuando NO hay camino QKD. No hay spillover dinámico a PQC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecurityLevel {
    /// Solo QKD. Si no hay camino QKD, la petición es insatisfacible.
    StrictQkd,
    /// QKD si hay camino QKD; PQC solo como respaldo de conectividad. Default.
    #[default]
    QkdPrefer,
    /// Cualquiera; prefiere PQC para conservar el material QKD (más escaso).
    NoWorry,
}

/// Resultado de aplicar un [`SecurityLevel`] a la conectividad de un par.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradeResolution {
    /// Servir/generar material de este grado.
    Serve(KeyGrade),
    /// No hay camino que satisfaga el nivel (p.ej. `strict_qkd` sin camino QKD).
    Unsatisfiable,
}

impl SecurityLevel {
    /// Token canónico (igual que la representación serde).
    pub fn as_str(self) -> &'static str {
        match self {
            SecurityLevel::StrictQkd => "strict_qkd",
            SecurityLevel::QkdPrefer => "qkd_prefer",
            SecurityLevel::NoWorry => "no_worry",
        }
    }

    /// Parsea el token de una petición (campo `security_level` de las
    /// extensions ETSI 014). Sin dependencias de `serde_json` para que el
    /// tipo viva en `common` sin arrastrarlo.
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "strict_qkd" => Some(SecurityLevel::StrictQkd),
            "qkd_prefer" => Some(SecurityLevel::QkdPrefer),
            "no_worry" => Some(SecurityLevel::NoWorry),
            _ => None,
        }
    }

    /// Orden de preferencia de grados al SERVIR una petición desde los buffers
    /// `(peer, grade)` del DKMS: se popea del primer grado con claves.
    ///
    /// * `strict_qkd` → solo `[Qkd]` (jamás PQC).
    /// * `qkd_prefer` → `[Qkd, Pqc]`: QKD primero; en un par QKD-conexo el
    ///   buffer PQC está vacío, así que nunca degrada a PQC por "solo
    ///   conectividad"; en un par inconexo el QKD está vacío y sirve PQC.
    /// * `no_worry`   → `[Pqc, Qkd]`: prefiere PQC para conservar las QKD.
    pub fn serve_pref(self) -> &'static [KeyGrade] {
        match self {
            SecurityLevel::StrictQkd => &[KeyGrade::Qkd],
            SecurityLevel::QkdPrefer => &[KeyGrade::Qkd, KeyGrade::Pqc],
            SecurityLevel::NoWorry => &[KeyGrade::Pqc, KeyGrade::Qkd],
        }
    }

    /// Decide el grado a servir/generar dado qué caminos existen para el par.
    /// Fuente única de la tabla del diseño:
    ///
    /// | nivel        | qkd & pqc | solo qkd | solo pqc | ninguno |
    /// |--------------|-----------|----------|----------|---------|
    /// | `strict_qkd` | Qkd       | Qkd      | —error   | —error  |
    /// | `qkd_prefer` | Qkd       | Qkd      | Pqc      | —error  |
    /// | `no_worry`   | Pqc       | Qkd      | Pqc      | —error  |
    pub fn resolve_grade(self, qkd_available: bool, pqc_available: bool) -> GradeResolution {
        use GradeResolution::{Serve, Unsatisfiable};
        match self {
            SecurityLevel::StrictQkd => {
                if qkd_available {
                    Serve(KeyGrade::Qkd)
                } else {
                    Unsatisfiable
                }
            }
            SecurityLevel::QkdPrefer => {
                if qkd_available {
                    Serve(KeyGrade::Qkd)
                } else if pqc_available {
                    Serve(KeyGrade::Pqc)
                } else {
                    Unsatisfiable
                }
            }
            SecurityLevel::NoWorry => {
                // Prefiere PQC para conservar el material QKD.
                if pqc_available {
                    Serve(KeyGrade::Pqc)
                } else if qkd_available {
                    Serve(KeyGrade::Qkd)
                } else {
                    Unsatisfiable
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GradeResolution::{Serve, Unsatisfiable};

    #[test]
    fn default_level_is_qkd_prefer() {
        assert_eq!(SecurityLevel::default(), SecurityLevel::QkdPrefer);
    }

    #[test]
    fn security_level_serde_roundtrips_snake_case() {
        for (lvl, tok) in [
            (SecurityLevel::StrictQkd, "\"strict_qkd\""),
            (SecurityLevel::QkdPrefer, "\"qkd_prefer\""),
            (SecurityLevel::NoWorry, "\"no_worry\""),
        ] {
            assert_eq!(serde_json::to_string(&lvl).unwrap(), tok);
            assert_eq!(
                serde_json::from_str::<SecurityLevel>(tok).unwrap(),
                lvl
            );
            // from_token coincide con la representación serde (sin comillas).
            assert_eq!(SecurityLevel::from_token(lvl.as_str()), Some(lvl));
        }
        assert_eq!(SecurityLevel::from_token("bogus"), None);
    }

    #[test]
    fn key_grade_serde_is_lowercase() {
        assert_eq!(serde_json::to_string(&KeyGrade::Qkd).unwrap(), "\"qkd\"");
        assert_eq!(serde_json::to_string(&KeyGrade::Pqc).unwrap(), "\"pqc\"");
        assert_eq!(
            serde_json::from_str::<KeyGrade>("\"pqc\"").unwrap(),
            KeyGrade::Pqc
        );
    }

    #[test]
    fn strict_qkd_needs_a_qkd_path() {
        assert_eq!(
            SecurityLevel::StrictQkd.resolve_grade(true, true),
            Serve(KeyGrade::Qkd)
        );
        assert_eq!(
            SecurityLevel::StrictQkd.resolve_grade(true, false),
            Serve(KeyGrade::Qkd)
        );
        // Sin camino QKD: insatisfacible aunque haya PQC.
        assert_eq!(
            SecurityLevel::StrictQkd.resolve_grade(false, true),
            Unsatisfiable
        );
        assert_eq!(
            SecurityLevel::StrictQkd.resolve_grade(false, false),
            Unsatisfiable
        );
    }

    #[test]
    fn qkd_prefer_is_connectivity_only() {
        // Hay camino QKD → QKD, sin spillover a PQC aunque exista.
        assert_eq!(
            SecurityLevel::QkdPrefer.resolve_grade(true, true),
            Serve(KeyGrade::Qkd)
        );
        // Sin camino QKD → PQC.
        assert_eq!(
            SecurityLevel::QkdPrefer.resolve_grade(false, true),
            Serve(KeyGrade::Pqc)
        );
        assert_eq!(
            SecurityLevel::QkdPrefer.resolve_grade(false, false),
            Unsatisfiable
        );
    }

    #[test]
    fn no_worry_prefers_pqc_to_conserve_qkd() {
        // Ambos disponibles → PQC (conserva QKD).
        assert_eq!(
            SecurityLevel::NoWorry.resolve_grade(true, true),
            Serve(KeyGrade::Pqc)
        );
        // Solo QKD → QKD (no hay otra).
        assert_eq!(
            SecurityLevel::NoWorry.resolve_grade(true, false),
            Serve(KeyGrade::Qkd)
        );
        assert_eq!(
            SecurityLevel::NoWorry.resolve_grade(false, false),
            Unsatisfiable
        );
    }
}
