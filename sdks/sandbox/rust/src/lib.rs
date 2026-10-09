// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Async Rust client for OpenSandbox.
//!
//! Dropping a client releases local resources only. Call [`Sandbox::kill`]
//! explicitly to destroy the remote sandbox.

pub mod api;
#[doc(hidden)]
pub mod code_execution;
pub mod commands;
pub mod config;
pub mod diagnostics;
pub mod egress;
pub mod error;
pub mod files;
pub mod isolated;
pub mod lifecycle;
pub mod metrics;
pub mod models;
mod observability;
pub mod pool;
pub mod sandbox;
mod stream;
mod transport;

pub use commands::{Commands, ExecutionStream};
pub use config::{ConnectionConfig, ConnectionConfigBuilder};
pub use diagnostics::{DiagnosticContent, DiagnosticKind, DiagnosticPayload};
pub use error::{Error, Result};
pub use files::Files;
pub use isolated::*;
pub use lifecycle::{EndpointOptions, SandboxManager};
pub use metrics::MetricsStream;
pub use models::execd::*;
pub use models::lifecycle::*;
pub use sandbox::Sandbox;

/// SDK version used in the HTTP user agent.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
