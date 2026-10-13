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
            if self.manager.kill(id).await.is_err() {
                cleanup_failure(&self.manager, id, "cleanup", "kill_failed");
            }
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
                    if manager.kill(&id).await.is_err() {
                        cleanup_failure(&manager, &id, "drop", "kill_failed");
                    }
                });
            } else {
                cleanup_failure(&manager, &id, "drop", "runtime_unavailable");
            }
        }
    }
}
// Best-effort diagnostics never include the kill error, which may contain credentials.
fn cleanup_failure(
    manager: &SandboxManager,
    id: &str,
    operation: &'static str,
    reason: &'static str,
) {
    #[cfg(feature = "tracing")]
    if manager.config.enable_tracing {
        use opentelemetry::trace::{Span, Status, Tracer};
        let mut span = opentelemetry::global::tracer("opensandbox.pool")
            .start_with_context("resource.cleanup.failure", &opentelemetry::Context::new());
        if !id.is_empty()
            && id.len() <= 1024
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            span.set_attribute(opentelemetry::KeyValue::new("sandbox.id", id.to_owned()));
        }
        span.set_attribute(opentelemetry::KeyValue::new("operation", operation));
        span.set_attribute(opentelemetry::KeyValue::new("failure.kind", reason));
        span.set_status(Status::error("best-effort resource cleanup failed"));
        span.end();
    }
    #[cfg(not(feature = "tracing"))]
    let _ = (manager, id, operation, reason);
}

#[cfg(all(test, feature = "tracing"))]
mod cleanup_diagnostics_tests {
    use super::*;
    use opentelemetry::trace::Status;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn manager(
        status: Option<u16>,
        enabled: bool,
    ) -> (SandboxManager, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!(
                "http://{}/private-path",
                listener.local_addr().unwrap()
            ))
            .api_key("private-auth-token")
            .enable_tracing(enabled)
            .disable_metrics(true)
            .build()
            .unwrap();
        let mut config = config;
        config.request_timeout = if status.is_none() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(3)
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("DELETE /private-path/v1/sandboxes/"));
            assert!(request.contains("private-auth-token"));
            if let Some(status) = status {
                let body = if status == 204 {
                    ""
                } else {
                    "private-error-body"
                };
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            } else {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
        (SandboxManager::new(config).unwrap(), server)
    }

    #[test]
    fn cleanup_failure_spans_are_opt_in_safe_and_best_effort() {
        let _guard = crate::observability::TRACER_TEST_LOCK.lock().unwrap();
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let previous = opentelemetry::global::set_tracer_provider(provider.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for (id, status, dropped, enabled) in [
                ("async-failed", Some(500), false, true),
                ("drop-failed", Some(500), true, true),
                ("timeout-failed", None, false, true),
                ("async-success", Some(204), false, true),
                ("drop-success", Some(204), true, true),
                ("opt-out", Some(500), false, false),
            ] {
                let (manager, server) = manager(status, enabled).await;
                let pool = SandboxPool::new(PoolConfig::new(
                    "diagnostics",
                    0,
                    manager.config.clone(),
                    CreateSandboxOptions::image("ubuntu"),
                ))
                .unwrap();
                let mut resource = Resource::id(manager, id.into());
                resource.operation = Some(Operation::new(pool.inner.clone(), false));
                if dropped {
                    drop(resource);
                } else {
                    resource.cleanup().await;
                }
                tokio::time::timeout(Duration::from_secs(3), async {
                    while pool.inner.operations.load(Ordering::SeqCst) != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                server.await.unwrap();
            }
        });
        let config = ConnectionConfig::builder()
            .enable_tracing(true)
            .build()
            .unwrap();
        let manager = SandboxManager::new(config).unwrap();
        drop(Resource::id(manager.clone(), "no-runtime".into()));
        cleanup_failure(
            &manager,
            "https://private-id/?secret",
            "cleanup",
            "kill_failed",
        );
        cleanup_failure(&manager, &"x".repeat(1025), "cleanup", "kill_failed");
        provider.force_flush().unwrap();
        opentelemetry::global::set_tracer_provider(previous);
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 6);
        for span in &spans {
            assert_eq!(span.name, "resource.cleanup.failure");
            assert_eq!(
                span.status,
                Status::error("best-effort resource cleanup failed")
            );
            assert!(span.attributes.iter().all(|attribute| matches!(
                attribute.key.as_str(),
                "sandbox.id" | "operation" | "failure.kind"
            )));
        }
        for (id, operation, reason) in [
            ("async-failed", "cleanup", "kill_failed"),
            ("drop-failed", "drop", "kill_failed"),
            ("timeout-failed", "cleanup", "kill_failed"),
            ("no-runtime", "drop", "runtime_unavailable"),
        ] {
            let span = spans
                .iter()
                .find(|span| {
                    span.attributes
                        .contains(&opentelemetry::KeyValue::new("sandbox.id", id))
                })
                .unwrap();
            assert!(span
                .attributes
                .contains(&opentelemetry::KeyValue::new("operation", operation)));
            assert!(span
                .attributes
                .contains(&opentelemetry::KeyValue::new("failure.kind", reason)));
        }
        assert_eq!(
            spans
                .iter()
                .filter(|span| !span
                    .attributes
                    .iter()
                    .any(|attribute| attribute.key.as_str() == "sandbox.id"))
                .count(),
            2
        );
        let output = format!("{spans:?}");
        for sensitive in [
            "private-auth-token",
            "private-error-body",
            "private-path",
            "http://",
            "https://",
            "secret",
        ] {
            assert!(!output.contains(sensitive));
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
