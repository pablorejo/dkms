//! Helpers around tonic.
//!
//! - [`DialOpts`] is the baseline every gRPC client in the workspace shares
//!   (connect/request timeouts, HTTP/2 keepalive, `TCP_NODELAY`, optional
//!   TLS).
//! - [`connect`] builds a `tonic::transport::Channel` against another module
//!   from those options.
//!
//! Servers are built where they live (`grpc_server.rs` in each module) with
//! `tonic::transport::Server` directly; there is no shared wrapper.

use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::error::CommonError;

/// Default keepalive / timeout values used by every cross-module client.
pub struct DialOpts {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub keepalive_interval: Duration,
    pub keepalive_timeout: Duration,
    pub tls: Option<ClientTlsConfig>,
}

impl Default for DialOpts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(15),
            keepalive_timeout: Duration::from_secs(5),
            tls: None,
        }
    }
}

/// Build a long-lived `Channel` to another module's gRPC endpoint.
pub async fn connect(url: &str, opts: DialOpts) -> Result<Channel, CommonError> {
    let mut ep = Endpoint::from_shared(url.to_owned())
        .map_err(|e| CommonError::invalid_arg(format!("bad grpc url {url}: {e}")))?
        .connect_timeout(opts.connect_timeout)
        .timeout(opts.request_timeout)
        .http2_keep_alive_interval(opts.keepalive_interval)
        .keep_alive_timeout(opts.keepalive_timeout)
        .tcp_nodelay(true);

    if let Some(tls) = opts.tls {
        ep = ep.tls_config(tls)?;
    }

    Ok(ep.connect_lazy())
}
