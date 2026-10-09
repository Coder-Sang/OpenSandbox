// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

/// Errors retain server status and request IDs without displaying credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid SDK configuration: {0}")]
    InvalidConfig(String),
    #[error("HTTP transport failed")]
    Transport(#[from] reqwest::Error),
    #[error("JSON decoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unexpected protocol response: {0}")]
    Protocol(String),
    #[error("API request failed (HTTP {status}): {message}")]
    Api {
        status: u16,
        code: Option<String>,
        message: String,
        request_id: Option<String>,
    },
    #[error("sandbox {sandbox_id} was created but initialization failed: {source}. Explicit cleanup is required")]
    SandboxSetup {
        sandbox_id: String,
        #[source]
        source: Box<Error>,
    },
    #[error("sandbox {sandbox_id} did not become ready within {timeout:?}. The sandbox still exists and must be explicitly cleaned up")]
    ReadyTimeout {
        sandbox_id: String,
        timeout: Duration,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
