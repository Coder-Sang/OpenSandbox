// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

pub use crate::api::execd::{
    CommandStatusResponse as CommandStatus, FileInfo, FileMetadata, Metrics as SandboxMetrics,
    Permission, RenameFileItem, ReplaceFileContentItem, ReplaceFileContentResult,
};

#[derive(Clone, Debug, Default)]
pub struct RunCommandOptions {
    /// Server execution timeout in whole milliseconds. Zero disables the
    /// server timeout, matching execd. None leaves the timeout unspecified.
    pub timeout: Option<Duration>,
    pub cwd: Option<String>,
    pub envs: Option<HashMap<String, String>>,
    pub background: bool,
    pub uid: Option<i32>,
    pub gid: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub execution_count: Option<i64>,
    #[serde(default)]
    pub execution_time: Option<i64>,
    #[serde(default)]
    pub results: Option<HashMap<String, serde_json::Value>>,
    #[serde(default)]
    pub error: Option<ExecutionError>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExecutionError {
    #[serde(default, alias = "name")]
    pub ename: String,
    #[serde(default, alias = "value")]
    pub evalue: String,
    #[serde(default, deserialize_with = "deserialize_traceback")]
    pub traceback: Vec<String>,
}

// execd's isolated command errors serialize an absent Go slice as JSON null.
// Keep the public Vec surface and reject malformed non-array traceback values.
fn deserialize_traceback<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
}
#[derive(Clone, Debug, Default)]
pub struct ExecutionLogs {
    pub stdout: Vec<OutputMessage>,
    pub stderr: Vec<OutputMessage>,
}
#[derive(Clone, Debug)]
pub struct OutputMessage {
    pub text: String,
    pub timestamp: Option<i64>,
}
#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub text: Option<String>,
    pub timestamp: Option<i64>,
    pub raw: HashMap<String, serde_json::Value>,
}
#[derive(Clone, Debug, Default)]
pub struct Execution {
    pub id: Option<String>,
    pub logs: ExecutionLogs,
    pub result: Vec<ExecutionResult>,
    pub error: Option<ExecutionError>,
    pub complete: bool,
    pub execution_count: Option<i64>,
    pub execution_time: Option<Duration>,
    pub exit_code: Option<i32>,
}
impl Execution {
    pub(crate) fn apply(&mut self, event: ExecutionEvent) {
        match event.event_type.as_str() {
            "init" => {
                if let Some(text) = event.text.filter(|s| !s.is_empty()) {
                    self.id = Some(text);
                }
            }
            "stdout" | "stderr" => {
                let msg = OutputMessage {
                    text: event.text.unwrap_or_default(),
                    timestamp: event.timestamp,
                };
                if event.event_type == "stdout" {
                    self.logs.stdout.push(msg);
                } else {
                    self.logs.stderr.push(msg);
                }
            }
            "result" => {
                let raw = event.results.unwrap_or_default();
                let text = raw
                    .get("text/plain")
                    .or_else(|| raw.get("text"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                self.result.push(ExecutionResult {
                    text,
                    raw,
                    timestamp: event.timestamp,
                });
            }
            "error" => self.error = event.error,
            "execution_count" => self.execution_count = event.execution_count,
            "execution_complete" => {
                self.complete = true;
                self.execution_time = event
                    .execution_time
                    .and_then(|ms| u64::try_from(ms).ok())
                    .map(Duration::from_millis);
            }
            _ => {}
        }
    }
}
#[derive(Clone, Debug)]
pub struct CommandLogs {
    pub content: String,
    pub cursor: Option<u64>,
}
#[derive(Clone, Debug, Default)]
pub struct DownloadOptions {
    /// HTTP byte range, for example `bytes=0-99`. Exclusive with line options.
    pub range: Option<String>,
    /// One-based starting line, not a byte offset.
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}
