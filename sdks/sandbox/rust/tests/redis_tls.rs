// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "redis")]

use opensandbox::pool::{PoolDestroyState, PoolError, PoolStateStore, RedisPoolStateStore};
use std::{
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Each child gets its own trust environment. Never mutate the parent process's
// environment or the user's native certificate store while tests run concurrently.
#[test]
fn redis_tls_certificate_and_authentication_validation() {
    if std::env::var("OPENSANDBOX_REDIS_TLS_TEST").ok().as_deref() != Some("1") {
        return;
    }
    let url = std::env::var("OPENSANDBOX_REDIS_TLS_URL")
        .expect("TLS tests require a dedicated TLS-only, password-protected Redis");
    let trusted_ca = std::env::var("OPENSANDBOX_REDIS_TLS_CA")
        .expect("TLS tests require the fixture's CA PEM file");
    let untrusted_ca = std::env::var("OPENSANDBOX_REDIS_TLS_UNTRUSTED_CA")
        .expect("TLS tests require an unrelated CA PEM file");
    let wrong_host = std::env::var("OPENSANDBOX_REDIS_TLS_WRONG_HOST_URL")
        .expect("TLS tests require the same server through a hostname absent from its certificate");
    for value in [&url, &wrong_host] {
        let parsed = url::Url::parse(value).expect("invalid TLS fixture URL");
        assert_eq!(parsed.scheme(), "rediss");
        assert!(
            parsed.fragment().is_none(),
            "verification must stay enabled"
        );
        assert!(parsed.password().is_some(), "fixture must require AUTH");
    }
    let trusted_url = url::Url::parse(&url).unwrap();
    let rejected_host_url = url::Url::parse(&wrong_host).unwrap();
    assert_eq!(trusted_url.host_str(), Some("localhost"));
    assert_eq!(rejected_host_url.host_str(), Some("127.0.0.1"));
    assert_eq!(trusted_url.port(), rejected_host_url.port());
    assert_eq!(trusted_url.path(), rejected_host_url.path());
    assert_eq!(trusted_url.username(), rejected_host_url.username());
    // Avoid assertion diagnostics that might expose a fixture password.
    assert!(trusted_url.password() == rejected_host_url.password());
    let mut wrong_password = trusted_url;
    wrong_password
        .set_password(Some("opensandbox-tls-intentionally-wrong-password"))
        .unwrap();
    for (case, child_url, ca) in [
        ("trusted", url.as_str(), trusted_ca.as_str()),
        ("untrusted-ca", url.as_str(), untrusted_ca.as_str()),
        ("wrong-hostname", wrong_host.as_str(), trusted_ca.as_str()),
        (
            "wrong-password",
            wrong_password.as_str(),
            trusted_ca.as_str(),
        ),
    ] {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "redis_tls_child", "--ignored", "--nocapture"])
            .env("OPENSANDBOX_REDIS_TLS_CHILD", case)
            .env("OPENSANDBOX_REDIS_TLS_CHILD_URL", child_url)
            .env("SSL_CERT_FILE", ca)
            .env_remove("SSL_CERT_DIR")
            .status()
            .expect("could not run isolated TLS case");
        assert!(status.success(), "Redis TLS case failed: {case}");
    }
}

#[test]
#[ignore = "invoked by the TLS test with a child-process-only trust configuration"]
fn redis_tls_child() {
    let Ok(case) = std::env::var("OPENSANDBOX_REDIS_TLS_CHILD") else {
        return;
    };
    let url = std::env::var("OPENSANDBOX_REDIS_TLS_CHILD_URL").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = RedisPoolStateStore::with_timeout(&url, Duration::from_secs(5)).unwrap();
        let pool = format!(
            "rust-tls-test-{}-{}-{case}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        if case != "trusted" {
            // A reference connection identifies the rejection cause, not merely
            // an unrelated outage. The public store must reject the same fixture.
            let client = redis::Client::open(url.as_str()).unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                client.get_multiplexed_async_connection(),
            )
            .await
            .expect("reference TLS connection must reject before the timeout");
            let error = match result {
                Ok(_) => panic!("invalid TLS/authentication fixture was accepted"),
                Err(error) => error.to_string(),
            };
            let expected = match case.as_str() {
                "untrusted-ca" => "UnknownIssuer",
                "wrong-hostname" => "certificate not valid for name",
                "wrong-password" => "AuthenticationFailed",
                _ => panic!("unknown TLS test case"),
            };
            assert!(error.contains(expected), "unexpected rejection: {error}");
            assert!(matches!(
                store.set_max_idle(&pool, 2).await,
                Err(PoolError::Store(message)) if message == "Redis connection failed"
            ));
            println!("TLS case {case}: reference cause {expected}, public store rejected");
            return;
        }

        let peer = RedisPoolStateStore::new(&url).unwrap();
        assert_eq!(store.get_max_idle(&pool).await.unwrap(), None);
        store.set_max_idle(&pool, 2).await.unwrap();
        assert_eq!(peer.get_max_idle(&pool).await.unwrap(), Some(2));
        store
            .set_idle_entry_ttl(&pool, Duration::from_secs(30))
            .await
            .unwrap();
        let lease = store
            .try_acquire_primary_lock(&pool, "tls-owner", Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        assert!(peer
            .put_idle(&pool, "tls-sandbox-id", &lease)
            .await
            .unwrap());
        let snapshot = store.snapshot_idle_entries(&pool).await.unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].sandbox_id, "tls-sandbox-id");
        assert_eq!(
            peer.try_take_idle(&pool, Duration::ZERO)
                .await
                .unwrap()
                .sandbox_id
                .as_deref(),
            Some("tls-sandbox-id")
        );
        peer.release_primary_lock(&pool, &lease).await.unwrap();
        assert_eq!(
            store.begin_destroy(&pool).await.unwrap(),
            PoolDestroyState::Destroying
        );
        store.clear_pool_state(&pool).await.unwrap();
        store
            .mark_destroyed(&pool, Some(Duration::from_millis(1)))
            .await
            .unwrap();
        // The generation key intentionally persists until this dedicated fixture
        // is deleted. No FLUSHDB or unrelated namespace cleanup is performed.
        println!("TLS case trusted: authenticated public-store operations and cleanup passed");
    });
}
