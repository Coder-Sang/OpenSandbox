// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
mod engine;
mod manager;
mod memory;
#[cfg(test)]
mod tests;
pub use manager::*;
#[cfg(feature = "redis")]
mod redis;
mod store;
pub use memory::InMemoryPoolStateStore;
#[cfg(feature = "redis")]
pub use redis::RedisPoolStateStore;
pub use store::*;

use crate::{
    config::ConnectionConfig, lifecycle::SandboxManager, models::lifecycle::CreateSandboxOptions,
    sandbox::Sandbox, Error,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{oneshot, Mutex, Notify, Semaphore},
    time::{sleep, timeout_at, Instant},
};

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error(transparent)]
    Sdk(#[from] Error),
    #[error("invalid pool configuration: {0}")]
    InvalidConfig(String),
    #[error("pool state store failed: {0}")]
    Store(String),
    #[error("pool namespace is {0:?}")]
    Destroyed(PoolDestroyState),
    #[error("pool has no usable idle sandbox")]
    Empty,
    #[error("pool is not running")]
    NotRunning,
    #[error("pool operation timed out")]
    Timeout,
    #[error("pool task failed: {0}")]
    Task(String),
}
pub type PoolResult<T> = Result<T, PoolError>;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AcquirePolicy {
    FailFast,
    #[default]
    DirectCreate,
    RetryNextIdle,
    RetryNextIdleThenCreate,
}
impl AcquirePolicy {
    pub fn falls_through(self) -> bool {
        matches!(self, Self::DirectCreate | Self::RetryNextIdleThenCreate)
    }
    pub fn attempts(self, max: usize) -> usize {
        if matches!(self, Self::RetryNextIdle | Self::RetryNextIdleThenCreate) {
            max.max(1)
        } else {
            1
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolLifecycleState {
    NotStarted,
    Starting,
    Running,
    Draining,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolState {
    Healthy,
    Degraded,
    Draining,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PooledSandboxCreateReason {
    Warmup,
    DirectCreate,
}
#[derive(Clone)]
pub struct PooledSandboxCreateContext {
    pub pool_name: String,
    pub owner_id: String,
    pub idle_timeout: Duration,
    pub reason: PooledSandboxCreateReason,
    pub ready_timeout: Duration,
    pub health_check_polling_interval: Duration,
    pub skip_health_check: bool,
    pub connection_config: ConnectionConfig,
}
pub type PooledSandboxCreator =
    Arc<dyn Fn(PooledSandboxCreateContext) -> PoolFuture<'static, Sandbox> + Send + Sync>;
pub type PoolHealthCheck = Arc<dyn Fn(Sandbox) -> PoolFuture<'static, bool> + Send + Sync>;
pub type PoolSandboxPreparer = Arc<dyn Fn(Sandbox) -> PoolFuture<'static, ()> + Send + Sync>;
/// Pool settings. A custom creator may replace `creation_spec`, and must honor staged skip-health-check.
#[derive(Clone)]
pub struct PoolConfig {
    pub pool_name: String,
    pub owner_id: String,
    pub max_idle: usize,
    pub state_store: Arc<dyn PoolStateStore>,
    pub connection_config: ConnectionConfig,
    pub creation_spec: Option<CreateSandboxOptions>,
    pub sandbox_creator: Option<PooledSandboxCreator>,
    pub warmup_create_qps: usize,
    pub warmup_concurrency: usize,
    pub primary_lock_ttl: Duration,
    pub degraded_threshold: usize,
    pub acquire_ready_timeout: Duration,
    pub acquire_health_check_polling_interval: Duration,
    pub acquire_health_check: Option<PoolHealthCheck>,
    pub acquire_skip_health_check: bool,
    pub acquire_min_remaining_ttl: Option<Duration>,
    pub max_acquire_retries: usize,
    pub warmup_ready_timeout: Duration,
    pub warmup_health_check_initial_delay: Duration,
    pub warmup_health_check_polling_interval: Duration,
    pub warmup_health_check: Option<PoolHealthCheck>,
    pub warmup_sandbox_preparer: Option<PoolSandboxPreparer>,
    pub warmup_post_prepare_health_check: Option<PoolHealthCheck>,
    pub warmup_post_prepare_health_check_timeout: Duration,
    pub warmup_skip_health_check: bool,
    pub idle_timeout: Duration,
    pub drain_timeout: Duration,
}
static OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
impl PoolConfig {
    pub fn new(
        pool_name: impl Into<String>,
        max_idle: usize,
        connection_config: ConnectionConfig,
        creation_spec: CreateSandboxOptions,
    ) -> Self {
        Self {
            pool_name: pool_name.into(),
            owner_id: format!(
                "pool-owner-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            max_idle,
            state_store: Arc::new(InMemoryPoolStateStore::new()),
            connection_config,
            creation_spec: Some(creation_spec),
            sandbox_creator: None,
            warmup_create_qps: 10,
            warmup_concurrency: 128,
            primary_lock_ttl: Duration::from_secs(60),
            degraded_threshold: 3,
            acquire_ready_timeout: Duration::from_secs(30),
            acquire_health_check_polling_interval: Duration::from_millis(200),
            acquire_health_check: None,
            acquire_skip_health_check: false,
            acquire_min_remaining_ttl: None,
            max_acquire_retries: 3,
            warmup_ready_timeout: Duration::from_secs(30),
            warmup_health_check_initial_delay: Duration::ZERO,
            warmup_health_check_polling_interval: Duration::from_millis(500),
            warmup_health_check: None,
            warmup_sandbox_preparer: None,
            warmup_post_prepare_health_check: None,
            warmup_post_prepare_health_check_timeout: Duration::from_secs(30),
            warmup_skip_health_check: false,
            idle_timeout: Duration::from_secs(86400),
            drain_timeout: Duration::from_secs(30),
        }
    }
    fn validate(&self) -> PoolResult<()> {
        if self.pool_name.trim().is_empty() || self.owner_id.trim().is_empty() {
            return Err(PoolError::InvalidConfig(
                "pool and owner names must not be blank".into(),
            ));
        }
        if self.creation_spec.is_none() && self.sandbox_creator.is_none() {
            return Err(PoolError::InvalidConfig(
                "creation_spec or sandbox_creator is required".into(),
            ));
        }
        if [
            self.warmup_create_qps,
            self.warmup_concurrency,
            self.degraded_threshold,
            self.max_acquire_retries,
        ]
        .contains(&0)
        {
            return Err(PoolError::InvalidConfig(
                "concurrency, QPS, failure threshold and attempts must be positive".into(),
            ));
        }
        for d in [
            self.primary_lock_ttl,
            self.acquire_ready_timeout,
            self.acquire_health_check_polling_interval,
            self.warmup_ready_timeout,
            self.warmup_health_check_polling_interval,
            self.warmup_post_prepare_health_check_timeout,
            self.idle_timeout,
        ] {
            if d.is_zero()
                || Instant::now().checked_add(d).is_none()
                || chrono::Duration::from_std(d).is_err()
            {
                return Err(PoolError::InvalidConfig(
                    "duration must be positive and representable".into(),
                ));
            }
        }
        for duration in [self.warmup_health_check_initial_delay, self.drain_timeout] {
            if Instant::now().checked_add(duration).is_none() {
                return Err(PoolError::InvalidConfig(
                    "stage or drain duration overflow".into(),
                ));
            }
        }
        if self.idle_timeout.subsec_nanos() != 0 || self.min_ttl() >= self.idle_timeout {
            return Err(PoolError::InvalidConfig(
                "idle_timeout must use whole seconds and minimum TTL must be smaller".into(),
            ));
        }
        Ok(())
    }
    fn min_ttl(&self) -> Duration {
        self.acquire_min_remaining_ttl
            .unwrap_or(self.idle_timeout / 2)
            .min(if self.acquire_min_remaining_ttl.is_none() {
                Duration::from_secs(60)
            } else {
                Duration::MAX
            })
    }
}
#[derive(Clone, Debug)]
pub struct PoolSnapshot {
    pub state: PoolState,
    pub lifecycle_state: PoolLifecycleState,
    pub idle_count: usize,
    pub max_idle: usize,
    pub failure_count: usize,
    pub backoff_active: bool,
    pub last_error: Option<String>,
    pub in_flight_operations: usize,
    pub warming_count: usize,
}
struct Local {
    phase: PoolLifecycleState,
    failures: usize,
    last_error: Option<String>,
    lease: Option<PrimaryLease>,
}
struct Inner {
    config: PoolConfig,
    manager: SandboxManager,
    local: Mutex<Local>,
    stopped: AtomicBool,
    warming: AtomicUsize,
    operations: AtomicUsize,
    creates: Arc<Semaphore>,
    stages: Arc<Semaphore>,
    notify: Notify,
    control: Mutex<()>,
}
/// Cloneable pool handle. Acquired sandboxes are transferred permanently to the caller.
#[derive(Clone)]
pub struct SandboxPool {
    inner: Arc<Inner>,
}
/// Owned resource guard. Every early return, abort, or failed delivery schedules cleanup.
struct Resource {
    operation: Option<Operation>,
    manager: SandboxManager,
    id: Option<String>,
    sandbox: Option<Sandbox>,
}
impl Resource {
    fn id(manager: SandboxManager, id: String) -> Self {
        Self {
            operation: None,
            manager,
            id: Some(id),
            sandbox: None,
        }
    }
    fn sandbox(manager: SandboxManager, sandbox: Sandbox) -> Self {
        Self {
            operation: None,
            manager,
            id: Some(sandbox.id().into()),
            sandbox: Some(sandbox),
        }
    }
    fn transfer(mut self) -> Sandbox {
        self.id = None;
        self.sandbox
            .take()
            .expect("resource contains connected sandbox")
    }
    async fn cleanup(mut self) {
        if let Some(id) = self.id.as_ref() {
            let _ = self.manager.kill(id).await;
            self.id = None;
        }
    }
}
impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let manager = self.manager.clone();
            let operation = self.operation.take();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _operation = operation;
                    let _ = manager.kill(&id).await;
                });
            }
        }
    }
}
struct Operation {
    inner: Arc<Inner>,
    warm: bool,
}
impl Operation {
    fn new(inner: Arc<Inner>, warm: bool) -> Self {
        inner.operations.fetch_add(1, Ordering::SeqCst);
        if warm {
            inner.warming.fetch_add(1, Ordering::SeqCst);
        }
        Self { inner, warm }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if self.warm {
            self.inner.warming.fetch_sub(1, Ordering::SeqCst);
        }
        self.inner.operations.fetch_sub(1, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }
}
impl SandboxPool {
    pub fn new(config: PoolConfig) -> PoolResult<Self> {
        config.validate()?;
        if config.warmup_concurrency > Semaphore::MAX_PERMITS
            || config.warmup_create_qps > Semaphore::MAX_PERMITS / 2
        {
            return Err(PoolError::InvalidConfig(
                "concurrency exceeds runtime capacity".into(),
            ));
        }
        let manager = SandboxManager::new(config.connection_config.clone())?;
        let create_limit = config.warmup_create_qps.saturating_mul(3).div_ceil(2);
        Ok(Self {
            inner: Arc::new(Inner {
                creates: Arc::new(Semaphore::new(create_limit)),
                stages: Arc::new(Semaphore::new(config.warmup_concurrency)),
                config,
                manager,
                local: Mutex::new(Local {
                    phase: PoolLifecycleState::NotStarted,
                    failures: 0,
                    last_error: None,
                    lease: None,
                }),
                stopped: AtomicBool::new(false),
                warming: AtomicUsize::new(0),
                operations: AtomicUsize::new(0),
                notify: Notify::new(),
                control: Mutex::new(()),
            }),
        })
    }
    pub async fn start(&self) -> PoolResult<()> {
        let pool = self.clone();
        tokio::spawn(async move { pool.start_owned().await })
            .await
            .map_err(|e| PoolError::Task(e.to_string()))?
    }
    async fn start_owned(&self) -> PoolResult<()> {
        let _control = self.inner.control.lock().await;
        {
            let mut local = self.inner.local.lock().await;
            match local.phase {
                PoolLifecycleState::Running => return Ok(()),
                PoolLifecycleState::NotStarted => local.phase = PoolLifecycleState::Starting,
                _ => return Err(PoolError::NotRunning),
            }
        }
        let result = async {
            self.inner.fence(false).await?;
            let c = &self.inner.config;
            if c.state_store.get_max_idle(&c.pool_name).await?.is_none() {
                c.state_store.set_max_idle(&c.pool_name, c.max_idle).await?;
            }
            c.state_store
                .set_idle_entry_ttl(&c.pool_name, c.idle_timeout)
                .await?;
            self.inner.fence(false).await
        }
        .await;
        if result.is_err() {
            self.inner.local.lock().await.phase = PoolLifecycleState::Stopped;
            self.inner.stopped.store(true, Ordering::SeqCst);
            return result;
        }
        self.inner.local.lock().await.phase = PoolLifecycleState::Running;
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let operation = Operation::new(inner.clone(), false);
                if let Err(e) = inner.reconcile().await {
                    inner.failure(&e).await;
                }
                drop(operation);
                drop(inner);
                sleep(Duration::from_secs(1)).await;
            }
        });
        // Renewal is independent of slow reconcile and stage work.
        let weak = Arc::downgrade(&self.inner);
        let cadence = (self.inner.config.primary_lock_ttl / 3).max(Duration::from_millis(1));
        tokio::spawn(async move {
            loop {
                sleep(cadence).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let lease = inner.local.lock().await.lease.clone();
                if let Some(lease) = lease {
                    let c = &inner.config;
                    match c
                        .state_store
                        .renew_primary_lock(&c.pool_name, &lease, c.primary_lock_ttl)
                        .await
                    {
                        Ok(true) => {}
                        _ => {
                            let mut local = inner.local.lock().await;
                            if local.lease.as_ref() == Some(&lease) {
                                local.lease = None;
                            }
                        }
                    }
                }
            }
        });
        Ok(())
    }
    /// The acquisition runs as an owned task. Dropping this future cannot interrupt resource cleanup.
    pub async fn acquire(
        &self,
        sandbox_timeout: Option<Duration>,
        policy: AcquirePolicy,
    ) -> PoolResult<Sandbox> {
        self.inner.running().await?;
        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();
        let operation = Operation::new(inner.clone(), false);
        tokio::spawn(async move {
            let mut operation = Some(operation);
            let mut result = inner.acquire_owned(sandbox_timeout, policy).await;
            if let Ok(resource) = &mut result {
                // Keep draining accountable while ownership is queued for delivery.
                // Cancellation moves this same permit into the resource's cleanup task.
                resource.operation = operation.take();
            }
            if let Err(Ok(resource)) = tx.send(result) {
                resource.cleanup().await;
            }
        });
        rx.await
            .map_err(|e| PoolError::Task(e.to_string()))?
            .map(Resource::transfer)
    }
    pub async fn resize(&self, max_idle: usize) -> PoolResult<()> {
        let c = &self.inner.config;
        c.state_store.set_max_idle(&c.pool_name, max_idle).await?;
        self.inner.notify.notify_one();
        Ok(())
    }
    pub async fn snapshot_idle_entries(&self) -> PoolResult<Vec<IdleEntry>> {
        let c = &self.inner.config;
        c.state_store.snapshot_idle_entries(&c.pool_name).await
    }
    pub async fn snapshot(&self) -> PoolResult<PoolSnapshot> {
        let idle_count = self.snapshot_idle_entries().await?.len();
        let c = &self.inner.config;
        let max_idle = c
            .state_store
            .get_max_idle(&c.pool_name)
            .await?
            .unwrap_or(c.max_idle);
        let local = self.inner.local.lock().await;
        let state = match local.phase {
            PoolLifecycleState::Draining => PoolState::Draining,
            PoolLifecycleState::Stopped => PoolState::Stopped,
            _ if local.failures >= c.degraded_threshold => PoolState::Degraded,
            _ => PoolState::Healthy,
        };
        Ok(PoolSnapshot {
            state,
            lifecycle_state: local.phase,
            idle_count,
            max_idle,
            failure_count: local.failures,
            backoff_active: false,
            last_error: local.last_error.clone(),
            in_flight_operations: self.inner.operations.load(Ordering::SeqCst),
            warming_count: self.inner.warming.load(Ordering::SeqCst),
        })
    }
    pub async fn release_all_idle(&self) -> PoolResult<usize> {
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.release_idle().await })
            .await
            .map_err(|e| PoolError::Task(e.to_string()))?
    }
    /// Stop this local instance only. Already borrowed sandboxes are never killed.
    pub async fn close(&self, graceful: bool) -> PoolResult<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.close(graceful).await })
            .await
            .map_err(|e| PoolError::Task(e.to_string()))?
    }
    /// Stop locally, await in-flight cleanup, then kill shared idle resources.
    /// Borrowed sandboxes remain caller-owned. A drain timeout returns `PoolError::Timeout`:
    /// workers continue cleanup and calling `kill` again retries the drain.
    pub async fn kill(&self) -> PoolResult<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            inner.close(false).await?;
            inner.wait_operations().await?;
            inner.release_idle().await?;
            Ok(())
        })
        .await
        .map_err(|e| PoolError::Task(e.to_string()))?
    }
}
