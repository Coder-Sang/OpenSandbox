// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    api::{client::FilesystemScope, execd_client as api},
    commands::nonblank,
    models::execd::{
        DownloadOptions, FileInfo, FileMetadata, Permission, RenameFileItem,
        ReplaceFileContentItem, ReplaceFileContentResult,
    },
    transport::Transport,
    Error, Result,
};
use reqwest::multipart::{Form, Part};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Files {
    transport: Transport,
    scope: Option<FileScope>,
}
#[derive(Clone)]
enum FileScope {
    Identity(u32, u32),
    Isolated(String),
}
impl Files {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            transport,
            scope: None,
        }
    }
    pub(crate) fn scoped(transport: Transport, session_id: &str) -> Result<Self> {
        nonblank(session_id, "session_id")?;
        if matches!(session_id, "." | "..") {
            return Err(Error::InvalidConfig("Invalid session ID".into()));
        }
        Ok(Self {
            transport,
            scope: Some(FileScope::Isolated(session_id.into())),
        })
    }
    /// Return a new file handle executing as the selected Linux UID/GID.
    ///
    /// IDs must be in `0..u32::MAX`. Owner/group metadata is independent of
    /// execution identity. Rebinding replaces an existing identity without
    /// changing the original handle. Isolated-session handles reject rebinding.
    /// Server errors never fall back to an ordinary filesystem route.
    pub fn with_identity(&self, uid: u32, gid: u32) -> Result<Self> {
        if uid == u32::MAX || gid == u32::MAX {
            return Err(Error::InvalidConfig(
                "Filesystem UID and GID must be between 0 and 4294967294".into(),
            ));
        }
        if matches!(self.scope, Some(FileScope::Isolated(_))) {
            return Err(Error::InvalidConfig(
                "Filesystem identity cannot be changed on an isolated-session handle".into(),
            ));
        }
        Ok(Self {
            transport: self.transport.clone(),
            scope: Some(FileScope::Identity(uid, gid)),
        })
    }
    fn client(&self) -> api::Client<'_> {
        let scope = match &self.scope {
            None => FilesystemScope::Default,
            Some(FileScope::Identity(uid, gid)) => FilesystemScope::Identity(*uid, *gid),
            Some(FileScope::Isolated(id)) => FilesystemScope::Isolated(id),
        };
        api::Client::filesystem(&self.transport, scope)
    }
    pub async fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<()> {
        self.upload_with_metadata(
            FileMetadata {
                path: Some(path.to_owned()),
                ..Default::default()
            },
            bytes,
        )
        .await
    }
    pub async fn upload_with_metadata(&self, metadata: FileMetadata, bytes: Vec<u8>) -> Result<()> {
        let path = metadata
            .path
            .as_deref()
            .ok_or_else(|| Error::InvalidConfig("Upload path is required".into()))?;
        nonblank(path, "path")?;
        let filename = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("file")
            .to_owned();
        let form = Form::new()
            .part(
                "metadata",
                Part::bytes(serde_json::to_vec(&metadata)?)
                    .file_name("metadata")
                    .mime_str("application/json")?,
            )
            .part(
                "file",
                Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str("application/octet-stream")?,
            );
        self.client().upload_file()?.multipart(form).empty().await
    }
    pub async fn write_text(&self, path: &str, text: &str) -> Result<()> {
        self.upload(path, text.as_bytes().to_vec()).await
    }
    pub async fn download(&self, path: &str) -> Result<Vec<u8>> {
        self.download_with_options(path, DownloadOptions::default())
            .await
    }
    pub async fn download_with_options(
        &self,
        path: &str,
        options: DownloadOptions,
    ) -> Result<Vec<u8>> {
        nonblank(path, "path")?;
        validate_download(&options)?;
        Ok(self
            .client()
            .download_file(api::DownloadFileParams {
                path,
                offset: options.offset,
                limit: options.limit,
                range: options.range.as_deref(),
            })?
            .raw()
            .await?
            .bytes()
            .await?
            .to_vec())
    }
    pub async fn read_text(&self, path: &str) -> Result<String> {
        String::from_utf8(self.download(path).await?)
            .map_err(|e| Error::Protocol(format!("File is not UTF-8: {e}")))
    }
    fn validate_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Err(Error::InvalidConfig("At least one path is required".into()));
        }
        for path in paths {
            nonblank(path, "path")?;
        }
        Ok(())
    }
    pub async fn info(&self, paths: &[String]) -> Result<HashMap<String, FileInfo>> {
        self.validate_paths(paths)?;
        self.client()
            .get_files_info(api::GetFilesInfoParams { path: paths })?
            .json_as()
            .await
    }
    pub async fn search(&self, path: &str, pattern: &str) -> Result<Vec<FileInfo>> {
        nonblank(path, "path")?;
        self.client()
            .search_files(api::SearchFilesParams {
                path,
                pattern: Some(pattern),
            })?
            .json()
            .await
    }
    pub async fn remove(&self, paths: &[String]) -> Result<()> {
        self.validate_paths(paths)?;
        self.client()
            .remove_files(api::RemoveFilesParams { path: paths })?
            .empty()
            .await
    }
    pub async fn move_files(&self, entries: &[RenameFileItem]) -> Result<()> {
        for entry in entries {
            nonblank(&entry.src, "src")?;
            nonblank(&entry.dest, "dest")?;
        }
        self.client().rename_files(entries)?.empty().await
    }
    pub async fn set_permissions(&self, entries: &HashMap<String, Permission>) -> Result<()> {
        validate_paths(entries.keys())?;
        let body = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.client().chmod_files(&body)?.empty().await
    }
    pub async fn create_directories(&self, entries: &HashMap<String, Permission>) -> Result<()> {
        validate_paths(entries.keys())?;
        let body = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.client().make_dirs(&body)?.empty().await
    }
    pub async fn remove_directories(&self, paths: &[String]) -> Result<()> {
        self.validate_paths(paths)?;
        self.client()
            .remove_dirs(api::RemoveDirsParams { path: paths })?
            .empty()
            .await
    }
    pub async fn list_directory(&self, path: &str, depth: Option<u32>) -> Result<Vec<FileInfo>> {
        nonblank(path, "path")?;
        self.client()
            .list_directory(api::ListDirectoryParams {
                path,
                depth: depth.map(u64::from),
            })?
            .json()
            .await
    }
    pub async fn replace_contents(
        &self,
        entries: &HashMap<String, ReplaceFileContentItem>,
    ) -> Result<HashMap<String, ReplaceFileContentResult>> {
        validate_paths(entries.keys())?;
        if entries.values().any(|entry| entry.old.is_empty()) {
            return Err(Error::InvalidConfig(
                "Replacement old text cannot be empty".into(),
            ));
        }
        let body = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.client()
            .replace_content(
                api::ReplaceContentParams {
                    verbose: Some(true),
                },
                &body,
            )?
            .json_as()
            .await
    }
}

