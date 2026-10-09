// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::models::lifecycle::CreateSandboxOptions;

fn pool(max: usize) -> SandboxPool {
    let c = ConnectionConfig::builder()
        .domain("http://127.0.0.1:9")
        .disable_metrics(true)
        .build()
        .unwrap();
    SandboxPool::new(PoolConfig::new(
        "test",
        max,
        c,
        CreateSandboxOptions::image("ubuntu"),
    ))
    .unwrap()
}
fn fake(inner: &Inner, id: &str) -> Sandbox {
    Sandbox {
        manager: inner.manager.clone(),
        transport: inner.manager.transport.clone(),
        id: id.into(),
        template_origin: false,
    }
}
#[test]
fn resource_drop_outside_runtime_does_not_panic() {
    let p = pool(0);
    drop(Resource::id(p.inner.manager.clone(), "box".into()));
}
#[tokio::test]
async fn shutdown_releases_lease_acquired_before_local_publication() {
    let p = pool(0);
    let c = &p.inner.config;
    let lease = c
        .state_store
        .try_acquire_primary_lock(&c.pool_name, &c.owner_id, c.primary_lock_ttl)
        .await
        .unwrap()
        .unwrap();
    // Shutdown sees no local lease while an admitted reconcile awaits store acquisition.
    p.close(false).await.unwrap();
    assert!(matches!(
        p.inner.accept_lease(&lease).await,
        Err(PoolError::NotRunning)
    ));
    assert!(p.inner.local.lock().await.lease.is_none());
    assert!(c
        .state_store
        .try_acquire_primary_lock(&c.pool_name, "replacement", c.primary_lock_ttl)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn queued_resource_cancellation_keeps_cleanup_in_shutdown_drain() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let e = entered.clone();
    let r = resume.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&request).starts_with("DELETE /v1/sandboxes/queued "));
        e.notify_one();
        r.notified().await;
        socket
            .write_all(b"HTTP/1.1 204 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let connection = ConnectionConfig::builder()
        .domain(format!("http://{address}"))
        .disable_metrics(true)
        .build()
        .unwrap();
    let mut c = PoolConfig::new(
        "queued",
        0,
        connection,
        CreateSandboxOptions::image("ubuntu"),
    );
    c.drain_timeout = Duration::ZERO;
    c.acquire_skip_health_check = true;
    let manager = SandboxManager::new(c.connection_config.clone()).unwrap();
    let created = Arc::new(Notify::new());
    let signal = created.clone();
    c.sandbox_creator = Some(Arc::new(move |_| {
        let manager = manager.clone();
        let signal = signal.clone();
        Box::pin(async move {
            signal.notify_one();
            Ok(Sandbox {
                transport: manager.transport.clone(),
                manager,
                id: "queued".into(),
                template_origin: false,
            })
        })
    }));
    let p = SandboxPool::new(c).unwrap();
    p.start().await.unwrap();
    // Poll only once to start acquisition, leaving the successful result queued.
    let waker = std::task::Waker::noop();
    let mut acquire = Box::pin(p.acquire(None, AcquirePolicy::DirectCreate));
    assert!(std::future::Future::poll(
        acquire.as_mut(),
        &mut std::task::Context::from_waker(waker)
    )
    .is_pending());
    created.notified().await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(p.inner.operations.load(Ordering::SeqCst), 1);
    drop(acquire);
    entered.notified().await;
    assert_eq!(p.inner.operations.load(Ordering::SeqCst), 1);
    let closing = p.clone();
    let mut kill = tokio::spawn(async move { closing.kill().await });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut kill)
        .await
        .is_err());
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(3), kill)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(p.inner.operations.load(Ordering::SeqCst), 0);
    assert!(p.snapshot_idle_entries().await.unwrap().is_empty());
    server.await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn readiness_final_deadline_check_and_auth_fail_fast() {
    let p = pool(0);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let check: PoolHealthCheck = Arc::new(move |_| {
        let count = counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(count == 2) })
    });
    p.inner
        .ready(
            &fake(&p.inner, "box"),
            &Some(check),
            Duration::from_secs(1),
            Duration::from_millis(500),
            true,
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let check: PoolHealthCheck = Arc::new(move |_| {
        Box::pin(async {
            Err(PoolError::Sdk(Error::Api {
                status: 401,
                code: None,
                message: "denied".into(),
                request_id: None,
            }))
        })
    });
    let before = Instant::now();
    assert!(matches!(
        p.inner
            .ready(
                &fake(&p.inner, "box"),
                &Some(check),
                Duration::from_secs(1),
                Duration::from_millis(500),
                true
            )
            .await,
        Err(PoolError::Sdk(Error::Api { status: 401, .. }))
    ));
    assert_eq!(Instant::now(), before);
}
#[tokio::test(start_paused = true)]
async fn qps_create_headroom_and_degraded_threshold_are_bounded() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let gate = Arc::new(Notify::new());
    let wait = gate.clone();
    let mut c = pool(10).inner.config.clone();
    c.warmup_create_qps = 2;
    c.degraded_threshold = 2;
    c.sandbox_creator = Some(Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        let wait = wait.clone();
        Box::pin(async move {
            wait.notified().await;
            Err(PoolError::Store("creation failed".into()))
        })
    }));
    let p = SandboxPool::new(c).unwrap();
    p.start().await.unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 3); // ceil(2 * 1.5) active creates
    assert_eq!(p.snapshot().await.unwrap().warming_count, 3);
    gate.notify_waiters();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(p.snapshot().await.unwrap().state, PoolState::Degraded);
    assert!(!p.snapshot().await.unwrap().backoff_active);
    p.close(false).await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn polling_delays_release_stage_capacity() {
    let mut c = pool(0).inner.config.clone();
    c.warmup_concurrency = 1;
    let p = SandboxPool::new(c).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let check: PoolHealthCheck = Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(false) })
    });
    let inner = p.inner.clone();
    let check_a = check.clone();
    let task = tokio::spawn(async move {
        inner
            .ready(
                &fake(&inner, "a"),
                &Some(check_a),
                Duration::from_secs(2),
                Duration::from_secs(1),
                true,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(p.inner.stages.try_acquire().is_ok());
    let inner = p.inner.clone();
    let check_b = check.clone();
    let other = tokio::spawn(async move {
        inner
            .ready(
                &fake(&inner, "b"),
                &Some(check_b),
                Duration::from_secs(2),
                Duration::from_secs(1),
                true,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(task.await.unwrap(), Err(PoolError::Timeout)));
    assert!(matches!(other.await.unwrap(), Err(PoolError::Timeout)));
}
