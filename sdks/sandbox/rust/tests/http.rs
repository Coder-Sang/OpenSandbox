use futures_util::StreamExt;
use opensandbox::{
    ConnectionConfig, DownloadOptions, Error, RunCommandOptions, SandboxFilter, SandboxManager,
};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
#[path = "support/http.rs"]
mod http;
use http::{endpoint, running, Reply, Server};

#[tokio::test]
async fn api_error_retains_status_code_and_request_id() {
    let mut reply = Reply::json(json!({"code":"DENIED","message":"not permitted"}));
    reply.status = 403;
    reply.headers = "x-request-id: request-123\r\n".into();
    let mut server = Server::start(vec![reply]).await;
    match server.manager().get("box").await.unwrap_err() {
        Error::Api {
            status,
            code,
            message,
            request_id,
        } => {
            assert_eq!(status, 403);
            assert_eq!(code.as_deref(), Some("DENIED"));
            assert_eq!(message, "not permitted");
            assert_eq!(request_id.as_deref(), Some("request-123"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let req = server.next().await;
    assert_eq!(req.target, "/deploy/v1/sandboxes/box");
    assert_eq!(req.headers["open-sandbox-api-key"], "control-secret");
}

#[tokio::test]
async fn ordinary_request_timeout_is_enforced() {
    let mut reply = running();
    reply.delay = Duration::from_secs(1);
    let server = Server::start(vec![reply]).await;
    let manager = SandboxManager::new(
        ConnectionConfig::builder()
            .disable_metrics(true)
            .domain(&server.url)
            .request_timeout(Duration::from_millis(30))
            .build()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(manager.get("box").await,Err(Error::Transport(e)) if e.is_timeout()));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let mut destination = Server::start(vec![running()]).await;
    let mut reply = Reply::bytes(Vec::new());
    reply.status = 302;
    reply.headers = format!("Location: {}/stolen\r\n", destination.url);
    let source = Server::start(vec![reply]).await;
    assert!(matches!(
        source.manager().get("box").await,
        Err(Error::Api { status: 302, .. })
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.requests.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lifecycle_prefix_and_nested_query_are_escaped() {
    let mut server = Server::start(vec![running(),Reply::json(json!({"items":[],"pagination":{"page":1,"pageSize":10,"totalItems":0,"totalPages":0,"hasNextPage":false}}))]).await;
    let manager = server.manager();
    manager.get("a/b?#%").await.unwrap();
    assert_eq!(
        server.next().await.target,
        "/deploy/v1/sandboxes/a%2Fb%3F%23%25"
    );
    let metadata = BTreeMap::from([("a&=%".into(), "hello world&=%".into())]);
    manager
        .list(SandboxFilter {
            states: vec!["Running".into(), "Paused".into()],
            metadata: metadata.clone(),
            page: Some(1),
            page_size: Some(10),
        })
        .await
        .unwrap();
    let req = server.next().await;
    let url = url::Url::parse(&format!("{}{}", server.url, req.target)).unwrap();
    let pairs: Vec<_> = url.query_pairs().collect();
    assert_eq!(pairs.iter().filter(|(k, _)| k == "state").count(), 2);
    let encoded = &pairs.iter().find(|(k, _)| k == "metadata").unwrap().1;
    let decoded: BTreeMap<String, String> = url::form_urlencoded::parse(encoded.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(decoded, metadata);
}

#[tokio::test]
async fn direct_and_proxy_headers_binary_multipart_and_range() {
    for proxy in [false, true] {
        let binary = vec![0, 255, 128, b'\r', b'\n', 1];
        let mut data = Server::start(vec![
            Reply::bytes(vec![]),
            Reply::bytes(vec![]),
            Reply::bytes(binary.clone()),
        ])
        .await;
        let mut control = Server::start(vec![running(), endpoint(&data.url)]).await;
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
        let path = "/tmp/a &?#%.bin";
        sandbox.files().upload(path, binary.clone()).await.unwrap();
        assert_eq!(
            sandbox
                .files()
                .download_with_options(
                    path,
                    DownloadOptions {
                        range: Some("bytes=0-5".into()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            binary
        );
        control.next().await;
        assert_eq!(
            control.next().await.target,
            format!("/deploy/v1/sandboxes/box/endpoints/44772?use_server_proxy={proxy}")
        );
        for index in 0..3 {
            let req = data.next().await;
            assert_eq!(
                req.headers.get("open-sandbox-api-key").map(String::as_str),
                if proxy {
                    Some("endpoint-control")
                } else {
                    None
                }
            );
            assert_eq!(req.headers["x-endpoint-token"], "endpoint-secret");
            assert_eq!(req.headers["x-custom"], "custom");
            if index == 0 {
                assert_eq!(req.target, "/route/ping");
            }
            if index == 1 {
                assert_eq!(req.method, "POST");
                assert_eq!(req.target, "/route/files/upload");
                let boundary = req.headers["content-type"]
                    .split("boundary=")
                    .nth(1)
                    .unwrap();
                assert!(req.body.starts_with(format!("--{boundary}\r\n").as_bytes()));
                assert!(req.body.windows(binary.len()).any(|part| part == binary));
                let text = String::from_utf8_lossy(&req.body);
                assert!(text.contains("name=\"metadata\""));
                assert!(text.contains("name=\"file\""));
                assert!(text.contains(&serde_json::to_string(path).unwrap()));
            }
            if index == 2 {
                assert_eq!(req.headers["range"], "bytes=0-5");
                let url = url::Url::parse(&format!("{}{}", data.url, req.target)).unwrap();
                assert_eq!(url.path(), "/route/files/download");
                assert_eq!(
                    url.query_pairs().find(|(k, _)| k == "path").unwrap().1,
                    path
                );
            }
        }
    }
}

#[tokio::test]
async fn public_sse_stream_aggregation_and_explicit_interrupt() {
    let body=b"data: {\"type\":\"init\",\"text\":\"cmd &?\"}\n\ndata: {\"type\":\"stdout\",\"text\":\"hello\"}\n\ndata: {\"type\":\"execution_complete\",\"execution_time\":5}\n\n".to_vec();
    let mut sse = Reply::bytes(body.clone());
    sse.headers = "Content-Type: text/event-stream\r\n".into();
    let mut data = Server::start(vec![
        Reply::bytes(vec![]),
        sse,
        Reply::bytes(body),
        Reply::bytes(vec![]),
    ])
    .await;
    let control = Server::start(vec![running(), endpoint(&data.url)]).await;
    let sandbox = control.manager().connect("box").await.unwrap();
    let execution = sandbox
        .commands()
        .run(
            "echo hello",
            RunCommandOptions {
                timeout: Some(Duration::from_millis(1500)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(execution.logs.stdout[0].text, "hello");
    assert!(execution.complete);
    assert_eq!(execution.exit_code, Some(0));
    let mut stream = sandbox
        .commands()
        .run_stream("sleep 10", Default::default())
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap().text.as_deref(),
        Some("cmd &?")
    );
    drop(stream);
    sandbox.commands().interrupt("cmd &?").await.unwrap();
    data.next().await;
    let run = data.next().await;
    assert_eq!(run.headers["accept"], "text/event-stream");
    assert_eq!(run.target, "/route/command");
    let body: serde_json::Value = serde_json::from_slice(&run.body).unwrap();
    assert_eq!(body["command"], "echo hello");
    assert_eq!(body["timeout"], 1500);
    data.next().await;
    let interrupt = data.next().await;
    assert_eq!(interrupt.method, "DELETE");
    assert_eq!(interrupt.target, "/route/command?id=cmd+%26%3F");
}