fn validate_paths<'a>(paths: impl Iterator<Item = &'a String>) -> Result<()> {
    for path in paths {
        nonblank(path, "path")?;
    }
    Ok(())
}
fn validate_download(options: &DownloadOptions) -> Result<()> {
    if options.range.is_some() && (options.offset.is_some() || options.limit.is_some()) {
        return Err(Error::InvalidConfig(
            "Byte range and line offset/limit are mutually exclusive".into(),
        ));
    }
    if options.offset == Some(0) || options.limit == Some(0) {
        return Err(Error::InvalidConfig(
            "Line offset and limit must be at least 1".into(),
        ));
    }
    if let Some(range) = &options.range {
        let valid = range.strip_prefix("bytes=").is_some_and(|value| {
            !value.is_empty()
                && value.split(',').all(|part| {
                    let Some((start, end)) = part.trim().split_once('-') else {
                        return false;
                    };
                    match (start.parse::<u64>(), end.parse::<u64>()) {
                        (Ok(start), Ok(end)) => start <= end,
                        (Ok(_), Err(_)) => end.is_empty(),
                        (Err(_), Ok(end)) => start.is_empty() && end > 0,
                        _ => false,
                    }
                })
        });
        if !valid {
            return Err(Error::InvalidConfig("Invalid HTTP byte range".into()));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn multipart_has_metadata_and_file_and_download_keeps_binary() {
        let (transport, task) = crate::commands::tests::serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        Files::new(transport)
            .upload("/workspace/hello.bin", vec![0, 255, 1])
            .await
            .unwrap();
        let request = task.await.unwrap();
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /prefix/v1/files/upload HTTP/1.1"));
        assert!(text.contains("name=\"metadata\"; filename=\"metadata\""));
        assert!(text.contains("Content-Type: application/json"));
        assert!(text.contains("name=\"file\"; filename=\"hello.bin\""));
        assert!(text.contains("\"path\":\"/workspace/hello.bin\""));
        assert!(request.windows(3).any(|bytes| bytes == [0, 255, 1]));
        let mut response =
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nConnection: close\r\n\r\n"
                .to_vec();
        response.extend([0, 255, 1]);
        let (transport, task) = crate::commands::tests::serve_once(response).await;
        let bytes = Files::new(transport)
            .download_with_options(
                "/workspace/a b",
                DownloadOptions {
                    range: Some("bytes=0-2".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(bytes, [0, 255, 1]);
        let request = String::from_utf8(task.await.unwrap()).unwrap();
        assert!(request.contains("path=%2Fworkspace%2Fa+b"));
        assert!(request.contains("range: bytes=0-2"));
    }
    #[test]
    fn byte_ranges_and_line_offsets_are_distinct() {
        for range in ["bytes=0-99", "bytes=100-", "bytes=-20", "bytes=0-10,20-30"] {
            assert!(validate_download(&DownloadOptions {
                range: Some(range.into()),
                ..Default::default()
            })
            .is_ok());
        }
        for range in ["bytes=10-1", "bytes=-0", "bytes=-", "lines=1-2"] {
            assert!(validate_download(&DownloadOptions {
                range: Some(range.into()),
                ..Default::default()
            })
            .is_err());
        }
        assert!(validate_download(&DownloadOptions {
            offset: Some(0),
            ..Default::default()
        })
        .is_err());
        assert!(validate_download(&DownloadOptions {
            range: Some("bytes=0-1".into()),
            offset: Some(1),
            ..Default::default()
        })
        .is_err());
    }
}
