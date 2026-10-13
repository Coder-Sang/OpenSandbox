// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::pool::{
    IdleEntry, PoolDestroyState, PoolError, PoolFuture, PoolResult, PoolStateStore, PrimaryLease,
    TakeIdleResult,
};
use chrono::{DateTime, Utc};
use std::{fmt, time::Duration};

/// Redis-backed Rust pool namespace. Keys intentionally do not share other SDKs' layout.
/// Each operation uses a bounded asynchronous connection and a single atomic Lua script.
#[derive(Clone)]
pub struct RedisPoolStateStore {
    client: redis::Client,
    timeout: Duration,
}
impl fmt::Debug for RedisPoolStateStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisPoolStateStore")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}
impl RedisPoolStateStore {
    pub fn new(url: &str) -> PoolResult<Self> {
        Self::with_timeout(url, Duration::from_secs(5))
    }
    pub fn with_timeout(url: &str, timeout: Duration) -> PoolResult<Self> {
        if timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err(PoolError::InvalidConfig(
                "Redis timeout must be positive".into(),
            ));
        }
        let client = redis::Client::open(url).map_err(|_| {
            PoolError::InvalidConfig("invalid Redis connection configuration".into())
        })?;
        Ok(Self { client, timeout })
    }
    async fn run<T: redis::FromRedisValue>(
        &self,
        pool: &str,
        op: &str,
        args: &[String],
    ) -> PoolResult<T> {
        let keys = keys(pool);
        tokio::time::timeout(self.timeout, async {
            let mut conn = self
                .client
                .get_multiplexed_async_connection()
                .await
                .map_err(|_| PoolError::Store("Redis connection failed".into()))?;
            let script = redis::Script::new(SCRIPT);
            let mut invocation = script.prepare_invoke();
            for key in &keys {
                invocation.key(key);
            }
            invocation.arg(op);
            for arg in args {
                invocation.arg(arg);
            }
            invocation
                .invoke_async(&mut conn)
                .await
                .map_err(|_| PoolError::Store("Redis pool operation failed".into()))
        })
        .await
        .map_err(|_| PoolError::Store("Redis pool operation timed out".into()))?
    }
    async fn fenced(&self, pool: &str, op: &str, args: &[String]) -> PoolResult<i64> {
        let value: i64 = self.run(pool, op, args).await?;
        match value {
            -1 => Err(PoolError::Destroyed(PoolDestroyState::Destroying)),
            -2 => Err(PoolError::Destroyed(PoolDestroyState::Destroyed)),
            -3 => Err(PoolError::Store("operation requires destroy fence".into())),
            _ => Ok(value),
        }
    }
}
// Hex encoding is injective over UTF-8, and prevents braces in names changing cluster slots.
fn keys(pool: &str) -> Vec<String> {
    let mut encoded = String::with_capacity(pool.len().saturating_mul(2));
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in pool.bytes() {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 15)] as char);
    }
    let prefix = format!("opensandbox:rust:pool:{{p{encoded}}}");
    [
        "queue",
        "expiry",
        "lease",
        "config",
        "destroy",
        "generation",
    ]
    .iter()
    .map(|s| format!("{prefix}:{s}"))
    .collect()
}
fn millis(duration: Duration, positive: bool) -> PoolResult<String> {
    if positive && duration.is_zero() {
        return Err(PoolError::InvalidConfig("duration must be positive".into()));
    }
    let ms = duration.as_millis();
    // Lua numbers represent integer milliseconds exactly only up to 2^53.
    if ms > 1_000_000_000_000 {
        return Err(PoolError::InvalidConfig("duration overflow".into()));
    }
    Ok(if positive { ms.max(1) } else { ms }.to_string())
}
fn state(value: i64) -> PoolDestroyState {
    match value {
        1 => PoolDestroyState::Destroying,
        2 => PoolDestroyState::Destroyed,
        _ => PoolDestroyState::Active,
    }
}
impl PoolStateStore for RedisPoolStateStore {
    fn try_take_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, TakeIdleResult> {
        Box::pin(async move {
            let (id, discarded): (String, Vec<String>) =
                self.run(pool, "take", &[millis(min_ttl, false)?]).await?;
            Ok(TakeIdleResult {
                sandbox_id: if id.is_empty() { None } else { Some(id) },
                discarded_alive_sandbox_ids: discarded,
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
            Ok(self
                .fenced(
                    pool,
                    "put",
                    &[id.into(), lease.owner_id.clone(), lease.token.clone()],
                )
                .await?
                == 1)
        })
    }
    fn remove_idle<'a>(&'a self, pool: &'a str, id: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.run::<i64>(pool, "remove", &[id.into()]).await?;
            Ok(())
        })
    }
    fn try_acquire_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        owner: &'a str,
        ttl: Duration,
    ) -> PoolFuture<'a, Option<PrimaryLease>> {
        Box::pin(async move {
            if owner.trim().is_empty() {
                return Err(PoolError::InvalidConfig("owner id is blank".into()));
            }
            let token: Option<String> = self
                .run(pool, "acquire", &[owner.into(), millis(ttl, true)?])
                .await?;
            Ok(token.map(|token| PrimaryLease {
                owner_id: owner.into(),
                token,
            }))
        })
    }
    fn renew_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
        ttl: Duration,
    ) -> PoolFuture<'a, bool> {
        Box::pin(async move {
            Ok(self
                .run::<i64>(
                    pool,
                    "renew",
                    &[
                        lease.owner_id.clone(),
                        lease.token.clone(),
                        millis(ttl, true)?,
                    ],
                )
                .await?
                == 1)
        })
    }
    fn release_primary_lock<'a>(
        &'a self,
        pool: &'a str,
        lease: &'a PrimaryLease,
    ) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.run::<i64>(
                pool,
                "release",
                &[lease.owner_id.clone(), lease.token.clone()],
            )
            .await?;
            Ok(())
        })
    }
    fn reap_expired_idle<'a>(
        &'a self,
        pool: &'a str,
        min_ttl: Duration,
    ) -> PoolFuture<'a, Vec<String>> {
        Box::pin(async move { self.run(pool, "reap", &[millis(min_ttl, false)?]).await })
    }
    fn snapshot_idle_entries<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Vec<IdleEntry>> {
        Box::pin(async move {
            let raw: Vec<(String, i64)> = self.run(pool, "snapshot", &[]).await?;
            raw.into_iter()
                .map(|(sandbox_id, ms)| {
                    let expires_at: DateTime<Utc> = DateTime::from_timestamp_millis(ms)
                        .ok_or_else(|| PoolError::Store("invalid Redis expiry".into()))?;
                    Ok(IdleEntry {
                        sandbox_id,
                        expires_at,
                    })
                })
                .collect()
        })
    }
    fn get_max_idle<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, Option<usize>> {
        Box::pin(async move { self.run(pool, "get_max", &[]).await })
    }
    fn set_max_idle<'a>(&'a self, pool: &'a str, max_idle: usize) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            if max_idle as u128 > 9_007_199_254_740_991 {
                return Err(PoolError::InvalidConfig("idle cap overflow".into()));
            }
            self.fenced(pool, "set_max", &[max_idle.to_string()])
                .await?;
            Ok(())
        })
    }
    fn set_idle_entry_ttl<'a>(&'a self, pool: &'a str, ttl: Duration) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.fenced(pool, "set_ttl", &[millis(ttl, true)?]).await?;
            Ok(())
        })
    }
    fn get_destroy_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState> {
        Box::pin(async move { Ok(state(self.run(pool, "state", &[]).await?)) })
    }
    fn begin_destroy<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, PoolDestroyState> {
        Box::pin(async move { Ok(state(self.run(pool, "begin", &[]).await?)) })
    }
    fn clear_pool_state<'a>(&'a self, pool: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            self.fenced(pool, "clear", &[]).await?;
            Ok(())
        })
    }
    fn mark_destroyed<'a>(&'a self, pool: &'a str, ttl: Option<Duration>) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            let ttl = ttl
                .map(|t| millis(t, true))
                .transpose()?
                .unwrap_or_default();
            self.fenced(pool, "mark", &[ttl]).await?;
            Ok(())
        })
    }
}

