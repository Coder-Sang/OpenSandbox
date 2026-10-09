// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{config::metrics_disabled_by_env, ConnectionConfig};
use serde::Serialize;
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const METRICS_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_FIELD_BYTES: usize = 1024;
static METRICS_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
static METRICS_CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
#[cfg(all(test, feature = "tracing"))]
pub(crate) static TRACER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateEvent {
    event_type: &'static str,
    create_duration_ms: u64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

/// Reports on drop as well, so cancellation does not replace or hide the outcome.
pub(crate) struct CreationMetric {
    config: ConnectionConfig,
    started: Instant,
    sandbox_id: Option<String>,
    image: Option<String>,
    success: bool,
}
impl CreationMetric {
    pub(crate) fn new(config: &ConnectionConfig, image: Option<&str>) -> Self {
        Self {
            config: config.clone(),
            started: Instant::now(),
            sandbox_id: None,
            image: image
                .filter(|value| {
                    !value.contains("://") && !value.contains('?') && !value.contains('#')
                })
                .and_then(field),
            success: false,
        }
    }
    pub(crate) fn created(&mut self, id: &str) {
        self.sandbox_id = field(id);
    }
    pub(crate) fn succeeded(&mut self) {
        self.success = true;
    }
}
fn field(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= MAX_FIELD_BYTES).then(|| value.to_owned())
}
impl Drop for CreationMetric {
    fn drop(&mut self) {
        if self.config.disable_metrics || metrics_disabled_by_env() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(permit) = METRICS_SLOTS
            .get_or_init(|| Arc::new(Semaphore::new(16)))
            .clone()
            .try_acquire_owned()
        else {
            return;
        };
        let Ok(url) = self.config.base_url.join("metrics/events") else {
            return;
        };
        let mut headers = self.config.headers.clone();
        headers.remove("traceparent");
        headers.remove("tracestate");
        let payload = CreateEvent {
            event_type: "sandbox.create",
            create_duration_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            success: self.success,
            sandbox_id: self.sandbox_id.take(),
            image: self.image.take(),
        };
        let timeout = METRICS_TIMEOUT.min(self.config.request_timeout);
        runtime.spawn(async move {
            let _permit = permit;
            let Some(client) = METRICS_CLIENT
                .get_or_init(|| {
                    reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(METRICS_TIMEOUT)
                        .pool_max_idle_per_host(0)
                        .build()
                        .ok()
                })
                .clone()
            else {
                return;
            };
            let _ = tokio::time::timeout(timeout, async {
                let Ok(mut response) = client
                    .post(url)
                    .headers(headers)
                    .json(&payload)
                    .send()
                    .await
                else {
                    return;
                };
                // Stream and discard, never allocate an unbounded response body.
                let mut drained = 0usize;
                while let Ok(Some(chunk)) = response.chunk().await {
                    drained = drained.saturating_add(chunk.len());
                    if drained > 64 * 1024 {
                        break;
                    }
                }
            })
            .await;
        });
    }
}

