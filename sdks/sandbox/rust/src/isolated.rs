// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Isolated execd sessions. Remote sessions are deleted only by explicit calls,
//! never on Drop. `run_once` awaits cleanup and reports it separately.
use crate::{
    api::execd::{self, CreateIsolatedSessionRequest, IsolatedRunRequest},
    api::execd_client as api,
    commands::{self, id_url, nonblank},
    files::Files,
    models::execd::Execution,
    stream::{self, ExecutionStream},
    transport::Transport,
    Error, Result,
};
use reqwest::{Method, Url};
use std::{collections::BTreeMap, time::Duration};

pub use execd::{
    BindMount, CapabilitiesResponse as IsolatedCapabilities,
    CreateIsolatedSessionRequestProfile as IsolationProfile,
    CreateIsolatedSessionRequestUidMode as IsolationUidMode, EnvPassthroughSpec,
    IsolatedBackgroundRunResponse as IsolatedBackgroundRun, IsolatedRunStatus,
    IsolatedSessionSummary, IsolatedWorkspaceSpec, IsolatedWorkspaceSpecMode as OverlayMode,
    IsolatedWorkspaceSpecMode as WorkspaceMode, SessionState as IsolatedSessionState,
};

/// Compatibility options retained for callers of the 1.1.1 SDK.
/// This branch does not accept multiple-overlay requests.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IsolatedOverlaySpec {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OverlayMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
    #[serde(flatten)]
    pub additional_properties: BTreeMap<String, serde_json::Value>,
}
impl std::fmt::Debug for IsolatedOverlaySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IsolatedOverlaySpec(<redacted>)")
    }
}

