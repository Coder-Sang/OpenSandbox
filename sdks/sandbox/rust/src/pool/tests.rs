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
