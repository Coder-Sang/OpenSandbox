// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Independent code interpreter facade. Dropping it never deletes a sandbox.

pub use opensandbox::code_execution::{CodeContext, Codes, CreateContextOptions, RunCodeOptions};
use opensandbox::metrics::Metrics;
use opensandbox::{Commands, Error, Files, Result, RunCommandOptions, Sandbox};
pub use opensandbox::{Execution, ExecutionEvent, ExecutionStream};
use std::{future::Future, time::Duration};

/// Prove that Jupyter accepts connections, rather than merely matching a process name.
pub const RUNTIME_CHECK_COMMAND: &str =
    "bash -c 'exec 3<>/dev/tcp/127.0.0.1/${JUPYTER_PORT:-44771}' && exit 0 || exit 1";

#[derive(Clone, Debug)]
pub struct CodeInterpreterOptions {
    pub skip_health_check: bool,
    /// Total budget, enclosing ping, command execution and polling sleeps.
    pub ready_timeout: Duration,
    pub polling_interval: Duration,
}
impl Default for CodeInterpreterOptions {
    fn default() -> Self {
        Self {
            skip_health_check: false,
            ready_timeout: Duration::from_secs(30),
            polling_interval: Duration::from_millis(200),
        }
    }
}

#[derive(Clone)]
pub struct CodeInterpreter {
    sandbox: Sandbox,
    codes: Codes,
}
impl CodeInterpreter {
    pub async fn create(sandbox: Sandbox, options: CodeInterpreterOptions) -> Result<Self> {
        validate_options(&options)?;
        let codes = sandbox.code_execution();
        let interpreter = Self { sandbox, codes };
        if !options.skip_health_check {
            interpreter
                .wait_until_ready(options.ready_timeout, options.polling_interval)
                .await?;
        }
        Ok(interpreter)
    }
    pub fn id(&self) -> &str {
        self.sandbox.id()
    }
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
    pub fn codes(&self) -> &Codes {
        &self.codes
    }
    pub fn commands(&self) -> Commands {
        self.sandbox.commands()
    }
    pub fn files(&self) -> Files {
        self.sandbox.files()
    }
    pub fn metrics(&self) -> Metrics {
        self.sandbox.metrics()
    }
    /// Single strict probe. Use wait_until_ready for a bounded readiness budget.
    pub async fn is_healthy(&self) -> bool {
        if self.sandbox.ping().await.is_err() {
            return false;
        }
        matches!(self.commands().run(RUNTIME_CHECK_COMMAND, RunCommandOptions::default()).await,
            Ok(execution) if execution.error.is_none() && execution.complete && execution.exit_code == Some(0))
    }
    pub async fn wait_until_ready(
        &self,
        budget: Duration,
        polling_interval: Duration,
    ) -> Result<()> {
        poll_ready(self.id(), budget, polling_interval, || self.is_healthy()).await
    }
}
fn validate_options(options: &CodeInterpreterOptions) -> Result<()> {
    if options.ready_timeout.is_zero()
        || options.polling_interval.is_zero()
        || std::time::Instant::now()
            .checked_add(options.ready_timeout)
            .is_none()
        || std::time::Instant::now()
            .checked_add(options.polling_interval)
            .is_none()
    {
        return Err(Error::InvalidConfig(
            "Readiness budget and polling interval must be positive".into(),
        ));
    }
    Ok(())
}
async fn poll_ready<F, Fut>(
    id: &str,
    budget: Duration,
    interval: Duration,
    mut probe: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    validate_options(&CodeInterpreterOptions {
        skip_health_check: false,
        ready_timeout: budget,
        polling_interval: interval,
    })?;
    tokio::time::timeout(budget, async {
        loop {
            if probe().await {
                return;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .map_err(|_| Error::ReadyTimeout {
        sandbox_id: id.into(),
        timeout: budget,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn readiness_budget_encloses_hung_probe_and_sleep() {
        let budget = Duration::from_millis(20);
        let start = tokio::time::Instant::now();
        assert!(matches!(
            poll_ready("id", budget, Duration::from_secs(5), || async {
                std::future::pending::<bool>().await
            })
            .await,
            Err(Error::ReadyTimeout { .. })
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(
            poll_ready("id", budget, Duration::from_secs(5), || async { false })
                .await
                .is_err()
        );
        assert!(
            poll_ready("id", budget, Duration::from_millis(1), || async { true })
                .await
                .is_ok()
        );
        assert!(
            poll_ready("id", Duration::ZERO, Duration::from_millis(1), || async {
                true
            })
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn runtime_probe_requires_a_listening_port() {
        // Execute the exact production command against a real local TCP listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let run = move || {
            std::process::Command::new("bash")
                .args(["-c", RUNTIME_CHECK_COMMAND])
                .env("JUPYTER_PORT", port.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(run());
        drop(listener);
        assert!(!run());
    }
}
