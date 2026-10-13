// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

use opensandbox::{ConnectionConfig, Sandbox, SandboxManager};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

pub struct Reply {
    pub status: u16,
    pub headers: String,
    pub body: Vec<u8>,
    pub delay: Duration,
}
impl Reply {
    pub fn sse(body: &str) -> Self {
        Self {
            headers: "content-type: text/event-stream\r\n".into(),
            ..Self::bytes(body.as_bytes().to_vec())
        }
    }
    pub fn json(body: serde_json::Value) -> Self {
        Self::bytes(body.to_string().into_bytes())
    }
    pub fn bytes(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: String::new(),
            body,
            delay: Duration::ZERO,
        }
    }
}
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
impl Request {
    pub fn url(&self) -> url::Url {
        url::Url::parse(&format!("http://fixture{}", self.target)).unwrap()
    }
    pub fn json_body(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}
pub struct Server {
    pub url: String,
    pub requests: mpsc::Receiver<Request>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "request ended before headers");
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let headers: BTreeMap<_, _> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len = headers
                    .get("content-length")
                    .map(|s| s.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while bytes.len() < end + len {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0, "request ended before body");
                    bytes.extend_from_slice(&buf[..n]);
                }
                tx.send(Request {
                    method,
                    target,
                    headers,
                    body: bytes[end..end + len].to_vec(),
                })
                .await
                .unwrap();
                tokio::time::sleep(reply.delay).await;
                let head = format!(
                    "HTTP/1.1 {} Response\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                    reply.status,
                    reply.body.len(),
                    reply.headers
                );
                if socket.write_all(head.as_bytes()).await.is_ok() {
                    let _ = socket.write_all(&reply.body).await;
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    pub async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    pub fn manager(&self) -> SandboxManager {
        SandboxManager::new(
            ConnectionConfig::builder()
                .disable_metrics(true)
                .domain(format!("{}/deploy", self.url))
                .api_key("control-secret")
                .build()
                .unwrap(),
        )
        .unwrap()
    }
}
pub fn running() -> Reply {
    Reply::json(
        json!({"id":"box","entrypoint":["tail","-f","/dev/null"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2026-01-01T01:00:00Z"}),
    )
}
pub fn endpoint(url: &str) -> Reply {
    Reply::json(
        json!({"endpoint":format!("{url}/route"),"headers":{"x-endpoint-token":"endpoint-secret","Open-Sandbox-Api-Key":"endpoint-control"}}),
    )
}

pub fn route(proxy: bool) -> &'static str {
    if proxy {
        "/deploy/v1/sandboxes/box/proxy/44772"
    } else {
        "/route"
    }
}

/// Attach through the public SDK, checking control/data credentials and prefixes.
/// The data server's first reply must be an empty successful ping response.
pub async fn sandbox(data: &mut Server, proxy: bool) -> Sandbox {
    let mut control = Server::start(vec![
        running(),
        Reply::json(json!({
            "endpoint": format!("{}{}", data.url, route(proxy)),
            "headers": {
                "x-endpoint-token": "endpoint-secret",
                "Open-Sandbox-Api-Key": "endpoint-control"
            }
        })),
    ])
    .await;
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .disable_metrics(true)
            .domain(format!("{}/deploy", control.url))
            .api_key("control-secret")
            .header("x-custom", "custom")
            .use_server_proxy(proxy)
            .build()
            .unwrap(),
    )
    .unwrap();
    let sandbox = manager.connect("box").await.unwrap();
    assert_eq!(control.next().await.target, "/deploy/v1/sandboxes/box");
    assert_eq!(
        control.next().await.target,
        format!("/deploy/v1/sandboxes/box/endpoints/44772?use_server_proxy={proxy}")
    );
    let ping = data.next().await;
    assert_eq!(ping.target, format!("{}/ping", route(proxy)));
    assert_headers(&ping, proxy);
    sandbox
}

pub fn assert_headers(request: &Request, proxy: bool) {
    assert_eq!(request.headers["x-custom"], "custom");
    assert_eq!(request.headers["x-endpoint-token"], "endpoint-secret");
    assert_eq!(
        request
            .headers
            .get("open-sandbox-api-key")
            .map(String::as_str),
        if proxy {
            Some("endpoint-control")
        } else {
            None
        }
    );
}
