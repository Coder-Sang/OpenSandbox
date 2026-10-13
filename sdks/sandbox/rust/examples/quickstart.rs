// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use opensandbox::{
    ConnectionConfig, CreateSandboxOptions, Error, RunCommandOptions, Sandbox, SandboxManager,
};

async fn scenario(sandbox: &Sandbox) -> opensandbox::Result<()> {
    let result = sandbox
        .commands()
        .run("echo 'Hello from Rust'", RunCommandOptions::default())
        .await?;
    for output in &result.logs.stdout {
        print!("{}", output.text);
    }
    if let Some(error) = result.error {
        return Err(Error::Protocol(format!(
            "sandbox command failed: {}",
            error.evalue
        )));
    }
    sandbox
        .files()
        .write_text("/tmp/rust-sdk.txt", "Hello, OpenSandbox!\n")
        .await?;
    println!("{}", sandbox.files().read_text("/tmp/rust-sdk.txt").await?);
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> opensandbox::Result<()> {
    // Reads OPEN_SANDBOX_DOMAIN and OPEN_SANDBOX_API_KEY when set.
    let manager = SandboxManager::new(ConnectionConfig::builder().build()?)?;
    let sandbox = match manager
        .create(CreateSandboxOptions::image("alpine:3.21"))
        .await
    {
        Ok(sandbox) => sandbox,
        Err(error @ (Error::ReadyTimeout { .. } | Error::SandboxSetup { .. })) => {
            let id = match &error {
                Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } => {
                    sandbox_id
                }
                _ => unreachable!(),
            };
            if let Err(cleanup) = manager.kill(id).await {
                eprintln!("cleanup failed for sandbox {id}: {cleanup}");
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let result = scenario(&sandbox).await;
    let cleanup = sandbox.kill().await;
    if let Err(error) = &cleanup {
        eprintln!("cleanup failed for sandbox {}: {error}", sandbox.id());
    }
    result?;
    cleanup
}
