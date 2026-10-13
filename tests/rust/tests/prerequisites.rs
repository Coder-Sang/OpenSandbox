// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Fail-closed runtime checks for the runner's actual isolation workload.
use opensandbox::*;
use std::{env, error::Error as StdError, time::Duration};

type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;

fn require(condition: bool, message: &str) -> Result {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

#[tokio::test]
#[ignore = "requires dedicated Docker runtime with isolation permissions and overlayfs"]
async fn docker_isolation_runtime_prerequisites() -> Result {
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(env::var("OPENSANDBOX_TEST_DOMAIN")?)
            .api_key(env::var("OPENSANDBOX_TEST_API_KEY").unwrap_or_default())
            .use_server_proxy(
                env::var("OPENSANDBOX_TEST_USE_SERVER_PROXY").as_deref() == Ok("true"),
            )
            .ready_timeout(Duration::from_secs(180))
            .build()?,
    )?;
    let mut options = CreateSandboxOptions::image(env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")?);
    options.timeout = Some(Duration::from_secs(300));
    options
        .extensions
        .insert("bootstrap.execd.isolation".into(), "enable".into());
    options
        .metadata
        .insert("rust-e2e-run".into(), env::var("OPENSANDBOX_RUST_RUN_ID")?);
    let sandbox = match manager.create(options).await {
        Ok(sandbox) => sandbox,
        Err(error) => {
            if let Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } =
                &error
            {
                manager.kill(sandbox_id).await?;
            }
            return Err(error.into());
        }
    };
    let outcome: Result = async {
        let facts = sandbox.commands().run(
            "uname -r; awk '/^(CapEff|CapBnd|NoNewPrivs|Seccomp):/{print}' /proc/self/status; stat -c 'gate-owner=%u:%g gate-mode=%a' /opt/opensandbox/opensandbox-session-gate; grep -w overlay /proc/filesystems",
            Default::default(),
        ).await?;
        for line in &facts.logs.stdout {
            eprint!("{}", line.text);
        }
        require(facts.exit_code == Some(0), "runner requires the trusted workload gate and Linux overlayfs")?;
        let service = sandbox.isolated();
        let caps = service.capabilities().await?;
        eprintln!("isolation prerequisites: {}", serde_json::json!({
            "available": caps.available,
            "setpriv_available": caps.setpriv_available,
            "userns_available": caps.userns_available,
            "phase2_commit_supported": caps.commit_supported,
        }));
        require(caps.available == Some(true) && caps.setpriv_available == Some(true),
            "bwrap namespace smoke test failed: check bootstrap.execd.isolation=enable, CAP_SYS_ADMIN, container security policy and trusted gate")?;
        let setup = sandbox.commands().run("mkdir -p /tmp/rust-isolation-preflight", Default::default()).await?;
        require(setup.exit_code == Some(0), "prerequisite workspace setup failed")?;
        let session = service.create(CreateIsolatedSessionOptions {
            workspace: IsolatedWorkspaceSpec {
                path: "/tmp/rust-isolation-preflight".into(),
                mode: Some(WorkspaceMode::Overlay),
                ..Default::default()
            },
            ..Default::default()
        }).await?;
        let checked: Result = async {
            let run = session.run("printf preflight > /tmp/rust-isolation-preflight/value.txt; cat /tmp/rust-isolation-preflight/value.txt", Default::default()).await?;
            require(run.exit_code == Some(0), "actual isolated overlay execution failed")?;
            require(run.logs.stdout.iter().map(|line| line.text.as_str()).collect::<String>() == "preflight",
                "actual overlay read/write mismatch")?;
            require(session.files().read_text("/tmp/rust-isolation-preflight/value.txt").await? == "preflight",
                "scoped overlay filesystem prerequisite failed")?;
            Ok(())
        }.await;
        let deleted = session.delete().await;
        match (checked, deleted) {
            (Err(error), Err(cleanup)) => return Err(format!("prerequisite={error}; session cleanup={cleanup}").into()),
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), Err(cleanup)) => return Err(cleanup.into()),
            (Ok(()), Ok(())) => (),
        }
        require(sandbox.files().read_text("/tmp/rust-isolation-preflight/value.txt").await.is_err(),
            "overlay prerequisite changed the lower filesystem")?;
        Ok(())
    }.await;
    let cleanup = sandbox.kill().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(format!("prerequisite={error}; sandbox cleanup={cleanup}").into())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}
