// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "redis")]

use opensandbox::pool::{PoolDestroyState, PoolError, PoolStateStore, RedisPoolStateStore};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn stores() -> Option<(Arc<RedisPoolStateStore>, Arc<RedisPoolStateStore>, String)> {
    if std::env::var("OPENSANDBOX_REDIS_TEST").ok().as_deref() != Some("1") {
        return None;
    }
    let url = std::env::var("OPENSANDBOX_REDIS_URL").expect("OPENSANDBOX_REDIS_TEST=1 requires OPENSANDBOX_REDIS_URL pointing at a dedicated test Redis");
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let pool = format!(
        "rust-test-{}-{}-{}{{雪}}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    Some((
        Arc::new(RedisPoolStateStore::new(&url).unwrap()),
        Arc::new(RedisPoolStateStore::new(&url).unwrap()),
        pool,
    ))
}
async fn cleanup(store: &RedisPoolStateStore, pool: &str) {
    store.begin_destroy(pool).await.unwrap();
    store.clear_pool_state(pool).await.unwrap();
    store
        .mark_destroyed(pool, Some(Duration::from_millis(1)))
        .await
        .unwrap();
    // The generation counter is intentionally retained, including in production.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_concurrent_capacity_and_unique_takes() {
    let Some((a, b, pool)) = stores() else {
        return;
    };
    assert_eq!(a.get_max_idle(&pool).await.unwrap(), None);
    a.set_max_idle(&pool, 7).await.unwrap();
    assert_eq!(b.get_max_idle(&pool).await.unwrap(), Some(7));
    let lease = a
        .try_acquire_primary_lock(&pool, "owner", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..40 {
        let s = if i % 2 == 0 { a.clone() } else { b.clone() };
        let p = pool.clone();
        let l = lease.clone();
        tasks.push(tokio::spawn(async move {
            s.put_idle(&p, &format!("id-{i}"), &l).await.unwrap()
        }));
    }
    let mut inserted = 0;
    for task in tasks {
        inserted += usize::from(task.await.unwrap());
    }
    assert_eq!(inserted, 7);
    let entries = a.snapshot_idle_entries(&pool).await.unwrap();
    assert_eq!(entries.len(), 7);
    assert!(b
        .put_idle(&pool, &entries[0].sandbox_id, &lease)
        .await
        .unwrap());
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let s = b.clone();
        let p = pool.clone();
        tasks.push(tokio::spawn(async move {
            s.try_take_idle(&p, Duration::ZERO)
                .await
                .unwrap()
                .sandbox_id
        }));
    }
    let mut ids = HashSet::new();
    for task in tasks {
        if let Some(id) = task.await.unwrap() {
            assert!(ids.insert(id));
        }
    }
    assert_eq!(ids.len(), 7);
    cleanup(&a, &pool).await;
}

#[tokio::test]
async fn redis_lease_incarnations_and_expiry_fence() {
    let Some((a, b, pool)) = stores() else {
        return;
    };
    a.set_max_idle(&pool, 10).await.unwrap();
    let other_pool = format!("{pool}-other");
    b.set_max_idle(&other_pool, 10).await.unwrap();
    let foreign = b
        .try_acquire_primary_lock(&other_pool, "same", Duration::from_secs(20))
        .await
        .unwrap()
        .unwrap();
    let old = a
        .try_acquire_primary_lock(&pool, "same", Duration::from_secs(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        b.try_acquire_primary_lock(&pool, "same", Duration::from_secs(20))
            .await
            .unwrap(),
        Some(old.clone())
    );
    assert!(b
        .try_acquire_primary_lock(&pool, "other", Duration::from_secs(1))
        .await
        .unwrap()
        .is_none());
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert!(!b.put_idle(&pool, "expired", &old).await.unwrap());
    let new = b
        .try_acquire_primary_lock(&pool, "same", Duration::from_secs(20))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old.token, new.token);
    assert!(!a.put_idle(&pool, "foreign", &foreign).await.unwrap());
    cleanup(&b, &other_pool).await;
    assert!(!a
        .renew_primary_lock(&pool, &old, Duration::from_secs(20))
        .await
        .unwrap());
    a.release_primary_lock(&pool, &old).await.unwrap();
    assert!(!a.put_idle(&pool, "stale", &old).await.unwrap());
    assert!(a
        .renew_primary_lock(&pool, &new, Duration::from_secs(20))
        .await
        .unwrap());
    assert!(a.put_idle(&pool, "valid", &new).await.unwrap());
    b.release_primary_lock(&pool, &new).await.unwrap();
    assert!(!a.put_idle(&pool, "released", &new).await.unwrap());
    cleanup(&a, &pool).await;
}