/// Creation options. Wire idle timeouts are whole seconds, represented here as Duration.
#[derive(Clone, Debug, Default)]
pub struct CreateIsolatedSessionOptions {
    pub workspace: IsolatedWorkspaceSpec,
    pub profile: Option<IsolationProfile>,
    pub extra_writable: Option<Vec<String>>,
    pub binds: Option<Vec<BindMount>>,
    pub share_net: Option<bool>,
    pub env_passthrough: Option<EnvPassthroughSpec>,
    pub uid: Option<i64>,
    pub gid: Option<i64>,
    pub uid_mode: Option<IsolationUidMode>,
    pub idle_timeout: Option<Duration>,
}
/// Compatibility options; this branch accepts a workspace and no overlays.
/// Mount paths use clean absolute Linux syntax, regardless of the client platform.
#[derive(Clone, Debug, Default)]
pub struct CreateIsolatedSessionAdvancedOptions {
    pub workspace: Option<IsolatedWorkspaceSpec>,
    pub overlays: Vec<IsolatedOverlaySpec>,
    pub profile: Option<IsolationProfile>,
    pub extra_writable: Option<Vec<String>>,
    pub binds: Option<Vec<BindMount>>,
    pub share_net: Option<bool>,
    pub env_passthrough: Option<EnvPassthroughSpec>,
    pub uid: Option<i64>,
    pub gid: Option<i64>,
    pub uid_mode: Option<IsolationUidMode>,
    pub idle_timeout: Option<Duration>,
}
impl From<CreateIsolatedSessionOptions> for CreateIsolatedSessionAdvancedOptions {
    fn from(options: CreateIsolatedSessionOptions) -> Self {
        Self {
            workspace: Some(options.workspace),
            overlays: vec![],
            profile: options.profile,
            extra_writable: options.extra_writable,
            binds: options.binds,
            share_net: options.share_net,
            env_passthrough: options.env_passthrough,
            uid: options.uid,
            gid: options.gid,
            uid_mode: options.uid_mode,
            idle_timeout: options.idle_timeout,
        }
    }
}
#[derive(Clone, Default)]
pub struct IsolatedRunOptions {
    pub envs: Option<BTreeMap<String, String>>,
    pub timeout: Option<Duration>,
}
impl std::fmt::Debug for IsolatedRunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IsolatedRunOptions(<redacted>)")
    }
}
#[derive(Debug)]
pub struct IsolatedRunLogs {
    pub text: String,
    pub cursor: u64,
}
/// A run's result and the separately observable deletion result. A cleanup failure
/// cannot mask the original execution failure. `run_once` attempts deletion on
/// cancellation while its Tokio runtime remains alive. Process/runtime shutdown
/// can prevent cleanup, so server idle GC remains the final safety net.
#[derive(Debug)]
pub struct RunOnceResult {
    /// Retained so callers can retry deletion if cleanup fails.
    pub session_id: String,
    pub execution: Result<Execution>,
    pub cleanup: Result<()>,
}
#[derive(Clone)]
pub struct IsolatedSessions {
    transport: Transport,
}
#[derive(Clone)]
pub struct IsolatedSession {
    transport: Transport,
    session_id: String,
    info: execd::IsolatedCreateSessionResponse,
    files: Files,
}
impl IsolatedSessions {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
    }
    pub async fn capabilities(&self) -> Result<IsolatedCapabilities> {
        let mut value = api::Client::new(&self.transport)
            .isolated_capabilities()?
            .json()
            .await?;
        value.setpriv_available.get_or_insert(false);
        value.userns_available.get_or_insert(false);
        Ok(value)
    }
    pub async fn list(&self) -> Result<Vec<IsolatedSessionSummary>> {
        let response = api::Client::new(&self.transport)
            .list_isolated_sessions()?
            .json()
            .await?;
        Ok(response.sessions)
    }
    pub async fn create(&self, options: CreateIsolatedSessionOptions) -> Result<IsolatedSession> {
        nonblank(&options.workspace.path, "workspace.path")?;
        self.create_shared(options.into()).await
    }
    pub async fn create_advanced(
        &self,
        options: CreateIsolatedSessionAdvancedOptions,
    ) -> Result<IsolatedSession> {
        validate_mounts(&options)?;
        self.create_shared(options).await
    }
    async fn create_shared(
        &self,
        options: CreateIsolatedSessionAdvancedOptions,
    ) -> Result<IsolatedSession> {
        let body = create_body(options)?;
        let info = api::Client::new(&self.transport)
            .create_isolated_session(&body)?
            .sensitive(
                "Sensitive API request failed",
                "Invalid sensitive API response",
            )
            .json()
            .await?;
        let id = info
            .session_id
            .as_deref()
            .ok_or_else(|| Error::Protocol("Isolated session response has no ID".into()))?
            .to_owned();
        self.handle(id, info)
    }
    fn handle(
        &self,
        id: String,
        info: execd::IsolatedCreateSessionResponse,
    ) -> Result<IsolatedSession> {
        let files = Files::scoped(self.transport.clone(), &id)?;
        Ok(IsolatedSession {
            transport: self.transport.clone(),
            session_id: id,
            info,
            files,
        })
    }
    pub async fn attach(&self, session_id: &str) -> Result<IsolatedSession> {
        let state = self.get(session_id).await?;
        // Preserve optional creation echoes without inventing defaults on older execd builds.
        let mut fields = serde_json::to_value(&state)?;
        let fields = fields
            .as_object_mut()
            .ok_or_else(|| Error::Protocol("Invalid session state".into()))?;
        fields.insert(
            "session_id".into(),
            serde_json::Value::String(session_id.into()),
        );
        let info = serde_json::from_value(serde_json::Value::Object(fields.clone()))?;
        self.handle(session_id.to_owned(), info)
    }
    pub async fn get(&self, session_id: &str) -> Result<IsolatedSessionState> {
        nonblank(session_id, "session_id")?;
        api::Client::new(&self.transport)
            .get_isolated_session(api::GetIsolatedSessionParams { session_id })?
            .json()
            .await
    }
    pub async fn delete(&self, session_id: &str) -> Result<()> {
        nonblank(session_id, "session_id")?;
        api::Client::new(&self.transport)
            .delete_isolated_session(api::DeleteIsolatedSessionParams { session_id })?
            .empty()
            .await
    }
    pub async fn run_once(
        &self,
        code: &str,
        create: CreateIsolatedSessionOptions,
        run: IsolatedRunOptions,
    ) -> Result<RunOnceResult> {
        nonblank(&create.workspace.path, "workspace.path")?;
        self.run_once_shared(code, create.into(), run).await
    }
    pub async fn run_once_advanced(
        &self,
        code: &str,
        create: CreateIsolatedSessionAdvancedOptions,
        run: IsolatedRunOptions,
    ) -> Result<RunOnceResult> {
        validate_mounts(&create)?;
        self.run_once_shared(code, create, run).await
    }
    async fn run_once_shared(
        &self,
        code: &str,
        create: CreateIsolatedSessionAdvancedOptions,
        run: IsolatedRunOptions,
    ) -> Result<RunOnceResult> {
        let service = self.clone();
        let code = code.to_owned();
        let (mut sender, receiver) = tokio::sync::oneshot::channel();
        // The owned task survives cancellation of the caller. Once creation
        // succeeds, receiver loss interrupts streaming and triggers deletion.
        tokio::spawn(async move {
            let session = match service.create_shared(create).await {
                Ok(session) => session,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            let execution = tokio::select! {
                result = session.run(&code, run) => result,
                _ = sender.closed() => {
                    let _ = session.delete().await;
                    return;
                }
            };
            let cleanup = session.delete().await;
            let _ = sender.send(Ok(RunOnceResult {
                session_id: session.session_id.clone(),
                execution,
                cleanup,
            }));
        });
        receiver
            .await
            .map_err(|_| Error::Protocol("Isolated run_once task terminated".into()))?
    }
}
impl IsolatedSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// Creation response, with creation echoes retained in additional_properties.
    /// Missing echoes on older servers remain absent.
    pub fn info(&self) -> &execd::IsolatedCreateSessionResponse {
        &self.info
    }
    pub fn files(&self) -> &Files {
        &self.files
    }
    fn url(&self, suffix: Option<&str>) -> Result<Url> {
        id_url(
            &self.transport,
            "/v1/isolated/session",
            &self.session_id,
            suffix,
        )
    }
    pub async fn get(&self) -> Result<IsolatedSessionState> {
        api::Client::new(&self.transport)
            .get_isolated_session(api::GetIsolatedSessionParams {
                session_id: &self.session_id,
            })?
            .json()
            .await
    }
    pub async fn delete(&self) -> Result<()> {
        api::Client::new(&self.transport)
            .delete_isolated_session(api::DeleteIsolatedSessionParams {
                session_id: &self.session_id,
            })?
            .empty()
            .await
    }
    pub async fn run(&self, code: &str, options: IsolatedRunOptions) -> Result<Execution> {
        commands::consume(self.run_stream(code, options).await?, false).await
    }
    pub async fn run_stream(
        &self,
        code: &str,
        options: IsolatedRunOptions,
    ) -> Result<ExecutionStream> {
        let body = run_body(code, options, false)?;
        let response = self
            .transport
            .streaming_request(Method::POST, self.url(Some("run"))?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::Transport(error.without_url()))?;
        Ok(stream::events(
            Transport::checked_sensitive(response).await?,
        ))
    }
    /// Background runs have no execution timeout. Any supplied timeout is ignored,
    /// matching Python and JavaScript. Read-only workspaces are rejected by execd.
    pub async fn run_background(
        &self,
        code: &str,
        options: IsolatedRunOptions,
    ) -> Result<IsolatedBackgroundRun> {
        let body = run_body(code, options, true)?;
        let response = api::Client::new(&self.transport)
            .run_in_isolated_session(
                api::RunInIsolatedSessionParams {
                    session_id: &self.session_id,
                },
                &body,
            )?
            .header("accept", "application/json")
            .sensitive(
                "Sensitive API request failed",
                "Invalid isolated background response",
            )
            .raw()
            .await?;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            return Err(Error::Protocol(
                "Background run must return HTTP 202".into(),
            ));
        }
        response
            .json()
            .await
            .map_err(|_| Error::Protocol("Invalid isolated background response".into()))
    }
    pub async fn status(&self, run_id: &str) -> Result<IsolatedRunStatus> {
        nonblank(run_id, "run_id")?;
        api::Client::new(&self.transport)
            .get_isolated_run_status(api::GetIsolatedRunStatusParams {
                session_id: &self.session_id,
                run_id,
            })?
            .json()
            .await
    }
    pub async fn logs(&self, run_id: &str, cursor: Option<u64>) -> Result<IsolatedRunLogs> {
        let cursor = cursor.unwrap_or(0);
        if cursor > i64::MAX as u64 {
            return Err(Error::InvalidConfig(
                "Log cursor exceeds signed protocol range".into(),
            ));
        }
        nonblank(run_id, "run_id")?;
        let response = api::Client::new(&self.transport)
            .get_isolated_run_logs(api::GetIsolatedRunLogsParams {
                session_id: &self.session_id,
                run_id,
                cursor: (cursor > 0).then_some(cursor),
            })?
            .header("accept", "text/plain")
            .raw()
            .await?;
        let next = response
            .headers()
            .get("EXECD-ISOLATED-TAIL-CURSOR")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|value| *value <= i64::MAX as u64);
        let bytes = response.bytes().await?;
        let byte_length = bytes.len() as u64;
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::Protocol("Isolated log is not UTF-8".into()))?;
        let cursor = match next {
            Some(value) => value,
            None => cursor
                .checked_add(byte_length)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or_else(|| Error::Protocol("Log cursor overflow".into()))?,
        };
        Ok(IsolatedRunLogs { text, cursor })
    }
}
fn seconds(value: Option<Duration>, field: &str) -> Result<Option<i64>> {
    value
        .map(|v| {
            if v.subsec_nanos() != 0 {
                return Err(Error::InvalidConfig(format!(
                    "{field} must be whole seconds"
                )));
            }
            i64::try_from(v.as_secs())
                .map_err(|_| Error::InvalidConfig(format!("{field} exceeds protocol range")))
        })
        .transpose()
}
fn validate_mounts(options: &CreateIsolatedSessionAdvancedOptions) -> Result<()> {
    let overlays = &options.overlays;
    let count = usize::from(options.workspace.is_some()) + overlays.len();
    if !(1..=16).contains(&count) {
        return Err(Error::InvalidConfig(
            "Expected 1 to 16 isolated mounts".into(),
        ));
    }
    let mut paths = std::collections::BTreeSet::new();
    for path in options
        .workspace
        .iter()
        .map(|w| w.path.as_str())
        .chain(overlays.iter().map(|o| o.path.as_str()))
    {
        if !path.starts_with('/')
            || path.contains('\0')
            || (path != "/" && path[1..].split('/').any(|s| matches!(s, "" | "." | "..")))
            || !paths.insert(path)
        {
            return Err(Error::InvalidConfig(
                "Mount paths must be absolute, clean and unique".into(),
            ));
        }
    }
    if overlays
        .iter()
        .any(|o| o.persist.is_some() && matches!(o.mode, Some(OverlayMode::Rw | OverlayMode::Ro)))
    {
        return Err(Error::InvalidConfig(
            "persist must be omitted for rw/ro overlays".into(),
        ));
    }
    Ok(())
}
fn create_body(
    options: CreateIsolatedSessionAdvancedOptions,
) -> Result<CreateIsolatedSessionRequest> {
    if options.uid.is_some_and(|v| u32::try_from(v).is_err())
        || options.gid.is_some_and(|v| u32::try_from(v).is_err())
        || (options.gid.is_some() && options.uid.is_none())
    {
        return Err(Error::InvalidConfig(
            "uid/gid must fit uint32 and gid requires uid".into(),
        ));
    }
    if !options.overlays.is_empty() {
        return Err(Error::InvalidConfig(
            "Multiple overlays are not supported by this branch".into(),
        ));
    }
    Ok(CreateIsolatedSessionRequest {
        workspace: options
            .workspace
            .ok_or_else(|| Error::InvalidConfig("workspace is required by this branch".into()))?,
        profile: options.profile,
        extra_writable: options.extra_writable,
        binds: options.binds,
        share_net: options.share_net,
        env_passthrough: options.env_passthrough,
        uid: options.uid,
        gid: options.gid,
        uid_mode: options.uid_mode,
        idle_timeout_seconds: seconds(options.idle_timeout, "idle_timeout")?,
        ..Default::default()
    })
}
fn run_body(
    code: &str,
    options: IsolatedRunOptions,
    background: bool,
) -> Result<IsolatedRunRequest> {
    nonblank(code, "code")?;
    Ok(IsolatedRunRequest {
        code: code.into(),
        envs: options.envs,
        timeout_seconds: if background {
            None
        } else {
            seconds(options.timeout, "timeout")?
        },
        background: background.then_some(true),
        ..Default::default()
    })
}
