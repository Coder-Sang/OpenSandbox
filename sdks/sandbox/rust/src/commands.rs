// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

pub use crate::stream::ExecutionStream;
use crate::{
    api::execd::{CreateSessionRequest, RunCommandRequest, RunInSessionRequest},
    api::execd_client as api,
    models::execd::{CommandLogs, CommandStatus, Execution, RunCommandOptions},
    stream::{self},
    transport::Transport,
    Error, Result,
};
use futures_util::StreamExt;
use reqwest::{Method, Url};

#[derive(Clone)]
pub struct Commands {
    transport: Transport,
}
impl Commands {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
    }
    pub async fn run(&self, command: &str, options: RunCommandOptions) -> Result<Execution> {
        let background = options.background;
        consume(self.run_stream(command, options).await?, background).await
    }
    pub async fn run_stream(
        &self,
        command: &str,
        options: RunCommandOptions,
    ) -> Result<ExecutionStream> {
        let body = command_body(command, options)?;
        self.start(self.transport.url("/command")?, &body).await
    }
    /// Execute native arguments literally and aggregate the execution stream.
    ///
    /// Requires a nonempty argv and executable name, with no NUL in any
    /// argument. Other arguments may be empty. Options and remote cleanup
    /// semantics match [`Self::run`]; no shell quoting or expansion is applied.
    pub async fn run_argv(
        &self,
        argv: &[impl AsRef<str>],
        options: RunCommandOptions,
    ) -> Result<Execution> {
        let background = options.background;
        consume(self.run_argv_stream(argv, options).await?, background).await
    }
    /// Execute native arguments literally and return their execution stream.
    ///
    /// Validation and options match [`Self::run_argv`]. Dropping the stream
    /// closes its response body; call [`Self::interrupt`] to stop the remote run.
    pub async fn run_argv_stream(
        &self,
        argv: &[impl AsRef<str>],
        options: RunCommandOptions,
    ) -> Result<ExecutionStream> {
        let body = argv_body(argv, options)?;
        self.start(self.transport.url("/command")?, &body).await
    }
    async fn start(&self, url: Url, body: &impl serde::Serialize) -> Result<ExecutionStream> {
        let response = self
            .transport
            .streaming_request(Method::POST, url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body)
            .send()
            .await?;
        Ok(stream::events(Transport::checked(response).await?))
    }
    pub async fn create_session(&self, cwd: Option<&str>) -> Result<String> {
        let body = CreateSessionRequest {
            cwd: cwd.map(str::to_owned),
            ..Default::default()
        };
        let response = api::Client::new(&self.transport)
            .create_session(Some(&body))?
            .json()
            .await?;
        if response.session_id.trim().is_empty() {
            return Err(Error::Protocol("Session response has an empty ID".into()));
        }
        Ok(response.session_id)
    }
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        nonblank(session_id, "session_id")?;
        api::Client::new(&self.transport)
            .delete_session(api::DeleteSessionParams { session_id })?
            .empty()
            .await
    }
    pub async fn run_in_session(
        &self,
        session_id: &str,
        command: &str,
        options: RunCommandOptions,
    ) -> Result<Execution> {
        consume(
            self.run_in_session_stream(session_id, command, options)
                .await?,
            false,
        )
        .await
    }
    pub async fn run_in_session_stream(
        &self,
        session_id: &str,
        command: &str,
        options: RunCommandOptions,
    ) -> Result<ExecutionStream> {
        nonblank(command, "command")?;
        if options.background
            || options.uid.is_some()
            || options.gid.is_some()
            || options.envs.is_some()
        {
            return Err(Error::InvalidConfig(
                "Session runs support only cwd and timeout options".into(),
            ));
        }
        let body = RunInSessionRequest {
            command: command.to_owned(),
            cwd: options.cwd,
            timeout: timeout_ms(options.timeout)?,
            ..Default::default()
        };
        self.start(
            id_url(&self.transport, "/session", session_id, Some("run"))?,
            &body,
        )
        .await
    }
    pub async fn status(&self, command_id: &str) -> Result<CommandStatus> {
        nonblank(command_id, "command_id")?;
        api::Client::new(&self.transport)
            .get_command_status(api::GetCommandStatusParams { id: command_id })?
            .json_as()
            .await
    }
    pub async fn logs(&self, command_id: &str, cursor: Option<u64>) -> Result<CommandLogs> {
        nonblank(command_id, "command_id")?;
        let response = api::Client::new(&self.transport)
            .get_background_command_logs(api::GetBackgroundCommandLogsParams {
                id: command_id,
                cursor,
            })?
            .raw()
            .await?;
        let cursor = response
            .headers()
            .get("EXECD-COMMANDS-TAIL-CURSOR")
            .map(|v| {
                v.to_str()
                    .map_err(|e| Error::Protocol(format!("Invalid log cursor: {e}")))?
                    .parse::<u64>()
                    .map_err(|e| Error::Protocol(format!("Invalid log cursor: {e}")))
            })
            .transpose()?;
        Ok(CommandLogs {
            content: response.text().await?,
            cursor,
        })
    }
    pub async fn interrupt(&self, command_id: &str) -> Result<()> {
        nonblank(command_id, "command_id")?;
        api::Client::new(&self.transport)
            .interrupt_command(api::InterruptCommandParams { id: command_id })?
            .empty()
            .await
    }
}
pub(crate) fn nonblank(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(Error::InvalidConfig(format!(
            "{field} must be nonempty and contain no NUL"
        )));
    }
    Ok(())
}
pub(crate) fn id_url(
    transport: &Transport,
    prefix: &str,
    id: &str,
    suffix: Option<&str>,
) -> Result<Url> {
    nonblank(id, "id")?;
    if id == "." || id == ".." {
        return Err(Error::InvalidConfig("Invalid path ID".into()));
    }
    let mut url = transport.url(prefix)?;
    {
        let mut segments = url.path_segments_mut().map_err(|_| {
            Error::InvalidConfig("Endpoint URL cannot contain path segments".into())
        })?;
        segments.pop_if_empty().push(id);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
    }
    Ok(url)
}
fn timeout_ms(timeout: Option<std::time::Duration>) -> Result<Option<i64>> {
    timeout
        .map(|duration| {
            if duration.subsec_nanos() % 1_000_000 != 0 {
                return Err(Error::InvalidConfig(
                    "Command timeout must be whole milliseconds".into(),
                ));
            }
            i64::try_from(duration.as_millis())
                .map_err(|_| Error::InvalidConfig("Command timeout exceeds protocol range".into()))
        })
        .transpose()
}
fn command_body(command: &str, options: RunCommandOptions) -> Result<RunCommandRequest> {
    nonblank(command, "command")?;
    Ok(RunCommandRequest {
        command: Some(command.to_owned()),
        ..command_options(options)?
    })
}
fn argv_body(argv: &[impl AsRef<str>], options: RunCommandOptions) -> Result<RunCommandRequest> {
    if argv
        .first()
        .is_none_or(|executable| executable.as_ref().is_empty())
    {
        return Err(Error::InvalidConfig(
            "argv must contain a nonempty executable name".into(),
        ));
    }
    if argv.iter().any(|argument| argument.as_ref().contains('\0')) {
        return Err(Error::InvalidConfig("argv must contain no NUL".into()));
    }
    Ok(RunCommandRequest {
        argv: Some(
            argv.iter()
                .map(|argument| argument.as_ref().to_owned())
                .collect(),
        ),
        ..command_options(options)?
    })
}
fn command_options(options: RunCommandOptions) -> Result<RunCommandRequest> {
    if options.gid.is_some() && options.uid.is_none() {
        return Err(Error::InvalidConfig(
            "uid is required when gid is provided".into(),
        ));
    }
    if options.uid.is_some_and(|id| id < 0) || options.gid.is_some_and(|id| id < 0) {
        return Err(Error::InvalidConfig(
            "uid and gid must be nonnegative".into(),
        ));
    }
    Ok(RunCommandRequest {
        cwd: options.cwd,
        background: Some(options.background),
        timeout: timeout_ms(options.timeout)?,
        uid: options.uid,
        gid: options.gid,
        envs: options.envs.map(|envs| envs.into_iter().collect()),
        ..Default::default()
    })
}
pub(crate) async fn consume(mut stream: ExecutionStream, background: bool) -> Result<Execution> {
    let mut execution = Execution::default();
    while let Some(event) = stream.next().await {
        execution.apply(event?);
        if execution.complete {
            break;
        }
    }
    if !execution.complete && execution.error.is_none() {
        return Err(Error::Protocol(
            "Execution stream ended before execution_complete or error".into(),
        ));
    }
    if !background {
        execution.exit_code = match &execution.error {
            Some(error) => error.evalue.trim().parse().ok(),
            None => Some(0),
        };
    }
    Ok(execution)
}
#[cfg(test)]
pub(crate) mod tests;
