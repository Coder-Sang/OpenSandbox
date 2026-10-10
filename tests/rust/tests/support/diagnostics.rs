// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Test-only evidence. Raw traces describe exactly the public HTTP request that
//! produced them, never a replay of an SDK execution. SDK summaries have no wire
//! evidence. Unknown strings, keys, headers, code and output are never persisted.
use opensandbox::{Error, Execution};
use serde_json::{json, Value};
use std::{
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_FRAMES: usize = 64;
const MAX_FRAME_BYTES: usize = 4096;
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn known_type(value: &str) -> &str {
    match value {
        "init" | "status" | "error" | "stdout" | "stderr" | "result" | "execution_complete"
        | "execution_count" | "ping" => value,
        _ => "[redacted]",
    }
}
fn content_type(value: Option<&str>) -> &str {
    match value.map(|v| v.split(';').next().unwrap_or("").trim()) {
        Some("text/event-stream") => "text/event-stream",
        Some("application/json") => "application/json",
        Some("text/plain") => "text/plain",
        None => "[missing]",
        _ => "[redacted]",
    }
}
pub fn innocuous_busy(value: &Value) -> bool {
    let error = value
        .get("error")
        .filter(|v| v.is_object())
        .unwrap_or(value);
    error.get("code").and_then(Value::as_str) == Some("RUNTIME_ERROR")
        && error.get("message").and_then(Value::as_str)
            == Some("error running codes session is busy")
}
fn scrub_json(data: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return json!({"classification":"invalid_json","payload":"[redacted]"});
    };
    let mut safe = json!({"classification":"missing_type","payload":"[redacted]"});
    if let Some(kind) = value.get("type") {
        safe["classification"] = json!("event");
        safe["type"] = json!(kind.as_str().map(known_type).unwrap_or("[redacted]"));
    }
    for field in [
        "text",
        "timestamp",
        "execution_count",
        "execution_time",
        "error",
    ] {
        if value.get(field).is_some() {
            safe[field] = json!("[redacted]");
        }
    }
    if let Some(results) = value.get("results") {
        let mut formats = Vec::new();
        for mime in [
            "text",
            "text/plain",
            "text/html",
            "application/json",
            "image/png",
            "image/jpeg",
        ] {
            if results.get(mime).is_some() {
                formats.push(mime);
            }
        }
        safe["result_formats"] = json!(formats);
        safe["results"] = json!("[redacted]");
    }
    // Exact pair, not prefix matching. A runtime can echo user input into other
    // errors. This one innocuous classification contains no execution content.
    if innocuous_busy(&value) {
        safe["allowlisted_error"] =
            json!({"code":"RUNTIME_ERROR","message":"error running codes session is busy"});
    }
    safe
}
fn scrub_frame(frame: &[u8]) -> String {
    let frame = String::from_utf8_lossy(frame);
    let mut data = Vec::new();
    let mut safe_lines = Vec::new();
    for line in frame.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start());
        } else if let Some(event) = line.strip_prefix("event:") {
            safe_lines.push(format!("event: {}", known_type(event.trim())));
        } else if line.starts_with("id:") {
            safe_lines.push("id: [redacted]".into());
        } else if line.starts_with(':') {
            safe_lines.push(": [redacted]".into());
        } else if line.trim_start().starts_with('{') {
            safe_lines.push(scrub_json(line).to_string());
        } else if !line.is_empty() {
            safe_lines.push("[redacted]".into());
        }
    }
    if !data.is_empty() {
        safe_lines.push(format!("data: {}", scrub_json(&data.join("\n"))));
    }
    safe_lines.join("\n")
}

/// Ordered redacted wire frames with explicit truncation. No arbitrary raw byte
/// prefix is ever written, including invalid JSON and truncated frames.
pub fn raw_trace(status: u16, mime: Option<&str>, body: &[u8], response_truncated: bool) -> Value {
    let captured = &body[..body.len().min(MAX_CAPTURE_BYTES)];
    let mut frames = Vec::new();
    let mut current = Vec::new();
    let mut overflow = false;
    let mut truncated = response_truncated || body.len() > captured.len();
    for line in captured.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let json_line = line.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{');
        if (line.is_empty() || json_line) && (!current.is_empty() || overflow) {
            if frames.len() == MAX_FRAMES {
                truncated = true;
                break;
            }
            frames.push(json!({"index":frames.len(),"raw_redacted":if overflow { "[oversized frame redacted]".into() } else { scrub_frame(&current) }}));
            current.clear();
            overflow = false;
        }
        if !line.is_empty() {
            if json_line {
                if frames.len() == MAX_FRAMES {
                    truncated = true;
                    break;
                }
                frames.push(json!({"index":frames.len(),"raw_redacted":if line.len() > MAX_FRAME_BYTES { "[oversized frame redacted]".into() } else { scrub_frame(line) }}));
            } else if current.len() + line.len() < MAX_FRAME_BYTES && !overflow {
                current.extend_from_slice(line);
                current.push(b'\n');
            } else {
                overflow = true;
                current.clear();
            }
        }
    }
    if !current.is_empty() || overflow {
        if frames.len() < MAX_FRAMES {
            frames.push(json!({"index":frames.len(),"raw_redacted":if overflow { "[oversized frame redacted]".into() } else { scrub_frame(&current) }}));
        } else {
            truncated = true;
        }
    }
    json!({"source":"same_public_raw_http_request_not_sdk_execution","http_status":status,"content_type":content_type(mime),"response_bytes_observed":body.len(),"response_truncated":response_truncated,"capture_truncated":truncated,"limits":{"capture_bytes":MAX_CAPTURE_BYTES,"frames":MAX_FRAMES,"frame_bytes":MAX_FRAME_BYTES},"frames":frames})
}

