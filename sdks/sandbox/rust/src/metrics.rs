// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{models::execd::SandboxMetrics, transport::Transport, Result};
use futures_core::Stream;
use reqwest::Method;
use std::pin::Pin;

/// Dropping the stream closes the response body, not the remote sandbox.
pub type MetricsStream = Pin<Box<dyn Stream<Item = Result<SandboxMetrics>> + Send>>;

#[derive(Clone)]
pub struct Metrics {
    transport: Transport,
}
impl Metrics {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
    }
    /// Subscribe to one-second SSE metric updates until this stream is dropped.
    pub async fn watch(&self) -> Result<MetricsStream> {
        let response = self
            .transport
            .streaming_request(Method::GET, self.transport.url("/metrics/watch")?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?;
        Ok(crate::stream::json_events(
            Transport::checked(response).await?,
        ))
    }
    fn generated_client(&self) -> crate::api::execd_client::Client<'_> {
        crate::api::execd_client::Client::new(&self.transport)
    }
    pub async fn get(&self) -> Result<SandboxMetrics> {
        self.generated_client().get_metrics()?.json_as().await
    }
}
