// Copyright 2026 The OpenSandbox Authors
// Licensed under the Apache License, Version 2.0.

use crate::{
    api::{lifecycle as api, lifecycle_client as client},
    config::ConnectionConfig,
    error::{Error, Result},
    models::lifecycle::*,
    sandbox::Sandbox,
    transport::Transport,
};
use chrono::Utc;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

/// Endpoint routing options. Expiry is serialized as whole Unix epoch seconds.
#[derive(Clone, Debug, Default)]
pub struct EndpointOptions {
    /// Override the connection's proxy setting for this endpoint lookup.
    pub use_server_proxy: Option<bool>,
    /// Request a signed route. Must be at or after the epoch with no subsecond part.
    pub expires_at: Option<SystemTime>,
}

#[derive(Clone)]
pub struct SandboxManager {
    pub(crate) transport: Transport,
    pub(crate) config: ConnectionConfig,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidConfig(message.into())
}
/// Reject truncation and overflow rather than silently changing the server TTL.
pub(crate) fn seconds(duration: Duration, name: &str) -> Result<i64> {
    if duration.subsec_nanos() != 0 || duration.is_zero() {
        return Err(invalid(format!(
            "{name} must be a positive whole number of seconds"
        )));
    }
    nonnegative_seconds(duration, name)
}
fn nonnegative_seconds(duration: Duration, name: &str) -> Result<i64> {
    if duration.subsec_nanos() != 0 {
        return Err(invalid(format!("{name} must be a whole number of seconds")));
    }
    i64::try_from(duration.as_secs())
        .map_err(|_| invalid(format!("{name} exceeds wire integer range")))
}
fn convert<T: DeserializeOwned>(value: impl Serialize) -> Result<T> {
    let value = serde_json::to_value(value)
        .map_err(|e| Error::Protocol(format!("model serialization failed: {e}")))?;
    serde_json::from_value(value)
        .map_err(|e| Error::Protocol(format!("model conversion failed: {e}")))
}
fn hook(command: &[String], timeout: Option<Duration>, max: i64) -> Result<Value> {
    if command.is_empty() {
        return Err(invalid("lifecycle hook command cannot be empty"));
    }
    let mut body = json!({"command":command});
    if let Some(timeout) = timeout {
        let timeout = seconds(timeout, "hook timeout")?;
        if timeout > max {
            return Err(invalid(format!("hook timeout exceeds {max} seconds")));
        }
        body["timeoutSeconds"] = json!(timeout);
    }
    Ok(body)
}
fn create_body(options: &CreateSandboxOptions) -> Result<api::CreateSandboxRequest> {
    if options.extensions.contains_key("poolRef")
        && !matches!(options.source, SandboxSource::Pool(_))
    {
        return Err(invalid(
            "use SandboxSource::Pool instead of extensions.poolRef",
        ));
    }
    let mut body = json!({"metadata":options.metadata,"extensions":options.extensions});
    if let Some(timeout) = options.timeout {
        body["timeout"] = json!(seconds(timeout, "timeout")?);
    } else {
        body["timeout"] = Value::Null;
    }
    if let Some(policy) = &options.network_policy {
        body["networkPolicy"] = json!(policy);
        if policy.default_action.is_none() {
            body["networkPolicy"]["defaultAction"] = json!("deny");
        }
    }
    if let Some(proxy) = &options.credential_proxy {
        body["credentialProxy"] = json!(proxy);
    }
    if let SandboxSource::Template(id) = &options.source {
        if id.is_empty() || options.timeout.is_none() {
            return Err(invalid(
                "template creation requires a template id and timeout",
            ));
        }
        if options.entrypoint.is_some()
            || !options.env.is_empty()
            || options.platform.is_some()
            || options.resource_requests.is_some()
            || !options.resource_limits.is_empty()
            || options.lifecycle.is_some()
            || !options.volumes.is_empty()
            || options.secure_access
            || options.credential_proxy.is_some()
        {
            return Err(invalid(
                "template mode only accepts timeout, metadata, network policy and extensions",
            ));
        }
        body["templateId"] = json!(id);
        return convert(body);
    }
    if !options.resource_limits.is_empty() {
        body["resourceLimits"] = json!(options.resource_limits);
    }
    body["env"] = json!(options.env);
    body["secureAccess"] = json!(options.secure_access);
    if let Some(resources) = &options.resource_requests {
        body["resourceRequests"] = json!(resources);
    }
    if let Some(platform) = &options.platform {
        body["platform"] = json!(platform);
    }
    if !options.volumes.is_empty() {
        body["volumes"] = json!(options.volumes);
    }
    match &options.source {
        SandboxSource::Image(image) => {
            if image.uri.is_empty() {
                return Err(invalid("image URI cannot be empty"));
            }
            body["image"] = json!(image);
            body["entrypoint"] = json!(options.entrypoint.clone().unwrap_or_else(|| vec![
                "tail".into(),
                "-f".into(),
                "/dev/null".into()
            ]));
        }
        SandboxSource::Snapshot(id) => {
            if id.is_empty() {
                return Err(invalid("snapshot id cannot be empty"));
            }
            body["snapshotId"] = json!(id);
        }
        SandboxSource::Pool(id) => {
            if id.is_empty() {
                return Err(invalid("pool reference cannot be empty"));
            }
            if options.network_policy.is_some()
                || options.lifecycle.is_some()
                || options.platform.is_some()
                || !options.volumes.is_empty()
            {
                return Err(invalid(
                    "pool mode does not support network policy or lifecycle hooks",
                ));
            }
            body["extensions"]["poolRef"] = json!(id);
        }
        SandboxSource::Template(_) => unreachable!(),
    }
    if let Some(entrypoint) = &options.entrypoint {
        if entrypoint.is_empty() {
            return Err(invalid("entrypoint cannot be empty"));
        }
        body["entrypoint"] = json!(entrypoint);
    }
    if let Some(lifecycle) = &options.lifecycle {
        let mut value = json!({});
        if let Some(pre) = &lifecycle.pre_start {
            value["preStart"] = hook(&pre.command, pre.timeout, 10800)?;
        }
        let mut periodic = Vec::new();
        for item in &lifecycle.periodic {
            let mut value = hook(&item.command, item.timeout, 300)?;
            value["name"] = json!(item.name);
            value["schedule"] = json!(item.schedule);
            periodic.push(value);
        }
        if !periodic.is_empty() {
            value["periodic"] = json!(periodic);
        }
        body["lifecycle"] = value;
    }
    convert(body)
}

