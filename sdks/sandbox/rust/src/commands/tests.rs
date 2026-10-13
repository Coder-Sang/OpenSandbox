// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

pub(crate) async fn serve_once(
    response: Vec<u8>,
) -> (
    crate::transport::Transport,
    tokio::task::JoinHandle<Vec<u8>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0; 4096];
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let mut buffer = [0; 4096];
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        socket.write_all(&response).await.unwrap();
        request
    });
    let config = crate::ConnectionConfig::builder()
        .domain(format!("http://{address}/prefix"))
        .header("x-endpoint-token", "test-token")
        .build()
        .unwrap();
    (crate::transport::Transport::new(&config).unwrap(), task)
}

#[tokio::test]
async fn stream_http_preserves_path_headers_and_timeout_units() {
    let body =
        "data: {\"type\":\"init\",\"text\":\"cmd\"}\n\ndata: {\"type\":\"execution_complete\"}\n\n";
    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let (transport, task) = serve_once(response.into_bytes()).await;
    let execution = Commands::new(transport)
        .run(
            "echo hi",
            RunCommandOptions {
                timeout: Some(std::time::Duration::from_millis(250)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(execution.id.as_deref(), Some("cmd"));
    let request = String::from_utf8(task.await.unwrap()).unwrap();
    assert!(request.starts_with("POST /prefix/v1/command HTTP/1.1"));
    assert!(request.contains("x-endpoint-token: test-token"));
    assert!(request.contains("\"timeout\":250"));
}

use super::*;
#[tokio::test]
async fn error_only_http_preserves_execution_failure() {
    for value in ["7", "execution timed out", "executable not found"] {
        let body = format!("data: {{\"type\":\"init\",\"text\":\"cmd\"}}\n\ndata: {{\"type\":\"stdout\",\"text\":\"before failure\"}}\n\ndata: {}\n\n", serde_json::json!({"type":"error","error":{"ename":"CommandError","evalue":value}}));
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (transport, task) = serve_once(response.into_bytes()).await;
        let execution = Commands::new(transport)
            .run("false", Default::default())
            .await
            .unwrap();
        assert_eq!(execution.id.as_deref(), Some("cmd"));
        assert_eq!(execution.logs.stdout[0].text, "before failure");
        assert_eq!(execution.error.unwrap().evalue, value);
        assert_eq!(execution.exit_code, value.parse::<i32>().ok());
        assert!(!execution.complete);
        task.await.unwrap();
    }
}

#[test]
fn command_mapping_and_validation() {
    let options = RunCommandOptions {
        timeout: Some(std::time::Duration::from_millis(1500)),
        ..Default::default()
    };
    assert_eq!(
        command_body("echo hi", options).unwrap().timeout,
        Some(1500)
    );
    assert!(command_body("", Default::default()).is_err());
    assert!(command_body(
        "true",
        RunCommandOptions {
            gid: Some(1),
            ..Default::default()
        }
    )
    .is_err());
    assert!(timeout_ms(Some(std::time::Duration::from_nanos(1))).is_err());
    assert_eq!(
        timeout_ms(Some(std::time::Duration::ZERO)).unwrap(),
        Some(0)
    );
    assert!(timeout_ms(Some(std::time::Duration::MAX)).is_err());
}
#[tokio::test]
async fn aggregation_and_premature_eof() {
    let source = [
        r#"{"type":"init","text":"cmd"}"#,
        r#"{"type":"stdout","text":"hello"}"#,
        r#"{"type":"error","error":{"ename":"exit","evalue":"7"}}"#,
        r#"{"type":"execution_complete"}"#,
    ];
    let events = source
        .into_iter()
        .map(|s| Ok(serde_json::from_str(s).unwrap()));
    let result = consume(Box::pin(futures_util::stream::iter(events)), false)
        .await
        .unwrap();
    assert_eq!(result.id.as_deref(), Some("cmd"));
    assert_eq!(result.logs.stdout[0].text, "hello");
    assert_eq!(result.exit_code, Some(7));
    assert!(consume(Box::pin(futures_util::stream::empty()), false)
        .await
        .is_err());
}
