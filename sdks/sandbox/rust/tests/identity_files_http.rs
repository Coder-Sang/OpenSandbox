// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use opensandbox::{
    DownloadOptions, Error, FileMetadata, Permission, RenameFileItem, ReplaceFileContentItem,
};
use serde_json::json;
use std::{collections::HashMap, time::Duration};

#[path = "support/http.rs"]
mod http;
use http::{assert_headers, route, sandbox, Reply, Server};

#[tokio::test]
async fn all_identity_operations_preserve_wire_and_direct_proxy_credentials() {
    for proxy in [false, true] {
        let path = "/workspace/a &?#%.bin";
        let second_path = "/workspace/你好.txt";
        let paths = [path.to_owned(), second_path.to_owned()];
        let binary = vec![0, 255, 128];
        let mut download = Reply::bytes(binary.clone());
        download.status = 206;
        let mut data = Server::start(vec![
            Reply::bytes(vec![]), // ping
            Reply::bytes(vec![]), // upload
            download,
            Reply::json(json!({})), // info
            Reply::bytes(vec![]),   // remove
            Reply::bytes(vec![]),   // permissions
            Reply::bytes(vec![]),   // move
            Reply::json(json!([])), // search
            Reply::json(json!({path: {"replacedCount": 2}})),
            Reply::bytes(vec![]),   // mkdir
            Reply::json(json!([])), // list
            Reply::bytes(vec![]),   // rmdir
        ])
        .await;
        let sandbox = sandbox(&mut data, proxy).await;
        let files = sandbox.files().with_identity(1000, 2000).unwrap();
        let metadata = FileMetadata {
            path: Some(path.into()),
            owner: Some("9999".into()),
            group: Some("8888".into()),
            mode: Some(640),
            ..Default::default()
        };
        files
            .upload_with_metadata(metadata, binary.clone())
            .await
            .unwrap();
        assert_eq!(
            files
                .download_with_options(
                    path,
                    DownloadOptions {
                        range: Some("bytes=0-2".into()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            binary
        );
        assert!(files.info(&paths).await.unwrap().is_empty());
        files.remove(&paths).await.unwrap();
        let permissions = HashMap::from([(
            path.into(),
            Permission {
                owner: Some("9999".into()),
                group: Some("8888".into()),
                mode: 640,
                ..Default::default()
            },
        )]);
        files.set_permissions(&permissions).await.unwrap();
        let moves = [RenameFileItem {
            src: path.into(),
            dest: second_path.into(),
            ..Default::default()
        }];
        files.move_files(&moves).await.unwrap();
        assert!(files
            .search("/workspace", "*.txt &?")
            .await
            .unwrap()
            .is_empty());
        let replacements = HashMap::from([(
            path.into(),
            ReplaceFileContentItem {
                old: "before".into(),
                new: "after".into(),
                ..Default::default()
            },
        )]);
        assert_eq!(
            files.replace_contents(&replacements).await.unwrap()[path].replaced_count,
            2
        );
        files.create_directories(&permissions).await.unwrap();
        assert!(files
            .list_directory("/workspace", Some(2))
            .await
            .unwrap()
            .is_empty());
        files.remove_directories(&paths).await.unwrap();

        for (index, (method, suffix)) in [
            ("POST", "/files/upload"),
            ("GET", "/files/download"),
            ("GET", "/files/info"),
            ("DELETE", "/files"),
            ("POST", "/files/permissions"),
            ("POST", "/files/mv"),
            ("GET", "/files/search"),
            ("POST", "/files/replace"),
            ("POST", "/directories"),
            ("GET", "/directories/list"),
            ("DELETE", "/directories"),
        ]
        .into_iter()
        .enumerate()
        {
            let request = data.next().await;
            assert_headers(&request, proxy);
            assert_eq!(request.method, method);
            let url = request.url();
            assert_eq!(
                url.path(),
                format!("{}/v1/filesystem/1000/2000{suffix}", route(proxy))
            );
            let query: Vec<_> = url.query_pairs().into_owned().collect();
            match index {
                0 => {
                    let text = String::from_utf8_lossy(&request.body);
                    assert!(text.contains("name=\"metadata\"; filename=\"metadata\""));
                    assert!(text.contains("Content-Type: application/json"));
                    assert!(text.contains("name=\"file\""));
                    assert!(text.contains("\"owner\":\"9999\""));
                    assert!(text.contains("\"group\":\"8888\""));
                    assert!(text.contains(&serde_json::to_string(path).unwrap()));
                    assert!(request
                        .body
                        .windows(binary.len())
                        .any(|bytes| bytes == binary));
                }
                1 => {
                    assert_eq!(request.headers["range"], "bytes=0-2");
                    assert_eq!(query, [("path".into(), path.into())]);
                }
                2 | 3 | 10 => assert_eq!(
                    query,
                    [
                        ("path".into(), path.into()),
                        ("path".into(), second_path.into())
                    ]
                ),
                4 | 8 => assert_eq!(request.json_body(), json!(permissions)),
                5 => assert_eq!(request.json_body(), json!(moves)),
                6 => assert_eq!(
                    query,
                    [
                        ("path".into(), "/workspace".into()),
                        ("pattern".into(), "*.txt &?".into())
                    ]
                ),
                7 => {
                    assert_eq!(query, [("verbose".into(), "true".into())]);
                    assert_eq!(request.json_body(), json!(replacements));
                }
                9 => assert_eq!(
                    query,
                    [
                        ("path".into(), "/workspace".into()),
                        ("depth".into(), "2".into())
                    ]
                ),
                _ => unreachable!(),
            }
        }
    }
}

#[tokio::test]
async fn identity_rebinding_cloning_and_text_helpers_preserve_original_scope() {
    let mut data = Server::start(vec![
        Reply::bytes(vec![]),
        Reply::bytes(vec![]),
        Reply::bytes("你好\n".as_bytes().to_vec()),
        Reply::json(json!({})),
        Reply::json(json!({})),
        Reply::json(json!({})),
    ])
    .await;
    let sandbox = sandbox(&mut data, false).await;
    let ordinary = sandbox.files();
    let identity = ordinary.with_identity(0, u32::MAX - 1).unwrap();
    let rebound = identity.with_identity(u32::MAX - 1, 0).unwrap();
    identity
        .clone()
        .write_text("/tmp/text", "你好\n")
        .await
        .unwrap();
    assert_eq!(rebound.read_text("/tmp/text").await.unwrap(), "你好\n");
    ordinary.info(&["/tmp/text".into()]).await.unwrap();
    identity.info(&["/tmp/text".into()]).await.unwrap();
    rebound.info(&["/tmp/text".into()]).await.unwrap();
    for expected in [
        "/route/v1/filesystem/0/4294967294/files/upload",
        "/route/v1/filesystem/4294967294/0/files/download",
        "/route/files/info",
        "/route/v1/filesystem/0/4294967294/files/info",
        "/route/v1/filesystem/4294967294/0/files/info",
    ] {
        assert_eq!(data.next().await.url().path(), expected);
    }
}

#[tokio::test]
async fn identity_download_line_options_remain_distinct_from_byte_ranges() {
    let mut data =
        Server::start(vec![Reply::bytes(vec![]), Reply::bytes(b"line\n".to_vec())]).await;
    let sandbox = sandbox(&mut data, false).await;
    let files = sandbox.files().with_identity(1, 2).unwrap();
    assert_eq!(
        files
            .download_with_options(
                "/tmp/file",
                DownloadOptions {
                    offset: Some(3),
                    limit: Some(1),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        b"line\n"
    );
    let request = data.next().await;
    assert_eq!(
        request.target,
        "/route/v1/filesystem/1/2/files/download?path=%2Ftmp%2Ffile&offset=3&limit=1"
    );
    assert!(!request.headers.contains_key("range"));
}

#[tokio::test]
async fn reserved_ids_and_isolated_scope_are_rejected_before_http() {
    let mut data = Server::start(vec![
        Reply::bytes(vec![]),
        Reply::json(json!({"status":"ready"})),
        Reply::json(json!({})), // remain listening to detect accidental requests
    ])
    .await;
    let sandbox = sandbox(&mut data, false).await;
    let files = sandbox.files();
    for (uid, gid) in [(u32::MAX, 0), (0, u32::MAX), (u32::MAX, u32::MAX)] {
        assert!(matches!(
            files.with_identity(uid, gid),
            Err(Error::InvalidConfig(_))
        ));
    }
    let session = sandbox.isolated().attach("session-a").await.unwrap();
    assert_eq!(
        data.next().await.target,
        "/route/v1/isolated/session/session-a"
    );
    assert!(matches!(
        session.files().with_identity(1000, 1000),
        Err(Error::InvalidConfig(_))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), data.requests.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn identity_errors_never_retry_an_unscoped_filesystem_route() {
    for proxy in [false, true] {
        for status in [403, 404, 501, 503] {
            let mut failure =
                Reply::json(json!({"code":"IDENTITY_ERROR", "message":"identity unavailable"}));
            failure.status = status;
            failure.headers = "x-request-id: identity-request\r\n".into();
            let mut data = Server::start(vec![
                Reply::bytes(vec![]),
                failure,
                Reply::bytes(b"must not be used".to_vec()),
            ])
            .await;
            let sandbox = sandbox(&mut data, proxy).await;
            let files = sandbox.files().with_identity(1000, 2000).unwrap();
            assert!(matches!(files.download("/workspace/file").await,
                Err(Error::Api {status: actual, code: Some(code), request_id: Some(id), ..})
                    if actual == status && code == "IDENTITY_ERROR" && id == "identity-request"));
            let request = data.next().await;
            assert_headers(&request, proxy);
            assert_eq!(
                request.url().path(),
                format!("{}/v1/filesystem/1000/2000/files/download", route(proxy))
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(30), data.requests.recv())
                    .await
                    .is_err()
            );
        }
    }
}