impl SandboxManager {
    pub fn new(config: ConnectionConfig) -> Result<Self> {
        Ok(Self {
            transport: Transport::new(&config)?,
            config,
        })
    }
    pub(crate) fn path(&self, parts: &[&str]) -> Result<Url> {
        let mut url = self.transport.lifecycle_url("")?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| invalid("lifecycle URL cannot be a base"))?;
            segments.pop_if_empty();
            for part in parts {
                if part.is_empty() || matches!(*part, "." | "..") {
                    return Err(invalid("resource id cannot be empty"));
                }
                segments.push(part);
            }
        }
        Ok(url)
    }
    pub async fn create(&self, options: CreateSandboxOptions) -> Result<Sandbox> {
        let image = match &options.source {
            SandboxSource::Image(image) => Some(image.uri.as_str()),
            SandboxSource::Snapshot(id) => Some(id.as_str()),
            _ => None,
        };
        let mut metric = crate::observability::CreationMetric::new(&self.config, image);
        let body = create_body(&options)?;
        let response = client::Client::new(&self.transport)
            .post_sandboxes(&body)?
            .json()
            .await?;
        let id = response.id.clone();
        metric.created(&id);
        let result = async {
            let info: SandboxInfo = convert(response)?;
            // Never delete a successfully created remote sandbox when attaching fails.
            self.attach(
                &info.id,
                matches!(options.source, SandboxSource::Template(_)),
            )
            .await
        }
        .await;
        if result.is_ok() {
            metric.succeeded();
        }
        result.map_err(|error| match error {
            Error::ReadyTimeout { .. } => error,
            source => Error::SandboxSetup {
                sandbox_id: id,
                source: Box::new(source),
            },
        })
    }
    /// Create and resolve endpoints without waiting for readiness.
    /// The caller must wait explicitly before use and kill the sandbox when finished.
    /// Setup errors retain the created ID and never delete the remote resource.
    pub async fn create_unready(&self, options: CreateSandboxOptions) -> Result<Sandbox> {
        let image = match &options.source {
            SandboxSource::Image(image) => Some(image.uri.as_str()),
            SandboxSource::Snapshot(id) => Some(id.as_str()),
            _ => None,
        };
        let mut metric = crate::observability::CreationMetric::new(&self.config, image);
        let body = create_body(&options)?;
        let response = client::Client::new(&self.transport)
            .post_sandboxes(&body)?
            .json()
            .await?;
        let id = response.id;
        metric.created(&id);
        let result = self
            .attach_unready(&id, matches!(options.source, SandboxSource::Template(_)))
            .await
            .map_err(|source| Error::SandboxSetup {
                sandbox_id: id,
                source: Box::new(source),
            });
        if result.is_ok() {
            metric.succeeded();
        }
        result
    }
    /// Resolve endpoints without waiting for readiness. Does not create a sandbox.
    pub async fn connect_unready(&self, id: &str) -> Result<Sandbox> {
        self.attach_unready(id, false).await
    }
    pub async fn connect(&self, id: &str) -> Result<Sandbox> {
        self.attach(id, false).await
    }
    pub async fn get(&self, id: &str) -> Result<SandboxInfo> {
        convert(
            client::Client::new(&self.transport)
                .get_sandboxes_sandbox_id(client::GetSandboxesSandboxIdParams { sandbox_id: id })?
                .json()
                .await?,
        )
    }
    pub async fn list(&self, filter: SandboxFilter) -> Result<ListSandboxesResponse> {
        let metadata = encode_metadata(&filter.metadata);
        convert(
            client::Client::new(&self.transport)
                .get_sandboxes(client::GetSandboxesParams {
                    state: Some(&filter.states),
                    metadata: metadata.as_deref(),
                    page: filter.page.map(u64::from),
                    page_size: filter.page_size.map(u64::from),
                })?
                .json()
                .await?,
        )
    }
    pub async fn kill(&self, id: &str) -> Result<()> {
        client::Client::new(&self.transport)
            .delete_sandboxes_sandbox_id(client::DeleteSandboxesSandboxIdParams { sandbox_id: id })?
            .empty()
            .await
    }
    pub async fn pause(&self, id: &str) -> Result<()> {
        client::Client::new(&self.transport)
            .post_sandboxes_sandbox_id_pause(client::PostSandboxesSandboxIdPauseParams {
                sandbox_id: id,
            })?
            .empty()
            .await
    }
    /// Accepts the resume request. Use connect to wait for Running and resolve fresh endpoints.
    pub async fn resume(&self, id: &str) -> Result<()> {
        client::Client::new(&self.transport)
            .post_sandboxes_sandbox_id_resume(client::PostSandboxesSandboxIdResumeParams {
                sandbox_id: id,
            })?
            .empty()
            .await
    }
    pub async fn renew(&self, id: &str, timeout: Duration) -> Result<()> {
        let seconds = seconds(timeout, "renew timeout")?;
        let delta = chrono::TimeDelta::try_seconds(seconds)
            .ok_or_else(|| invalid("renew timeout overflows timestamp"))?;
        let expires = Utc::now()
            .checked_add_signed(delta)
            .ok_or_else(|| invalid("renew expiration overflows timestamp"))?;
        let body: api::RenewSandboxExpirationRequest =
            convert(json!({"expiresAt":expires.to_rfc3339()}))?;
        client::Client::new(&self.transport)
            .post_sandboxes_sandbox_id_renew_expiration(
                client::PostSandboxesSandboxIdRenewExpirationParams { sandbox_id: id },
                &body,
            )?
            .empty()
            .await
    }
    pub async fn endpoint(&self, id: &str, port: u16) -> Result<Endpoint> {
        self.endpoint_with_options(id, port, EndpointOptions::default())
            .await
    }
    pub async fn endpoint_with_options(
        &self,
        id: &str,
        port: u16,
        options: EndpointOptions,
    ) -> Result<Endpoint> {
        let proxy = options
            .use_server_proxy
            .unwrap_or(self.config.use_server_proxy);
        let expiry = options
            .expires_at
            .map(|expiry| {
                if proxy {
                    return Err(invalid(
                        "signed endpoint expiry cannot be combined with server proxy",
                    ));
                }
                let duration = expiry
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| invalid("endpoint expiry must not precede the Unix epoch"))?;
                if duration.subsec_nanos() != 0 {
                    return Err(invalid("endpoint expiry must be whole Unix epoch seconds"));
                }
                Ok(duration.as_secs())
            })
            .transpose()?;
        if port == 0 {
            return Err(invalid("endpoint port must be nonzero"));
        }
        let expiry = expiry.map(|value| value.to_string());
        let response = client::Client::new(&self.transport)
            .get_sandboxes_sandbox_id_endpoints_port(
                client::GetSandboxesSandboxIdEndpointsPortParams {
                    sandbox_id: id,
                    port: u64::from(port),
                    use_server_proxy: Some(proxy),
                    expires: expiry.as_deref(),
                },
            )?
            .raw()
            .await?;
        let origin = response
            .headers()
            .get("open-sandbox-origin")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut endpoint: Endpoint = response.json().await?;
        endpoint.origin = origin;
        Ok(endpoint)
    }
    pub async fn patch_metadata(
        &self,
        id: &str,
        patch: SandboxMetadataPatch,
    ) -> Result<SandboxInfo> {
        let body: api::PatchSandboxMetadataRequest = convert(patch)?;
        convert(
            client::Client::new(&self.transport)
                .patch_sandboxes_sandbox_id_metadata(
                    client::PatchSandboxesSandboxIdMetadataParams { sandbox_id: id },
                    &body,
                )?
                .json()
                .await?,
        )
    }
    pub async fn create_snapshot(
        &self,
        id: &str,
        options: CreateSnapshotOptions,
    ) -> Result<SnapshotInfo> {
        let body: api::CreateSnapshotRequest = convert(options)?;
        convert(
            client::Client::new(&self.transport)
                .post_sandboxes_sandbox_id_snapshots(
                    client::PostSandboxesSandboxIdSnapshotsParams { sandbox_id: id },
                    Some(&body),
                )?
                .json()
                .await?,
        )
    }
    pub async fn get_snapshot(&self, id: &str) -> Result<SnapshotInfo> {
        convert(
            client::Client::new(&self.transport)
                .get_snapshots_snapshot_id(client::GetSnapshotsSnapshotIdParams {
                    snapshot_id: id,
                })?
                .json()
                .await?,
        )
    }
    pub async fn list_snapshots(&self, filter: SnapshotFilter) -> Result<ListSnapshotsResponse> {
        convert(
            client::Client::new(&self.transport)
                .get_snapshots(client::GetSnapshotsParams {
                    sandbox_id: filter.sandbox_id.as_deref(),
                    name: filter.name.as_deref(),
                    state: Some(&filter.states),
                    page: filter.page.map(u64::from),
                    page_size: filter.page_size.map(u64::from),
                })?
                .json()
                .await?,
        )
    }
    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        client::Client::new(&self.transport)
            .delete_snapshots_snapshot_id(client::DeleteSnapshotsSnapshotIdParams {
                snapshot_id: id,
            })?
            .empty()
            .await
    }
    fn template_body(options: CreateTemplateOptions) -> Result<Value> {
        let mut value = json!({"image":options.image,"publish":options.publish,"entrypoint":options.entrypoint,"metadata":options.metadata});
        if let Some(resources) = options.resource_limits {
            value["resourceLimits"] = json!(resources);
        }
        if let Some(format) = options.format {
            value["format"] = json!(format);
        }
        if let Some(readiness) = options.readiness {
            let mut wire = json!({});
            if let Some(probe) = readiness.probe {
                wire["probe"] = json!(probe);
            }
            if let Some(warmup) = readiness.warmup {
                wire["warmupSeconds"] = json!(nonnegative_seconds(warmup, "template warmup")?);
            }
            value["readiness"] = wire;
        }
        Ok(value)
    }
    pub async fn create_template(&self, options: CreateTemplateOptions) -> Result<TemplateInfo> {
        let body: api::CreateFsbTemplateRequest = convert(Self::template_body(options)?)?;
        convert(
            client::Client::new(&self.transport)
                .create_template(&body)?
                .json()
                .await?,
        )
    }
    /// Create a template with literal environment values, including an explicit empty map.
    /// Names must match ASCII `[A-Za-z_][A-Za-z0-9_]*`. Values are not shell-expanded.
    /// Environment defaults may be baked into the template image. Do not use secrets.
    pub async fn create_template_with_env(
        &self,
        options: CreateTemplateOptions,
        env: BTreeMap<String, String>,
    ) -> Result<TemplateInfoWithEnv> {
        for name in env.keys() {
            let mut bytes = name.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(invalid(
                    "template env names must match [A-Za-z_][A-Za-z0-9_]*",
                ));
            }
        }
        let mut body: api::CreateFsbTemplateRequest = convert(Self::template_body(options)?)?;
        body.env = Some(env);
        client::Client::new(&self.transport)
            .create_template(&body)?
            .sensitive(
                "Sensitive API request failed",
                "Invalid sensitive API response",
            )
            .json_as()
            .await
    }
    /// Fetch a template while retaining its environment defaults.
    pub async fn get_template_with_env(&self, id: &str) -> Result<TemplateInfoWithEnv> {
        client::Client::new(&self.transport)
            .get_template(client::GetTemplateParams { template_id: id })?
            .sensitive(
                "Sensitive API request failed",
                "Invalid sensitive API response",
            )
            .json_as()
            .await
    }
    /// List templates while retaining environment defaults and pagination.
    pub async fn list_templates_with_env(
        &self,
        filter: TemplateFilter,
    ) -> Result<ListTemplatesWithEnvResponse> {
        let metadata = encode_metadata(&filter.metadata);
        client::Client::new(&self.transport)
            .list_templates(client::ListTemplatesParams {
                metadata: metadata.as_deref(),
                page: filter.page.map(u64::from),
                page_size: filter.page_size.map(u64::from),
            })?
            .sensitive(
                "Sensitive API request failed",
                "Invalid sensitive API response",
            )
            .json_as()
            .await
    }
    pub async fn get_template(&self, id: &str) -> Result<TemplateInfo> {
        convert(
            client::Client::new(&self.transport)
                .get_template(client::GetTemplateParams { template_id: id })?
                .json()
                .await?,
        )
    }
    pub async fn list_templates(&self, filter: TemplateFilter) -> Result<ListTemplatesResponse> {
        let metadata = encode_metadata(&filter.metadata);
        convert(
            client::Client::new(&self.transport)
                .list_templates(client::ListTemplatesParams {
                    metadata: metadata.as_deref(),
                    page: filter.page.map(u64::from),
                    page_size: filter.page_size.map(u64::from),
                })?
                .json()
                .await?,
        )
    }
    pub async fn delete_template(&self, id: &str) -> Result<()> {
        client::Client::new(&self.transport)
            .delete_template(client::DeleteTemplateParams { template_id: id })?
            .empty()
            .await
    }
}
fn encode_metadata(metadata: &BTreeMap<String, String>) -> Option<String> {
    if metadata.is_empty() {
        return None;
    }
    let mut encoded = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in metadata {
        encoded.append_pair(key, value);
    }
    Some(encoded.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_proxy_mapping_and_template_rejection() {
        let mut options = CreateSandboxOptions::image("ubuntu:24.04");
        options.credential_proxy = Some(serde_json::from_value(json!({"enabled":true})).unwrap());
        let value = serde_json::to_value(create_body(&options).unwrap()).unwrap();
        assert_eq!(value["credentialProxy"]["enabled"], true);
        options.source = SandboxSource::Template("template-test".into());
        options.resource_limits.clear();
        assert!(create_body(&options).is_err());
    }
    #[test]
    fn create_sources_null_ttl_and_hook_units() {
        let mut options = CreateSandboxOptions::image("ubuntu:24.04");
        options.timeout = None;
        let value = serde_json::to_value(create_body(&options).unwrap()).unwrap();
        assert!(value["timeout"].is_null());
        assert!(value.get("timeout").is_some());
        assert_eq!(value["entrypoint"], json!(["tail", "-f", "/dev/null"]));
        options.lifecycle = Some(SandboxLifecycle {
            pre_start: Some(LifecycleHook {
                command: vec!["true".into()],
                timeout: Some(Duration::from_millis(1001)),
            }),
            periodic: Vec::new(),
        });
        assert!(create_body(&options).is_err());
        let pool = CreateSandboxOptions::new(SandboxSource::Pool("pool-a".into()));
        let value = serde_json::to_value(create_body(&pool).unwrap()).unwrap();
        assert_eq!(value["extensions"]["poolRef"], "pool-a");
        assert!(value.get("image").is_none());
        let mut options = CreateSandboxOptions::image("ubuntu");
        options.extensions.insert("poolRef".into(), "pool-a".into());
        assert!(create_body(&options).is_err());
        options.extensions.clear();
        options.network_policy = Some(NetworkPolicy::default());
        let value = serde_json::to_value(create_body(&options).unwrap()).unwrap();
        assert_eq!(value["networkPolicy"]["defaultAction"], "deny");
        let mut options = CreateSandboxOptions::new(SandboxSource::Template("tpl_a".into()));
        options.resource_limits.insert("cpu".into(), "1".into());
        assert!(create_body(&options).is_err());
        assert!(hook(&["true".into()], Some(Duration::from_secs(301)), 300).is_err());
        let auth = ImageAuth {
            username: Some("user-secret".into()),
            password: Some("password-secret".into()),
        };
        assert!(!format!("{auth:?}").contains("secret"));
    }
    #[test]
    fn resource_ids_cannot_escape_path_or_prefix() {
        let config = ConnectionConfig::builder()
            .domain("http://localhost/deploy")
            .build()
            .unwrap();
        let manager = SandboxManager::new(config).unwrap();
        let url = manager.path(&["sandboxes", "a/b?#%"]).unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost/deploy/v1/sandboxes/a%2Fb%3F%23%25"
        );
        assert!(manager.path(&["sandboxes", ".."]).is_err());
    }
    #[tokio::test]
    async fn renew_rejects_invalid_durations_before_network() {
        let manager = SandboxManager::new(
            ConnectionConfig::builder()
                .domain("http://127.0.0.1:1")
                .build()
                .unwrap(),
        )
        .unwrap();
        for duration in [
            Duration::ZERO,
            Duration::from_millis(1),
            Duration::from_secs(i64::MAX as u64),
            Duration::from_secs(u64::MAX),
        ] {
            assert!(matches!(
                manager.renew("sandbox-a", duration).await,
                Err(Error::InvalidConfig(_))
            ));
        }
    }
    #[test]
    fn seconds_are_checked() {
        assert!(seconds(Duration::ZERO, "ttl").is_err());
        assert!(seconds(Duration::from_millis(1500), "ttl").is_err());
        assert!(seconds(Duration::from_secs(u64::MAX), "ttl").is_err());
        assert_eq!(seconds(Duration::from_secs(10), "ttl").unwrap(), 10);
        assert_eq!(nonnegative_seconds(Duration::ZERO, "warmup").unwrap(), 0);
        assert!(nonnegative_seconds(Duration::from_millis(1), "warmup").is_err());
    }
    #[test]
    fn metadata_filter_roundtrips_delimiters() {
        let values = [("a&=%".into(), "hello world&=%".into())].into();
        let encoded = encode_metadata(&values).unwrap();
        let decoded: BTreeMap<_, _> = url::form_urlencoded::parse(encoded.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(decoded, values);
    }
    #[test]
    fn template_payload_has_no_workload_shape() {
        let options = CreateSandboxOptions::new(SandboxSource::Template("tpl_123".into()));
        let value = serde_json::to_value(create_body(&options).unwrap()).unwrap();
        assert_eq!(value["templateId"], "tpl_123");
        assert!(value.get("resourceLimits").is_none_or(Value::is_null));
        assert!(value.get("image").is_none_or(Value::is_null));
    }
}
