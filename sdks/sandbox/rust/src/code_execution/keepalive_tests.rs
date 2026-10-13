// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

async fn request(socket: &mut TcpStream) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            assert!(bytes.is_empty());
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() == end + 4 + length {
                return Some(bytes);
            }
        }
    }
}

#[tokio::test]
async fn reused_connection_stream_outlives_previous_normal_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let reused = Arc::new(AtomicUsize::new(0));
    let server_reused = reused.clone();
    let server = tokio::spawn(async move {
        let mut workers = JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let reused = server_reused.clone();
            workers.spawn(async move {
                let mut count = 0;
                while let Some(bytes) = request(&mut socket).await {
                    if count > 0 {
                        reused.fetch_add(1, Ordering::Relaxed);
                    }
                    count += 1;
                    if bytes.starts_with(b"GET ") {
                        let body = r#"{"id":"ctx","language":"python"}"#;
                        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    } else {
                        assert!(bytes.starts_with(b"POST "));
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                        // Exceed the ordinary request deadline on an actually reused socket.
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        let body = "data: {\"type\":\"stdout\",\"text\":\"你好\"}\n\ndata: {\"type\":\"error\",\"error\":{\"ename\":\"ValueError\",\"evalue\":\"bad\",\"traceback\":[\"trace\"]}}\n\n";
                        socket.write_all(format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    }
                }
            });
        }
    });
    let config = crate::ConnectionConfig::builder()
        .domain(format!("http://{address}"))
        .request_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let codes = Codes::new(crate::transport::Transport::new(&config).unwrap());
    for _ in 0..5 {
        let context = codes.get_context("ctx").await.unwrap();
        let execution = codes
            .run(
                "cell",
                RunCodeOptions {
                    context: Some(context),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(execution.logs.stdout[0].text, "你好");
        assert_eq!(execution.error.unwrap().ename, "ValueError");
        assert!(!execution.complete);
        assert_eq!(execution.exit_code, None);
    }
    assert!(
        reused.load(Ordering::Relaxed) > 0,
        "fixture must exercise connection reuse"
    );
    server.abort();
}
