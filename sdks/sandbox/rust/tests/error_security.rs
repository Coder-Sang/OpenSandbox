// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use futures_util::StreamExt;
use opensandbox::{ConnectionConfig, Error, SandboxManager};
use std::{error::Error as _, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const SECRET: &str = "signed-route-secret";
fn assert_redacted(error: &Error) {
    assert!(!format!("{error:?} {error}").contains(SECRET));
    let Error::Transport(transport) = error else {
        panic!("expected transport error: {error:?}");
    };
    assert!(transport.url().is_none());
    assert!(!format!("{transport:?} {transport}").contains(SECRET));
    let mut source = error.source();
    while let Some(cause) = source {
        assert!(!format!("{cause:?} {cause}").contains(SECRET));
        source = cause.source();
    }
}
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
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
    let len = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    while bytes.len() < end + len {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn signed_endpoint_midstream_error_has_no_url_or_secret_source() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = format!("{{\"endpoint\":\"http://{address}/{SECRET}\",\"headers\":{{}}}}");
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with(&format!("POST /{SECRET}/command ")));
        // A chunked SSE body terminates without the required zero chunk.
        let frame = "data: {\"type\":\"init\",\"text\":\"run\"}\n\n";
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{frame}\r\n",frame.len()).as_bytes()).await.unwrap();
    });
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(format!("http://{address}"))
            .build()
            .unwrap(),
    )
    .unwrap();
    let sandbox = manager.connect_unready("box").await.unwrap();
    let mut stream = sandbox
        .commands()
        .run_stream("true", Default::default())
        .await
        .unwrap();
    let mut failure = None;
    while let Some(event) = stream.next().await {
        if let Err(error) = event {
            failure = Some(error);
            break;
        }
    }
    assert_redacted(&failure.expect("truncated SSE must fail"));
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_classification_survives_url_redaction() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        std::future::pending::<()>().await;
    });
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(format!("http://{address}/{SECRET}"))
            .request_timeout(Duration::from_millis(50))
            .build()
            .unwrap(),
    )
    .unwrap();
    let error = manager.get("box").await.unwrap_err();
    assert_redacted(&error);
    assert!(matches!(error,Error::Transport(e) if e.is_timeout()));
    server.abort();
}

#[tokio::test]
async fn connect_classification_survives_global_conversion() {
    // Port zero cannot be a listening server, avoiding ephemeral port reuse races.
    let raw = reqwest::Client::new()
        .get(format!("http://127.0.0.1:0/{SECRET}"))
        .send()
        .await
        .unwrap_err();
    assert!(raw.is_connect());
    assert!(raw.url().is_some());
    let error: Error = raw.into();
    assert_redacted(&error);
    assert!(matches!(error,Error::Transport(e) if e.is_connect()));
}
