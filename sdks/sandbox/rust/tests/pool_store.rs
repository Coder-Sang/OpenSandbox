use opensandbox::pool::*;
use std::{sync::Arc, time::Duration};

#[tokio::test(start_paused = true)]
async fn lease_incarnations_fence_expired_and_released_owners() {
    let store: Arc<dyn PoolStateStore> = Arc::new(InMemoryPoolStateStore::new());
    store.set_max_idle("p", 2).await.unwrap();
    let old = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .try_acquire_primary_lock("p", "b", Duration::from_secs(5))
        .await
        .unwrap()
        .is_none());
    tokio::time::advance(Duration::from_secs(5)).await;
    let new = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old.token, new.token);
    assert!(!store.put_idle("p", "stale", &old).await.unwrap());
    assert!(!store
        .renew_primary_lock("p", &old, Duration::from_secs(5))
        .await
        .unwrap());
    store.release_primary_lock("p", &old).await.unwrap();
    assert!(store.put_idle("p", "live", &new).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn ttl_discards_alive_near_expiry_but_not_expired() {
    let store = InMemoryPoolStateStore::new();
    store.set_max_idle("p", 3).await.unwrap();
    store
        .set_idle_entry_ttl("p", Duration::from_secs(10))
        .await
        .unwrap();
    let lease = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(100))
        .await
        .unwrap()
        .unwrap();
    store.put_idle("p", "near", &lease).await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    store.put_idle("p", "fresh", &lease).await.unwrap();
    let taken = store
        .try_take_idle("p", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(taken.discarded_alive_sandbox_ids, ["near"]);
    assert_eq!(taken.sandbox_id.as_deref(), Some("fresh"));
    store.put_idle("p", "expired", &lease).await.unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let taken = store
        .try_take_idle("p", Duration::from_secs(1))
        .await
        .unwrap();
    assert!(taken.sandbox_id.is_none());
    assert!(taken.discarded_alive_sandbox_ids.is_empty());
}

#[tokio::test(start_paused = true)]
async fn destroy_fences_writers_and_old_tokens_even_after_tombstone_expires() {
    let store = InMemoryPoolStateStore::new();
    store.set_max_idle("p", 2).await.unwrap();
    let lease = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(100))
        .await
        .unwrap()
        .unwrap();
    store.put_idle("p", "idle", &lease).await.unwrap();
    assert_eq!(
        store.begin_destroy("p").await.unwrap(),
        PoolDestroyState::Destroying
    );
    assert!(matches!(
        store.put_idle("p", "late", &lease).await,
        Err(PoolError::Destroyed(_))
    ));
    assert!(matches!(
        store.set_max_idle("p", 5).await,
        Err(PoolError::Destroyed(_))
    ));
    assert!(matches!(
        store.set_idle_entry_ttl("p", Duration::from_secs(3)).await,
        Err(PoolError::Destroyed(_))
    ));
    assert!(store
        .try_acquire_primary_lock("p", "b", Duration::from_secs(10))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .try_take_idle("p", Duration::ZERO)
            .await
            .unwrap()
            .sandbox_id
            .as_deref(),
        Some("idle")
    );
    store.clear_pool_state("p").await.unwrap();
    assert_eq!(
        store.get_destroy_state("p").await.unwrap(),
        PoolDestroyState::Destroying
    );
    store
        .mark_destroyed("p", Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(
        store.begin_destroy("p").await.unwrap(),
        PoolDestroyState::Destroyed
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(
        store.get_destroy_state("p").await.unwrap(),
        PoolDestroyState::Active
    );
    store.set_max_idle("p", 2).await.unwrap();
    let current = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(current.token, lease.token);
    assert!(!store.put_idle("p", "zombie", &lease).await.unwrap());
}

#[tokio::test]
async fn concurrent_publish_obeys_shared_cap_and_duplicate_is_idempotent() {
    let store = Arc::new(InMemoryPoolStateStore::new());
    store.set_max_idle("p", 1).await.unwrap();
    let lease = store
        .try_acquire_primary_lock("p", "a", Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    let (a, b) = tokio::join!(
        store.put_idle("p", "a", &lease),
        store.put_idle("p", "b", &lease)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    let entries = store.snapshot_idle_entries("p").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(store
        .put_idle("p", &entries[0].sandbox_id, &lease)
        .await
        .unwrap());
    store.set_max_idle("p", 0).await.unwrap();
    assert!(!store.put_idle("p", "c", &lease).await.unwrap());
}

#[test]
fn all_acquire_policies_have_total_attempt_budgets() {
    assert_eq!(AcquirePolicy::FailFast.attempts(3), 1);
    assert_eq!(AcquirePolicy::DirectCreate.attempts(3), 1);
    assert_eq!(AcquirePolicy::RetryNextIdle.attempts(3), 3);
    assert_eq!(AcquirePolicy::RetryNextIdleThenCreate.attempts(3), 3);
    assert!(!AcquirePolicy::FailFast.falls_through());
    assert!(!AcquirePolicy::RetryNextIdle.falls_through());
    assert!(AcquirePolicy::DirectCreate.falls_through());
    assert!(AcquirePolicy::RetryNextIdleThenCreate.falls_through());
}
