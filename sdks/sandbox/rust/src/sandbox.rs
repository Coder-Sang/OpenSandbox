// Copyright 2026 The OpenSandbox Authors
// Licensed under the Apache License, Version 2.0.

use crate::{
    commands::Commands,
    egress::Egress,
    error::{Error, Result},
    files::Files,
    lifecycle::SandboxManager,
    metrics::Metrics,
    models::lifecycle::*,
    transport::Transport,
};
use reqwest::Method;
use tokio::time::{sleep, timeout};

/// A cloneable handle. Dropping it never deletes the remote sandbox.
#[derive(Clone)]
pub struct Sandbox {
    pub(crate) manager: SandboxManager,
    pub(crate) transport: Transport,
    pub(crate) id: String,
    pub(crate) template_origin: bool,
}
impl Sandbox {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn commands(&self) -> Commands {
        Commands::new(self.transport.clone())
    }
    pub fn files(&self) -> Files {
        Files::new(self.transport.clone())
    }
    pub fn metrics(&self) -> Metrics {
        Metrics::new(self.transport.clone())
    }
    /// Resolve egress lazily. Template-backed sandboxes route policy to the control plane.
    pub async fn egress(&self) -> Result<Egress> {
        if self.template_origin {
            return Ok(Egress::lifecycle(
                self.manager.transport.clone(),
                self.id.clone(),
            ));
        }
        let endpoint = self.manager.endpoint(&self.id, 18080).await?;
        Ok(Egress::new(self.manager.transport.data_plane(&endpoint)?))
    }
    pub async fn info(&self) -> Result<SandboxInfo> {
        self.manager.get(&self.id).await
    }
    pub async fn kill(&self) -> Result<()> {
        self.manager.kill(&self.id).await
    }
    pub async fn pause(&self) -> Result<()> {
        self.manager.pause(&self.id).await
    }
    /// Resume and return a fresh handle, since published endpoints may change.
    pub async fn resume(&self) -> Result<Self> {
        self.manager.resume(&self.id).await?;
        self.manager.attach(&self.id, self.template_origin).await
    }
    pub async fn renew(&self, duration: std::time::Duration) -> Result<()> {
        self.manager.renew(&self.id, duration).await
    }
    pub async fn endpoint(&self, port: u16) -> Result<Endpoint> {
        self.manager.endpoint(&self.id, port).await
    }
    pub async fn patch_metadata(&self, patch: SandboxMetadataPatch) -> Result<SandboxInfo> {
        self.manager.patch_metadata(&self.id, patch).await
    }
    pub async fn create_snapshot(&self, options: CreateSnapshotOptions) -> Result<SnapshotInfo> {
        self.manager.create_snapshot(&self.id, options).await
    }
    pub async fn ping(&self) -> Result<()> {
        self.transport
            .empty(Method::GET, self.transport.url("/ping")?, None::<&()>)
            .await
    }
}
fn retryable(error: &Error) -> bool {
    match error {
        Error::Transport(error) => error.is_timeout() || error.is_connect(),
        Error::Api { status, code, .. } => {
            *status >= 500
                || *status == 408
                || *status == 429
                || (*status == 404 && code.as_deref() == Some("KUBERNETES::POD_IP_NOT_AVAILABLE"))
        }
        _ => false,
    }
}
impl SandboxManager {
    pub(crate) async fn attach(&self, id: &str, template_origin: bool) -> Result<Sandbox> {
        // Validate before starting the budget. timeout encloses state polling, endpoint
        // publication and ping, so no individual request can overrun the total budget.
        self.path(&["sandboxes", id])?;
        let operation = async {
            loop {
                match self.get(id).await {
                    Ok(info) if info.status.state == "Running" => break,
                    Ok(info)
                        if matches!(
                            info.status.state.as_str(),
                            "Failed" | "Error" | "Terminated" | "Deleted" | "Stopping" | "Deleting"
                        ) =>
                    {
                        return Err(Error::Api {
                            status: 409,
                            code: Some("SANDBOX_NOT_RUNNING".into()),
                            message: format!(
                                "sandbox {id} entered {}: {}",
                                info.status.state,
                                info.status.message.unwrap_or_default()
                            ),
                            request_id: None,
                        });
                    }
                    Ok(_) => {}
                    Err(error) if retryable(&error) => {}
                    Err(error) => return Err(error),
                }
                sleep(self.config.polling_interval).await;
            }
            let endpoint = loop {
                match self.endpoint(id, 44772).await {
                    Ok(endpoint) => break endpoint,
                    Err(error) if retryable(&error) => sleep(self.config.polling_interval).await,
                    Err(error) => return Err(error),
                }
            };
            let sandbox = Sandbox {
                manager: self.clone(),
                transport: self.transport.data_plane(&endpoint)?,
                id: id.into(),
                template_origin: template_origin || endpoint.origin.as_deref() == Some("template"),
            };
            loop {
                match sandbox.ping().await {
                    Ok(()) => return Ok(sandbox),
                    Err(
                        error @ Error::Api {
                            status: 401 | 403, ..
                        },
                    ) => return Err(error),
                    Err(Error::Api { .. } | Error::Transport(_)) => {
                        sleep(self.config.polling_interval).await
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        timeout(self.config.ready_timeout, operation)
            .await
            .map_err(|_| Error::ReadyTimeout {
                sandbox_id: id.into(),
                timeout: self.config.ready_timeout,
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn server(
        pending: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let domain = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = domain.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes);
                let line = request.lines().next().unwrap().to_owned();
                captured.lock().unwrap().push(line.clone());
                let state = if pending { "Pending" } else { "Running" };
                let body = if line.contains("/endpoints/") {
                    serde_json::json!({"endpoint":format!("{endpoint}/execd"),"headers":{"x-endpoint":"secure"}}).to_string()
                } else if line.contains("/ping") {
                    assert!(request.to_ascii_lowercase().contains("x-endpoint: secure"));
                    "{}".into()
                } else {
                    serde_json::json!({"id":"sandbox-1","status":{"state":state},"entrypoint":["tail"],"createdAt":"2026-01-01T00:00:00Z"}).to_string()
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nOPEN-SANDBOX-ORIGIN: template\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (domain, requests, task)
    }

    #[tokio::test]
    async fn connect_checks_running_endpoint_headers_ping_and_template_origin() {
        let (domain, requests, server) = server(false).await;
        let manager = SandboxManager::new(
            crate::ConnectionConfig::builder()
                .domain(domain)
                .build()
                .unwrap(),
        )
        .unwrap();
        let sandbox = manager.connect("sandbox-1").await.unwrap();
        assert!(sandbox.template_origin);
        assert_eq!(sandbox.id(), "sandbox-1");
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 3);
        assert!(captured[0].starts_with("GET /v1/sandboxes/sandbox-1 "));
        assert!(captured[1].contains("/endpoints/44772?use_server_proxy=false"));
        assert!(captured[2].starts_with("GET /execd/ping "));
        drop(sandbox);
        assert_eq!(requests.lock().unwrap().len(), 3);
        server.abort();
    }

    #[tokio::test]
    async fn create_timeout_retains_remote_id_and_never_deletes() {
        let (domain, requests, server) = server(true).await;
        let config = crate::ConnectionConfig::builder()
            .domain(domain)
            .ready_timeout(Duration::from_millis(40))
            .polling_interval(Duration::from_millis(5))
            .build()
            .unwrap();
        let manager = SandboxManager::new(config).unwrap();
        let result = manager.create(CreateSandboxOptions::image("ubuntu")).await;
        assert!(
            matches!(result, Err(Error::ReadyTimeout { sandbox_id, .. }) if sandbox_id == "sandbox-1")
        );
        let captured = requests.lock().unwrap();
        assert!(captured[0].starts_with("POST /v1/sandboxes "));
        assert!(captured.iter().all(|line| !line.starts_with("DELETE ")));
        assert!(captured.iter().all(|line| !line.contains("/ping")));
        server.abort();
    }
    #[test]
    fn readiness_does_not_retry_auth_or_missing_sandbox() {
        for status in [401, 403, 404] {
            assert!(!retryable(&Error::Api {
                status,
                code: None,
                message: String::new(),
                request_id: None
            }));
        }
        assert!(retryable(&Error::Api {
            status: 404,
            code: Some("KUBERNETES::POD_IP_NOT_AVAILABLE".into()),
            message: String::new(),
            request_id: None
        }));
        assert!(retryable(&Error::Api {
            status: 503,
            code: None,
            message: String::new(),
            request_id: None
        }));
    }
}
