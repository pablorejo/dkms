//! Tracing subscriber bootstrap.
//!
//! Call `logging::init("module_name")` at the top of every binary's main.
//! Reads `RUST_LOG` (default `info`) for the env filter and emits JSON to
//! stdout when `LOG_FORMAT=json`, otherwise human-readable pretty output.

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn init(service_name: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tonic=warn,h2=warn"));

    let json = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    let registry = tracing_subscriber::registry().with(filter);

    if json {
        registry
            .with(fmt::layer().json().with_current_span(false).with_target(true))
            .init();
    } else {
        registry
            .with(fmt::layer().with_target(true).with_thread_ids(false).compact())
            .init();
    }

    tracing::info!(service = service_name, "logging initialized");
}
