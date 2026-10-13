// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use opensandbox::*;
use serde_json::{json, Value};
use std::{collections::HashMap, env, error::Error as StdError, time::Duration};

type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
const UID: u32 = 23111;
const GID: u32 = 23112;

fn require(ok: bool, message: &str) -> Result {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn require_permission_denied<T>(result: opensandbox::Result<T>, operation: &str) -> Result {
    match result {
        Err(Error::Api { message, .. })
            if message.to_lowercase().contains("permission denied")
                || message.to_lowercase().contains("operation not permitted") =>
        {
            Ok(())
        }
        Err(error) => Err(format!("{operation}: expected permission denial, got {error}").into()),
        Ok(_) => Err(format!("{operation}: unexpectedly succeeded").into()),
    }
}

async fn create() -> Result<Sandbox> {
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(env::var("OPENSANDBOX_TEST_DOMAIN")?)
            .protocol(env::var("OPENSANDBOX_TEST_PROTOCOL").unwrap_or_else(|_| "http".into()))
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
    match manager.create(options).await {
        Ok(sandbox) => Ok(sandbox),
        Err(error) => {
            if let Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } =
                &error
            {
                manager.kill(sandbox_id).await?;
            }
            Err(error.into())
        }
    }
}

async fn finish(sandbox: &Sandbox, outcome: Result) -> Result {
    match (outcome, sandbox.kill().await) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (outcome, Err(error)) => Err(format!("scenario={outcome:?}; cleanup={error}").into()),
    }
}

fn stdout(execution: &Execution) -> String {
    execution
        .logs
        .stdout
        .iter()
        .map(|m| m.text.as_str())
        .collect()
}

async fn python(sandbox: &Sandbox, code: &str, args: &[&str]) -> Result<Value> {
    let mut argv = vec!["python3", "-c", code];
    argv.extend_from_slice(args);
    let execution = sandbox
        .commands()
        .run_argv(&argv, Default::default())
        .await?;
    require(execution.exit_code == Some(0), "Python probe failed")?;
    Ok(serde_json::from_str(stdout(&execution).trim())?)
}

