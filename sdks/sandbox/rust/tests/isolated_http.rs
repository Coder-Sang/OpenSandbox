use opensandbox::{isolated::*, ConnectionConfig, Sandbox, SandboxManager};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test]
async fn cancelled_run_once_deletes_created_session() {
    let mut data = Server::start(vec![
        reply(json!({"session_id":"cancelled"})),
        (
            200,
            "content-type: text/event-stream\r\nx-test-held: true\r\n".into(),
            "data: {\"type\":\"init\",\"text\":\"run\"}\n\n".into(),
        ),
        (204, String::new(), String::new()),
    ])
    .await;
    let sandbox = sandbox(&mut data).await;
    let task = tokio::spawn(async move {
        sandbox
            .isolated()
            .run_once("sleep 1000", create(), Default::default())
            .await
    });
    assert!(data
        .next()
        .await
        .starts_with("POST /route/v1/isolated/session "));
    assert!(data
        .next()
        .await
        .starts_with("POST /route/v1/isolated/session/cancelled/run "));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(data
        .next()
        .await
        .starts_with("DELETE /route/v1/isolated/session/cancelled "));
}
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
    async fn start(mut replies: Vec<(u16, String, String)>) -> Self {
        // Dataplane fixtures use the token header, control-plane fixtures do not.
        if !replies
            .first()
            .is_some_and(|(_, _, body)| body.contains("\"entrypoint\""))
        {
            replies.insert(0, (200, String::new(), String::new()));
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let end = loop {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let length = String::from_utf8_lossy(&bytes[..end])
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
                tx.send(String::from_utf8_lossy(&bytes).into_owned())
                    .await
                    .unwrap();
                let held = headers.contains("x-test-held: true");
                let length = if held { 1_000_000 } else { body.len() };
                let response = format!("HTTP/1.1 {status} Response\r\nContent-Length: {length}\r\nConnection: close\r\n{headers}\r\n{body}");
                socket.write_all(response.as_bytes()).await.unwrap();
                if held {
                    tokio::spawn(async move {
                        let mut byte = [0];
                        let _ = socket.read(&mut byte).await;
                    });
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn reply(value: Value) -> (u16, String, String) {
    (
        200,
        "content-type: application/json\r\n".into(),
        value.to_string(),
    )
}
async fn sandbox(data: &mut Server) -> Sandbox {
    let control = Server::start(vec![
        reply(json!({"id":"box","entrypoint":["tail","-f","/dev/null"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2026-01-01T01:00:00Z"})),
        reply(json!({"endpoint":format!("{}/route",data.url),"headers":{"x-endpoint-token":"secret"}})),
    ]).await;
    let sandbox = SandboxManager::new(
        ConnectionConfig::builder()
            .domain(&control.url)
            .build()
            .unwrap(),
    )
    .unwrap()
    .connect("box")
    .await
    .unwrap();
    assert!(data.next().await.starts_with("GET /route/ping "));
    sandbox
}
fn create() -> CreateIsolatedSessionOptions {
    CreateIsolatedSessionOptions {
        workspace: IsolatedWorkspaceSpec {
            path: "/workspace".into(),
            ..Default::default()
        },
        idle_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    }
}
fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn cursor_bounds_and_sensitive_errors() {
    let secret = "private-env-command-secret";
    let mut data = Server::start(vec![
        reply(json!({"session_id":"cursor"})),
        (200, "EXECD-ISOLATED-TAIL-CURSOR: -1\r\n".into(), "é".into()),
        (
            200,
            "EXECD-ISOLATED-TAIL-CURSOR: 18446744073709551615\r\n".into(),
            "é".into(),
        ),
        (200, String::new(), "x".into()),
        (400, String::new(), secret.into()),
        (400, String::new(), secret.into()),
        (400, String::new(), secret.into()),
    ])
    .await;
    let sandbox = sandbox(&mut data).await;
    let service = sandbox.isolated();
    let session = service.create(create()).await.unwrap();
    data.next().await;
    assert!(session.logs("r", Some(u64::MAX)).await.is_err());
    assert_eq!(session.logs("r", Some(3)).await.unwrap().cursor, 5);
    data.next().await;
    assert_eq!(session.logs("r", None).await.unwrap().cursor, 2);
    data.next().await;
    assert!(session.logs("r", Some(i64::MAX as u64)).await.is_err());
    data.next().await;
    let error = service.create(create()).await.err().unwrap();
    assert!(!format!("{error:?} {error}").contains(secret));
    data.next().await;
    let error = session.run(secret, Default::default()).await.unwrap_err();
    assert!(!format!("{error:?} {error}").contains(secret));
    data.next().await;
    let error = session
        .run_background(secret, Default::default())
        .await
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(secret));
    data.next().await;
}

#[tokio::test]
async fn create_run_background_status_logs_files_and_delete() {
    let mut data = Server::start(vec![
        reply(json!({"session_id":"s/a ?#","created_at":"now"})),
        (200,"content-type: text/event-stream\r\n".into(),"data: {\"type\":\"init\",\"text\":\"run\"}\n\ndata: {\"type\":\"stdout\",\"text\":\"ok\"}\n\ndata: {\"type\":\"execution_complete\"}\n\n".into()),
        (202,"content-type: application/json\r\n".into(),json!({"session_id":"s/a ?#","run_id":"r/a","started_at":"now"}).to_string()),
        reply(json!({"session_id":"s/a ?#","run_id":"r/a","running":false,"exit_code":0,"started_at":"now"})),
        (200,"EXECD-ISOLATED-TAIL-CURSOR: invalid\r\n".into(),"你好".into()),
        (200,String::new(),"file".into()),
        (204,String::new(),String::new()),
    ]).await;
    let sandbox = sandbox(&mut data).await;
    let session = sandbox.isolated().create(create()).await.unwrap();
    let request = data.next().await;
    assert!(request.starts_with("POST /route/v1/isolated/session "));
    assert!(request.contains("x-endpoint-token: secret"));
    assert_eq!(body(&request)["idle_timeout_seconds"], 60);
    let result = session
        .run(
            "echo ok",
            IsolatedRunOptions {
                timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    let request = data.next().await;
    assert!(request.starts_with("POST /route/v1/isolated/session/s%2Fa%20%3F%23/run "));
    assert_eq!(body(&request)["timeout_seconds"], 2);
    session
        .run_background(
            "sleep 1",
            IsolatedRunOptions {
                timeout: Some(Duration::from_nanos(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request = data.next().await;
    assert_eq!(body(&request)["background"], true);
    assert!(body(&request).get("timeout_seconds").is_none());
    assert!(!session.status("r/a").await.unwrap().running);
    assert!(data
        .next()
        .await
        .starts_with("GET /route/v1/isolated/session/s%2Fa%20%3F%23/runs/r%2Fa "));
    assert_eq!(session.logs("r/a", Some(4)).await.unwrap().cursor, 10);
    assert!(data.next().await.contains("/runs/r%2Fa/logs?cursor=4 "));
    assert_eq!(session.files().read_text("/a b").await.unwrap(), "file");
    assert!(data.next().await.contains("/files/download?path=%2Fa+b "));
    session.delete().await.unwrap();
    assert!(data
        .next()
        .await
        .starts_with("DELETE /route/v1/isolated/session/s%2Fa%20%3F%23 "));
}

#[tokio::test]
async fn older_attach_capabilities_list_and_validation() {
    let mut data = Server::start(vec![
        reply(json!({"status":"active"})),
        reply(json!({"available":true})),
        reply(json!({"sessions":[]})),
    ])
    .await;
    let sandbox = sandbox(&mut data).await;
    let isolated = sandbox.isolated();
    assert!(isolated.attach("..").await.is_err());
    let session = isolated.attach("old").await.unwrap();
    assert_eq!(session.session_id(), "old");
    assert!(!session
        .info()
        .additional_properties
        .contains_key("workspace"));
    data.next().await;
    let caps = isolated.capabilities().await.unwrap();
    assert_eq!(caps.userns_available, Some(false));
    data.next().await;
    assert!(isolated.list().await.unwrap().is_empty());
    data.next().await;
    assert!(session
        .run(
            "true",
            IsolatedRunOptions {
                timeout: Some(Duration::from_millis(1)),
                ..Default::default()
            }
        )
        .await
        .is_err());
    assert!(session.status("..").await.is_err());
    assert!(session.run(" ", Default::default()).await.is_err());
    let mut options = create();
    options.idle_timeout = Some(Duration::MAX);
    assert!(isolated.create(options).await.is_err());
    let mut options = create();
    options.uid = Some(i64::from(u32::MAX) + 1);
    assert!(isolated.create(options).await.is_err());
}

#[tokio::test]
async fn run_once_preserves_run_error_and_exposes_cleanup_error() {
    for run_status in [200, 500] {
        let mut data = Server::start(vec![
            reply(json!({"session_id":"once"})),
            (
                run_status,
                "content-type: text/event-stream\r\n".into(),
                if run_status == 200 {
                    "data: {\"type\":\"stdout\",\"text\":\"truncated\"}\n\n".into()
                } else {
                    "failed".into()
                },
            ),
            (500, String::new(), "cleanup failed".into()),
        ])
        .await;
        let sandbox = sandbox(&mut data).await;
        let result = sandbox
            .isolated()
            .run_once("false", create(), Default::default())
            .await
            .unwrap();
        assert!(result.execution.is_err());
        assert!(result.cleanup.is_err());
        data.next().await;
        data.next().await;
        assert!(data
            .next()
            .await
            .starts_with("DELETE /route/v1/isolated/session/once "));
    }
}

#[tokio::test]
async fn attach_echoes_get_and_stream_error_cleanup() {
    use futures_util::StreamExt;
    let mut data = Server::start(vec![
        reply(json!({"status":"active","workspace":{"path":"/workspace","mode":"overlay"},"share_net":false})),
        reply(json!({"status":"active","uid":1000})),
        (200,"content-type: text/event-stream\r\n".into(),"data: {\"type\":\"error\",\"error\":{\"ename\":\"exit\",\"evalue\":\"7\"}}\n\n".into()),
        reply(json!({"session_id":"once"})),
        (200,"content-type: text/event-stream\r\n".into(),"data: {\"type\":\"error\",\"error\":{\"ename\":\"exit\",\"evalue\":\"7\"}}\n\n".into()),
        (204,String::new(),String::new()),
    ]).await;
    let sandbox = sandbox(&mut data).await;
    let session = sandbox.isolated().attach("echoed").await.unwrap();
    assert_eq!(
        session.info().additional_properties["workspace"]["mode"],
        "overlay"
    );
    assert_eq!(session.get().await.unwrap().uid, Some(1000));
    let mut stream = session
        .run_stream("false", Default::default())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    let result = sandbox
        .isolated()
        .run_once("false", create(), Default::default())
        .await
        .unwrap();
    assert_eq!(result.execution.unwrap().exit_code, Some(7));
    assert!(result.cleanup.is_ok());
    for _ in 0..5 {
        data.next().await;
    }
    assert!(data
        .next()
        .await
        .starts_with("DELETE /route/v1/isolated/session/once "));
}
