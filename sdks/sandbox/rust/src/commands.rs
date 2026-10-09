// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

pub use crate::stream::ExecutionStream;
use crate::{
    api::execd::{
        CreateSessionRequest, CreateSessionResponse, RunCommandRequest, RunInSessionRequest,
    },
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
        let response: CreateSessionResponse = self
            .transport
            .json(Method::POST, self.transport.url("/session")?, Some(&body))
            .await?;
        if response.session_id.trim().is_empty() {
            return Err(Error::Protocol("Session response has an empty ID".into()));
        }
        Ok(response.session_id)
    }
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let url = id_url(&self.transport, "/session", session_id, None)?;
        self.transport.empty(Method::DELETE, url, None::<&()>).await
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
        self.transport
            .json(
                Method::GET,
                id_url(&self.transport, "/command/status", command_id, None)?,
                None::<&()>,
            )
            .await
    }
    pub async fn logs(&self, command_id: &str, cursor: Option<u64>) -> Result<CommandLogs> {
        let mut url = id_url(&self.transport, "/command", command_id, Some("logs"))?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("cursor", &cursor.to_string());
        }
        let response =
            Transport::checked(self.transport.request(Method::GET, url).send().await?).await?;
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
        let mut url = self.transport.url("/command")?;
        url.query_pairs_mut().append_pair("id", command_id);
        self.transport.empty(Method::DELETE, url, None::<&()>).await
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
        command: Some(command.to_owned()),
        argv: None,
        cwd: options.cwd,
        background: Some(options.background),
        timeout: timeout_ms(options.timeout)?,
        uid: options.uid,
        gid: options.gid,
        envs: options.envs.map(|envs| envs.into_iter().collect()),
        ..Default::default()
    })
}
async fn consume(mut stream: ExecutionStream, background: bool) -> Result<Execution> {
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
pub(crate) mod tests {
    pub(crate) async fn serve_once(
        response: Vec<u8>,
    ) -> (
        crate::transport::Transport,
        tokio::task::JoinHandle<Vec<u8>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket.write_all(&response).await.unwrap();
            request
        });
        let config = crate::ConnectionConfig::builder()
            .domain(format!("http://{address}/prefix"))
            .header("x-endpoint-token", "test-token")
            .build()
            .unwrap();
        (crate::transport::Transport::new(&config).unwrap(), task)
    }

    #[tokio::test]
    async fn stream_http_preserves_path_headers_and_timeout_units() {
        let body = "data: {\"type\":\"init\",\"text\":\"cmd\"}\n\ndata: {\"type\":\"execution_complete\"}\n\n";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (transport, task) = serve_once(response.into_bytes()).await;
        let execution = Commands::new(transport)
            .run(
                "echo hi",
                RunCommandOptions {
                    timeout: Some(std::time::Duration::from_millis(250)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(execution.id.as_deref(), Some("cmd"));
        let request = String::from_utf8(task.await.unwrap()).unwrap();
        assert!(request.starts_with("POST /prefix/v1/command HTTP/1.1"));
        assert!(request.contains("x-endpoint-token: test-token"));
        assert!(request.contains("\"timeout\":250"));
    }

    use super::*;
    #[tokio::test]
    async fn error_only_http_preserves_execution_failure() {
        for value in ["7", "execution timed out", "executable not found"] {
            let body = format!("data: {{\"type\":\"init\",\"text\":\"cmd\"}}\n\ndata: {{\"type\":\"stdout\",\"text\":\"before failure\"}}\n\ndata: {}\n\n", serde_json::json!({"type":"error","error":{"ename":"CommandError","evalue":value}}));
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let (transport, task) = serve_once(response.into_bytes()).await;
            let execution = Commands::new(transport)
                .run("false", Default::default())
                .await
                .unwrap();
            assert_eq!(execution.id.as_deref(), Some("cmd"));
            assert_eq!(execution.logs.stdout[0].text, "before failure");
            assert_eq!(execution.error.unwrap().evalue, value);
            assert_eq!(execution.exit_code, value.parse::<i32>().ok());
            assert!(!execution.complete);
            task.await.unwrap();
        }
    }

    #[test]
    fn command_mapping_and_validation() {
        let options = RunCommandOptions {
            timeout: Some(std::time::Duration::from_millis(1500)),
            ..Default::default()
        };
        assert_eq!(
            command_body("echo hi", options).unwrap().timeout,
            Some(1500)
        );
        assert!(command_body("", Default::default()).is_err());
        assert!(command_body(
            "true",
            RunCommandOptions {
                gid: Some(1),
                ..Default::default()
            }
        )
        .is_err());
        assert!(timeout_ms(Some(std::time::Duration::from_nanos(1))).is_err());
        assert_eq!(
            timeout_ms(Some(std::time::Duration::ZERO)).unwrap(),
            Some(0)
        );
        assert!(timeout_ms(Some(std::time::Duration::MAX)).is_err());
    }
    #[tokio::test]
    async fn aggregation_and_premature_eof() {
        let source = [
            r#"{"type":"init","text":"cmd"}"#,
            r#"{"type":"stdout","text":"hello"}"#,
            r#"{"type":"error","error":{"ename":"exit","evalue":"7"}}"#,
            r#"{"type":"execution_complete"}"#,
        ];
        let events = source
            .into_iter()
            .map(|s| Ok(serde_json::from_str(s).unwrap()));
        let result = consume(Box::pin(futures_util::stream::iter(events)), false)
            .await
            .unwrap();
        assert_eq!(result.id.as_deref(), Some("cmd"));
        assert_eq!(result.logs.stdout[0].text, "hello");
        assert_eq!(result.exit_code, Some(7));
        assert!(consume(Box::pin(futures_util::stream::empty()), false)
            .await
            .is_err());
    }
}
