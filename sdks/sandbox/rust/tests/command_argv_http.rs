// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use futures_util::StreamExt;
use opensandbox::{Error, RunCommandOptions};
use serde_json::json;
use std::{collections::HashMap, time::Duration};

#[path = "support/http.rs"]
mod http;
use http::{assert_headers, route, sandbox, Reply, Server};

const SUCCESS: &str = "data: {\"type\":\"init\",\"text\":\"command-id\"}\n\ndata: {\"type\":\"stdout\",\"text\":\"hello\"}\n\ndata: {\"type\":\"execution_complete\",\"execution_time\":5}\n\n";

#[tokio::test]
async fn argv_is_literal_and_all_command_options_preserve_direct_proxy_mapping() {
    for proxy in [false, true] {
        let mut data = Server::start(vec![
            Reply::bytes(vec![]),
            Reply::sse(SUCCESS),
            Reply::sse(SUCCESS),
        ])
        .await;
        let sandbox = sandbox(&mut data, proxy).await;
        let argv = [
            "./program",
            "",
            "a b",
            "$HOME",
            "$(echo untouched)",
            "'\"; &|*",
            "你好\nline",
        ];
        let envs = HashMap::from([("PATH".into(), "/usr/bin".into())]);
        let options = RunCommandOptions {
            timeout: Some(Duration::from_millis(250)),
            cwd: Some("/workspace/a b".into()),
            envs: Some(envs.clone()),
            uid: Some(1000),
            gid: Some(2000),
            background: false,
        };
        let commands = sandbox.commands();
        let execution = commands.run_argv(&argv, options.clone()).await.unwrap();
        assert!(execution.complete);
        assert_eq!(execution.id.as_deref(), Some("command-id"));
        assert_eq!(execution.logs.stdout[0].text, "hello");
        assert_eq!(execution.exit_code, Some(0));
        assert_eq!(execution.execution_time, Some(Duration::from_millis(5)));
        let request = data.next().await;
        assert_headers(&request, proxy);
        assert_eq!(request.method, "POST");
        assert_eq!(request.target, format!("{}/command", route(proxy)));
        assert_eq!(request.headers["accept"], "text/event-stream");
        assert_eq!(
            request.json_body(),
            json!({
                "argv": argv, "timeout": 250, "cwd": "/workspace/a b", "envs": envs,
                "uid": 1000, "gid": 2000, "background": false,
            })
        );
        // The existing shell interface must retain its signature and payload.
        commands.run("echo $HOME", options).await.unwrap();
        let body = data.next().await.json_body();
        assert_eq!(body["command"], "echo $HOME");
        assert!(body.get("argv").is_none());
    }
}

#[tokio::test]
async fn owned_argv_stream_exposes_events_and_interrupt_remains_explicit() {
    let mut data = Server::start(vec![
        Reply::bytes(vec![]),
        Reply::sse(SUCCESS),
        Reply::bytes(vec![]),
    ])
    .await;
    let sandbox = sandbox(&mut data, true).await;
    let argv = vec!["sleep".to_owned(), "10".to_owned()];
    let mut stream = sandbox
        .commands()
        .run_argv_stream(&argv, Default::default())
        .await
        .unwrap();
    let event = stream.next().await.unwrap().unwrap();
    assert_eq!(event.event_type, "init");
    assert_eq!(event.text.as_deref(), Some("command-id"));
    drop(stream);
    let request = data.next().await;
    assert_eq!(
        request.json_body(),
        json!({"argv": argv, "background": false})
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), data.requests.recv())
            .await
            .is_err()
    );
    sandbox.commands().interrupt("command-id").await.unwrap();
    let interrupt = data.next().await;
    assert_eq!(interrupt.method, "DELETE");
    assert_eq!(
        interrupt.target,
        format!("{}/command?id=command-id", route(true))
    );
    assert_headers(&interrupt, true);
}

#[tokio::test]
async fn argv_background_failures_and_premature_eof_match_shell_execution() {
    let mut data = Server::start(vec![
        Reply::bytes(vec![]), Reply::sse("data: {\"type\":\"init\",\"text\":\"background-id\"}\n\ndata: {\"type\":\"execution_complete\"}\n\n"),
        Reply::sse("data: {\"type\":\"error\",\"error\":{\"ename\":\"CommandError\",\"evalue\":\"7\"}}\n\n"),
        Reply::sse("data: {\"type\":\"stdout\",\"text\":\"partial\"}\n\n"),
    ]).await;
    let sandbox = sandbox(&mut data, false).await;
    let commands = sandbox.commands();
    let background = commands
        .run_argv(
            &["sleep", "10"],
            RunCommandOptions {
                background: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(background.id.as_deref(), Some("background-id"));
    assert_eq!(background.exit_code, None);
    assert_eq!(data.next().await.json_body()["background"], true);
    let failure = commands
        .run_argv(&["false"], Default::default())
        .await
        .unwrap();
    assert_eq!(failure.error.unwrap().evalue, "7");
    assert_eq!(failure.exit_code, Some(7));
    assert!(!failure.complete);
    data.next().await;
    assert!(matches!(
        commands
            .run_argv(&["printf", "partial"], Default::default())
            .await,
        Err(Error::Protocol(_))
    ));
    data.next().await;
}

#[tokio::test]
async fn invalid_argv_and_command_options_fail_before_http() {
    let mut data = Server::start(vec![Reply::bytes(vec![]), Reply::sse(SUCCESS)]).await;
    let sandbox = sandbox(&mut data, false).await;
    let commands = sandbox.commands();
    for argv in [
        vec![],
        vec![""],
        vec!["", "arg"],
        vec!["program\0"],
        vec!["program", "a\0b"],
    ] {
        assert!(matches!(
            commands.run_argv(&argv, Default::default()).await,
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            commands.run_argv_stream(&argv, Default::default()).await,
            Err(Error::InvalidConfig(_))
        ));
    }
    for options in [
        RunCommandOptions {
            gid: Some(1),
            ..Default::default()
        },
        RunCommandOptions {
            uid: Some(-1),
            ..Default::default()
        },
        RunCommandOptions {
            uid: Some(1),
            gid: Some(-1),
            ..Default::default()
        },
        RunCommandOptions {
            timeout: Some(Duration::from_nanos(1)),
            ..Default::default()
        },
        RunCommandOptions {
            timeout: Some(Duration::MAX),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            commands.run_argv(&["program"], options.clone()).await,
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            commands.run_argv_stream(&["program"], options).await,
            Err(Error::InvalidConfig(_))
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), data.requests.recv())
            .await
            .is_err()
    );
    // A whitespace executable is a literal name, not empty shell text.
    commands
        .run_argv(
            &[" "],
            RunCommandOptions {
                timeout: Some(Duration::ZERO),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        data.next().await.json_body(),
        json!({"argv":[" "], "background":false, "timeout":0})
    );
}
