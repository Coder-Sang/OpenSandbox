// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use opensandbox::{ConnectionConfig, Error, SandboxManager};
use opensandbox_code_interpreter::{CodeInterpreter, CodeInterpreterOptions, RunCodeOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> opensandbox::Result<()> {
    let id = std::env::var("SANDBOX_ID").map_err(|_| {
        Error::InvalidConfig("Set SANDBOX_ID to an existing interpreter sandbox".into())
    })?;
    let manager = SandboxManager::new(ConnectionConfig::builder().build()?)?;
    let interpreter = CodeInterpreter::create(
        manager.connect(&id).await?,
        CodeInterpreterOptions::default(),
    )
    .await?;
    let result = interpreter
        .codes()
        .run("print('Hello from Rust')", RunCodeOptions::default())
        .await?;
    for output in result.logs.stdout {
        print!("{}", output.text);
    }
    if let Some(error) = result.error {
        return Err(Error::Protocol(format!(
            "Kernel execution failed: {}",
            error.evalue
        )));
    }
    // Attaching does not transfer ownership. Leave the existing sandbox alive.
    Ok(())
}
