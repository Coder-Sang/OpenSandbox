// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::{api::egress::*, egress::Egress, ConnectionConfig, Error, SandboxManager};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
struct Server {
    url: String,
    requests: mpsc::Receiver<Request>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(replies: Vec<(u16, Value, String)>, template: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = json!({"endpoint":format!("{url}/route"),"headers":{"OPENSANDBOX-EGRESS-AUTH":"endpoint-secret"}});
        let mut all = vec![
            ok(
                json!({"id":"box","entrypoint":[],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2026-01-01T01:00:00Z"}),
            ),
            (
                200,
                endpoint.clone(),
                if template {
                    "open-sandbox-origin: template\r\n".into()
                } else {
                    String::new()
                },
            ),
            ok(json!({})),
        ];
        if !template {
            all.push(ok(endpoint));
        }
        all.extend(replies);
        let (tx, requests) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            for (status, body, headers) in all {
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
                let head = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let request_headers: BTreeMap<_, _> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len = request_headers
                    .get("content-length")
                    .map(|v| v.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while bytes.len() < end + len {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                tx.send(Request {
                    method,
                    target,
                    headers: request_headers,
                    body: bytes[end..end + len].to_vec(),
                })
                .await
                .unwrap();
                let body = if status == 204 {
                    String::new()
                } else {
                    body.to_string()
                };
                socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            // Keep listening so unexpected requests are detected rather than refused.
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            tx.send(Request {
                method: "unexpected".into(),
                target: String::from_utf8_lossy(&buf[..n]).into_owned(),
                headers: BTreeMap::new(),
                body: Vec::new(),
            })
            .await
            .unwrap();
        });
        Self {
            url,
            requests,
            task,
        }
    }
    async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn egress(&mut self, template: bool) -> Egress {
        let manager = SandboxManager::new(
            ConnectionConfig::builder()
                .domain(&self.url)
                .api_key("control-secret")
                .build()
                .unwrap(),
        )
        .unwrap();
        let sandbox = manager.connect("box").await.unwrap();
        let egress = sandbox.egress().await.unwrap();
        for _ in 0..if template { 3 } else { 4 } {
            self.next().await;
        }
        egress
    }
    async fn no_request(&mut self) {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), self.requests.recv())
                .await
                .is_err()
        );
    }
}
fn ok(body: Value) -> (u16, Value, String) {
    (200, body, String::new())
}
fn state() -> Value {
    json!({"revision":1,"credentials":[],"bindings":[]})
}

#[tokio::test]
async fn vault_round_trip_and_revision_conflict() {
    let credential = json!({"name":"key","source":{"type":"inline","value":"vault-secret"}});
    let binding = json!({"name":"api","match":{"hosts":["api.example.com"]},"auth":{"type":"bearer","credential":"key"}});
    let meta = json!({"name":"key","sourceType":"inline","revision":1});
    let binding_meta = json!({"name":"api","revision":1,"auth":{"type":"bearer"}});
    let mut server = Server::start(
        vec![
            ok(state()),
            ok(state()),
            ok(state()),
            (
                409,
                json!({"code":"vault-secret","message":"vault-secret"}),
                "x-request-id: vault-secret\r\n".into(),
            ),
            ok(json!({"revision":1,"credentials":[meta.clone()]})),
            ok(meta),
            ok(json!({"revision":1,"bindings":[binding_meta.clone()]})),
            ok(binding_meta),
            (204, Value::Null, String::new()),
        ],
        false,
    )
    .await;
    let egress = server.egress(false).await;
    let create: CredentialVaultCreateRequest = serde_json::from_value(
        json!({"credentials":[credential.clone()],"bindings":[binding.clone()]}),
    )
    .unwrap();
    assert!(!format!("{create:?}").contains("vault-secret"));
    assert_eq!(egress.create(&create).await.unwrap().revision, 1);
    assert_eq!(egress.get().await.unwrap().revision, 1);
    let patch: CredentialVaultMutationRequest = serde_json::from_value(json!({"expectedRevision":1,"credentials":{"add":[],"replace":[credential],"delete":["old"]},"bindings":{"add":[binding],"replace":[],"delete":["old"]}})).unwrap();
    assert_eq!(egress.patch(&patch).await.unwrap().revision, 1);
    let error = egress.patch(&patch).await.unwrap_err();
    assert!(matches!(error, Error::Api { status: 409, .. }));
    assert!(!format!("{error:?} {error}").contains("vault-secret"));
    assert_eq!(egress.list_credentials().await.unwrap()[0].name, "key");
    assert_eq!(egress.get_credential("a/b?#%").await.unwrap().name, "key");
    assert_eq!(egress.list_bindings().await.unwrap()[0].name, "api");
    assert_eq!(egress.get_binding("a/b?#%").await.unwrap().name, "api");
    egress.delete().await.unwrap();
    for (method, path) in [
        ("POST", ""),
        ("GET", ""),
        ("PATCH", ""),
        ("PATCH", ""),
        ("GET", "/credentials"),
        ("GET", "/credentials/a%2Fb%3F%23%25"),
        ("GET", "/bindings"),
        ("GET", "/bindings/a%2Fb%3F%23%25"),
        ("DELETE", ""),
    ] {
        let request = server.next().await;
        assert_eq!(request.method, method);
        assert_eq!(request.target, format!("/route/credential-vault{path}"));
        assert_eq!(
            request.headers["opensandbox-egress-auth"],
            "endpoint-secret"
        );
        assert!(!request.headers.contains_key("open-sandbox-api-key"));
        if method == "PATCH" {
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                serde_json::to_value(&patch).unwrap()
            );
        }
        if method == "POST" {
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                serde_json::to_value(&create).unwrap()
            );
        }
        if method == "GET" || method == "DELETE" {
            assert!(request.body.is_empty());
        }
    }
}
#[tokio::test]
async fn template_rejected_locally() {
    let mut server = Server::start(vec![], true).await;
    let egress = server.egress(true).await;
    assert!(matches!(
        egress.create(&Default::default()).await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(egress.get().await, Err(Error::InvalidConfig(_))));
    assert!(matches!(
        egress.patch(&Default::default()).await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        egress.delete().await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        egress.list_credentials().await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        egress.get_credential("key").await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        egress.list_bindings().await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        egress.get_binding("api").await,
        Err(Error::InvalidConfig(_))
    ));
    server.no_request().await;
}
#[tokio::test]
async fn redirects_and_decode_errors_do_not_leak() {
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut server = Server::start(
        vec![
            (
                307,
                json!({"message":"vault-secret"}),
                format!(
                    "Location: http://{}/vault-secret\r\n",
                    destination.local_addr().unwrap()
                ),
            ),
            ok(json!({"revision":"vault-secret","credentials":[],"bindings":[]})),
            ok(state()),
        ],
        false,
    )
    .await;
    let egress = server.egress(false).await;
    let error = egress.create(&Default::default()).await.unwrap_err();
    assert!(matches!(error, Error::Api { status: 307, .. }));
    assert!(!format!("{error:?} {error}").contains("vault-secret"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
    let error = egress.get().await.unwrap_err();
    assert!(!format!("{error:?} {error}").contains("vault-secret"));
    egress.patch(&Default::default()).await.unwrap();
    server.next().await;
    server.next().await;
    assert_eq!(
        serde_json::from_slice::<Value>(&server.next().await.body).unwrap(),
        json!({})
    );
    for id in ["", " ", ".", ".."] {
        assert!(matches!(
            egress.get_credential(id).await,
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            egress.get_binding(id).await,
            Err(Error::InvalidConfig(_))
        ));
    }
    server.no_request().await;
}
