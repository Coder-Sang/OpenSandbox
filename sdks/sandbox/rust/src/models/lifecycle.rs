// Copyright 2026 The OpenSandbox Authors
// Licensed under the Apache License, Version 2.0.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

pub type ResourceLimits = BTreeMap<String, String>;
pub type SandboxMetadataPatch = BTreeMap<String, Option<String>>;

#[derive(Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(skip)]
    pub origin: Option<String>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("endpoint", &"<redacted>")
            .field("headers", &"<redacted>")
            .field("origin", &self.origin)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageSpec {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<ImageAuth>,
}
impl ImageSpec {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            auth: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ImageAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}
impl std::fmt::Debug for ImageAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageAuth").finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlatformSpec {
    pub os: String,
    pub arch: String,
}

#[derive(Clone, Debug)]
pub enum SandboxSource {
    Image(ImageSpec),
    Snapshot(String),
    Template(String),
    Pool(String),
}
#[derive(Clone, Debug)]
pub struct CreateSandboxOptions {
    pub source: SandboxSource,
    /// None opts into server-supported manual cleanup. Durations must be whole seconds.
    pub timeout: Option<Duration>,
    pub entrypoint: Option<Vec<String>>,
    pub resource_limits: ResourceLimits,
    pub resource_requests: Option<ResourceLimits>,
    pub env: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    pub extensions: BTreeMap<String, String>,
    pub platform: Option<PlatformSpec>,
    pub secure_access: bool,
    pub network_policy: Option<NetworkPolicy>,
    pub credential_proxy: Option<crate::api::lifecycle::CredentialProxyConfig>,
    pub lifecycle: Option<SandboxLifecycle>,
    pub volumes: Vec<Volume>,
}
impl CreateSandboxOptions {
    pub fn new(source: SandboxSource) -> Self {
        let resource_limits =
            if matches!(&source, SandboxSource::Template(_) | SandboxSource::Pool(_)) {
                BTreeMap::new()
            } else {
                [("cpu".into(), "1".into()), ("memory".into(), "2Gi".into())].into()
            };
        Self {
            source,
            timeout: Some(Duration::from_secs(600)),
            entrypoint: None,
            resource_limits,
            resource_requests: None,
            env: BTreeMap::new(),
            metadata: BTreeMap::new(),
            extensions: BTreeMap::new(),
            platform: None,
            secure_access: false,
            network_policy: None,
            credential_proxy: None,
            lifecycle: None,
            volumes: Vec::new(),
        }
    }
    pub fn image(uri: impl Into<String>) -> Self {
        Self::new(SandboxSource::Image(ImageSpec::new(uri)))
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_action: Option<NetworkRuleAction>,
    #[serde(default)]
    pub egress: Vec<NetworkRule>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkRuleAction {
    Allow,
    Deny,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkRule {
    pub action: NetworkRuleAction,
    pub target: String,
}
#[derive(Clone, Debug)]
pub struct LifecycleHook {
    pub command: Vec<String>,
    pub timeout: Option<Duration>,
}
#[derive(Clone, Debug)]
pub struct PeriodicLifecycleHook {
    pub name: String,
    pub schedule: String,
    pub command: Vec<String>,
    pub timeout: Option<Duration>,
}
#[derive(Clone, Debug, Default)]
pub struct SandboxLifecycle {
    pub pre_start: Option<LifecycleHook>,
    pub periodic: Vec<PeriodicLifecycleHook>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub name: String,
    pub mount_path: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<String>,
    #[serde(flatten)]
    pub backend: VolumeBackend,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeBackend {
    Host(HostVolume),
    Pvc(PvcVolume),
    Ossfs(OssfsVolume),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostVolume {
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PvcVolume {
    pub claim_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_if_not_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_on_sandbox_termination: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_modes: Option<Vec<String>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OssfsVolume {
    pub bucket: String,
    pub endpoint: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
}
impl std::fmt::Debug for OssfsVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OssfsVolume")
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    /// Kept as a string to accept future server states.
    pub state: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub last_transition_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationSummary {
    pub mode: String,
    pub pool_ref: String,
    pub state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInfo {
    pub id: String,
    pub image: Option<ImageSpec>,
    pub snapshot_id: Option<String>,
    pub platform: Option<PlatformSpec>,
    pub status: SandboxStatus,
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub extensions: BTreeMap<String, String>,
    pub allocation: Option<AllocationSummary>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}
#[derive(Clone, Debug, Default)]
pub struct SandboxFilter {
    pub states: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}
#[derive(Clone, Debug, Default)]
pub struct SnapshotFilter {
    pub sandbox_id: Option<String>,
    pub name: Option<String>,
    pub states: Vec<String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}
#[derive(Clone, Debug, Default)]
pub struct TemplateFilter {
    pub metadata: BTreeMap<String, String>,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaginationInfo {
    pub page: u64,
    pub page_size: u64,
    pub total_items: u64,
    pub total_pages: u64,
    pub has_next_page: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListSandboxesResponse {
    pub items: Vec<SandboxInfo>,
    pub pagination: Option<PaginationInfo>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateSnapshotOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub id: String,
    pub sandbox_id: String,
    pub name: Option<String>,
    pub status: SandboxStatus,
    pub created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListSnapshotsResponse {
    pub items: Vec<SnapshotInfo>,
    pub pagination: Option<PaginationInfo>,
}
#[derive(Clone, Debug)]
pub struct TemplateReadiness {
    pub probe: Option<String>,
    pub warmup: Option<Duration>,
}
#[derive(Clone, Debug)]
pub struct CreateTemplateOptions {
    pub image: String,
    pub publish: String,
    pub resource_limits: Option<ResourceLimits>,
    pub entrypoint: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub readiness: Option<TemplateReadiness>,
    pub format: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateStatus {
    pub phase: String,
    pub manifest_ref: Option<String>,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub template_id: String,
    pub image: String,
    pub publish: String,
    pub format: String,
    pub status: TemplateStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resource_limits: Option<ResourceLimits>,
    pub entrypoint: Option<Vec<String>>,
    pub metadata: Option<BTreeMap<String, String>>,
    pub readiness: Option<TemplateReadiness>,
}
impl<'de> Deserialize<'de> for TemplateReadiness {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            probe: Option<String>,
            warmup_seconds: Option<u64>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            probe: wire.probe,
            warmup: wire.warmup_seconds.map(Duration::from_secs),
        })
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct ListTemplatesResponse {
    pub items: Vec<TemplateInfo>,
    pub pagination: Option<PaginationInfo>,
}
