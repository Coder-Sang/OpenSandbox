// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::*;
use std::{
    env,
    error::Error as StdError,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
fn manager() -> Result<SandboxManager> {
    Ok(SandboxManager::new(
        ConnectionConfig::builder()
            .domain(env::var("OPENSANDBOX_TEST_DOMAIN")?)
            .api_key(
                env::var("OPENSANDBOX_TEST_API_KEY").unwrap_or_else(|_| "kubernetes-e2e".into()),
            )
            .ready_timeout(Duration::from_secs(300))
            .build()?,
    )?)
}
async fn create(manager: &SandboxManager, options: CreateSandboxOptions) -> Result<Sandbox> {
    match manager.create(options).await {
        Ok(sandbox) => Ok(sandbox),
        Err(error) => {
            if let Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } =
                &error
            {
                if let Err(cleanup) = manager.kill(sandbox_id).await {
                    return Err(format!("creation={error}; cleanup={cleanup}").into());
                }
            }
            Err(error.into())
        }
    }
}
async fn state(manager: &SandboxManager, id: &str, expected: &str) -> Result {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let status = manager.get(id).await?.status;
            if status.state == expected {
                return Ok::<(), opensandbox::Error>(());
            }
            if status.state == "Failed" {
                return Err(opensandbox::Error::Protocol(format!(
                    "sandbox failed: {status:?}"
                )));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await??;
    Ok(())
}
fn signed_route_expiry(now: SystemTime) -> Result<SystemTime> {
    Ok(UNIX_EPOCH + Duration::from_secs(now.duration_since(UNIX_EPOCH)?.as_secs() + 120))
}
#[test]
fn signed_route_expiry_uses_whole_unix_seconds() -> Result {
    let now = UNIX_EPOCH + Duration::new(1_000, 123_456_789);
    let expiry = signed_route_expiry(now)?.duration_since(UNIX_EPOCH)?;
    assert_eq!(expiry, Duration::from_secs(1_120));
    Ok(())
}
async fn secure_signed_route(sandbox: &Sandbox) -> Result {
    let endpoint = sandbox
        .endpoint_with_options(
            44772,
            EndpointOptions {
                use_server_proxy: Some(false),
                expires_at: Some(signed_route_expiry(SystemTime::now())?),
            },
        )
        .await?;
    let base = if endpoint.endpoint.starts_with("http") {
        endpoint.endpoint.clone()
    } else {
        format!("http://{}", endpoint.endpoint)
    };
    let mut url = reqwest::Url::parse(&base)?;
    url.set_path(&format!("{}/ping", url.path().trim_end_matches('/')));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client.get(url.clone());
    for (name, value) in &endpoint.headers {
        request = request.header(name, value);
    }
    if !request
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .status()
        .is_success()
    {
        return Err("signed secure route was rejected".into());
    }
    // URI signed routes put the signature immediately before the request path.
    let mut parts: Vec<_> = url.path().split('/').map(str::to_owned).collect();
    if parts.len() < 5 {
        return Err("signed route test requires URI gateway routing".into());
    }
    let signature_index = parts.len() - 2;
    parts[signature_index] = "invalid-rust-e2e-signature".into();
    url.set_path(&parts.join("/"));
    let mut request = client.get(url);
    for (name, value) in &endpoint.headers {
        request = request.header(name, value);
    }
    let denied = request
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .status();
    if denied != reqwest::StatusCode::FORBIDDEN
        && denied != reqwest::StatusCode::UNAUTHORIZED
        && denied != reqwest::StatusCode::BAD_REQUEST
    {
        return Err(format!("tampered signed route not denied: {denied}").into());
    }
    let execution = sandbox
        .commands()
        .run("printf 'secure-route'", Default::default())
        .await?;
    if execution.exit_code != Some(0) {
        return Err("secure route command failed".into());
    }
    Ok(())
}
#[tokio::test]
#[ignore = "dedicated signed ingress runtime required, no snapshot support needed"]
async fn k8s_secure_signed_route() -> Result {
    let manager = manager()?;
    let mut options = CreateSandboxOptions::image(env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")?);
    options.secure_access = true;
    options
        .metadata
        .insert("rust-e2e-run".into(), env::var("OPENSANDBOX_RUST_RUN_ID")?);
    let sandbox = create(&manager, options).await?;
    let outcome = secure_signed_route(&sandbox).await;
    match (outcome, sandbox.kill().await) {
        (outcome, Ok(())) => outcome,
        (outcome, Err(cleanup)) => Err(format!("scenario={outcome:?}; cleanup={cleanup}").into()),
    }
}
#[tokio::test]
#[ignore = "dedicated containerd snapshot and signed ingress runtime required"]
async fn k8s_secure_signed_pause_snapshot_restore() -> Result {
    let manager = manager()?;
    let mut options = CreateSandboxOptions::image(env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")?);
    options.secure_access = true;
    options
        .metadata
        .insert("rust-e2e-run".into(), env::var("OPENSANDBOX_RUST_RUN_ID")?);
    let sandbox = create(&manager, options).await?;
    let mut snapshot_id = None;
    let mut restored_id = None;
    let outcome: Result = async {
        sandbox
            .files()
            .write_text("/root/rust-persist.txt", "snapshot-persistence")
            .await?;
        secure_signed_route(&sandbox).await?;
        sandbox.pause().await?;
        state(&manager, sandbox.id(), "Paused").await?;
        let resumed = sandbox.resume().await?;
        if resumed.files().read_text("/root/rust-persist.txt").await? != "snapshot-persistence" {
            return Err("pause/resume lost rootfs marker".into());
        }
        let snapshot = manager
            .create_snapshot(sandbox.id(), CreateSnapshotOptions::default())
            .await?;
        snapshot_id = Some(snapshot.id.clone());
        tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                let status = manager.get_snapshot(&snapshot.id).await?.status;
                match status.state.as_str() {
                    "Ready" => return Ok::<(), opensandbox::Error>(()),
                    "Failed" => {
                        return Err(opensandbox::Error::Protocol(format!(
                            "snapshot failed: {status:?}"
                        )))
                    }
                    _ => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        })
        .await??;
        let restored = create(
            &manager,
            CreateSandboxOptions::new(SandboxSource::Snapshot(snapshot.id)),
        )
        .await?;
        restored_id = Some(restored.id().to_owned());
        if restored.files().read_text("/root/rust-persist.txt").await? != "snapshot-persistence" {
            return Err("snapshot restore lost marker".into());
        }
        Ok(())
    }
    .await;
    let mut cleanup = Vec::new();
    if let Some(id) = restored_id {
        if let Err(e) = manager.kill(&id).await {
            cleanup.push(e.to_string());
        }
    }
    if let Some(id) = snapshot_id {
        if let Err(e) = manager.delete_snapshot(&id).await {
            cleanup.push(e.to_string());
        }
    }
    if let Err(e) = sandbox.kill().await {
        cleanup.push(e.to_string());
    }
    if !cleanup.is_empty() {
        return Err(format!("scenario={outcome:?}; cleanup={cleanup:?}").into());
    }
    outcome
}
#[tokio::test]
#[ignore = "dedicated fast-sandbox runtime with template builder required"]
async fn k8s_template_create_ready_instantiate_delete() -> Result {
    let manager = manager()?;
    // This explicit value overrides inherited PATH and remains literal.
    let expected_env = std::collections::BTreeMap::from([
        ("RUST_TEMPLATE_MARKER".into(), "literal-$HOME-模板".into()),
        (
            "PATH".into(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/rust-template-env-marker".into(),
        ),
    ]);
    let created = manager
        .create_template_with_env(
            CreateTemplateOptions {
                image: env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")?,
                publish: env::var("OPENSANDBOX_RUST_TEMPLATE_PUBLISH")?,
                resource_limits: None,
                entrypoint: vec!["sh".into(), "-c".into(), "sleep infinity".into()],
                metadata: [("rust-e2e-run".into(), env::var("OPENSANDBOX_RUST_RUN_ID")?)].into(),
                readiness: None,
                format: None,
            },
            expected_env.clone(),
        )
        .await?;
    let template = &created.template;
    let mut sandbox_id: Option<String> = None;
    let outcome: Result = async {
        if created.env.as_ref() != Some(&expected_env) {
            return Err("template create env echo mismatch".into());
        }
        let fetched = manager.get_template_with_env(&template.template_id).await?;
        if fetched.env.as_ref() != Some(&expected_env) {
            return Err("template get env echo mismatch".into());
        }
        let listed = manager
            .list_templates_with_env(TemplateFilter {
                metadata: template.metadata.clone().unwrap_or_default(),
                ..Default::default()
            })
            .await?;
        if !listed.items.iter().any(|item| {
            item.template.template_id == template.template_id
                && item.env.as_ref() == Some(&expected_env)
        }) {
            return Err("template list lost env echo".into());
        }
        tokio::time::timeout(Duration::from_secs(600), async {
            loop {
                let status = manager.get_template(&template.template_id).await?.status;
                match status.phase.as_str() {
                    "Succeeded" => return Ok::<(), opensandbox::Error>(()),
                    "Failed" => {
                        return Err(opensandbox::Error::Protocol(format!(
                            "template failed: {status:?}"
                        )))
                    }
                    _ => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        })
        .await??;
        let sandbox = create(
            &manager,
            CreateSandboxOptions::new(SandboxSource::Template(template.template_id.clone())),
        )
        .await?;
        sandbox_id = Some(sandbox.id().into());
        let environment = sandbox
            .commands()
            .run(
                "printf '%s\\n%s' \"$RUST_TEMPLATE_MARKER\" \"$PATH\"",
                Default::default(),
            )
            .await?;
        let actual = environment
            .logs
            .stdout
            .iter()
            .map(|item| item.text.as_str())
            .collect::<String>();
        let expected = format!(
            "{}\n{}",
            expected_env["RUST_TEMPLATE_MARKER"], expected_env["PATH"]
        );
        if environment.exit_code != Some(0) || actual != expected {
            return Err("template instance env mismatch".into());
        }
        Ok(())
    }
    .await;
    let mut cleanup = Vec::new();
    if let Some(id) = sandbox_id {
        if let Err(e) = manager.kill(&id).await {
            cleanup.push(e.to_string());
        }
    }
    if let Err(e) = manager.delete_template(&template.template_id).await {
        cleanup.push(e.to_string());
    }
    if !cleanup.is_empty() {
        return Err(format!("scenario={outcome:?}; cleanup={cleanup:?}").into());
    }
    outcome
}

#[tokio::test]
#[ignore = "dedicated Kubernetes bwrap Pool and trusted mount selectors required"]
async fn k8s_bwrap_pool_create_argv_files_and_cleanup() -> Result {
    use opensandbox::{
        SandboxIsolation, SandboxIsolationMount, SandboxIsolationMountMode, SandboxIsolationType,
    };
    let manager = manager()?;
    let target = env::var("OPENSANDBOX_RUST_BWRAP_TARGET")?;
    let isolation = SandboxIsolation {
        type_: SandboxIsolationType::Bwrap,
        mounts: vec![SandboxIsolationMount {
            root: env::var("OPENSANDBOX_RUST_BWRAP_ROOT")?,
            sub_path: env::var("OPENSANDBOX_RUST_BWRAP_SUBPATH")?,
            target: target.clone(),
            mode: SandboxIsolationMountMode::Rw,
        }],
    };
    let options = CreateSandboxOptions::new(opensandbox::SandboxSource::Pool(env::var(
        "OPENSANDBOX_RUST_BWRAP_POOL_REF",
    )?));
    let sandbox = match manager.create_with_isolation(options, isolation).await {
        Ok(s) => s,
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
        let output = sandbox
            .commands()
            .run_argv(
                &[
                    "/bin/sh",
                    "-c",
                    "printf '%s|%s' \"$1\" \"$2\"",
                    "sdk-test",
                    "$HOME",
                    "space value",
                ],
                Default::default(),
            )
            .await?;
        if output.exit_code != Some(0)
            || output
                .logs
                .stdout
                .iter()
                .map(|v| v.text.as_str())
                .collect::<String>()
                != "$HOME|space value"
        {
            return Err("native argv literal execution failed".into());
        }
        let status = sandbox
            .commands()
            .run_argv(
                &["/bin/sh", "-c", "cat /proc/self/status"],
                Default::default(),
            )
            .await?;
        if status.exit_code != Some(0) {
            return Err("could not inspect bwrap workload hardening".into());
        }
        let text = status
            .logs
            .stdout
            .iter()
            .map(|v| v.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for field in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
            if !text.lines().any(|l| {
                l.split_once(':')
                    .is_some_and(|(k, v)| k == field && v.trim() == "0000000000000000")
            }) {
                return Err(format!("bwrap workload {field} was not cleared").into());
            }
        }
        if !text.lines().any(|l| {
            l.split_once(':')
                .is_some_and(|(k, v)| k == "NoNewPrivs" && v.trim() == "1")
        }) {
            return Err("bwrap workload requires no_new_privs".into());
        }
        let name = format!(
            "{}/.rust-sdk-{}.txt",
            target,
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        sandbox.files().write_text(&name, "bwrap-sdk").await?;
        let checked = sandbox.files().read_text(&name).await;
        let remove = sandbox.files().remove(std::slice::from_ref(&name)).await;
        let actual = checked?;
        remove?;
        if actual != "bwrap-sdk" {
            return Err("bwrap allocation scoped file roundtrip failed".into());
        }
        if sandbox.files().read_text(&name).await.is_ok() {
            return Err("removed bwrap allocation file remains readable".into());
        }
        Ok(())
    }
    .await;
    let cleanup = sandbox.kill().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(e),
        (result, Err(e)) => Err(format!("scenario={result:?}; sandbox cleanup={e}").into()),
    }
}
