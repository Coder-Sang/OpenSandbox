// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::*;
use opensandbox_code_interpreter::{CodeInterpreter, CodeInterpreterOptions, RunCodeOptions};
use std::{env, error::Error as StdError, time::Duration};
type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
fn manager() -> Result<SandboxManager> {
    Ok(SandboxManager::new(
        ConnectionConfig::builder()
            .domain(env::var("OPENSANDBOX_TEST_DOMAIN")?)
            .api_key(env::var("OPENSANDBOX_TEST_API_KEY").unwrap_or_default())
            .use_server_proxy(
                env::var("OPENSANDBOX_TEST_USE_SERVER_PROXY").as_deref() == Ok("true"),
            )
            .ready_timeout(Duration::from_secs(180))
            .build()?,
    )?)
}
fn require(ok: bool, message: &str) -> Result {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
async fn finish(sandbox: &Sandbox, outcome: Result) -> Result {
    let cleanup = sandbox.kill().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(e),
        (result, Err(e)) => {
            Err(format!("scenario={result:?}; cleanup {}: {e}", sandbox.id()).into())
        }
    }
}
async fn create(manager: &SandboxManager, options: CreateSandboxOptions) -> Result<Sandbox> {
    match manager.create(options).await {
        Ok(sandbox) => Ok(sandbox),
        Err(error) => {
            if let Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } =
                &error
            {
                if let Err(cleanup) = manager.kill(sandbox_id).await {
                    return Err(format!("creation={error}; cleanup {sandbox_id}={cleanup}").into());
                }
            }
            Err(error.into())
        }
    }
}
fn options() -> Result<CreateSandboxOptions> {
    let mut options = CreateSandboxOptions::image(env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")?);
    options.timeout = Some(Duration::from_secs(600));
    if env::var("OPENSANDBOX_RUST_DEDICATED_JUPYTER").as_deref() == Ok("true") {
        configure_dedicated_jupyter(&mut options);
    }
    options
        .metadata
        .insert("rust-e2e-run".into(), env::var("OPENSANDBOX_RUST_RUN_ID")?);
    Ok(options)
}
fn configure_dedicated_jupyter(options: &mut CreateSandboxOptions) {
    options
        .env
        .insert("JUPYTER_HOST".into(), "http://127.0.0.1:44771".into());
    options
        .env
        .insert("JUPYTER_TOKEN".into(), "rust-e2e-jupyter-safe-token".into());
    options.entrypoint = Some(vec![
        "jupyter".into(),
        "server".into(),
        "--ip=0.0.0.0".into(),
        "--port=44771".into(),
        "--allow-root".into(),
        "--no-browser".into(),
        "--IdentityProvider.token=rust-e2e-jupyter-safe-token".into(),
    ]);
}
#[test]
fn dedicated_jupyter_fixture_has_matching_host_token_and_entrypoint() {
    // Execd ignores the stock python3 kernelspec when selecting language python.
    assert!(include_str!("../fixtures/Dockerfile").contains(
        "python -m ipykernel install --prefix=/usr/local --name python --display-name Python"
    ));
    let mut options = CreateSandboxOptions::image("fixture");
    configure_dedicated_jupyter(&mut options);
    assert_eq!(options.env["JUPYTER_HOST"], "http://127.0.0.1:44771");
    assert_eq!(options.env["JUPYTER_TOKEN"], "rust-e2e-jupyter-safe-token");
    let entrypoint = options.entrypoint.unwrap();
    assert_eq!(&entrypoint[..2], &["jupyter", "server"]);
    assert!(entrypoint.iter().any(|arg| arg == "--port=44771"));
    assert!(entrypoint
        .iter()
        .any(|arg| arg == "--IdentityProvider.token=rust-e2e-jupyter-safe-token"));
}
#[tokio::test]
#[ignore = "real execd isolation runtime required"]
async fn docker_isolated_foreground_background_scoped_files() -> Result {
    let sandbox = create(&manager()?, options()?).await?;
    let outcome = async {
        sandbox
            .commands()
            .run("mkdir -p /tmp/rust-isolated", Default::default())
            .await?;
        let sessions = sandbox.isolated();
        let caps = sessions.capabilities().await?;
        require(
            caps.setpriv_available.unwrap_or(false),
            &format!(
                "setpriv unavailable: isolated runtime prerequisites unmet; capabilities={caps:?}"
            ),
        )?;
        let session = sessions
            .create(CreateIsolatedSessionOptions {
                workspace: IsolatedWorkspaceSpec {
                    path: "/tmp/rust-isolated".into(),
                    mode: Some(WorkspaceMode::Rw),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await?;
        let result = async {
            let run = session
                .run("printf 'foreground'", Default::default())
                .await?;
            require(run.exit_code == Some(0), "isolated foreground failed")?;
            require(
                run.logs
                    .stdout
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
                    == "foreground",
                "isolated foreground stdout mismatch",
            )?;
            let binary = vec![0, 255, 128, 42];
            session
                .files()
                .upload("/tmp/rust-isolated/binary.bin", binary.clone())
                .await?;
            require(
                session
                    .files()
                    .download("/tmp/rust-isolated/binary.bin")
                    .await?
                    == binary,
                "scoped binary roundtrip mismatch",
            )?;
            session
                .files()
                .write_text("/tmp/rust-isolated/scoped.txt", "scoped 文本")
                .await?;
            require(
                session
                    .files()
                    .read_text("/tmp/rust-isolated/scoped.txt")
                    .await?
                    == "scoped 文本",
                "scoped file mismatch",
            )?;
            require(
                session.files().read_text("/etc/passwd").await.is_err(),
                "scoped files escaped workspace",
            )?;
            let bg = session
                .run_background(
                    "printf 'background'; sleep 1; printf 'done'",
                    Default::default(),
                )
                .await?;
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if !session.status(&bg.run_id).await?.running {
                        break Ok::<(), opensandbox::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await??;
            let status = session.status(&bg.run_id).await?;
            require(
                status.exit_code == Some(Some(0)),
                "background exit code mismatch",
            )?;
            let logs = session.logs(&bg.run_id, None).await?;
            require(
                logs.text.contains("background") && logs.text.contains("done"),
                "background logs incomplete",
            )?;
            require(
                session
                    .logs(&bg.run_id, Some(logs.cursor))
                    .await?
                    .text
                    .is_empty(),
                "log cursor duplicated output",
            )?;
            require(
                sessions
                    .list()
                    .await?
                    .iter()
                    .any(|s| s.session_id == session.session_id()),
                "session missing from list",
            )?;
            Ok(())
        }
        .await;
        let deleted = session.delete().await;
        deleted?;
        result
    }
    .await;
    finish(&sandbox, outcome).await
}
#[tokio::test]
#[ignore = "real sandbox lifecycle required"]
async fn docker_pool_acquired_ownership_survives_shutdown() -> Result {
    use opensandbox::pool::{AcquirePolicy, PoolConfig, SandboxPool};
    let connection = ConnectionConfig::builder()
        .domain(env::var("OPENSANDBOX_TEST_DOMAIN")?)
        .api_key(env::var("OPENSANDBOX_TEST_API_KEY").unwrap_or_default())
        .use_server_proxy(env::var("OPENSANDBOX_TEST_USE_SERVER_PROXY").as_deref() == Ok("true"))
        .build()?;
    let mut config = PoolConfig::new(
        env::var("OPENSANDBOX_RUST_RUN_ID")?,
        1,
        connection,
        options()?,
    );
    config.warmup_ready_timeout = Duration::from_secs(120);
    config.acquire_ready_timeout = Duration::from_secs(120);
    let pool = SandboxPool::new(config)?;
    pool.start().await?;
    let outcome: Result = async {
        tokio::time::timeout(Duration::from_secs(150), async {
            loop {
                if pool.snapshot().await?.idle_count == 1 {
                    return Ok::<(), opensandbox::pool::PoolError>(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await??;
        let sandbox = pool
            .acquire(Some(Duration::from_secs(300)), AcquirePolicy::FailFast)
            .await?;
        let result = async {
            pool.kill().await?;
            require(
                sandbox
                    .commands()
                    .run("printf caller-owned", Default::default())
                    .await?
                    .exit_code
                    == Some(0),
                "pool shutdown destroyed acquired sandbox",
            )?;
            Ok(())
        }
        .await;
        finish(&sandbox, result).await
    }
    .await;
    let cleanup = pool.kill().await;
    if let Err(e) = cleanup {
        return Err(format!("scenario={outcome:?}; pool cleanup={e}").into());
    }
    outcome
}
fn vault_request() -> opensandbox::api::egress::CredentialVaultCreateRequest {
    serde_json::from_value(serde_json::json!({
        "credentials":[{"name":"test-token","source":{"type":"inline","value":"rust-e2e-safe-token"}}],
        "bindings":[{"name":"test-binding","match":{"schemes":["http"],"hosts":["rust-vault.opensandbox.test"],"methods":["GET"],"paths":["/check"]},"auth":{"type":"bearer","credential":"test-token"}}]
    })).expect("valid dedicated vault fixture")
}
fn vault_mutation(revision: i64) -> opensandbox::api::egress::CredentialVaultMutationRequest {
    serde_json::from_value(serde_json::json!({"expectedRevision":revision,"credentials":{"replace":[{"name":"test-token","source":{"type":"inline","value":"rust-e2e-safe-token"}}]}})).expect("valid vault mutation fixture")
}
#[test]
fn credential_fixtures_deserialize_before_allocation() {
    let request = vault_request();
    let wire = serde_json::to_value(request).unwrap();
    assert_eq!(wire["credentials"][0]["source"]["type"], "inline");
    assert_eq!(wire["bindings"][0]["auth"]["credential"], "test-token");
    assert_eq!(wire["bindings"][0]["auth"]["type"], "bearer");
    let mutation = serde_json::to_value(vault_mutation(1)).unwrap();
    assert_eq!(mutation["expectedRevision"], 1);
    assert_eq!(
        mutation["credentials"]["replace"][0]["source"]["type"],
        "inline"
    );
    let _: opensandbox::api::lifecycle::CredentialProxyConfig =
        serde_json::from_value(serde_json::json!({"enabled":true})).unwrap();
    let _: NetworkPolicy =
        serde_json::from_value(serde_json::json!({"defaultAction":"allow","egress":[]})).unwrap();
}
#[tokio::test]
#[ignore = "real credential proxy sidecar and dedicated target required"]
async fn docker_vault_injection_revision_metadata() -> Result {
    use serde_json::json;
    let ip = env::var("OPENSANDBOX_RUST_VAULT_TARGET_IP")?;
    require(
        ip.parse::<std::net::Ipv4Addr>().is_ok(),
        "invalid target IP",
    )?;
    let mut options = options()?;
    options.credential_proxy = Some(serde_json::from_value(json!({"enabled": true}))?);
    options.network_policy = Some(serde_json::from_value(
        json!({"defaultAction":"allow","egress":[]}),
    )?);
    let sandbox = create(&manager()?, options).await?;
    let outcome = async {
        let vault = sandbox.egress().await?;
        let request = vault_request();
        let created = vault.create(&request).await?;
        let metadata = serde_json::to_string(&vault.get().await?)?;
        require(!metadata.contains("rust-e2e-safe-token"), "vault leaked credential value")?;
        require(vault.list_credentials().await?.len() == 1 && vault.list_bindings().await?.len() == 1, "vault metadata listing mismatch")?;
        vault.get_credential("test-token").await?;
        vault.get_binding("test-binding").await?;
        let command = format!("curl --fail --silent --show-error --max-time 20 --resolve rust-vault.opensandbox.test:80:{ip} http://rust-vault.opensandbox.test/check");
        let response = sandbox.commands().run(&command, Default::default()).await?;
        require(response.exit_code == Some(0), "real credential injection rejected upstream")?;
        let body: serde_json::Value = serde_json::from_str(&response.logs.stdout.iter().map(|s| s.text.as_str()).collect::<String>())?;
        require(body["ok"] == true, "credential injection target mismatch")?;
        let mutation = vault_mutation(created.revision);
        let patched = vault.patch(&mutation).await?;
        require(patched.revision > created.revision, "vault revision did not advance")?;
        require(vault.patch(&mutation).await.is_err(), "stale vault revision accepted")?;
        vault.delete().await?;
        Ok(())
    }.await;
    finish(&sandbox, outcome).await
}
#[tokio::test]
#[ignore = "real code-interpreter image and Jupyter required"]
async fn docker_interpreter_context_rich_errors_interrupt() -> Result {
    let sandbox = create(&manager()?, options()?).await?;
    let outcome = async {
        let interpreter = CodeInterpreter::create(sandbox.clone(), CodeInterpreterOptions::default()).await?;
        let codes = interpreter.codes();
        eprintln!("interpreter: creating context");
        let context = codes.create_context("python").await?;
        let id = context.id.clone().ok_or("missing context ID")?;
        let run_options = RunCodeOptions { context: Some(context), language: None };
        let result = async {
            eprintln!("interpreter: initializing persistent state");
            let initialized = codes.run("answer = 41", run_options.clone()).await?;
            require(initialized.error.is_none(), &format!("context initialization failed: {initialized:?}"))?;
            let execution = codes.run("answer + 1", run_options.clone()).await?;
            require(execution.result.iter().any(|r| r.text.as_deref() == Some("42")), &format!("context state lost: {execution:?}"))?;
            require(codes.get_context(&id).await?.id.as_deref() == Some(&id), "context get mismatch")?;
            require(codes.list_contexts(Some("python")).await?.iter().any(|c| c.id.as_deref() == Some(&id)), "context absent")?;
            eprintln!("interpreter: rich outputs");
            let rich = codes.run("from IPython.display import display, HTML, JSON\ndisplay(HTML('<b>rust</b>'))\ndisplay(JSON({'answer': 42}))", run_options.clone()).await?;
            require(rich.result.iter().any(|r| r.raw.contains_key("text/html")), &format!("HTML result lost: {rich:?}"))?;
            require(rich.result.iter().any(|r| r.raw.contains_key("application/json")), &format!("JSON result lost: {rich:?}"))?;
            let failure = codes.run("print('before-error')\nraise ValueError('rust-e2e-error')", run_options.clone()).await?;
            require(failure.error.is_some() && failure.logs.stdout.iter().any(|s| s.text.contains("before-error")), "kernel error/stdout lost")?;
            eprintln!("interpreter: interrupt");
            let running = codes.run("import time\nprint('interrupt-start', flush=True)\ntime.sleep(60)", run_options.clone());
            let interrupt = async { tokio::time::sleep(Duration::from_secs(2)).await; codes.interrupt(&id).await };
            let (execution, interrupted) = tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(running, interrupt) }).await?;
            interrupted?;
            require(execution?.error.is_some(), "interrupt did not yield kernel error")?;
            require(codes.run("answer + 1", run_options).await?.error.is_none(), "context unusable after interrupt")?;
            Ok(())
        }.await;
        if result.is_err() {
            let diagnostic = sandbox.commands().run("ps aux; curl --silent --max-time 3 -H 'Authorization: token rust-e2e-jupyter-safe-token' http://127.0.0.1:44771/api/kernelspecs; tail -80 /tmp/jupyter.log 2>/dev/null || true", Default::default()).await;
            eprintln!("dedicated interpreter diagnostics: {diagnostic:?}");
        }
        codes.delete_context(&id).await?;
        result
    }.await;
    finish(&sandbox, outcome).await
}