pub(crate) struct WarmupTrace {
    #[cfg(feature = "tracing")]
    context: Option<opentelemetry::Context>,
}
pub(crate) struct WarmupPhase {
    #[cfg(feature = "tracing")]
    context: Option<opentelemetry::Context>,
}
impl WarmupTrace {
    pub(crate) fn new(config: &ConnectionConfig) -> Self {
        #[cfg(feature = "tracing")]
        {
            use opentelemetry::trace::{TraceContextExt, Tracer};
            let context = config.enable_tracing.then(|| {
                let span = opentelemetry::global::tracer("opensandbox")
                    .start_with_context("pool.warmup", &opentelemetry::Context::new());
                opentelemetry::Context::new().with_span(span)
            });
            Self { context }
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = config;
            Self {}
        }
    }
    pub(crate) fn phase(&self, name: &'static str) -> WarmupPhase {
        let name = match name {
            "create" | "readiness" | "prepare" | "post_prepare_readiness" | "renew" | "commit" => {
                name
            }
            _ => "phase",
        };
        #[cfg(feature = "tracing")]
        {
            use opentelemetry::trace::{TraceContextExt, Tracer};
            WarmupPhase {
                context: self.context.as_ref().map(|parent| {
                    let span = opentelemetry::global::tracer("opensandbox")
                        .start_with_context(format!("pool.warmup.{name}"), parent);
                    parent.with_span(span)
                }),
            }
        }
        #[cfg(not(feature = "tracing"))]
        {
            let _ = name;
            WarmupPhase {}
        }
    }
    pub(crate) fn attribute(&self, key: &'static str, value: &str) {
        #[cfg(feature = "tracing")]
        attribute(self.context.as_ref(), key, value);
        #[cfg(not(feature = "tracing"))]
        let _ = (key, value);
    }
    pub(crate) fn counter(&self, key: &'static str, value: u64) {
        #[cfg(feature = "tracing")]
        counter(self.context.as_ref(), key, value);
        #[cfg(not(feature = "tracing"))]
        let _ = (key, value);
    }
    pub(crate) fn finish(&mut self, success: bool) {
        #[cfg(feature = "tracing")]
        finish(&mut self.context, success);
        #[cfg(not(feature = "tracing"))]
        let _ = success;
    }
}
impl WarmupPhase {
    pub(crate) fn scoped_config(&self, config: &ConnectionConfig) -> ConnectionConfig {
        #[allow(unused_mut)]
        let mut config = config.clone();
        #[cfg(feature = "tracing")]
        if let Some(context) = &self.context {
            use opentelemetry::trace::TraceContextExt;
            let span = context.span();
            let sc = span.span_context();
            if sc.is_valid() {
                let value = format!(
                    "00-{}-{}-{:02x}",
                    sc.trace_id(),
                    sc.span_id(),
                    sc.trace_flags().to_u8()
                );
                if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
                    config.headers.insert("traceparent", value);
                    config.headers.remove("tracestate");
                }
            }
        }
        config
    }
    pub(crate) fn attribute(&self, key: &'static str, value: &str) {
        #[cfg(feature = "tracing")]
        attribute(self.context.as_ref(), key, value);
        #[cfg(not(feature = "tracing"))]
        let _ = (key, value);
    }
    pub(crate) fn counter(&self, key: &'static str, value: u64) {
        #[cfg(feature = "tracing")]
        counter(self.context.as_ref(), key, value);
        #[cfg(not(feature = "tracing"))]
        let _ = (key, value);
    }
    pub(crate) fn finish(&mut self, success: bool) {
        #[cfg(feature = "tracing")]
        finish(&mut self.context, success);
        #[cfg(not(feature = "tracing"))]
        let _ = success;
    }
}
#[cfg(feature = "tracing")]
fn attribute(context: Option<&opentelemetry::Context>, key: &'static str, value: &str) {
    use opentelemetry::trace::TraceContextExt;
    if !matches!(
        key,
        "pool.name" | "pool.owner_id" | "sandbox.id" | "outcome" | "failure.kind"
    ) || value.len() > MAX_FIELD_BYTES
    {
        return;
    }
    if let Some(context) = context {
        context
            .span()
            .set_attribute(opentelemetry::KeyValue::new(key, value.to_owned()));
    }
}
#[cfg(feature = "tracing")]
fn counter(context: Option<&opentelemetry::Context>, key: &'static str, value: u64) {
    use opentelemetry::trace::TraceContextExt;
    if !matches!(key, "pool.warmup.attempt" | "pool.warmup.failure_count") {
        return;
    }
    if let Some(context) = context {
        context.span().set_attribute(opentelemetry::KeyValue::new(
            key,
            value.min(i64::MAX as u64) as i64,
        ));
    }
}
#[cfg(feature = "tracing")]
fn finish(context: &mut Option<opentelemetry::Context>, success: bool) {
    use opentelemetry::trace::{Status, TraceContextExt};
    if let Some(context) = context.take() {
        let span = context.span();
        span.set_attribute(opentelemetry::KeyValue::new(
            "outcome",
            if success { "success" } else { "failure" },
        ));
        span.set_status(if success {
            Status::Ok
        } else {
            Status::error("warmup phase failed or cancelled")
        });
        span.end();
    }
}
impl Drop for WarmupPhase {
    fn drop(&mut self) {
        self.finish(false);
    }
}
impl Drop for WarmupTrace {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateSandboxOptions, Error, SandboxManager};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn capture(listener: TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail").await.unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[tokio::test]
    async fn http_payload_failure_setup_id_and_safe_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}/prefix", listener.local_addr().unwrap()))
            .api_key("key")
            .header("traceparent", "private-trace")
            .build()
            .unwrap();
        let task = tokio::spawn(capture(listener));
        let mut metric = CreationMetric::new(&config, Some("ubuntu:24.04"));
        metric.created("created-id");
        drop(metric);
        let request = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("POST /prefix/v1/metrics/events HTTP/1.1"));
        assert!(request.contains("open-sandbox-api-key: key"));
        assert!(!request.contains("traceparent"));
        let payload: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(payload["eventType"], "sandbox.create");
        assert_eq!(payload["sandboxId"], "created-id");
        assert_eq!(payload["image"], "ubuntu:24.04");
        assert_eq!(payload["success"], false);
        assert!(payload["createDurationMs"].is_u64());
    }

    #[tokio::test]
    async fn creation_validation_failure_reports_without_masking_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}", listener.local_addr().unwrap()))
            .build()
            .unwrap();
        let task = tokio::spawn(capture(listener));
        let manager = SandboxManager::new(config).unwrap();
        assert!(matches!(
            manager.create(CreateSandboxOptions::image("")).await,
            Err(Error::InvalidConfig(_))
        ));
        let request = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("\"success\":false"));
    }

    #[tokio::test]
    async fn actual_setup_failure_retains_id_in_error_and_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}", listener.local_addr().unwrap()))
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            let body = r#"{"id":"setup-box","entrypoint":["true"],"status":{"state":"Pending"},"createdAt":"2026-01-01T00:00:00Z"}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            drop(socket);
            let endpoint = capture_one_response(&listener).await;
            assert!(endpoint.starts_with("GET "));
            capture(listener).await
        });
        let manager = SandboxManager::new(config).unwrap();
        match manager
            .create_unready(CreateSandboxOptions::image("ubuntu"))
            .await
        {
            Err(Error::SandboxSetup { sandbox_id, source }) => {
                assert_eq!(sandbox_id, "setup-box");
                assert!(matches!(*source, Error::Api { status: 503, .. }));
            }
            _ => panic!("expected retained setup ID"),
        }
        let request = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("\"sandboxId\":\"setup-box\""));
        assert!(request.contains("\"success\":false"));
    }

    async fn capture_one_response(listener: &TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 8192];
        let n = socket.read(&mut buffer).await.unwrap();
        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        String::from_utf8(buffer[..n].to_vec()).unwrap()
    }

    #[tokio::test]
    async fn success_and_cancellation_events_are_distinct() {
        for success in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let config = ConnectionConfig::builder()
                .domain(format!("http://{}", listener.local_addr().unwrap()))
                .build()
                .unwrap();
            let server = tokio::spawn(capture(listener));
            let mut metric = CreationMetric::new(&config, None);
            metric.created("box");
            if success {
                metric.succeeded();
            }
            drop(metric);
            let request = server.await.unwrap();
            let payload: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(payload["success"], success);
            assert!(payload.get("image").is_none());
        }
    }

    #[tokio::test]
    async fn aborted_creation_guard_reports_without_masking_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}", listener.local_addr().unwrap()))
            .build()
            .unwrap();
        let server = tokio::spawn(capture(listener));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut metric = CreationMetric::new(&config, None);
            metric.created("cancelled-box");
            tx.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(metric);
        });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let request = tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("\"sandboxId\":\"cancelled-box\""));
        assert!(request.contains("\"success\":false"));
    }

    #[tokio::test]
    async fn metrics_never_follow_redirects() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}", source.local_addr().unwrap()))
            .api_key("secret")
            .build()
            .unwrap();
        let location = format!("http://{}/stolen", target.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = source.accept().await.unwrap();
            let mut buffer = [0; 8192];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        drop(CreationMetric::new(&config, None));
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stalled_response_releases_task_slot_after_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = ConnectionConfig::builder()
            .domain(format!("http://{}", listener.local_addr().unwrap()))
            .build()
            .unwrap();
        config.request_timeout = Duration::from_millis(200);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\na")
                .await
                .unwrap();
            tx.send(()).unwrap();
            let mut byte = [0; 1];
            socket.read(&mut byte).await.unwrap()
        });
        drop(CreationMetric::new(&config, None));
        rx.await.unwrap();
        // Timeout drops the response and closes the stalled connection.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn disabled_and_unreachable_metrics_do_not_affect_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = ConnectionConfig::builder()
            .domain(format!("http://{}", listener.local_addr().unwrap()))
            .disable_metrics(true)
            .build()
            .unwrap();
        drop(CreationMetric::new(&config, None));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let config = ConnectionConfig::builder()
            .domain("http://127.0.0.1:1")
            .build()
            .unwrap();
        let manager = SandboxManager::new(config).unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            manager.create_unready(CreateSandboxOptions::image("")),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(Error::InvalidConfig(_))));
    }

    #[test]
    fn tracing_default_is_noop_and_scoped_config_preserves_original() {
        let config = ConnectionConfig::builder().build().unwrap();
        assert!(!config.enable_tracing);
        let mut root = WarmupTrace::new(&config);
        let mut phase = root.phase("create");
        assert_eq!(phase.scoped_config(&config).headers, config.headers);
        phase.finish(true);
        root.finish(true);
    }

    #[cfg(feature = "tracing")]
    #[test]
    fn opt_in_phase_spans_export_under_one_root_and_inject_w3c() {
        let _guard = TRACER_TEST_LOCK.lock().unwrap();
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());
        let config = ConnectionConfig::builder()
            .enable_tracing(true)
            .build()
            .unwrap();
        let mut root = WarmupTrace::new(&config);
        root.attribute("pool.name", "public-pool");
        root.attribute("user.code", "secret-code");
        root.counter("pool.warmup.attempt", 3);
        let mut create = root.phase("create");
        create.attribute("sandbox.id", "box");
        create.attribute("authorization", "secret-token");
        create.counter("pool.warmup.attempt", 3);
        let scoped = create.scoped_config(&config);
        let traceparent = scoped.headers["traceparent"].to_str().unwrap();
        assert_eq!(traceparent.len(), 55);
        assert!(traceparent.starts_with("00-"));
        assert!(!config.headers.contains_key("traceparent"));
        create.finish(true);
        let mut ready = root.phase("readiness");
        ready.finish(false);
        for name in ["prepare", "post_prepare_readiness", "renew", "commit"] {
            root.phase(name).finish(true);
        }
        root.finish(false);
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let root = spans
            .iter()
            .find(|span| span.name == "pool.warmup")
            .unwrap();
        let children: Vec<_> = spans
            .iter()
            .filter(|span| span.parent_span_id == root.span_context.span_id())
            .collect();
        assert_eq!(children.len(), 6);
        for child in children {
            assert_eq!(child.span_context.trace_id(), root.span_context.trace_id());
            assert!(child.attributes.iter().all(|attribute| matches!(
                attribute.key.as_str(),
                "outcome" | "sandbox.id" | "pool.warmup.attempt"
            )));
        }
    }
}
