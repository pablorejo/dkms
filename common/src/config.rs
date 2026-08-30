//! Layered config loading shared by every module.
//!
//! Resolution order (later wins):
//!   1. `config/default.toml` next to the binary or in `CONFIG_DIR`
//!   2. `config/local.toml`   (optional, gitignored, for dev overrides)
//!   3. environment variables (prefix derived from the module name)

use std::path::PathBuf;

use config::{Config, Environment, File, FileFormat};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config build failed: {0}")]
    Build(#[from] config::ConfigError),

    #[error("config dir not found at {0:?}")]
    DirMissing(PathBuf),
}

/// Load a strongly-typed config struct.
///
/// `module_name` becomes the env-var prefix in SCREAMING_SNAKE_CASE.
/// Example: `load_config::<QkcConfig>("qkc")` reads `QKC_TCP_BIND` etc.
pub fn load_config<T: DeserializeOwned>(module_name: &str) -> Result<T, ConfigError> {
    let config_dir = std::env::var("CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config"));

    let default_path = config_dir.join("default.toml");
    let local_path = config_dir.join("local.toml");

    let env_prefix = module_name.to_ascii_uppercase();

    let cfg = Config::builder()
        .add_source(
            File::from(default_path)
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(
            File::from(local_path)
                .format(FileFormat::Toml)
                .required(false),
        )
        .add_source(
            Environment::with_prefix(&env_prefix)
                .separator("__")
                .try_parsing(true),
        )
        .build()?;

    Ok(cfg.try_deserialize()?)
}

/// Valor de configuración que es un secreto: PSKs de enlace, semillas de
/// firma. Se deserializa como la cadena que envuelve y se usa como `&str`
/// (`Deref`), pero su `Debug` no enseña el valor. Hace falta porque los
/// módulos hacen `info!(?cfg, "… starting")` al arrancar y `docker logs` es
/// el canal de diagnóstico documentado: con un `Debug` derivado, cada
/// `link_psk` acababa en claro en el log del contenedor.
///
/// No implementa `Display` a propósito: `%secreto` en un `info!` fallaría al
/// compilar en vez de filtrar el valor.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// El valor en claro. El nombre deja a la vista, en el llamador, que
    /// está sacando un secreto de su envoltorio.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for SecretString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SecretString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<[u8]> for SecretString {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, Serialize)]
    struct Cfg {
        name: String,
        psk: Option<SecretString>,
    }

    #[test]
    fn secret_string_is_transparent_for_serde_and_opaque_for_debug() {
        let cfg: Cfg = toml::from_str("name = \"a\"\npsk = \"s3cr3t\"\n").unwrap();
        let psk = cfg.psk.as_ref().unwrap();
        assert_eq!(psk.expose(), "s3cr3t");
        assert_eq!(cfg.psk.as_deref(), Some("s3cr3t"));
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("s3cr3t"), "el Debug filtra el secreto: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(toml::to_string(&cfg).unwrap().contains("psk = \"s3cr3t\""));
    }
}
