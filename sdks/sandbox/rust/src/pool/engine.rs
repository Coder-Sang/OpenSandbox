// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use super::store::destroyed;
use super::*;

fn auth(error: &PoolError) -> bool {
    matches!(
        error,
        PoolError::Sdk(Error::Api {
            status: 401 | 403,
            ..
        })
    )
}
impl Inner {
    pub(super) async fn running(&self) -> PoolResult<()> {
        if self.local.lock().await.phase != PoolLifecycleState::Running
            || self.stopped.load(Ordering::SeqCst)
        {
            Err(PoolError::NotRunning)
        } else {
            Ok(())
        }
    }
    pub(super) async fn fence(&self, allow_outage: bool) -> PoolResult<()> {
        let c = &self.config;
        match c.state_store.get_destroy_state(&c.pool_name).await {
            Ok(state) => destroyed(state),
            Err(PoolError::Destroyed(state)) => Err(PoolError::Destroyed(state)),
            Err(_) if allow_outage => Ok(()),
            Err(error) => Err(error),
        }
    }
    pub(super) async fn failure(&self, error: &PoolError) {
        let mut local = self.local.lock().await;
        local.failures = local.failures.saturating_add(1);
        local.last_error = Some(error.to_string());
        if matches!(error, PoolError::Destroyed(_)) {
            self.stopped.store(true, Ordering::SeqCst);
            local.phase = PoolLifecycleState::Stopped;
            local.lease = None;
        }
    }
    async fn success(&self) {
        let mut local = self.local.lock().await;
        local.failures = 0;
        local.last_error = None;
    }
    async fn cleanup_create_error(&self, error: &PoolError) {
        if let PoolError::Sdk(
            Error::SandboxSetup { sandbox_id, .. } | Error::ReadyTimeout { sandbox_id, .. },
        ) = error
        {
            Resource::id(self.manager.clone(), sandbox_id.clone())
                .cleanup()
                .await;
        }
    }
    async fn create(
        &self,
        reason: PooledSandboxCreateReason,
        ttl: Duration,
        connection: Option<ConnectionConfig>,
    ) -> PoolResult<Resource> {
        let c = &self.config;
        let warm = reason == PooledSandboxCreateReason::Warmup;
        let result = if let Some(creator) = &c.sandbox_creator {
            creator(PooledSandboxCreateContext {
                pool_name: c.pool_name.clone(),
                owner_id: c.owner_id.clone(),
                idle_timeout: ttl,
                reason,
                ready_timeout: if warm {
                    c.warmup_ready_timeout
                } else {
                    c.acquire_ready_timeout
                },
                health_check_polling_interval: if warm {
                    c.warmup_health_check_polling_interval
                } else {
                    c.acquire_health_check_polling_interval
                },
                skip_health_check: true,
                connection_config: connection
                    .clone()
                    .unwrap_or_else(|| c.connection_config.clone()),
            })
            .await
        } else {
            let mut spec = c
                .creation_spec
                .clone()
                .ok_or_else(|| PoolError::InvalidConfig("missing creation spec".into()))?;
            spec.timeout = Some(ttl);
            let manager = if let Some(connection) = connection {
                SandboxManager::new(connection)?
            } else {
                self.manager.clone()
            };
            manager.create_unready(spec).await.map_err(PoolError::from)
        };
        match result {
            Ok(sb) => Ok(Resource::sandbox(self.manager.clone(), sb)),
            Err(error) => {
                self.cleanup_create_error(&error).await;
                Err(error)
            }
        }
    }
    /// Poll waits do not hold a post-create permit. The final check occurs at the deadline.
    pub(super) async fn ready(
        &self,
        sb: &Sandbox,
        check: &Option<PoolHealthCheck>,
        budget: Duration,
        poll: Duration,
        staged: bool,
    ) -> PoolResult<()> {
        let deadline = Instant::now() + budget;
        loop {
            let permit = if staged {
                Some(
                    timeout_at(deadline, self.stages.acquire())
                        .await
                        .map_err(|_| PoolError::Timeout)?
                        .map_err(|e| PoolError::Task(e.to_string()))?,
                )
            } else {
                None
            };
            let result = if let Some(check) = check {
                timeout_at(deadline, check(sb.clone()))
                    .await
                    .map_err(|_| PoolError::Timeout)?
            } else {
                timeout_at(deadline, sb.ping())
                    .await
                    .map_err(|_| PoolError::Timeout)?
                    .map(|_| true)
                    .map_err(PoolError::from)
            };
            drop(permit);
            match result {
                Ok(true) => return Ok(()),
                Err(e) if auth(&e) => return Err(e),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(PoolError::Timeout);
            }
            sleep(poll.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
    }
    pub(super) async fn acquire_owned(
        &self,
        ttl: Option<Duration>,
        policy: AcquirePolicy,
    ) -> PoolResult<Resource> {
        if let Some(ttl) = ttl {
            crate::lifecycle::seconds(ttl, "sandbox timeout")?;
        }
        self.running().await?;
        self.fence(policy.falls_through()).await?;
        let c = &self.config;
        let mut last_error = None;
        for _ in 0..policy.attempts(c.max_acquire_retries) {
            let taken = match c.state_store.try_take_idle(&c.pool_name, c.min_ttl()).await {
                Ok(taken) => taken,
                Err(e @ PoolError::Destroyed(_)) => return Err(e),
                Err(e) if policy.falls_through() => {
                    last_error = Some(e);
                    break;
                }
                Err(e) => return Err(e),
            };
            for id in taken.discarded_alive_sandbox_ids {
                Resource::id(self.manager.clone(), id).cleanup().await;
            }
            let Some(id) = taken.sandbox_id else {
                break;
            };
            let mut resource = Resource::id(self.manager.clone(), id.clone());
            let result = async {
                resource.sandbox = Some(self.manager.connect_unready(&id).await?);
                let sb = resource.sandbox.as_ref().expect("connected sandbox");
                if !c.acquire_skip_health_check {
                    self.ready(
                        sb,
                        &c.acquire_health_check,
                        c.acquire_ready_timeout,
                        c.acquire_health_check_polling_interval,
                        false,
                    )
                    .await?;
                }
                if let Some(ttl) = ttl {
                    sb.renew(ttl).await?;
                }
                self.running().await
            }
            .await;
            if result.is_ok() {
                if let Err(error) = self.fence(false).await {
                    resource.cleanup().await;
                    return Err(error);
                }
            }
            match result {
                Ok(()) => return Ok(resource),
                Err(error) => {
                    resource.cleanup().await;
                    if auth(&error)
                        || matches!(error, PoolError::Destroyed(_) | PoolError::NotRunning)
                    {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
        }
        if !policy.falls_through() {
            return Err(last_error.unwrap_or(PoolError::Empty));
        }
        self.running().await?;
        self.fence(true).await?;
        let resource = self
            .create(
                PooledSandboxCreateReason::DirectCreate,
                ttl.unwrap_or(c.idle_timeout),
                None,
            )
            .await?;
        let result = async {
            if !c.acquire_skip_health_check {
                self.ready(
                    resource.sandbox.as_ref().expect("created sandbox"),
                    &c.acquire_health_check,
                    c.acquire_ready_timeout,
                    c.acquire_health_check_polling_interval,
                    false,
                )
                .await?;
            }
            self.fence(true).await?;
            self.running().await
        }
        .await;
        if let Err(e) = result {
            resource.cleanup().await;
            return Err(e);
        }
        Ok(resource)
    }
    pub(super) async fn reconcile(self: &Arc<Self>) -> PoolResult<()> {
        self.running().await?;
        self.fence(false).await?;
        let c = &self.config;
        let Some(lease) = c
            .state_store
            .try_acquire_primary_lock(&c.pool_name, &c.owner_id, c.primary_lock_ttl)
            .await?
        else {
            self.local.lock().await.lease = None;
            return Ok(());
        };
        self.local.lock().await.lease = Some(lease.clone());
        if !c
            .state_store
            .renew_primary_lock(&c.pool_name, &lease, c.primary_lock_ttl)
            .await?
        {
            self.local.lock().await.lease = None;
            return Ok(());
        }
        for id in c
            .state_store
            .reap_expired_idle(&c.pool_name, c.min_ttl())
            .await?
        {
            Resource::id(self.manager.clone(), id).cleanup().await;
        }
        let count = c
            .state_store
            .snapshot_idle_entries(&c.pool_name)
            .await?
            .len();
        let max = c
            .state_store
            .get_max_idle(&c.pool_name)
            .await?
            .unwrap_or(c.max_idle);
        if count > max {
            for _ in 0..(count - max).min(c.warmup_concurrency) {
                if !c
                    .state_store
                    .renew_primary_lock(&c.pool_name, &lease, c.primary_lock_ttl)
                    .await?
                {
                    break;
                }
                let result = c
                    .state_store
                    .try_take_idle(&c.pool_name, Duration::ZERO)
                    .await?;
                if let Some(id) = result.sandbox_id {
                    Resource::id(self.manager.clone(), id).cleanup().await;
                }
            }
            return Ok(());
        }
        let deficit = max.saturating_sub(count + self.warming.load(Ordering::SeqCst));
        for _ in 0..deficit.min(c.warmup_create_qps) {
            let Ok(permit) = self.creates.clone().try_acquire_owned() else {
                break;
            };
            let inner = self.clone();
            let lease = lease.clone();
            let operation = Operation::new(inner.clone(), true);
            tokio::spawn(async move {
                let _operation = operation;
                let result = inner.warmup(&lease, permit).await;
                match result {
                    Ok(()) => inner.success().await,
                    Err(error) => inner.failure(&error).await,
                }
            });
        }
        Ok(())
    }
    async fn valid_warmup(&self, lease: &PrimaryLease) -> PoolResult<()> {
        self.running().await?;
        self.fence(false).await?;
        if self.local.lock().await.lease.as_ref() != Some(lease) {
            return Err(PoolError::Store("primary lease lost".into()));
        }
        let c = &self.config;
        if !c
            .state_store
            .renew_primary_lock(&c.pool_name, lease, c.primary_lock_ttl)
            .await?
        {
            return Err(PoolError::Store("primary lease expired".into()));
        }
        Ok(())
    }
    async fn warmup(
        &self,
        lease: &PrimaryLease,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> PoolResult<()> {
        self.valid_warmup(lease).await?;
        let c = &self.config;
        let mut trace = crate::observability::WarmupTrace::new(&c.connection_config);
        trace.attribute("pool.name", &c.pool_name);
        trace.attribute("pool.owner_id", &c.owner_id);
        trace.counter("pool.warmup.attempt", 1);
        let mut create_phase = trace.phase("create");
        create_phase.attribute("pool.name", &c.pool_name);
        create_phase.counter("pool.warmup.attempt", 1);
        let mut staged_config = create_phase.scoped_config(&c.connection_config);
        staged_config.disable_metrics = true;
        let created = self
            .create(
                PooledSandboxCreateReason::Warmup,
                c.idle_timeout,
                Some(staged_config),
            )
            .await;
        create_phase.finish(created.is_ok());
        let resource = created?;
        trace.attribute(
            "sandbox.id",
            resource.sandbox.as_ref().expect("created sandbox").id(),
        );
        drop(permit);
        let result = async {
            sleep(c.warmup_health_check_initial_delay).await;
            self.valid_warmup(lease).await?;
            let sb = resource.sandbox.as_ref().expect("created sandbox");
            if !c.warmup_skip_health_check {
                let mut phase = trace.phase("readiness");
                let result = self
                    .ready(
                        sb,
                        &c.warmup_health_check,
                        c.warmup_ready_timeout,
                        c.warmup_health_check_polling_interval,
                        true,
                    )
                    .await;
                phase.finish(result.is_ok());
                result?;
            }
            self.valid_warmup(lease).await?;
            if let Some(preparer) = &c.warmup_sandbox_preparer {
                let _permit = self
                    .stages
                    .acquire()
                    .await
                    .map_err(|e| PoolError::Task(e.to_string()))?;
                let mut phase = trace.phase("prepare");
                let result = preparer(sb.clone()).await;
                phase.finish(result.is_ok());
                result?;
            }
            if c.warmup_post_prepare_health_check.is_some() {
                let mut phase = trace.phase("post_prepare_readiness");
                let result = self
                    .ready(
                        sb,
                        &c.warmup_post_prepare_health_check,
                        c.warmup_post_prepare_health_check_timeout,
                        c.warmup_health_check_polling_interval,
                        true,
                    )
                    .await;
                phase.finish(result.is_ok());
                result?;
            }
            self.valid_warmup(lease).await?;
            {
                let _permit = self
                    .stages
                    .acquire()
                    .await
                    .map_err(|e| PoolError::Task(e.to_string()))?;
                let mut phase = trace.phase("renew");
                let result = sb.renew(c.idle_timeout).await;
                phase.finish(result.is_ok());
                result?;
            }
            self.valid_warmup(lease).await?;
            // Remote atomic fencing protects the interval AFTER the local validation.
            let mut phase = trace.phase("commit");
            let published = c.state_store.put_idle(&c.pool_name, sb.id(), lease).await;
            phase.finish(matches!(published, Ok(true)));
            if !published? {
                return Err(PoolError::Store("warmup fenced or idle buffer full".into()));
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            trace.attribute("failure.kind", "warmup_failed");
            trace.counter("pool.warmup.failure_count", 1);
        }
        trace.finish(result.is_ok());
        if let Err(error) = result {
            if let Some(sb) = &resource.sandbox {
                let _ = c.state_store.remove_idle(&c.pool_name, sb.id()).await;
            }
            resource.cleanup().await;
            return Err(error);
        }
        // Published resources belong to the shared idle store, not this task.
        let _ = resource.transfer();
        Ok(())
    }
    pub(super) async fn release_idle(&self) -> PoolResult<usize> {
        let c = &self.config;
        // Bound to the initial visible membership so an active leader cannot make draining endless.
        let entries = c.state_store.snapshot_idle_entries(&c.pool_name).await?;
        let mut count = 0;
        for entry in entries {
            // Atomically take, rather than remove(snapshot ID), to avoid killing a concurrent borrow.
            let taken = c
                .state_store
                .try_take_idle(&c.pool_name, Duration::ZERO)
                .await?;
            if let Some(id) = taken.sandbox_id {
                Resource::id(self.manager.clone(), id).cleanup().await;
                count += 1;
            }
            let _ = entry;
        }
        Ok(count)
    }
    pub(super) async fn close(&self, graceful: bool) -> PoolResult<()> {
        let _control = self.control.lock().await;
        if self.local.lock().await.phase == PoolLifecycleState::Stopped {
            return Ok(());
        }
        self.stopped.store(true, Ordering::SeqCst);
        self.local.lock().await.phase = if graceful {
            PoolLifecycleState::Draining
        } else {
            PoolLifecycleState::Stopped
        };
        self.notify.notify_waiters();
        let lease = self.local.lock().await.lease.take();
        if let Some(lease) = lease {
            let c = &self.config;
            c.state_store
                .release_primary_lock(&c.pool_name, &lease)
                .await?;
        }
        if graceful {
            let wait = async {
                loop {
                    let notified = self.notify.notified();
                    if self.operations.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    notified.await;
                }
            };
            if self.config.drain_timeout.is_zero() {
                wait.await;
            } else {
                let _ = tokio::time::timeout(self.config.drain_timeout, wait).await;
            }
        }
        self.local.lock().await.phase = PoolLifecycleState::Stopped;
        Ok(())
    }
}
