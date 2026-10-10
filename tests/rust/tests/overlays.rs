// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::*;
use std::{env, error::Error as StdError, time::Duration};

type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;

fn require(ok: bool, message: &str) -> Result {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn mounts() -> CreateIsolatedSessionAdvancedOptions {
    CreateIsolatedSessionAdvancedOptions {
        overlays: vec![
            IsolatedOverlaySpec {
                path: "/tmp/rust-overlays/persistent".into(),
                mode: Some(OverlayMode::Overlay),
                persist: Some(true),
                ..Default::default()
            },
            IsolatedOverlaySpec {
                path: "/tmp/rust-overlays/ephemeral".into(),
                mode: Some(OverlayMode::Overlay),
                persist: Some(false),
                ..Default::default()
            },
            IsolatedOverlaySpec {
                path: "/tmp/rust-overlays/direct".into(),
                mode: Some(OverlayMode::Rw),
                ..Default::default()
            },
            IsolatedOverlaySpec {
                path: "/tmp/rust-overlays/readonly".into(),
                mode: Some(OverlayMode::Ro),
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}
#[test]
fn overlay_fixture_covers_persist_and_bind_modes_without_workspace() {
    let options = mounts();
    assert!(options.workspace.is_none());
    assert_eq!(options.overlays.len(), 4);
    assert_eq!(options.overlays[0].persist, Some(true));
    assert_eq!(options.overlays[1].persist, Some(false));
    assert!(options.overlays[2..]
        .iter()
        .all(|entry| entry.persist.is_none()));
}

#[tokio::test]
#[ignore = "dedicated execd runtime with namespace and overlay support required"]
async fn docker_overlays_only_persistent_ephemeral_rw_ro() -> Result {
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
    options.timeout = Some(Duration::from_secs(600));
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
        let setup = sandbox.commands().run("mkdir -p /tmp/rust-overlays/{persistent,ephemeral,direct,readonly}; printf lower > /tmp/rust-overlays/ephemeral/lower.txt", Default::default()).await?;
        require(setup.exit_code == Some(0), "overlay fixture setup failed")?;
        let service = sandbox.isolated();
        let capabilities = service.capabilities().await?;
        require(capabilities.setpriv_available.unwrap_or(false), "isolated namespace prerequisites unavailable, not proof of a missing setpriv executable")?;
        let session = service.create_advanced(mounts()).await?;
        let execution: Result = async {
            let written = session.run("printf persistent > /tmp/rust-overlays/persistent/value.txt; printf ephemeral > /tmp/rust-overlays/ephemeral/value.txt; printf direct > /tmp/rust-overlays/direct/value.txt", Default::default()).await?;
            require(written.exit_code == Some(0), "multi-overlay write failed")?;
            require(session.files().read_text("/tmp/rust-overlays/persistent/value.txt").await? == "persistent", "persistent upper missing from scoped files")?;
            let ephemeral = session.run("cat /tmp/rust-overlays/ephemeral/value.txt", Default::default()).await?;
            require(ephemeral.logs.stdout.iter().map(|item| item.text.as_str()).collect::<String>() == "ephemeral", "ephemeral upper lost between runs")?;
            require(session.files().read_text("/tmp/rust-overlays/ephemeral/lower.txt").await? == "lower", "ephemeral lower unavailable to scoped files")?;
            require(session.files().read_text("/tmp/rust-overlays/ephemeral/value.txt").await.is_err(), "files API unexpectedly exposed ephemeral upper")?;
            require(session.files().write_text("/tmp/rust-overlays/ephemeral/denied.txt", "denied").await.is_err(), "files API wrote into ephemeral overlay")?;
            require(sandbox.files().read_text("/tmp/rust-overlays/direct/value.txt").await? == "direct", "rw bind did not modify lower")?;
            let readonly = session.run("printf forbidden > /tmp/rust-overlays/readonly/value.txt", Default::default()).await?;
            require(readonly.error.is_some() || readonly.exit_code.is_some_and(|code| code != 0), "ro bind accepted a write")?;
            Ok(())
        }.await;
        let deletion = session.delete().await;
        match (execution, deletion) {
            (Err(error), Err(cleanup)) => return Err(format!("scenario={error}; session cleanup={cleanup}").into()),
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), Err(cleanup)) => return Err(cleanup.into()),
            (Ok(()), Ok(())) => (),
        }
        require(sandbox.files().read_text("/tmp/rust-overlays/persistent/value.txt").await.is_err(), "persistent overlay changed the lower")?;
        require(sandbox.files().read_text("/tmp/rust-overlays/ephemeral/value.txt").await.is_err(), "ephemeral overlay changed the lower")?;
        let once = service.run_once_advanced("printf one-shot", mounts(), Default::default()).await?;
        let run = once.execution?;
        once.cleanup?;
        require(run.exit_code == Some(0) && run.logs.stdout.iter().map(|item| item.text.as_str()).collect::<String>() == "one-shot", "advanced run_once failed")?;
        require(service.get(&once.session_id).await.is_err(), "one-shot session survived cleanup")?;
        // An ephemeral first mount cannot hold background log storage.
        let ephemeral = service.create_advanced(CreateIsolatedSessionAdvancedOptions {
            overlays: vec![mounts().overlays[1].clone()], ..Default::default()
        }).await?;
        let denied = ephemeral.run_background("printf background", Default::default()).await.is_err();
        let deletion = ephemeral.delete().await;
        deletion?;
        require(denied, "ephemeral first overlay accepted background log storage")?;
        Ok(())
    }.await;
    match (outcome, sandbox.kill().await) {
        (Err(error), Err(cleanup)) => {
            Err(format!("scenario={error}; sandbox cleanup={cleanup}").into())
        }
        (outcome, Ok(())) => outcome,
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
    }
}
