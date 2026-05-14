//! gRPC client to a (typically co-located) QKC. Same pattern as `SdnClient`.

use std::sync::Arc;

use common::ipc::grpc::{connect, DialOpts};
use common::proto::qkc::v1::qkc_control_client::QkcControlClient;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{DkmsError, Result};

pub struct QkcClient {
    url: String,
    inner: Arc<parking_lot::Mutex<Option<QkcControlClient<Channel>>>>,
}

impl QkcClient {
    pub fn new(url: String) -> Self {
        Self { url, inner: Arc::new(parking_lot::Mutex::new(None)) }
    }

    pub async fn get(&self) -> Result<QkcControlClient<Channel>> {
        if let Some(c) = self.inner.lock().as_ref() {
            return Ok(c.clone());
        }
        let ch = connect(&self.url, DialOpts::default()).await.map_err(|e| {
            warn!(url = %self.url, error = %e, "qkc dial failed");
            DkmsError::Upstream(format!("qkc: {e}"))
        })?;
        let c = QkcControlClient::new(ch);
        *self.inner.lock() = Some(c.clone());
        Ok(c)
    }
}
