// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::{
    ConnectionConfig, CreateTemplateOptions, Error, SandboxManager, TemplateFilter,
    TemplateReadiness,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

const SECRET: &str = "literal-secret-$HOME-$(echo untouched)";
struct Server {
    manager: SandboxManager,
    requests: mpsc::Receiver<(String, Value)>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(replies: Vec<(u16, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = SandboxManager::new(
            ConnectionConfig::builder()
                .domain(format!("http://{}", listener.local_addr().unwrap()))
                .disable_metrics(true)
                .build()
                .unwrap(),
        )
        .unwrap();
        let (tx, requests) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            for (status, body) in replies {
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
                let len = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + len {
                    let mut buf = [0; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                let request = if len == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + len]).unwrap()
                };
                tx.send((head.lines().next().unwrap().to_owned(), request))
                    .await
                    .unwrap();
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Reply\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            manager,
            requests,
            task,
        }
    }
    async fn next(&mut self) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn options() -> CreateTemplateOptions {
    CreateTemplateOptions {
        image: "ubuntu".into(),
        publish: "s3://fixture/publish".into(),
        resource_limits: Some([("cpu".into(), "2".into())].into()),
        entrypoint: vec!["true".into()],
        metadata: BTreeMap::new(),
        readiness: Some(TemplateReadiness {
            probe: Some("true".into()),
            warmup: Some(Duration::from_secs(2)),
        }),
        format: Some("native".into()),
    }
}
fn template() -> Value {
    json!({"templateId":"tpl-a","image":"ubuntu","publish":"s3://fixture/publish","format":"native","status":{"phase":"Succeeded"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-02T00:00:00Z","readiness":{"probe":"true","warmupSeconds":2}})
}
fn env() -> BTreeMap<String, String> {
    [("_Token9".into(), SECRET.into())].into()
}

#[tokio::test]
async fn create_response_preserves_absent_null_and_empty_env() {
    let mut null_env = template();
    null_env["env"] = Value::Null;
    let mut empty_env = template();
    empty_env["env"] = json!({});
    let mut server =
        Server::start(vec![(200, template()), (200, null_env), (200, empty_env)]).await;
    for expected in [None, None, Some(BTreeMap::new())] {
        let info = server
            .manager
            .create_template_with_env(options(), env())
            .await
            .unwrap();
        assert_eq!(info.env, expected);
        server.next().await;
    }
}

#[tokio::test]
async fn create_preserves_literal_env_and_legacy_omission() {
    let mut response = template();
    response["env"] = json!(env());
    let mut server = Server::start(vec![
        (200, response.clone()),
        (200, template()),
        (200, template()),
    ])
    .await;
    let created = server
        .manager
        .create_template_with_env(options(), env())
        .await
        .unwrap();
    assert_eq!(created.template.template_id, "tpl-a");
    assert_eq!(created.env, Some(env()));
    assert_eq!(
        created.template.readiness.unwrap().warmup,
        Some(Duration::from_secs(2))
    );
    let (line, body) = server.next().await;
    assert_eq!(line, "POST /v1/templates HTTP/1.1");
    assert_eq!(body["env"], json!(env()));
    assert_eq!(body["readiness"]["warmupSeconds"], 2);
    assert_eq!(body["resourceLimits"]["cpu"], "2");
    server
        .manager
        .create_template_with_env(options(), BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(server.next().await.1["env"], json!({}));
    let legacy = server.manager.create_template(options()).await.unwrap();
    assert_eq!(legacy.template_id, "tpl-a");
    assert!(server.next().await.1.get("env").is_none());
}

#[tokio::test]
async fn get_and_list_retain_env_null_absence_empty_and_pagination() {
    let mut with_env = template();
    with_env["env"] = json!(env());
    with_env["status"]["message"] = json!(SECRET);
    with_env["readiness"]["probe"] = json!(SECRET);
    with_env["metadata"] = json!({"echo": SECRET});
    let mut null_env = template();
    null_env["env"] = Value::Null;
    let mut empty = template();
    empty["env"] = json!({});
    let pagination =
        json!({"page":2,"pageSize":4,"totalItems":8,"totalPages":2,"hasNextPage":false});
    let mut server = Server::start(vec![
        (200, with_env.clone()),
        (200, template()),
        (200, null_env.clone()),
        (200, empty.clone()),
        (
            200,
            json!({"items":[with_env, template(), null_env, empty],"pagination":pagination}),
        ),
        (200, json!({"items":[],"pagination":null})),
    ])
    .await;
    let info = server.manager.get_template_with_env("tpl-a").await.unwrap();
    assert_eq!(info.env, Some(env()));
    assert!(!format!("{info:?}").contains(SECRET));
    assert_eq!(server.next().await.0, "GET /v1/templates/tpl-a HTTP/1.1");
    for expected in [None, None, Some(BTreeMap::new())] {
        assert_eq!(
            server
                .manager
                .get_template_with_env("tpl-a")
                .await
                .unwrap()
                .env,
            expected
        );
        server.next().await;
    }
    let list = server
        .manager
        .list_templates_with_env(TemplateFilter {
            page: Some(2),
            page_size: Some(4),
            metadata: [("team".into(), "sdk".into())].into(),
        })
        .await
        .unwrap();
    assert_eq!(list.items[0].env, Some(env()));
    assert!(list.items[1].env.is_none());
    assert!(list.items[2].env.is_none());
    assert_eq!(list.items[3].env, Some(BTreeMap::new()));
    assert!(!format!("{list:?}").contains(SECRET));
    let page = list.pagination.unwrap();
    assert_eq!(
        (
            page.page,
            page.page_size,
            page.total_items,
            page.total_pages,
            page.has_next_page
        ),
        (2, 4, 8, 2, false)
    );
    let line = server.next().await.0;
    assert!(line.contains("metadata=team%3Dsdk&page=2&pageSize=4"));
    assert!(server
        .manager
        .list_templates_with_env(TemplateFilter::default())
        .await
        .unwrap()
        .pagination
        .is_none());
}

#[tokio::test]
async fn errors_never_echo_secret_and_invalid_names_make_no_request() {
    let mut server = Server::start(vec![
        (
            400,
            json!({"message":SECRET,"code":SECRET,"requestId":SECRET}),
        ),
        (200, json!({"templateId":SECRET})),
        (500, json!({"message":SECRET})),
        (500, json!({"message":SECRET})),
    ])
    .await;
    for name in ["", "9TOKEN", "A-B", "A B", "é", "A\n", SECRET] {
        let error = server
            .manager
            .create_template_with_env(options(), [(name.into(), SECRET.into())].into())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
        assert!(!format!("{error:?} {error}").contains(SECRET));
        assert!(server.requests.try_recv().is_err());
    }
    let error = server
        .manager
        .create_template_with_env(options(), env())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api { status: 400, .. }));
    assert!(!format!("{error:?} {error}").contains(SECRET));
    server.next().await;
    let error = server
        .manager
        .create_template_with_env(options(), env())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Protocol(_)));
    assert!(!format!("{error:?} {error}").contains(SECRET));
    server.next().await;
    let error = server
        .manager
        .get_template_with_env("tpl-a")
        .await
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(SECRET));
    server.next().await;
    let error = server
        .manager
        .list_templates_with_env(TemplateFilter::default())
        .await
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains(SECRET));
}
