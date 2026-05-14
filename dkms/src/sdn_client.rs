//! gRPC client to the SDN.
//!
//! Wraps `tonic`-generated client with our local types and retry semantics.
//! Used for: route computation, admission checks, topology subscriptions,
//! metric push.

use std::sync::Arc;

use common::ipc::grpc::{connect, DialOpts};
use common::proto::sdn::v1::sdn_control_client::SdnControlClient;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{DkmsError, Result};

pub struct SdnClient {
    url: String,
    inner: Arc<parking_lot::Mutex<Option<SdnControlClient<Channel>>>>,
}

impl SdnClient {
    pub fn new(url: String) -> Self {
        Self { url, inner: Arc::new(parking_lot::Mutex::new(None)) }
    }

    pub async fn get(&self) -> Result<SdnControlClient<Channel>> {
        if let Some(c) = self.inner.lock().as_ref() {
            return Ok(c.clone());
        }
        let ch = connect(&self.url, DialOpts::default()).await.map_err(|e| {
            warn!(url = %self.url, error = %e, "sdn dial failed");
            DkmsError::Upstream(format!("sdn: {e}"))
        })?;
        let c = SdnControlClient::new(ch);
        *self.inner.lock() = Some(c.clone());
        Ok(c)
    }
}
