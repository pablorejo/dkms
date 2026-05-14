//! Layered config loading shared by every module.
//!
//! Resolution order (later wins):
//!   1. `config/default.toml` next to the binary or in `CONFIG_DIR`
//!   2. `config/local.toml`   (optional, gitignored, for dev overrides)
//!   3. environment variables (prefix derived from the module name)

use std::path::PathBuf;

use config::{Config, Environment, File, FileFormat};
use serde::de::DeserializeOwned;
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
        .add_source(File::from(default_path).format(FileFormat::Toml).required(false))
        .add_source(File::from(local_path).format(FileFormat::Toml).required(false))
        .add_source(
            Environment::with_prefix(&env_prefix)
                .separator("__")
                .try_parsing(true),
        )
        .build()?;

    Ok(cfg.try_deserialize()?)
}