const SCRIPT: &str = r#"
local q,e,l,c,d,g = unpack(KEYS)
local op = ARGV[1]
local t = redis.call('TIME')
local now = tonumber(t[1])*1000 + math.floor(tonumber(t[2])/1000)
local fence = tonumber(redis.call('GET',d) or '0')
local function owns(owner, token)
  return redis.call('PTTL',l)>0 and redis.call('HGET',l,'owner')==owner and redis.call('HGET',l,'token')==token
end
local function reap(cutoff)
  local alive = {}
  local ids = redis.call('LRANGE',q,0,-1)
  for _,id in ipairs(ids) do
    local exp = tonumber(redis.call('HGET',e,id) or '0')
    if exp<=cutoff then
      redis.call('HDEL',e,id)
      redis.call('LREM',q,0,id)
      if exp>now then table.insert(alive,id) end
    end
  end
  return alive
end
if op=='put' then
  if fence>0 then return -fence end
  if not owns(ARGV[3],ARGV[4]) then return 0 end
  reap(now)
  if redis.call('HEXISTS',e,ARGV[2])==1 then return 1 end
  local cap=tonumber(redis.call('HGET',c,'max') or '0')
  if redis.call('HLEN',e)>=cap then return 0 end
  local ttl=tonumber(redis.call('HGET',c,'ttl') or '86400000')
  redis.call('RPUSH',q,ARGV[2])
  redis.call('HSET',e,ARGV[2],now+ttl)
  return 1