#[tokio::test]
async fn redis_shared_ttl_fifo_filtering_and_removal() {
    let Some((a, b, pool)) = stores() else {
        return;
    };
    a.set_max_idle(&pool, 10).await.unwrap();
    let l = a
        .try_acquire_primary_lock(&pool, "owner", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    a.set_idle_entry_ttl(&pool, Duration::from_secs(2))
        .await
        .unwrap();
    b.put_idle(&pool, "near", &l).await.unwrap();
    a.set_idle_entry_ttl(&pool, Duration::from_secs(20))
        .await
        .unwrap();
    b.put_idle(&pool, "far", &l).await.unwrap();
    let taken = a
        .try_take_idle(&pool, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(taken.sandbox_id.as_deref(), Some("far"));
    assert_eq!(taken.discarded_alive_sandbox_ids, vec!["near"]);
    a.set_idle_entry_ttl(&pool, Duration::from_secs(2))
        .await
        .unwrap();
    b.put_idle(&pool, "reap-near", &l).await.unwrap();
    assert_eq!(
        a.reap_expired_idle(&pool, Duration::from_secs(5))
            .await
            .unwrap(),
        vec!["reap-near"]
    );
    a.set_idle_entry_ttl(&pool, Duration::from_millis(30))
        .await
        .unwrap();
    b.put_idle(&pool, "expired", &l).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(a.snapshot_idle_entries(&pool).await.unwrap().is_empty());
    assert!(a
        .reap_expired_idle(&pool, Duration::ZERO)
        .await
        .unwrap()
        .is_empty());
    a.set_idle_entry_ttl(&pool, Duration::from_secs(20))
        .await
        .unwrap();
    a.put_idle(&pool, "remove", &l).await.unwrap();
    b.remove_idle(&pool, "remove").await.unwrap();
    assert!(a
        .try_take_idle(&pool, Duration::ZERO)
        .await
        .unwrap()
        .sandbox_id
        .is_none());
    cleanup(&a, &pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_destroy_is_atomic_and_clear_preserves_tombstone() {
    let Some((a, b, pool)) = stores() else {
        return;
    };
    assert!(a.clear_pool_state(&pool).await.is_err());
    assert!(a.mark_destroyed(&pool, None).await.is_err());
    a.set_max_idle(&pool, 100).await.unwrap();
    let l = a
        .try_acquire_primary_lock(&pool, "owner", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    a.put_idle(&pool, "drain", &l).await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..30 {
        let s = b.clone();
        let p = pool.clone();
        let lease = l.clone();
        tasks.push(tokio::spawn(async move {
            s.put_idle(&p, &format!("racing-{i}"), &lease).await
        }));
    }
    assert_eq!(
        a.begin_destroy(&pool).await.unwrap(),
        PoolDestroyState::Destroying
    );
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) | Err(PoolError::Destroyed(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(matches!(
        b.put_idle(&pool, "after", &l).await,
        Err(PoolError::Destroyed(PoolDestroyState::Destroying))
    ));
    assert!(!b
        .renew_primary_lock(&pool, &l, Duration::from_secs(30))
        .await
        .unwrap());
    assert!(b
        .try_acquire_primary_lock(&pool, "owner", Duration::from_secs(30))
        .await
        .unwrap()
        .is_none());
    assert!(b.set_max_idle(&pool, 1).await.is_err());
    assert!(b
        .set_idle_entry_ttl(&pool, Duration::from_secs(1))
        .await
        .is_err());
    assert_eq!(
        b.try_take_idle(&pool, Duration::ZERO)
            .await
            .unwrap()
            .sandbox_id
            .as_deref(),
        Some("drain")
    );
    a.clear_pool_state(&pool).await.unwrap();
    assert_eq!(
        b.get_destroy_state(&pool).await.unwrap(),
        PoolDestroyState::Destroying
    );
    a.mark_destroyed(&pool, None).await.unwrap();
    b.clear_pool_state(&pool).await.unwrap();
    assert_eq!(
        a.begin_destroy(&pool).await.unwrap(),
        PoolDestroyState::Destroyed
    );
    assert!(matches!(
        a.put_idle(&pool, "after-clear", &l).await,
        Err(PoolError::Destroyed(PoolDestroyState::Destroyed))
    ));
    a.mark_destroyed(&pool, Some(Duration::from_millis(30)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        b.get_destroy_state(&pool).await.unwrap(),
        PoolDestroyState::Active
    );
    let next = b
        .try_acquire_primary_lock(&pool, "owner", Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(next.token, l.token);
    assert!(!a.put_idle(&pool, "old-incarnation", &l).await.unwrap());
    cleanup(&a, &pool).await;
}
