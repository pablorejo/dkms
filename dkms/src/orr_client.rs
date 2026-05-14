//! gRPC client to the ORR. Same pattern as `SdnClient`.

use std::sync::Arc;

use common::ipc::grpc::{connect, DialOpts};
use common::proto::orr::v1::orr_control_client::OrrControlClient;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{DkmsError, Result};

pub struct OrrClient {
    url: String,
    inner: Arc<parking_lot::Mutex<Option<OrrControlClient<Channel>>>>,
}

impl OrrClient {
    pub fn new(url: String) -> Self {
        Self { url, inner: Arc::new(parking_lot::Mutex::new(None)) }
    }

    pub async fn get(&self) -> Result<OrrControlClient<Channel>> {
        if let Some(c) = self.inner.lock().as_ref() {
            return Ok(c.clone());
        }
        let ch = connect(&self.url, DialOpts::default()).await.map_err(|e| {
            warn!(url = %self.url, error = %e, "orr dial failed");
            DkmsError::Upstream(format!("orr: {e}"))
        })?;
        let c = OrrControlClient::new(ch);
        *self.inner.lock() = Some(c.clone());
        Ok(c)
    }
}