elseif op=='take' then
  local discarded={}
  local cutoff=now+tonumber(ARGV[2])
  while true do
    local id=redis.call('LPOP',q)
    if not id then return {'',discarded} end
    local exp=tonumber(redis.call('HGET',e,id) or '0')
    redis.call('HDEL',e,id)
    if exp>cutoff then return {id,discarded} end
    if exp>now then table.insert(discarded,id) end
  end
elseif op=='remove' then
  redis.call('LREM',q,0,ARGV[2]); redis.call('HDEL',e,ARGV[2]); return 1
elseif op=='reap' then return reap(now+tonumber(ARGV[2]))
elseif op=='snapshot' then
  reap(now)
  local result={}
  for _,id in ipairs(redis.call('LRANGE',q,0,-1)) do
    table.insert(result,{id,tonumber(redis.call('HGET',e,id))})
  end
  return result
elseif op=='acquire' then
  if fence>0 then return false end
  if redis.call('EXISTS',l)==1 then
    if redis.call('HGET',l,'owner')==ARGV[2] and redis.call('PTTL',l)>0 then return redis.call('HGET',l,'token') end
    return false
  end
  redis.call('INCR',g)
  local token=g..':'..redis.call('GET',g)
  redis.call('HSET',l,'owner',ARGV[2],'token',token)
  redis.call('PEXPIRE',l,ARGV[3])
  return token
elseif op=='renew' then
  if fence>0 or not owns(ARGV[2],ARGV[3]) then return 0 end
  redis.call('PEXPIRE',l,ARGV[4]); return 1
elseif op=='release' then
  if owns(ARGV[2],ARGV[3]) then redis.call('DEL',l) end
  return 1
elseif op=='get_max' then return redis.call('HGET',c,'max')
elseif op=='set_max' or op=='set_ttl' then
  if fence>0 then return -fence end
  local field='max'; if op=='set_ttl' then field='ttl' end
  redis.call('HSET',c,field,ARGV[2]); return 1
elseif op=='state' then return fence
elseif op=='begin' then
  if fence==0 then redis.call('SET',d,1); fence=1 end
  redis.call('DEL',l)
  return fence
elseif op=='clear' then
  if fence==0 then return -3 end
  redis.call('DEL',q,e,l,c)
  return 1
elseif op=='mark' then
  if fence==0 then return -3 end
  redis.call('DEL',l)
  redis.call('SET',d,2)
  for _,key in ipairs({q,e,c,d}) do
    if ARGV[2]~='' then redis.call('PEXPIRE',key,ARGV[2]) else redis.call('PERSIST',key) end
  end
  return 1
end
return redis.error_reply('unknown pool operation')
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_timeout_and_supports_secure_urls() {
        assert!(RedisPoolStateStore::with_timeout("redis://localhost/", Duration::MAX).is_err());
        assert!(RedisPoolStateStore::with_timeout("redis://localhost/", Duration::ZERO).is_err());
        // The Redis dependency must enable tokio-rustls-comp, not silently downgrade TLS.
        assert!(RedisPoolStateStore::new("rediss://localhost/").is_ok());
    }
    #[test]
    fn cluster_keys_encode_names_and_debug_redacts_credentials() {
        let a = keys("a{b}:雪");
        assert!(a
            .iter()
            .all(|key| key.matches('{').count() == 1 && key.matches('}').count() == 1));
        assert_ne!(a, keys("a{b}:雪 "));
        let debug = format!(
            "{:?}",
            RedisPoolStateStore::new("redis://user:secret@localhost/").unwrap()
        );
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("localhost"));
    }
}
