// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use super::{PoolError, PoolResult};
use chrono::{DateTime, Utc};
use std::{future::Future, pin::Pin, time::Duration};

pub type PoolFuture<'a, T> = Pin<Box<dyn Future<Output = PoolResult<T>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleEntry {
    pub sandbox_id: String,
    pub expires_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TakeIdleResult {
    pub sandbox_id: Option<String>,
    pub discarded_alive_sandbox_ids: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PoolDestroyState {
    #[default]
    Active,
    Destroying,
    Destroyed,
}
/// A unique lease incarnation, not merely an owner name. Never reuse a token after expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryLease {
    pub owner_id: String,
    pub token: String,
}

/// Object-safe asynchronous shared namespace store.
///
/// All mutations must be atomic at the storage boundary. `put_idle` must check the
/// destroy fence, exact lease incarnation, unexpired lease, and current idle cap in
/// the SAME transaction as insertion. `clear_pool_state` preserves the destroy
/// fence. Taking/draining idles deliberately remains possible during destruction.
/// Tombstone and lease durations use native durations, expiry timestamps use UTC.
pub trait PoolStateStore: Send + Sync {
    fn try_take_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, TakeIdleResult>;
    /// Return false on lease loss or a full buffer, error on namespace destruction.
    fn put_idle<'a>(
        &'a self,
        pool: &'a str,
        id: &'a str,
        lease: &'a PrimaryLease,
    ) -> PoolFuture<'a, bool>;
    fn remove_idle<'a>(&'a self, pool: &'a str, id: &'a str) -> PoolFuture<'a, ()>;
    fn try_acquire_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        owner: &'a str,
        ttl: Duration,
    ) -> PoolFuture<'a, Option<PrimaryLease>>;
    fn renew_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
        ttl: Duration,
    ) -> PoolFuture<'a, bool>;
    fn release_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
    ) -> PoolFuture<'a, ()>;
    fn reap_expired_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, Vec<String>>;
    fn snapshot_idle_entries<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Vec<IdleEntry>>;
    fn get_max_idle<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Option<usize>>;
    fn set_max_idle<'a>(&'a self, pool: &'a str, max_idle: usize) -> PoolFuture<'a, ()>;
    fn set_idle_entry_ttl<'a>(&'a self, pool: &'a str, ttl: Duration) -> PoolFuture<'a, ()>;
    fn get_destroy_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState>;
    /// Idempotently install DESTROYING and invalidate all leases. Return existing state.
    fn begin_destroy<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState>;
    fn clear_pool_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, ()>;
    /// Must only transition a fenced namespace, never silently destroy an ACTIVE one.
    fn mark_destroyed<'a>(&'a self, pool: &'a str, ttl: Option<Duration>) -> PoolFuture<'a, ()>;
}

pub(crate) fn destroyed(state: PoolDestroyState) -> PoolResult<()> {
    if state == PoolDestroyState::Active {
        Ok(())
    } else {
        Err(PoolError::Destroyed(state))
    }
}
