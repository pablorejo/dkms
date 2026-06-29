//! Resolución del [`SecurityLevel`] de una petición ETSI 014.
//!
//! El nivel viaja en los campos extension del estándar:
//!   * `extension_mandatory` → el KME DEBE honrarlo o rechazar (4xx). Un
//!     `security_level` desconocido aquí es un error.
//!   * `extension_optional`  → best-effort; un valor desconocido se ignora y
//!     se cae al default configurado.
//!
//! Formato: un objeto con la clave `security_level`, p.ej.
//! `{"security_level": "strict_qkd"}`.
//!
//! El default (cuando la petición no especifica) lo decide el llamador: el
//! override por-peer de `PeerCfg` si existe, si no el global del `DkmsConfig`.

use common::security::SecurityLevel;
use etsi::v014::Etsi014KeyRequest;
use serde_json::{Map, Value};

/// Clave de extension que transporta el nivel de seguridad.
const EXT_KEY: &str = "security_level";

/// Busca `security_level` en una lista de extensions (la primera aparición).
fn find_in(ext: &Option<Vec<Map<String, Value>>>) -> Option<&str> {
    ext.as_ref()?
        .iter()
        .find_map(|m| m.get(EXT_KEY).and_then(Value::as_str))
}

/// Nivel pedido **explícitamente** por la petición (o `None` si no especifica).
///
/// * Si `extension_mandatory` trae `security_level`: debe ser un token válido
///   (`strict_qkd`/`qkd_prefer`/`no_worry`) o se devuelve `Err` → el handler
///   responde 4xx (extension obligatoria no soportada).
/// * Si solo `extension_optional` lo trae y es válido: `Some(ese nivel)`.
/// * Si no aparece (o el optional es inválido): `None` → el llamador aplica su
///   default (por-peer o global).
pub fn requested(req: &Etsi014KeyRequest) -> Result<Option<SecurityLevel>, String> {
    if let Some(tok) = find_in(&req.extension_mandatory) {
        return SecurityLevel::from_token(tok)
            .map(Some)
            .ok_or_else(|| format!("unsupported mandatory security_level: {tok:?}"));
    }
    if let Some(tok) = find_in(&req.extension_optional) {
        if let Some(lvl) = SecurityLevel::from_token(tok) {
            return Ok(Some(lvl));
        }
        // optional + desconocido → ignorar y caer al default del llamador.
    }
    Ok(None)
}

/// Como [`requested`] pero sustituyendo `None` por `default`.
pub fn resolve(req: &Etsi014KeyRequest, default: SecurityLevel) -> Result<SecurityLevel, String> {
    Ok(requested(req)?.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(level: &str) -> Vec<Map<String, Value>> {
        let mut m = Map::new();
        m.insert(EXT_KEY.into(), Value::String(level.into()));
        vec![m]
    }

    fn req() -> Etsi014KeyRequest {
        Etsi014KeyRequest::default()
    }

    #[test]
    fn no_extension_uses_default() {
        let r = req();
        assert_eq!(
            resolve(&r, SecurityLevel::QkdPrefer).unwrap(),
            SecurityLevel::QkdPrefer
        );
        // El default es lo que decida el llamador (p.ej. config por-peer).
        assert_eq!(
            resolve(&r, SecurityLevel::NoWorry).unwrap(),
            SecurityLevel::NoWorry
        );
    }

    #[test]
    fn mandatory_strict_qkd_is_honored() {
        let mut r = req();
        r.extension_mandatory = Some(ext("strict_qkd"));
        assert_eq!(
            resolve(&r, SecurityLevel::QkdPrefer).unwrap(),
            SecurityLevel::StrictQkd
        );
    }

    #[test]
    fn optional_level_overrides_default() {
        let mut r = req();
        r.extension_optional = Some(ext("no_worry"));
        assert_eq!(
            resolve(&r, SecurityLevel::QkdPrefer).unwrap(),
            SecurityLevel::NoWorry
        );
    }

    #[test]
    fn mandatory_takes_precedence_over_optional() {
        let mut r = req();
        r.extension_mandatory = Some(ext("strict_qkd"));
        r.extension_optional = Some(ext("no_worry"));
        assert_eq!(
            resolve(&r, SecurityLevel::QkdPrefer).unwrap(),
            SecurityLevel::StrictQkd
        );
    }

    #[test]
    fn unknown_mandatory_is_rejected() {
        let mut r = req();
        r.extension_mandatory = Some(ext("ultra_secret"));
        assert!(resolve(&r, SecurityLevel::QkdPrefer).is_err());
    }

    #[test]
    fn unknown_optional_falls_back_to_default() {
        let mut r = req();
        r.extension_optional = Some(ext("ultra_secret"));
        assert_eq!(
            resolve(&r, SecurityLevel::QkdPrefer).unwrap(),
            SecurityLevel::QkdPrefer
        );
    }
}
