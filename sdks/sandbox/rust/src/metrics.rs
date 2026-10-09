// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{models::execd::SandboxMetrics, transport::Transport, Result};
use reqwest::Method;

#[derive(Clone)]
pub struct Metrics {
    transport: Transport,
}
impl Metrics {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
    }
    pub async fn get(&self) -> Result<SandboxMetrics> {
        self.transport
            .json(Method::GET, self.transport.url("/metrics")?, None::<&()>)
            .await
    }
}
