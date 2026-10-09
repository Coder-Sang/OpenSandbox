// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PoolDestroyStrategy {
    #[default]
    Force,
}
#[derive(Clone, Debug)]
pub struct PoolDestroyOptions {
    pub strategy: PoolDestroyStrategy,
    pub drain_timeout: Duration,
    /// None keeps a permanent tombstone.
    pub tombstone_ttl: Option<Duration>,
}
impl Default for PoolDestroyOptions {
    fn default() -> Self {
        Self {
            strategy: PoolDestroyStrategy::Force,
            drain_timeout: Duration::from_secs(30),
            tombstone_ttl: Some(Duration::from_secs(7 * 86400)),
        }
    }
}
#[derive(Clone, Debug)]
pub struct PoolDestroyResult {
    pub pool_name: String,
    pub state: PoolDestroyState,
    pub drained_idle_count: usize,
    pub killed_idle_count: usize,
    pub persistent_state_cleared: bool,
}
#[derive(Clone)]
pub struct SandboxPoolManager {
    store: Arc<dyn PoolStateStore>,
    manager: SandboxManager,
}
impl SandboxPoolManager {
    pub fn new(store: Arc<dyn PoolStateStore>, config: ConnectionConfig) -> PoolResult<Self> {
        Ok(Self {
            store,
            manager: SandboxManager::new(config)?,
        })
    }
    /// Namespace-wide retirement. Cancellation of the caller does not cancel the destroy worker.
    pub async fn destroy(
        &self,
        pool_name: &str,
        options: PoolDestroyOptions,
    ) -> PoolResult<PoolDestroyResult> {
        if pool_name.trim().is_empty() || options.tombstone_ttl.is_some_and(|ttl| ttl.is_zero()) {
            return Err(PoolError::InvalidConfig(
                "pool name and positive tombstone TTL required".into(),
            ));
        }
        let manager = self.clone();
        let pool = pool_name.to_owned();
        tokio::spawn(async move { manager.destroy_owned(pool, options).await })
            .await
            .map_err(|e| PoolError::Task(e.to_string()))?
    }
    async fn destroy_owned(
        &self,
        pool: String,
        options: PoolDestroyOptions,
    ) -> PoolResult<PoolDestroyResult> {
        let mut result = PoolDestroyResult {
            pool_name: pool.clone(),
            state: PoolDestroyState::Destroying,
            drained_idle_count: 0,
            killed_idle_count: 0,
            persistent_state_cleared: false,
        };
        if self.store.begin_destroy(&pool).await? == PoolDestroyState::Destroyed {
            result.state = PoolDestroyState::Destroyed;
            result.persistent_state_cleared = true;
            return Ok(result);
        }
        let drain = async {
            let entries = self.store.snapshot_idle_entries(&pool).await?;
            for entry in entries {
                // Kill before removing: cancellation/failure leaves a persistently actionable ID.
                // DESTROYING fences every successful acquire and publication, so snapshots are safe.
                match self.manager.kill(&entry.sandbox_id).await {
                    Ok(()) | Err(Error::Api { status: 404, .. }) => {
                        result.killed_idle_count += 1;
                        self.store.remove_idle(&pool, &entry.sandbox_id).await?;
                        result.drained_idle_count += 1;
                    }
                    Err(error) => return Err(PoolError::Sdk(error)),
                }
            }
            Ok::<(), PoolError>(())
        };
        if options.drain_timeout.is_zero() {
            drain.await?;
        } else {
            match tokio::time::timeout(options.drain_timeout, drain).await {
                Ok(outcome) => outcome?,
                Err(_) => return Ok(result),
            }
        }
        self.store.clear_pool_state(&pool).await?;
        result.persistent_state_cleared = true;
        self.store
            .mark_destroyed(&pool, options.tombstone_ttl)
            .await?;
        result.state = PoolDestroyState::Destroyed;
        Ok(result)
    }
}
