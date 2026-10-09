//! Run explicitly: cargo test --manifest-path tests/rust/Cargo.toml --locked sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture
use opensandbox::{
    ConnectionConfig, CreateSandboxOptions, Error, NetworkPolicy, NetworkRuleAction,
    RunCommandOptions, Sandbox, SandboxManager,
};
use std::{env, error::Error as StdError, time::Duration};

type TestResult = Result<(), Box<dyn StdError + Send + Sync>>;
fn check(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn manager() -> Result<SandboxManager, Box<dyn StdError + Send + Sync>> {
    let domain = env::var("OPENSANDBOX_TEST_DOMAIN")
        .map_err(|_| "OPENSANDBOX_TEST_DOMAIN is required for explicit e2e runs")?;
    let config = ConnectionConfig::builder()
        .domain(domain)
        .protocol(env::var("OPENSANDBOX_TEST_PROTOCOL").unwrap_or_else(|_| "http".into()))
        .api_key(env::var("OPENSANDBOX_TEST_API_KEY").unwrap_or_default())
        .ready_timeout(Duration::from_secs(120))
        .request_timeout(Duration::from_secs(30))
        .use_server_proxy(env::var("OPENSANDBOX_TEST_USE_SERVER_PROXY").as_deref() == Ok("true"))
        .build()?;
    Ok(SandboxManager::new(config)?)
}
async fn scenario(sandbox: &Sandbox, network_policy: bool) -> TestResult {
    let execution = sandbox
        .commands()
        .run(
            "printf 'rust-sdk-e2e'",
            RunCommandOptions {
                timeout: Some(Duration::from_secs(15)),
                ..Default::default()
            },
        )
        .await?;
    check(
        execution.exit_code == Some(0),
        "command did not exit successfully",
    )?;
    let stdout: String = execution
        .logs
        .stdout
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    check(stdout == "rust-sdk-e2e", "stdout mismatch")?;
    let failure = sandbox
        .commands()
        .run(
            "printf 'before-error'; exit 7",
            RunCommandOptions::default(),
        )
        .await?;
    check(
        failure.error.is_some(),
        "nonzero command lost structured error",
    )?;
    check(
        failure.exit_code == Some(7),
        "nonzero command exit code mismatch",
    )?;
    check(
        failure
            .logs
            .stdout
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>()
            == "before-error",
        "nonzero command lost stdout",
    )?;
    let files = sandbox.files();
    let binary = vec![0, 255, 128, 1, b'\r', b'\n', 0, 42];
    files
        .upload("/tmp/rust-sdk-e2e.bin", binary.clone())
        .await?;
    check(
        files.download("/tmp/rust-sdk-e2e.bin").await? == binary,
        "binary roundtrip mismatch",
    )?;
    let text = "Rust SDK 文本\nsecond line\n";
    files.write_text("/tmp/rust-sdk-e2e.txt", text).await?;
    check(
        files.read_text("/tmp/rust-sdk-e2e.txt").await? == text,
        "text roundtrip mismatch",
    )?;
    let metrics = sandbox.metrics().get().await?;
    check(
        metrics.cpu_count > 0.0 && metrics.mem_total_mib > 0.0,
        "metrics reported invalid capacity",
    )?;
    if network_policy {
        let status = sandbox.egress().await?.get_policy_status().await?;
        check(
            status.policy.is_some(),
            "enabled network policy was omitted",
        )?;
    }
    Ok(())
}
async fn create_and_cleanup(
    manager: &SandboxManager,
    options: CreateSandboxOptions,
    network_policy: bool,
) -> TestResult {
    let sandbox = match manager.create(options).await {
        Ok(sandbox) => sandbox,
        Err(error) => {
            let id = match &error {
                Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } => {
                    Some(sandbox_id)
                }
                _ => None,
            };
            if let Some(id) = id {
                if let Err(cleanup) = manager.kill(id).await {
                    return Err(format!(
                        "creation failed: {error}; cleanup of {id} also failed: {cleanup}"
                    )
                    .into());
                }
            } else {
                eprintln!("Creation failed without an accessible sandbox ID. If the server allocated a sandbox before the response failed, inspect server resources for manual cleanup.");
            }
            return Err(error.into());
        }
    };
    let outcome = scenario(&sandbox, network_policy).await;
    let cleanup = sandbox.kill().await;
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => {
            Err(format!("sandbox {} cleanup failed: {error}", sandbox.id()).into())
        }
        (Err(error), Err(cleanup)) => Err(format!(
            "scenario failed: {error}; sandbox {} cleanup also failed: {cleanup}",
            sandbox.id()
        )
        .into()),
    }
}
#[tokio::test]
#[ignore = "requires live OpenSandbox: cargo test --manifest-path tests/rust/Cargo.toml --locked sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture"]
async fn sandbox_create_command_files_metrics_egress_kill() {
    let outcome = async {
        let manager = manager()?;
        let image = env::var("OPENSANDBOX_SANDBOX_DEFAULT_IMAGE")
            .unwrap_or_else(|_| "opensandbox/code-interpreter:latest".into());
        let enabled = env::var("OPENSANDBOX_TEST_NETWORK_POLICY").as_deref() == Ok("true");
        let mut options = CreateSandboxOptions::image(image);
        options.timeout = Some(Duration::from_secs(300));
        if let Ok(run_id) = env::var("OPENSANDBOX_RUST_RUN_ID") {
            options.metadata.insert("rust-e2e-run".into(), run_id);
        }
        options.secure_access = env::var("OPENSANDBOX_TEST_SECURE_ACCESS").as_deref() == Ok("true");
        if enabled {
            options.network_policy = Some(NetworkPolicy {
                default_action: Some(NetworkRuleAction::Allow),
                egress: Vec::new(),
            });
        }
        create_and_cleanup(&manager, options, enabled).await
    }
    .await;
    assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
}

// These fixtures must already be Ready and belong to a runtime that supports
// the corresponding source. Missing configuration is an error, not a skip.
#[tokio::test]
#[ignore = "opt-in supported snapshot runtime: set OPENSANDBOX_TEST_SNAPSHOT_ID and run --ignored snapshot_source"]
async fn snapshot_source() {
    let outcome = async {
        let manager = manager()?;
        let id = env::var("OPENSANDBOX_TEST_SNAPSHOT_ID").map_err(|_| {
            "set OPENSANDBOX_TEST_SNAPSHOT_ID to a Ready snapshot on a supported runtime"
        })?;
        let options = CreateSandboxOptions::new(opensandbox::SandboxSource::Snapshot(id));
        create_and_cleanup(&manager, options, false).await
    }
    .await;
    assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
}

#[tokio::test]
#[ignore = "opt-in supported template runtime: set OPENSANDBOX_TEST_TEMPLATE_ID and run --ignored template_source"]
async fn template_source() {
    let outcome = async {
        let manager = manager()?;
        let id = env::var("OPENSANDBOX_TEST_TEMPLATE_ID").map_err(|_| {
            "set OPENSANDBOX_TEST_TEMPLATE_ID to a Ready template on a supported runtime"
        })?;
        let options = CreateSandboxOptions::new(opensandbox::SandboxSource::Template(id));
        create_and_cleanup(&manager, options, false).await
    }
    .await;
    assert!(outcome.is_ok(), "{}", outcome.unwrap_err());
}
