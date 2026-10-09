// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Async Rust client for OpenSandbox.
//!
//! Dropping a client releases local resources only. Call [`Sandbox::kill`]
//! explicitly to destroy the remote sandbox.

pub mod api;
pub mod commands;
pub mod config;
pub mod egress;
pub mod error;
pub mod files;
pub mod lifecycle;
pub mod metrics;
pub mod models;
pub mod sandbox;
mod stream;
mod transport;

pub use commands::{Commands, ExecutionStream};
pub use config::{ConnectionConfig, ConnectionConfigBuilder};
pub use error::{Error, Result};
pub use files::Files;
pub use lifecycle::SandboxManager;
pub use models::execd::*;
pub use models::lifecycle::*;
pub use sandbox::Sandbox;

/// SDK version used in the HTTP user agent.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
