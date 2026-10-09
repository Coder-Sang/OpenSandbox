use opensandbox::{pool::*, ConnectionConfig, CreateSandboxOptions};
use serde_json::json;
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

#[tokio::test]
async fn four_policies_attempt_exact_candidate_budget_and_fallback() {
    for (policy, budget, expected) in [
        (AcquirePolicy::FailFast, 3, None),
        (AcquirePolicy::DirectCreate, 3, Some("fresh")),
        (AcquirePolicy::RetryNextIdle, 3, Some("good")),
        (AcquirePolicy::RetryNextIdleThenCreate, 2, Some("fresh")),
    ] {
        let mut server = Server::new(Arc::new(AtomicUsize::new(0))).await;
        let store = Arc::new(InMemoryPoolStateStore::new());
        seed(store.as_ref(), &["bad1", "bad2", "good"]).await;
        let mut c = config(&server, store.clone(), 3);
        c.acquire_skip_health_check = false;
        c.max_acquire_retries = budget;
        c.acquire_ready_timeout = Duration::from_millis(1);
        c.acquire_health_check_polling_interval = Duration::from_millis(1);
        c.acquire_health_check = Some(Arc::new(move |sb| {
            Box::pin(async move { Ok(!sb.id().starts_with("bad")) })
        }));
        let pool = SandboxPool::new(c).unwrap();
        pool.start().await.unwrap();
        let acquired = pool.acquire(None, policy).await;
        match expected {
            Some(id) => assert_eq!(acquired.unwrap().id(), id),
            None => assert!(acquired.is_err()),
        }
        pool.close(true).await.unwrap();
        let requests: Vec<_> = std::iter::from_fn(|| server.requests.try_recv().ok()).collect();
        let deleted: Vec<_> = requests
            .iter()
            .filter(|line| line.starts_with("DELETE"))
            .collect();
        assert_eq!(
            deleted.len(),
            if matches!(
                policy,
                AcquirePolicy::FailFast | AcquirePolicy::DirectCreate
            ) {
                1
            } else {
                2
            }
        );
        let creates = requests
            .iter()
            .filter(|line| line.starts_with("POST /v1/sandboxes "))
            .count();
        assert_eq!(creates, usize::from(policy.falls_through()));
        assert_eq!(
            store.snapshot_idle_entries("p").await.unwrap().len(),
            match policy {
                AcquirePolicy::FailFast | AcquirePolicy::DirectCreate => 2,
                AcquirePolicy::RetryNextIdle => 0,
                AcquirePolicy::RetryNextIdleThenCreate => 1,
            }
        );
    }
}

struct Server {
    config: ConnectionConfig,
    requests: mpsc::UnboundedReceiver<String>,
    task: tokio::task::JoinHandle<()>,
}

