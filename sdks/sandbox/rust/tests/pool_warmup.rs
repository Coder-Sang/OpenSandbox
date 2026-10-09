use opensandbox::{pool::*, ConnectionConfig, CreateSandboxOptions, SandboxManager};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, Notify},
};

async fn fixture() -> (
    ConnectionConfig,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                loop {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&bytes[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&data)
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                let _ = tx.send(request.clone());
                let body = if request.contains("endpoints") {
                    format!("{{\"endpoint\":\"http://{address}/execd\"}}")
                } else if request.starts_with("GET /v1/sandboxes/") {
                    serde_json::json!({"id":"warm","entrypoint":["tail"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2027-01-01T00:00:00Z"}).to_string()
                } else {
                    String::new()
                };
                let code = if body.is_empty() { 204 } else { 200 };
                let response = format!(
                    "HTTP/1.1 {code} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (
        ConnectionConfig::builder()
            .domain(format!("http://{address}"))
            .disable_metrics(true)
            .build()
            .unwrap(),
        rx,
        task,
    )
}

#[tokio::test]
async fn late_creator_after_destroy_is_killed_not_published() {
    let (connection, mut requests, server) = fixture().await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let manager = SandboxManager::new(connection.clone()).unwrap();
    let mut c = PoolConfig::new("p", 1, connection, CreateSandboxOptions::image("ubuntu"));
    c.state_store = store.clone();
    c.warmup_skip_health_check = true;
    let e = entered.clone();
    let r = resume.clone();
    c.sandbox_creator = Some(Arc::new(move |context| {
        assert!(context.skip_health_check);
        assert!(context.connection_config.disable_metrics);
        let manager = manager.clone();
        let e = e.clone();
        let r = r.clone();
        Box::pin(async move {
            e.notify_one();
            r.notified().await;
            Ok(manager.connect_unready("late").await?)
        })
    }));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    entered.notified().await;
    store.begin_destroy("p").await.unwrap();
    resume.notify_one();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        if line.starts_with("DELETE /v1/sandboxes/late ") {
            break;
        }
    }
    assert!(store.snapshot_idle_entries("p").await.unwrap().is_empty());
    pool.close(true).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn late_creator_after_primary_lease_loss_is_killed_not_published() {
    let (connection, mut requests, server) = fixture().await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let manager = SandboxManager::new(connection.clone()).unwrap();
    let mut c = PoolConfig::new("p", 1, connection, CreateSandboxOptions::image("ubuntu"));
    c.owner_id = "leader".into();
    c.state_store = store.clone();
    c.warmup_skip_health_check = true;
    let e = entered.clone();
    let r = resume.clone();
    c.sandbox_creator = Some(Arc::new(move |_| {
        let manager = manager.clone();
        let e = e.clone();
        let r = r.clone();
        Box::pin(async move {
            e.notify_one();
            r.notified().await;
            Ok(manager.connect_unready("late").await?)
        })
    }));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    entered.notified().await;
    let lease = store
        .try_acquire_primary_lock("p", "leader", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    store.release_primary_lock("p", &lease).await.unwrap();
    let replacement = store
        .try_acquire_primary_lock("p", "peer", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    resume.notify_one();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        if line.starts_with("DELETE /v1/sandboxes/late ") {
            break;
        }
    }
    assert!(store.snapshot_idle_entries("p").await.unwrap().is_empty());
    assert!(store
        .renew_primary_lock("p", &replacement, Duration::from_secs(60))
        .await
        .unwrap());
    pool.close(true).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn post_prepare_retries_do_not_rerun_preparer_and_resize_shrinks() {
    let (connection, mut requests, server) = fixture().await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    let manager = SandboxManager::new(connection.clone()).unwrap();
    let prepared = Arc::new(AtomicUsize::new(0));
    let checked = Arc::new(AtomicUsize::new(0));
    let mut c = PoolConfig::new("p", 1, connection, CreateSandboxOptions::image("ubuntu"));
    c.state_store = store.clone();
    c.warmup_skip_health_check = true;
    c.warmup_health_check_polling_interval = Duration::from_millis(1);
    c.sandbox_creator = Some(Arc::new(move |_| {
        let manager = manager.clone();
        Box::pin(async move { Ok(manager.connect_unready("warm").await?) })
    }));
    let count = prepared.clone();
    c.warmup_sandbox_preparer = Some(Arc::new(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }));
    let count = checked.clone();
    c.warmup_post_prepare_health_check = Some(Arc::new(move |_| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(n >= 2) })
    }));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !store.snapshot_idle_entries("p").await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(prepared.load(Ordering::SeqCst), 1);
    assert_eq!(checked.load(Ordering::SeqCst), 3);
    pool.resize(0).await.unwrap();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        if line.starts_with("DELETE /v1/sandboxes/warm ") {
            break;
        }
    }
    assert_eq!(pool.snapshot().await.unwrap().max_idle, 0);
    pool.close(true).await.unwrap();
    server.abort();
}

#[cfg(feature = "tracing")]
#[tokio::test]
async fn actual_warmup_exports_all_six_child_phases() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    let (mut connection, _requests, server) = fixture().await;
    connection.enable_tracing = true;
    let store = Arc::new(InMemoryPoolStateStore::new());
    let manager = SandboxManager::new(connection.clone()).unwrap();
    let mut c = PoolConfig::new(
        "traced",
        1,
        connection,
        CreateSandboxOptions::image("ubuntu"),
    );
    c.state_store = store.clone();
    c.sandbox_creator = Some(Arc::new(move |_| {
        let manager = manager.clone();
        Box::pin(async move { Ok(manager.connect_unready("warm").await?) })
    }));
    c.warmup_health_check = Some(Arc::new(|_| Box::pin(async { Ok(true) })));
    c.warmup_sandbox_preparer = Some(Arc::new(|_| Box::pin(async { Ok(()) })));
    c.warmup_post_prepare_health_check = Some(Arc::new(|_| Box::pin(async { Ok(true) })));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !store
                .snapshot_idle_entries("traced")
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    pool.close(true).await.unwrap();
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let root = spans
        .iter()
        .find(|span| {
            span.name == "pool.warmup"
                && span
                    .attributes
                    .iter()
                    .any(|a| a.key.as_str() == "pool.name" && a.value.to_string() == "traced")
        })
        .unwrap();
    let children: Vec<_> = spans
        .iter()
        .filter(|span| span.parent_span_id == root.span_context.span_id())
        .collect();
    let names: Vec<_> = children.iter().map(|span| span.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "pool.warmup.create",
            "pool.warmup.readiness",
            "pool.warmup.prepare",
            "pool.warmup.post_prepare_readiness",
            "pool.warmup.renew",
            "pool.warmup.commit"
        ]
    );
    assert!(children
        .iter()
        .all(|span| span.span_context.trace_id() == root.span_context.trace_id()));
    assert!(root
        .attributes
        .iter()
        .any(|a| a.key.as_str() == "sandbox.id"));
    server.abort();
}
