use futures_util::StreamExt;
use opensandbox::{
    diagnostics::{DiagnosticContent, DiagnosticPayload},
    lifecycle::EndpointOptions,
    ConnectionConfig, Error, SandboxManager,
};
use serde_json::{json, Value};
use std::time::{Duration, UNIX_EPOCH};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

struct Server {
    url: String,
    requests: mpsc::Receiver<String>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.windows(4).any(|b| b == b"\r\n\r\n") {
                        break;
                    }
                }
                tx.send(String::from_utf8(bytes).unwrap()).await.unwrap();
                let head = format!(
                    "HTTP/1.1 {status} Reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                // Fragment all UTF-8 and SSE boundaries deliberately.
                for byte in body.as_bytes() {
                    if socket.write_all(&[*byte]).await.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    fn manager(&self, proxy: bool) -> SandboxManager {
        SandboxManager::new(
            ConnectionConfig::builder()
                .disable_metrics(true)
                .domain(format!("{}/deploy", self.url))
                .api_key("control-secret")
                .use_server_proxy(proxy)
                .build()
                .unwrap(),
        )
        .unwrap()
    }
    async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn inline() -> Value {
    json!({"sandboxId":"a/b?#%", "kind":"logs", "scope":"all", "delivery":"inline", "contentType":"text/plain; charset=utf-8", "content":"你好\nsecret", "truncated":true, "warnings":["retention gap"]})
}

#[tokio::test]
async fn signed_endpoint_seconds_escaping_and_legacy_defaults() {
    let mut server = Server::start(vec![
        (
            200,
            json!({"endpoint":"https://signed.example/path?token=secret","headers":{}}).to_string(),
        ),
        (
            200,
            json!({"endpoint":"localhost:80","headers":{}}).to_string(),
        ),
    ])
    .await;
    let manager = server.manager(true);
    let endpoint = manager
        .endpoint_with_options(
            "a/b?#%",
            8080,
            EndpointOptions {
                use_server_proxy: Some(false),
                expires_at: Some(UNIX_EPOCH + Duration::from_secs(1_800_000_123)),
            },
        )
        .await
        .unwrap();
    assert!(endpoint.endpoint.contains("token=secret"));
    let request = server.next().await;
    assert!(request.starts_with("GET /deploy/v1/sandboxes/a%2Fb%3F%23%25/endpoints/8080?use_server_proxy=false&expires=1800000123 "));
    manager.endpoint("safe", 80).await.unwrap();
    let request = server.next().await;
    assert!(request.contains("use_server_proxy=true"));
    assert!(!request.contains("expires="));
}
#[tokio::test]
async fn create_unready_preserves_setup_id_and_avoids_readiness_calls() {
    let created = json!({"id":"box","entrypoint":["tail","-f","/dev/null"],"status":{"state":"Pending"},"createdAt":"2026-01-01T00:00:00Z"});
    let mut server = Server::start(vec![
        (200, created.to_string()),
        (
            200,
            json!({"endpoint":"https://example.invalid/data","headers":{}}).to_string(),
        ),
        (200, created.to_string()),
        (
            500,
            json!({"code":"ENDPOINT_FAILURE","message":"not published"}).to_string(),
        ),
    ])
    .await;
    let manager = server.manager(false);
    let sandbox = manager
        .create_unready(opensandbox::CreateSandboxOptions::image("ubuntu"))
        .await
        .unwrap();
    assert_eq!(sandbox.id(), "box");
    assert!(server
        .next()
        .await
        .starts_with("POST /deploy/v1/sandboxes "));
    assert!(server
        .next()
        .await
        .starts_with("GET /deploy/v1/sandboxes/box/endpoints/44772?"));
    match manager
        .create_unready(opensandbox::CreateSandboxOptions::image("ubuntu"))
        .await
    {
        Err(Error::SandboxSetup { sandbox_id, source }) => {
            assert_eq!(sandbox_id, "box");
            assert!(matches!(*source, Error::Api { status: 500, .. }));
        }
        _ => panic!("expected setup failure with retained ID"),
    }
    assert!(server.next().await.starts_with("POST "));
    assert!(server.next().await.starts_with("GET "));
}

#[tokio::test]
async fn endpoint_epoch_and_remote_error() {
    let mut server = Server::start(vec![(
        400,
        json!({"code":"INVALID_EXPIRY","message":"expired"}).to_string(),
    )])
    .await;
    assert!(matches!(
        server
            .manager(false)
            .endpoint_with_options(
                "id",
                80,
                EndpointOptions {
                    expires_at: Some(UNIX_EPOCH),
                    ..Default::default()
                }
            )
            .await,
        Err(Error::Api { status: 400, .. })
    ));
    assert!(server.next().await.contains("expires=0"));
}

#[tokio::test]
async fn endpoint_invalid_options_are_local_errors() {
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .disable_metrics(true)
            .domain("http://127.0.0.1:1")
            .build()
            .unwrap(),
    )
    .unwrap();
    for options in [
        EndpointOptions {
            use_server_proxy: Some(true),
            expires_at: Some(UNIX_EPOCH),
        },
        EndpointOptions {
            expires_at: Some(UNIX_EPOCH - Duration::from_secs(1)),
            ..Default::default()
        },
        EndpointOptions {
            expires_at: Some(UNIX_EPOCH + Duration::from_millis(1)),
            ..Default::default()
        },
    ] {
        assert!(matches!(
            manager.endpoint_with_options("id", 80, options).await,
            Err(Error::InvalidConfig(_))
        ));
    }
    assert!(matches!(
        manager.endpoint("id", 0).await,
        Err(Error::InvalidConfig(_))
    ));
    assert!(matches!(
        manager.diagnostic_logs("id", "  ").await,
        Err(Error::InvalidConfig(_))
    ));
}
#[tokio::test]
async fn diagnostics_inline_url_and_errors_never_download() {
    let url = json!({"sandboxId":"id","kind":"events","scope":"runtime","delivery":"url","contentType":"text/plain","contentUrl":"https://example.invalid/logs?token=secret","expiresAt":"2026-10-10T00:00:00Z","contentLength":12,"truncated":false});
    let mut server = Server::start(vec![
        (200, inline().to_string()),
        (200, url.to_string()),
        (
            501,
            json!({"code":"NOT_IMPLEMENTED","message":"not supported"}).to_string(),
        ),
        (200, "legacy plaintext".into()),
    ])
    .await;
    let manager = server.manager(false);
    let result = manager
        .diagnostic_logs("a/b?#%", "all &?=%你好")
        .await
        .unwrap();
    assert!(
        matches!(&result.payload, DiagnosticPayload::Inline { content } if content == "你好\nsecret")
    );
    assert!(result.truncated);
    assert_eq!(result.warnings, ["retention gap"]);
    let request = server.next().await;
    let target = request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let parsed = url::Url::parse(&format!("http://localhost{target}")).unwrap();
    assert_eq!(
        parsed.query_pairs().find(|(k, _)| k == "scope").unwrap().1,
        "all &?=%你好"
    );
    assert!(parsed.path().contains("a%2Fb%3F%23%25"));
    assert!(request
        .to_lowercase()
        .contains("open-sandbox-api-key: control-secret"));
    let result = manager.diagnostic_events("id", "runtime").await.unwrap();
    assert!(matches!(result.payload, DiagnosticPayload::Url { .. }));
    server.next().await;
    assert!(matches!(
        manager.diagnostic_logs("id", "all").await,
        Err(Error::Api { status: 501, .. })
    ));
    server.next().await;
    assert!(manager.diagnostic_logs("id", "all").await.is_err());
    server.next().await;
}
#[test]
fn malformed_diagnostic_delivery_and_urls_are_rejected() {
    for patch in [
        json!({"content":null}),
        json!({"contentUrl":"https://example.com"}),
        json!({"expiresAt":"2026-01-01T00:00:00Z"}),
        json!({"contentLength":-1}),
        json!({"delivery":"other"}),
    ] {
        let mut value = inline();
        for (k, v) in patch.as_object().unwrap() {
            value[k] = v.clone();
        }
        let wire: opensandbox::api::diagnostic::DiagnosticContentResponse =
            serde_json::from_value(value).unwrap();
        assert!(DiagnosticContent::try_from(wire).is_err());
    }
    let valid_url = json!({"sandboxId":"id","kind":"events","scope":"all","delivery":"url","contentType":"text/plain","contentUrl":"https://example.com/logs","expiresAt":"2026-10-10T00:00:00Z","truncated":false});
    for patch in [
        json!({"contentUrl":null}),
        json!({"expiresAt":null}),
        json!({"expiresAt":"invalid"}),
        json!({"content":"forbidden"}),
        json!({"kind":"unknown"}),
    ] {
        let mut value = valid_url.clone();
        for (key, value_patch) in patch.as_object().unwrap() {
            value[key] = value_patch.clone();
        }
        let wire: opensandbox::api::diagnostic::DiagnosticContentResponse =
            serde_json::from_value(value).unwrap();
        assert!(DiagnosticContent::try_from(wire).is_err());
    }
    let descriptor = DiagnosticContent::try_from(
        serde_json::from_value::<opensandbox::api::diagnostic::DiagnosticContentResponse>(
            valid_url,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!format!("{:?}", descriptor.payload).contains("example.com"));
    for address in [
        "file:///etc/passwd",
        "https://user:secret@example.com",
        "https://example.com/#secret",
        "not-a-url",
    ] {
        let wire: opensandbox::api::diagnostic::DiagnosticContentResponse = serde_json::from_value(json!({"sandboxId":"id","kind":"events","scope":"all","delivery":"url","contentType":"text/plain","contentUrl":address,"expiresAt":"2026-10-10T00:00:00Z","truncated":false})).unwrap();
        assert!(DiagnosticContent::try_from(wire).is_err());
    }
}

#[tokio::test]
async fn metrics_watch_fragmentation_json_lines_eof_and_errors() {
    let metric = json!({"cpu_count":2,"cpu_used_pct":10,"mem_total_mib":100,"mem_used_mib":20,"timestamp":42});
    let body =
        format!(": heartbeat\r\nevent: metrics\r\ndata: {metric}\r\n\r\n{metric}\ndata: {metric}");
    let mut data = Server::start(vec![
        (200, body),
        (200, "data: {broken}\n\n".into()),
        (
            500,
            json!({"code":"FAIL","message":"metrics unavailable"}).to_string(),
        ),
    ])
    .await;
    let mut server = Server::start(vec![(200,json!({"endpoint":format!("{}/data",data.url),"headers":{"X-Endpoint-Key":"endpoint-secret"}}).to_string())]).await;
    let sandbox = server.manager(false).connect_unready("id").await.unwrap();
    server.next().await;
    let mut stream = sandbox.metrics().watch().await.unwrap();
    for _ in 0..3 {
        assert_eq!(stream.next().await.unwrap().unwrap().timestamp, 42);
    }
    assert!(stream.next().await.is_none());
    let request = data.next().await.to_lowercase();
    assert!(request.starts_with("get /data/metrics/watch "));
    assert!(request.contains("accept: text/event-stream"));
    assert!(request.contains("x-endpoint-key: endpoint-secret"));
    assert!(!request.contains("control-secret"));
    let mut stream = sandbox.metrics().watch().await.unwrap();
    assert!(stream.next().await.unwrap().is_err());
    data.next().await;
    assert!(matches!(
        sandbox.metrics().watch().await,
        Err(Error::Api { status: 500, .. })
    ));
    data.next().await;
}