pub fn sdk_summary(outcome: &opensandbox::Result<Execution>) -> Value {
    match outcome {
        Ok(execution) => {
            json!({"source":"sdk_aggregate_no_wire_capture","complete":execution.complete,"kernel_error":execution.error.is_some(),"stdout_count":execution.logs.stdout.len(),"stderr_count":execution.logs.stderr.len(),"result_count":execution.result.len()})
        }
        Err(error) => {
            let classification = match error {
                Error::Json(error) if error.to_string().starts_with("missing field `type`") => {
                    "missing_type"
                }
                Error::Json(_) => "json_decode",
                Error::Protocol(_) => "protocol",
                Error::Transport(_) => "transport",
                Error::Api { .. } => "http_error",
                Error::InvalidConfig(_) => "invalid_config",
                Error::SandboxSetup { .. } => "setup",
                Error::ReadyTimeout { .. } => "ready_timeout",
            };
            let mut summary =
                json!({"source":"sdk_aggregate_no_wire_capture","classification":classification});
            if let Error::Api { status, .. } = error {
                summary["http_status"] = json!(status);
            }
            summary
        }
    }
}

pub fn save(label: &str, evidence: &Value) -> std::io::Result<()> {
    let Some(directory) = env::var_os("OPENSANDBOX_RUST_ARTIFACT_DIR") else {
        return Ok(());
    };
    if label.is_empty()
        || label.len() > 100
        || !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(std::io::Error::other("invalid diagnostic label"));
    }
    let directory = PathBuf::from(directory);
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "diagnostics require a private directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other(
                "diagnostic directory must be private (0700)",
            ));
        }
    }
    let mode = if env::var("OPENSANDBOX_TEST_USE_SERVER_PROXY").as_deref() == Ok("true") {
        "proxy"
    } else {
        "direct"
    };
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(
        "{label}-{mode}-{}-{sequence}.json",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(serde_json::to_string_pretty(evidence)?.as_bytes())?;
    file.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_secrets_unknown_fields_and_only_allowlists_exact_busy_pair() {
        let body = b"event: https://secret/url\nid: secret-token\ndata: {\"type\":\"stdout\",\"text\":\"print(user_code) stdout env TOKEN https://secret/url\",\"unknown-secret\":\"secret-token\"}\n\ndata: {\"code\":\"RUNTIME_ERROR\",\"message\":\"error running codes session is busy\"}\n\ndata: {\"code\":\"RUNTIME_ERROR\",\"message\":\"error running codes session is busy secret-token\"}\n\ndata: {invalid secret-token}\n\n";
        let trace = raw_trace(
            200,
            Some("text/event-stream; token=secret-token"),
            body,
            false,
        );
        let text = trace.to_string();
        for secret in [
            "secret-token",
            "https://secret",
            "user_code",
            "unknown-secret",
            "TOKEN",
        ] {
            assert!(!text.contains(secret));
        }
        assert_eq!(trace["frames"].as_array().unwrap().len(), 4);
        assert!(trace["frames"][1]["raw_redacted"]
            .as_str()
            .unwrap()
            .contains("error running codes session is busy"));
        assert!(!trace["frames"][2]["raw_redacted"]
            .as_str()
            .unwrap()
            .contains("error running codes"));
        assert_eq!(
            raw_trace(500, Some("secret-content-type"), b"secret-token", false)["content_type"],
            "[redacted]"
        );
    }
    #[test]
    fn preserves_frame_order_crlf_multidata_and_json_lines() {
        let trace = raw_trace(200, None, b"data: {\r\ndata: \"type\":\"stdout\",\"text\":\"private\"}\r\n\r\n{\"type\":\"execution_complete\"}\n{\"type\":\"stderr\"}", false);
        let frames = trace["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 3);
        for (index, kind) in ["stdout", "execution_complete", "stderr"]
            .iter()
            .enumerate()
        {
            assert_eq!(frames[index]["index"], index);
            assert!(frames[index]["raw_redacted"]
                .as_str()
                .unwrap()
                .contains(kind));
        }
    }
    #[test]
    fn bounds_frames_bytes_and_oversized_invalid_payloads() {
        let many = "data: {\"type\":\"stdout\"}\n\n".repeat(MAX_FRAMES + 5);
        let trace = raw_trace(200, None, many.as_bytes(), false);
        assert_eq!(trace["frames"].as_array().unwrap().len(), MAX_FRAMES);
        assert_eq!(trace["capture_truncated"], true);
        let large = format!("data: {}\n\n", "sensitive".repeat(MAX_CAPTURE_BYTES));
        let trace = raw_trace(200, None, large.as_bytes(), true);
        assert_eq!(trace["capture_truncated"], true);
        assert!(trace.to_string().len() < 2000);
        assert!(!trace.to_string().contains("sensitive"));
    }
    #[test]
    fn sdk_summary_never_includes_error_text_or_output() {
        let error = Err(Error::Api {
            status: 500,
            code: Some("secret-code".into()),
            message: "secret-message https://private".into(),
            request_id: Some("secret-id".into()),
        });
        let summary = sdk_summary(&error).to_string();
        assert!(!summary.contains("secret"));
        assert!(!summary.contains("https"));
        assert_eq!(sdk_summary(&error)["http_status"], 500);
    }
}