#[tokio::test]
#[ignore = "allocates an owned sandbox; requires Linux execd identity routes and Python 3"]
async fn docker_identity_files_permissions_ownership_and_all_routes() -> Result {
    let sandbox = create().await?;
    let outcome = async {
        let root = sandbox.files();
        let files = root.with_identity(UID, GID)?;
        let denied = root.with_identity(UID + 1, GID + 1)?;
        let base = "/tmp/rust-identity";
        let sub = format!("{base}/nested space 文本");
        root.create_directories(&HashMap::from([(
            base.into(),
            Permission {
                mode: 700,
                ..Default::default()
            },
        )]))
        .await?;
        // Metadata owner/group are account names, not execution identities.
        // Prepare numeric ownership independently, without adding OS accounts.
        python(&sandbox,
            "import os,sys,json; os.chown(sys.argv[1],int(sys.argv[2]),int(sys.argv[3])); print(json.dumps(True))",
            &[base, &UID.to_string(), &GID.to_string()]).await?;
        files
            .create_directories(&HashMap::from([(
                sub.clone(),
                Permission { mode: 700, ..Default::default() },
            )]))
            .await?;
        let binary_path = format!("{sub}/binary.dat");
        let text_path = format!("{sub}/lines.txt");
        let moved = format!("{sub}/moved.txt");
        let binary = vec![0, 255, 128, 10, 0, 42];
        files.upload(&binary_path, binary.clone()).await?;
        files.write_text(&text_path, "first\nsecond 文本\nthird\n").await?;
        require(files.download(&binary_path).await? == binary, "binary roundtrip changed bytes")?;
        require(
            files.download_with_options(&binary_path, DownloadOptions {
                range: Some("bytes=1-3".into()), ..Default::default()
            }).await? == [255, 128, 10],
            "identity byte range mismatch",
        )?;
        require(
            files.download_with_options(&text_path, DownloadOptions {
                offset: Some(2), limit: Some(1), ..Default::default()
            }).await? == "second 文本".as_bytes(),
            "identity line selection mismatch",
        )?;
        let info = files.info(&[binary_path.clone(), text_path.clone()]).await?;
        require(info.len() == 2 && info[&binary_path].size == 6, "identity info mismatch")?;
        let ownership = python(&sandbox,
            "import os,sys,json; s=os.stat(sys.argv[1]); print(json.dumps([s.st_uid,s.st_gid]))",
            &[&binary_path]).await?;
        require(ownership == json!([UID, GID]), "upload ran as wrong kernel identity")?;
        require_permission_denied(denied.download(&text_path).await, "another identity reading private directory")?;
        require_permission_denied(denied.write_text(&text_path, "forbidden").await, "another identity overwriting private file")?;
        require(root.read_text(&text_path).await? == "first\nsecond 文本\nthird\n",
            "identity denial changed contents or original root handle")?;
        require_permission_denied(
            files.set_permissions(&HashMap::from([(
                binary_path.clone(), Permission {
                    owner: Some("root".into()), mode: 600, ..Default::default()
                }
            )])).await,
            "unprivileged worker changing owner to root",
        )?;
        files.move_files(&[RenameFileItem {
            src: text_path.clone(), dest: moved.clone(), ..Default::default()
        }]).await?;
        require(files.search(&sub, "*.txt").await?.iter().any(|f| f.path == moved),
            "identity search omitted renamed file")?;
        let replaced = files.replace_contents(&HashMap::from([(
            moved.clone(), ReplaceFileContentItem {
                old: "second".into(), new: "changed".into(), ..Default::default()
            }
        )])).await?;
        require(replaced[&moved].replaced_count == 1, "verbose replacement count mismatch")?;
        files.set_permissions(&HashMap::from([(
            moved.clone(), Permission { mode: 600, ..Default::default() }
        )])).await?;
        let stat = python(&sandbox,
            "import os,sys,json,stat; s=os.stat(sys.argv[1]); print(json.dumps([s.st_uid,s.st_gid,stat.S_IMODE(s.st_mode)]))",
            &[&moved]).await?;
        require(stat == json!([UID, GID, 0o600]), "permission operation changed kernel ownership")?;
        require(files.read_text(&moved).await? == "first\nchanged 文本\nthird\n", "replacement mismatch")?;
        require(files.list_directory(&sub, Some(1)).await?.len() == 2, "identity directory listing mismatch")?;
        // A numeric user absent from /etc/passwd must not inherit root's groups.
        root.upload_with_metadata(FileMetadata {
            path: Some("/tmp/rust-root-group-only".into()), owner: Some("root".into()),
            group: Some("root".into()), mode: Some(640), ..Default::default()
        }, b"private".to_vec()).await?;
        require_permission_denied(files.download("/tmp/rust-root-group-only").await, "worker reading root group file")?;
        require(python(&sandbox, "import os,json; print(json.dumps([os.getuid(),os.getgid()]))", &[]).await? == json!([0, 0]),
            "filesystem worker changed daemon identity")?;
        files.remove(&[binary_path.clone(), moved.clone()]).await?;
        require(files.info(&[binary_path]).await.is_err(), "identity removal left file")?;
        files.remove_directories(&[sub]).await?;
        files.remove_directories(&[base.into()]).await?;
        println!("identity: all 11 routes, kernel UID/GID, permissions, denied access and root compatibility passed");
        Ok(())
    }.await;
    finish(&sandbox, outcome).await
}

