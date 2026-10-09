// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use opensandbox::{ConnectionConfig, SandboxManager};
use opensandbox_code_interpreter::{
    CodeInterpreter, CodeInterpreterOptions, RUNTIME_CHECK_COMMAND,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn fixture(error: bool) -> (opensandbox::Sandbox, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let endpoint = format!("{{\"endpoint\":\"http://{address}/route\",\"headers\":{{\"x-endpoint-token\":\"secret\"}}}}");
        let execution = if error {
            "data: {\"type\":\"error\",\"error\":{\"ename\":\"CommandError\",\"evalue\":\"1\"}}\n\n"
        } else {
            "data: {\"type\":\"execution_complete\"}\n\n"
        };
        let mut requests = Vec::new();
        for body in [&endpoint, "", execution] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < end + length {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(format!("http://{address}"))
            .build()
            .unwrap(),
    )
    .unwrap();
    (manager.connect_unready("sandbox").await.unwrap(), task)
}

#[tokio::test]
async fn strict_health_checks_ping_then_actual_runtime_command() {
    for error in [false, true] {
        let (sandbox, task) = fixture(error).await;
        let interpreter = CodeInterpreter::create(
            sandbox,
            CodeInterpreterOptions {
                skip_health_check: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(interpreter.is_healthy().await, !error);
        let requests = task.await.unwrap();
        assert!(requests[1].starts_with("GET /route/ping "));
        assert!(requests[2].starts_with("POST /route/command "));
        assert!(requests[2].contains(RUNTIME_CHECK_COMMAND));
        assert!(requests[2].contains("x-endpoint-token: secret"));
    }
}

#[tokio::test]
async fn create_readiness_uses_an_explicit_total_budget() {
    let (sandbox, task) = fixture(true).await;
    let result = CodeInterpreter::create(
        sandbox,
        CodeInterpreterOptions {
            ready_timeout: Duration::from_millis(30),
            polling_interval: Duration::from_secs(5),
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(opensandbox::Error::ReadyTimeout { .. })
    ));
    task.await.unwrap();
}
