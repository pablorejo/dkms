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

    #[derive(Debug, Deserialize)]
    struct Nested {
        capacity: u64,
        batch: u64,
    }

    #[derive(Debug, Deserialize)]
    struct Layered {
        name: String,
        buffer: Nested,
    }

    /// Lo que hace config-rs 0.14 de verdad, fijado: una variable de
    /// entorno que pone UN campo de una sección anidada se MEZCLA con la
    /// sección del fichero — el otro campo sobrevive. (CLAUDE.md decía lo
    /// contrario durante meses: «sustituye la sección entera». Medido aquí
    /// el 2026-08-30 y corregido allí.) Si config-rs cambiara de criterio,
    /// este test lo dice antes que un despliegue.
    #[test]
    fn an_env_override_of_one_nested_field_merges_into_the_section() {
        let dir = std::env::temp_dir().join(format!("cfg_layers_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("default.toml"),
            "name = \"x\"\n[buffer]\ncapacity = 4096\nbatch = 256\n",
        )
        .unwrap();
        let module = format!("cfgtest{}", std::process::id());
        let prefix = module.to_ascii_uppercase();
        let var = format!("{prefix}__BUFFER__CAPACITY");
        std::env::set_var("CONFIG_DIR", &dir);

        // Sin env: las capas de fichero se leen enteras.
        std::env::remove_var(&var);
        let base: Layered = load_config(&module).expect("carga base");
        assert_eq!(base.name, "x");
        assert_eq!((base.buffer.capacity, base.buffer.batch), (4096, 256));

        // Con env sobre un solo campo anidado: se mezcla, no sustituye.
        std::env::set_var(&var, "8192");
        let overridden: Result<Layered, _> = load_config(&module);
        std::env::remove_var(&var);
        std::env::remove_var("CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = overridden.expect("con el override la sección se mezcla, no se pierde `batch`");
        assert_eq!(
            (cfg.buffer.capacity, cfg.buffer.batch),
            (8192, 256),
            "capacity viene del entorno y batch sigue viniendo del fichero"
        );
    }
}
