// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0
use opensandbox::Error;
use std::time::Duration;
#[path = "support/http.rs"]
mod http;
use http::{sandbox, Reply, Server};

#[tokio::test]
async fn absent_identity_contract_rejects_without_http_or_fallback() {
    for proxy in [false, true] {
        let mut data = Server::start(vec![Reply::bytes(vec![]), Reply::bytes(vec![])]).await;
        let sandbox = sandbox(&mut data, proxy).await;
        for (uid, gid) in [(0, 0), (1000, 2000), (u32::MAX, 0), (0, u32::MAX)] {
            assert!(matches!(
                sandbox.files().with_identity(uid, gid),
                Err(Error::InvalidConfig(_))
            ));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), data.requests.recv())
                .await
                .is_err()
        );
    }
}
