// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use super::store::*;
use super::{PoolError, PoolResult};
use chrono::{DateTime, Utc};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;

#[derive(Default)]
struct Namespace {
    entries: VecDeque<IdleEntry>,
    max_idle: Option<usize>,
    ttl: Option<Duration>,
    lease: Option<(PrimaryLease, Instant)>,
    destroy: PoolDestroyState,
    tombstone: Option<Instant>,
}
/// Process-local store with the same atomic fencing requirements as distributed stores.
pub struct InMemoryPoolStateStore {
    pools: Mutex<HashMap<String, Namespace>>,
    epoch: DateTime<Utc>,
    started: Instant,
    sequence: AtomicU64,
    identity: u64,
}
impl Default for InMemoryPoolStateStore {
    fn default() -> Self {
        Self::new()
    }
}
impl InMemoryPoolStateStore {
    pub fn new() -> Self {
        Self {
            pools: Mutex::new(HashMap::new()),
            epoch: Utc::now(),
            started: Instant::now(),
            identity: super::OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            sequence: AtomicU64::new(1),
        }
    }
    fn now(&self) -> PoolResult<DateTime<Utc>> {
        Ok(self.epoch
            + chrono::Duration::from_std(self.started.elapsed())
                .map_err(|_| PoolError::Store("clock overflow".into()))?)
    }
    fn with<T>(
        &self,
        pool: &str,
        f: impl FnOnce(&mut Namespace) -> PoolResult<T>,
    ) -> PoolResult<T> {
        let mut pools = self
            .pools
            .lock()
            .map_err(|_| PoolError::Store("memory store poisoned".into()))?;
        let state = pools.entry(pool.to_owned()).or_default();
        if state.tombstone.is_some_and(|until| until <= Instant::now()) {
            *state = Namespace::default();
        }
        f(state)
    }
}
fn add(now: DateTime<Utc>, duration: Duration) -> PoolResult<DateTime<Utc>> {
    now.checked_add_signed(
        chrono::Duration::from_std(duration)
            .map_err(|_| PoolError::InvalidConfig("duration overflow".into()))?,
    )
    .ok_or_else(|| PoolError::InvalidConfig("expiry overflow".into()))
}
fn lease_valid(state: &Namespace, lease: &PrimaryLease) -> bool {
    state
        .lease
        .as_ref()
        .is_some_and(|(current, until)| current == lease && *until > Instant::now())
}
impl PoolStateStore for InMemoryPoolStateStore {
    fn try_take_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, TakeIdleResult> {
        Box::pin(async move {
            let now = self.now()?;
            let cutoff = add(now, min_ttl)?;
            self.with(pool, |s| {
                let mut result = TakeIdleResult::default();
                while let Some(entry) = s.entries.pop_front() {
                    if entry.expires_at > cutoff {
                        result.sandbox_id = Some(entry.sandbox_id);
                        break;
                    }
                    if entry.expires_at > now {
                        result.discarded_alive_sandbox_ids.push(entry.sandbox_id);
                    }
                }
                Ok(result)
            })
        })
    }
    fn put_idle<'a>(
        &'a self,
        pool: &'a str,
        id: &'a str,
        lease: &'a PrimaryLease,
    ) -> PoolFuture<'a, bool> {
        Box::pin(async move {
            if id.trim().is_empty() {
                return Err(PoolError::InvalidConfig("sandbox id is blank".into()));
            }
            let now = self.now()?;
            self.with(pool, |s| {
                destroyed(s.destroy)?;
                if !lease_valid(s, lease) {
                    return Ok(false);
                }
                s.entries.retain(|e| e.expires_at > now);
                if s.entries.iter().any(|e| e.sandbox_id == id) {
                    return Ok(true);
                }
                if s.entries.len() >= s.max_idle.unwrap_or(0) {
                    return Ok(false);
                }
                s.entries.push_back(IdleEntry {
                    sandbox_id: id.into(),
                    expires_at: add(now, s.ttl.unwrap_or(Duration::from_secs(86400)))?,
                });
                Ok(true)
            })
        })
    }
    fn remove_idle<'a>(&'a self, pool: &'a str, id: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.with(pool, |s| {
                s.entries.retain(|e| e.sandbox_id != id);
                Ok(())
            })
        })
    }
    fn try_acquire_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        owner: &'a str,
        ttl: Duration,
    ) -> PoolFuture<'a, Option<PrimaryLease>> {
        Box::pin(async move {
            if owner.trim().is_empty() || ttl.is_zero() {
                return Err(PoolError::InvalidConfig(
                    "owner and positive lease TTL required".into(),
                ));
            }
            self.with(pool, |s| {
                if s.destroy != PoolDestroyState::Active {
                    return Ok(None);
                }
                if let Some((lease, until)) = &s.lease {
                    if *until > Instant::now() {
                        return Ok(if lease.owner_id == owner {
                            Some(lease.clone())
                        } else {
                            None
                        });
                    }
                }
                let lease = PrimaryLease {
                    owner_id: owner.into(),
                    token: format!(
                        "{}:{}:{}",
                        self.identity,
                        pool,
                        self.sequence.fetch_add(1, Ordering::Relaxed)
                    ),
                };
                s.lease = Some((
                    lease.clone(),
                    Instant::now()
                        .checked_add(ttl)
                        .ok_or_else(|| PoolError::InvalidConfig("lease TTL overflow".into()))?,
                ));
                Ok(Some(lease))
            })
        })
    }
    fn renew_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
        ttl: Duration,
    ) -> PoolFuture<'a, bool> {
        Box::pin(async move {
            self.with(pool, |s| {
                if ttl.is_zero() {
                    return Err(PoolError::InvalidConfig(
                        "lease TTL must be positive".into(),
                    ));
                }
                if s.destroy != PoolDestroyState::Active || !lease_valid(s, lease) {
                    return Ok(false);
                }
                s.lease = Some((
                    lease.clone(),
                    Instant::now()
                        .checked_add(ttl)
                        .ok_or_else(|| PoolError::InvalidConfig("lease TTL overflow".into()))?,
                ));
                Ok(true)
            })
        })
    }
    fn release_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
    ) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.with(pool, |s| {
                if s.lease
                    .as_ref()
                    .is_some_and(|(current, _)| current == lease)
                {
                    s.lease = None;
                }
                Ok(())
            })
        })
    }
    fn reap_expired_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, Vec<String>> {
        Box::pin(async move {
            let now = self.now()?;
            let cutoff = add(now, min_ttl)?;
            self.with(pool, |s| {
                let mut alive = Vec::new();
                s.entries.retain(|e| {
                    if e.expires_at > cutoff {
                        true
                    } else {
                        if e.expires_at > now {
                            alive.push(e.sandbox_id.clone());
                        }
                        false
                    }
                });
                Ok(alive)
            })
        })
    }
    fn snapshot_idle_entries<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Vec<IdleEntry>> {
        Box::pin(async move {
            let now = self.now()?;
            self.with(pool, |s| {
                s.entries.retain(|e| e.expires_at > now);
                Ok(s.entries.iter().cloned().collect())
            })
        })
    }
    fn get_max_idle<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Option<usize>> {
        Box::pin(async move { self.with(pool, |s| Ok(s.max_idle)) })
    }
    fn set_max_idle<'a>(&'a self, pool: &'a str, max_idle: usize) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.with(pool, |s| {
                destroyed(s.destroy)?;
                s.max_idle = Some(max_idle);
                Ok(())
            })
        })
    }
    fn set_idle_entry_ttl<'a>(&'a self, pool: &'a str, ttl: Duration) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            add(self.now()?, ttl)?;
            self.with(pool, |s| {
                destroyed(s.destroy)?;
                if ttl.is_zero() {
                    return Err(PoolError::InvalidConfig("idle TTL must be positive".into()));
                }
                s.ttl = Some(ttl);
                Ok(())
            })
        })
    }
    fn get_destroy_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState> {
        Box::pin(async move { self.with(pool, |s| Ok(s.destroy)) })
    }
    fn begin_destroy<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState> {
        Box::pin(async move {
            self.with(pool, |s| {
                if s.destroy != PoolDestroyState::Destroyed {
                    s.destroy = PoolDestroyState::Destroying;
                    s.lease = None;
                }
                Ok(s.destroy)
            })
        })
    }
    fn clear_pool_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.with(pool, |s| {
                if s.destroy == PoolDestroyState::Active {
                    return Err(PoolError::Store("clear requires destroy fence".into()));
                }
                s.entries.clear();
                s.max_idle = None;
                s.ttl = None;
                s.lease = None;
                Ok(())
            })
        })
    }
    fn mark_destroyed<'a>(&'a self, pool: &'a str, ttl: Option<Duration>) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.with(pool, |s| {
                if s.destroy == PoolDestroyState::Active {
                    return Err(PoolError::Store("tombstone requires destroy fence".into()));
                }
                if ttl.is_some_and(|t| t.is_zero()) {
                    return Err(PoolError::InvalidConfig(
                        "tombstone TTL must be positive or None".into(),
                    ));
                }
                s.tombstone = ttl
                    .map(|t| {
                        Instant::now().checked_add(t).ok_or_else(|| {
                            PoolError::InvalidConfig("tombstone TTL overflow".into())
                        })
                    })
                    .transpose()?;
                s.destroy = PoolDestroyState::Destroyed;
                s.lease = None;
                Ok(())
            })
        })
    }
}