#[tokio::test]
#[ignore = "allocates an owned sandbox; requires native argv execd and Python 3"]
async fn docker_native_argv_literal_options_stream_background_failure_interrupt() -> Result {
    let sandbox = create().await?;
    let outcome = async {
        let commands = sandbox.commands();
        let literal = ["", "two words", "中文", "line\nbreak", "$HOME", "$(touch /tmp/rust-argv-expanded)", "'\";|&<>*?"];
        let code = "import os,sys,json; print(json.dumps({'argv':sys.argv[1:],'cwd':os.getcwd(),'uid':os.getuid(),'gid':os.getgid(),'env':os.getenv('RUST_ARGV_LITERAL')}))";
        let mut argv = vec!["python3", "-c", code];
        argv.extend_from_slice(&literal);
        let execution = commands.run_argv(&argv, RunCommandOptions {
            cwd: Some("/tmp".into()), uid: Some(UID as i32), gid: Some(GID as i32),
            envs: Some(HashMap::from([("RUST_ARGV_LITERAL".into(), "$HOME; literal 文本".into())])),
            timeout: Some(Duration::from_secs(10)), ..Default::default()
        }).await?;
        require(execution.complete && execution.exit_code == Some(0), "native argv execution failed")?;
        let result: Value = serde_json::from_str(stdout(&execution).trim())?;
        require(result == json!({"argv":literal,"cwd":"/tmp","uid":UID,"gid":GID,"env":"$HOME; literal 文本"}),
            "argv literals or execution options changed")?;
        require(python(&sandbox, "import os,json; print(json.dumps(os.path.exists('/tmp/rust-argv-expanded')))", &[]).await? == json!(false),
            "native argv applied shell command substitution")?;
        let mut stream = commands.run_argv_stream(
            &["python3".to_owned(), "-c".into(), "print('stream 文本')".into()], Default::default()
        ).await?;
        let mut output = String::new();
        let mut initialized = false;
        let mut complete = false;
        while let Some(event) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            let event = event?;
            match event.event_type.as_str() {
                "init" => initialized = event.text.is_some(),
                "stdout" => output.push_str(event.text.as_deref().unwrap_or_default()),
                "execution_complete" => complete = true,
                "error" => return Err("native argv stream emitted error".into()),
                _ => {}
            }
        }
        require(initialized && complete && output.trim() == "stream 文本", "native stream lost events")?;
        let background = commands.run_argv(&["python3", "-c", "import time; print('background',flush=True); time.sleep(.3); print('done')"],
            RunCommandOptions { background: true, ..Default::default() }).await?;
        let id = background.id.ok_or("background native execution omitted ID")?;
        let status = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let status = commands.status(&id).await?;
                if status.running == Some(false) { return Ok::<_, opensandbox::Error>(status); }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await??;
        require(status.exit_code == Some(Some(0)), "native background exit code mismatch")?;
        let logs = commands.logs(&id, None).await?;
        require(logs.content.contains("background") && logs.content.contains("done"), "native background logs incomplete")?;
        let failed = commands.run_argv(&["python3", "-c", "import sys; print('out'); print('err',file=sys.stderr); sys.exit(7)"], Default::default()).await?;
        require(failed.exit_code == Some(7) && failed.error.is_some() && stdout(&failed).trim() == "out"
            && failed.logs.stderr.iter().any(|m| m.text.contains("err")), "native failure lost exit code or logs")?;
        let running = commands.run_argv(&["python3", "-c", "import time; time.sleep(60)"],
            RunCommandOptions { background: true, ..Default::default() }).await?;
        let id = running.id.ok_or("interrupt target omitted ID")?;
        commands.interrupt(&id).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if commands.status(&id).await?.running == Some(false) { return Ok::<_, opensandbox::Error>(()); }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }).await??;
        let shell = commands.run("printf 'existing-shell'", Default::default()).await?;
        require(shell.exit_code == Some(0) && stdout(&shell) == "existing-shell", "existing shell API regressed")?;
        println!("argv: literals, cwd/env/UID/GID, stream, background/logs, exit failure, interrupt and shell compatibility passed");
        Ok(())
    }.await;
    finish(&sandbox, outcome).await
}
