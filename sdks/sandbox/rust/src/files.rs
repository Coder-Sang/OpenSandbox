// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    commands::nonblank,
    models::execd::{
        DownloadOptions, FileInfo, FileMetadata, Permission, RenameFileItem,
        ReplaceFileContentItem, ReplaceFileContentResult,
    },
    transport::Transport,
    Error, Result,
};
use reqwest::{
    multipart::{Form, Part},
    Method, Url,
};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Files {
    transport: Transport,
}
impl Files {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
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
        Transport::checked(
            self.transport
                .request(Method::POST, self.transport.url("/files/upload")?)
                .multipart(form)
                .send()
                .await?,
        )
        .await?
        .bytes()
        .await?;
        Ok(())
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
        let mut url = self.transport.url("/files/download")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("path", path);
            if let Some(offset) = options.offset {
                query.append_pair("offset", &offset.to_string());
            }
            if let Some(limit) = options.limit {
                query.append_pair("limit", &limit.to_string());
            }
        }
        let mut request = self.transport.request(Method::GET, url);
        if let Some(range) = options.range {
            request = request.header(reqwest::header::RANGE, range);
        }
        Ok(Transport::checked(request.send().await?)
            .await?
            .bytes()
            .await?
            .to_vec())
    }
    pub async fn read_text(&self, path: &str) -> Result<String> {
        String::from_utf8(self.download(path).await?)
            .map_err(|e| Error::Protocol(format!("File is not UTF-8: {e}")))
    }
    fn paths_url(&self, endpoint: &str, paths: &[String]) -> Result<Url> {
        if paths.is_empty() {
            return Err(Error::InvalidConfig("At least one path is required".into()));
        }
        let mut url = self.transport.url(endpoint)?;
        for path in paths {
            nonblank(path, "path")?;
            url.query_pairs_mut().append_pair("path", path);
        }
        Ok(url)
    }
    pub async fn info(&self, paths: &[String]) -> Result<HashMap<String, FileInfo>> {
        self.transport
            .json(
                Method::GET,
                self.paths_url("/files/info", paths)?,
                None::<&()>,
            )
            .await
    }
    pub async fn search(&self, path: &str, pattern: &str) -> Result<Vec<FileInfo>> {
        nonblank(path, "path")?;
        let mut url = self.transport.url("/files/search")?;
        url.query_pairs_mut()
            .append_pair("path", path)
            .append_pair("pattern", pattern);
        self.transport.json(Method::GET, url, None::<&()>).await
    }
    pub async fn remove(&self, paths: &[String]) -> Result<()> {
        self.transport
            .empty(
                Method::DELETE,
                self.paths_url("/files", paths)?,
                None::<&()>,
            )
            .await
    }
    pub async fn move_files(&self, entries: &[RenameFileItem]) -> Result<()> {
        for entry in entries {
            nonblank(&entry.src, "src")?;
            nonblank(&entry.dest, "dest")?;
        }
        self.transport
            .empty(
                Method::POST,
                self.transport.url("/files/mv")?,
                Some(&entries),
            )
            .await
    }
    pub async fn set_permissions(&self, entries: &HashMap<String, Permission>) -> Result<()> {
        validate_paths(entries.keys())?;
        self.transport
            .empty(
                Method::POST,
                self.transport.url("/files/permissions")?,
                Some(entries),
            )
            .await
    }
    pub async fn create_directories(&self, entries: &HashMap<String, Permission>) -> Result<()> {
        validate_paths(entries.keys())?;
        self.transport
            .empty(
                Method::POST,
                self.transport.url("/directories")?,
                Some(entries),
            )
            .await
    }
    pub async fn remove_directories(&self, paths: &[String]) -> Result<()> {
        self.transport
            .empty(
                Method::DELETE,
                self.paths_url("/directories", paths)?,
                None::<&()>,
            )
            .await
    }
    pub async fn list_directory(&self, path: &str, depth: Option<u32>) -> Result<Vec<FileInfo>> {
        nonblank(path, "path")?;
        let mut url = self.transport.url("/directories/list")?;
        url.query_pairs_mut().append_pair("path", path);
        if let Some(depth) = depth {
            url.query_pairs_mut()
                .append_pair("depth", &depth.to_string());
        }
        self.transport.json(Method::GET, url, None::<&()>).await
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
        let mut url = self.transport.url("/files/replace")?;
        url.query_pairs_mut().append_pair("verbose", "true");
        self.transport.json(Method::POST, url, Some(entries)).await
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