#[tokio::test]
async fn destroy_timeout_keeps_fence_and_actionable_idle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = ConnectionConfig::builder()
        .domain(format!("http://{}", listener.local_addr().unwrap()))
        .disable_metrics(true)
        .build()
        .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let stalled = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let _ = socket.read(&mut bytes).await;
        let _ = entered_tx.send(());
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let store = Arc::new(InMemoryPoolStateStore::new());
    seed(store.as_ref(), &["idle"]).await;
    let manager = SandboxPoolManager::new(store.clone(), connection).unwrap();
    let task = tokio::spawn(async move {
        manager
            .destroy(
                "p",
                PoolDestroyOptions {
                    drain_timeout: Duration::from_millis(50),
                    ..Default::default()
                },
            )
            .await
    });
    entered_rx.await.unwrap();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.state, PoolDestroyState::Destroying);
    assert!(!result.persistent_state_cleared);
    assert_eq!(
        store.snapshot_idle_entries("p").await.unwrap()[0].sandbox_id,
        "idle"
    );
    assert_eq!(
        store.get_destroy_state("p").await.unwrap(),
        PoolDestroyState::Destroying
    );
    stalled.abort();
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(fail_kill: Arc<AtomicUsize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let tx = tx.clone();
                let failures = fail_kill.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let end = loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(i) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let line = head.lines().next().unwrap().to_owned();
                    let len = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while bytes.len() < end + len {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let _ = tx.send(line.clone());
                    let (status, body) = if line.starts_with("DELETE") {
                        if failures
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                        {
                            (500, json!({"message":"kill failed"}))
                        } else {
                            (204, json!(null))
                        }
                    } else if line.contains("/endpoints/") {
                        (200, json!({"endpoint":format!("http://{address}/route")}))
                    } else if line.starts_with("POST /v1/sandboxes ") {
                        (
                            200,
                            json!({"id":"fresh","entrypoint":["tail"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2027-01-01T00:00:00Z"}),
                        )
                    } else if line.starts_with("GET /v1/sandboxes/") {
                        (
                            200,
                            json!({"id":line.split_whitespace().nth(1).unwrap().trim_start_matches("/v1/sandboxes/"),"entrypoint":["tail"],"status":{"state":"Running"},"createdAt":"2026-01-01T00:00:00Z","expiresAt":"2027-01-01T00:00:00Z"}),
                        )
                    } else {
                        (204, json!(null))
                    };
                    let body = if status == 204 {
                        String::new()
                    } else {
                        body.to_string()
                    };
                    let reply = format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self {
            config: ConnectionConfig::builder()
                .domain(format!("http://{address}"))
                .disable_metrics(true)
                .build()
                .unwrap(),
            requests,
            task,
        }
    }
    async fn request(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
fn config(server: &Server, store: Arc<dyn PoolStateStore>, max: usize) -> PoolConfig {
    let mut config = PoolConfig::new(
        "p",
        max,
        server.config.clone(),
        CreateSandboxOptions::image("ubuntu"),
    );
    config.state_store = store;
    config.acquire_skip_health_check = true;
    config.warmup_skip_health_check = true;
    config
}
async fn seed(store: &dyn PoolStateStore, ids: &[&str]) {
    store.set_max_idle("p", ids.len()).await.unwrap();
    let lease = store
        .try_acquire_primary_lock("p", "seed", Duration::from_secs(100))
        .await
        .unwrap()
        .unwrap();
    for id in ids {
        assert!(store.put_idle("p", id, &lease).await.unwrap());
    }
}

#[tokio::test]
async fn cancelled_acquire_cleans_up_after_delayed_health_callback() {
    let mut server = Server::new(Arc::new(AtomicUsize::new(0))).await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    seed(store.as_ref(), &["idle"]).await;
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let mut c = config(&server, store, 1);
    c.acquire_skip_health_check = false;
    let e = entered.clone();
    let r = resume.clone();
    c.acquire_health_check = Some(Arc::new(move |_| {
        let e = e.clone();
        let r = r.clone();
        Box::pin(async move {
            e.notify_one();
            r.notified().await;
            Ok(true)
        })
    }));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move { task_pool.acquire(None, AcquirePolicy::FailFast).await });
    entered.notified().await;
    task.abort();
    let _ = task.await;
    resume.notify_one();
    loop {
        if server
            .request()
            .await
            .starts_with("DELETE /v1/sandboxes/idle ")
        {
            break;
        }
    }
    pool.close(true).await.unwrap();
}

#[tokio::test]
async fn namespace_destroy_between_connect_and_return_kills_taken_idle() {
    let mut server = Server::new(Arc::new(AtomicUsize::new(0))).await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    seed(store.as_ref(), &["idle"]).await;
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let mut c = config(&server, store.clone(), 1);
    c.acquire_skip_health_check = false;
    let e = entered.clone();
    let r = resume.clone();
    c.acquire_health_check = Some(Arc::new(move |_| {
        let e = e.clone();
        let r = r.clone();
        Box::pin(async move {
            e.notify_one();
            r.notified().await;
            Ok(true)
        })
    }));
    let pool = SandboxPool::new(c).unwrap();
    pool.start().await.unwrap();
    let task_pool = pool.clone();
    let task =
        tokio::spawn(async move { task_pool.acquire(None, AcquirePolicy::DirectCreate).await });
    entered.notified().await;
    store.begin_destroy("p").await.unwrap();
    resume.notify_one();
    assert!(matches!(task.await.unwrap(), Err(PoolError::Destroyed(_))));
    loop {
        if server
            .request()
            .await
            .starts_with("DELETE /v1/sandboxes/idle ")
        {
            break;
        }
    }
    pool.close(false).await.unwrap();
}

#[tokio::test]
async fn failed_destroy_preserves_idle_for_retry_and_tombstone_is_idempotent() {
    let server = Server::new(Arc::new(AtomicUsize::new(1))).await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    seed(store.as_ref(), &["idle"]).await;
    let manager = SandboxPoolManager::new(store.clone(), server.config.clone()).unwrap();
    assert!(manager
        .destroy("p", PoolDestroyOptions::default())
        .await
        .is_err());
    assert_eq!(
        store.get_destroy_state("p").await.unwrap(),
        PoolDestroyState::Destroying
    );
    assert_eq!(store.snapshot_idle_entries("p").await.unwrap().len(), 1);
    let result = manager
        .destroy("p", PoolDestroyOptions::default())
        .await
        .unwrap();
    assert_eq!(result.state, PoolDestroyState::Destroyed);
    assert_eq!(result.killed_idle_count, 1);
    let repeat = manager
        .destroy("p", PoolDestroyOptions::default())
        .await
        .unwrap();
    assert_eq!(repeat.killed_idle_count, 0);
    assert!(SandboxPool::new(config(&server, store, 1))
        .unwrap()
        .start()
        .await
        .is_err());
}

#[tokio::test]
async fn close_and_release_idle_never_kill_borrowed_sandbox() {
    let mut server = Server::new(Arc::new(AtomicUsize::new(0))).await;
    let store = Arc::new(InMemoryPoolStateStore::new());
    seed(store.as_ref(), &["idle", "spare"]).await;
    let pool = SandboxPool::new(config(&server, store.clone(), 2)).unwrap();
    pool.start().await.unwrap();
    let borrowed = pool.acquire(None, AcquirePolicy::FailFast).await.unwrap();
    assert_eq!(borrowed.id(), "idle");
    pool.close(true).await.unwrap();
    assert_eq!(pool.release_all_idle().await.unwrap(), 1);
    pool.kill().await.unwrap();
    let mut deletes = Vec::new();
    while let Ok(request) = server.requests.try_recv() {
        if request.starts_with("DELETE") {
            deletes.push(request);
        }
    }
    assert_eq!(deletes, ["DELETE /v1/sandboxes/spare HTTP/1.1"]);
    assert_eq!(
        pool.snapshot().await.unwrap().lifecycle_state,
        PoolLifecycleState::Stopped
    );
    // Explicit caller cleanup is the only operation that kills the borrowed resource.
    borrowed.kill().await.unwrap();
    assert!(server.request().await.contains("/sandboxes/idle"));
}
