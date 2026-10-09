use futures_util::StreamExt;
use opensandbox::{ConnectionConfig, SandboxManager};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

async fn request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let len = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + len {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}
async fn reply(socket: &mut TcpStream, body: &str) {
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
}

#[tokio::test]
async fn dropping_live_stream_closes_body_without_implicitly_interrupting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        assert!(request(&mut socket)
            .await
            .starts_with("GET /v1/sandboxes/box "));
        reply(&mut socket,&json!({"id":"box","entrypoint":["tail"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z"}).to_string()).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        request(&mut socket).await;
        reply(
            &mut socket,
            &json!({"endpoint":format!("http://{address}/exec"),"headers":{}}).to_string(),
        )
        .await;
        let (mut socket, _) = listener.accept().await.unwrap();
        assert!(request(&mut socket).await.starts_with("GET /exec/ping "));
        reply(&mut socket, "").await;
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(request(&mut stream)
            .await
            .starts_with("POST /exec/command "));
        // The stream is intentionally unfinished and starts after the ordinary request timeout.
        tokio::time::sleep(Duration::from_millis(100)).await;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let init = "data: {\"type\":\"init\",\"text\":\"running-id\"}\n\n";
        stream
            .write_all(format!("{:x}\r\n{init}\r\n", init.len()).as_bytes())
            .await
            .unwrap();
        let mut byte = [0; 1];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0, "drop must release the unfinished HTTP response");
        closed_tx.send(()).unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        // If dropping implicitly interrupted, a second DELETE would appear here.
        assert!(request(&mut socket)
            .await
            .starts_with("DELETE /exec/command?id=running-id "));
        reply(&mut socket, "").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(format!("http://{address}"))
            .request_timeout(Duration::from_millis(50))
            .build()
            .unwrap(),
    )
    .unwrap();
    let sandbox = manager.connect("box").await.unwrap();
    let commands = sandbox.commands();
    let mut stream = commands
        .run_stream("sleep 10", Default::default())
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(event.text.as_deref(), Some("running-id"));
    drop(stream);
    closed_rx.await.unwrap();
    commands.interrupt("running-id").await.unwrap();
    task.await.unwrap();
}
