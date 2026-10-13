// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared safe code execution service for the independent interpreter SDK.

pub use crate::api::execd::CodeContext;
use crate::{
    api::execd::{CodeContextRequest, RunCodeRequest},
    api::execd_client as api,
    commands::{consume, nonblank},
    stream::{self, ExecutionStream},
    transport::Transport,
    Error, Execution, Result,
};
use reqwest::Method;

#[derive(Clone, Debug, Default)]
pub struct RunCodeOptions {
    /// Persistent context. Mutually exclusive with language.
    pub context: Option<CodeContext>,
    /// Runtime for a fresh execution. Defaults to python.
    pub language: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct CreateContextOptions {
    /// Initial working directory inside the sandbox.
    pub cwd: Option<String>,
}

#[derive(Clone)]
pub struct Codes {
    transport: Transport,
}
impl Codes {
    pub(crate) fn new(transport: Transport) -> Self {
        Self { transport }
    }
    pub async fn run(&self, code: &str, options: RunCodeOptions) -> Result<Execution> {
        let mut execution = consume(self.run_stream(code, options).await?, false).await?;
        // Code execution errors are kernel errors, not process exit statuses.
        execution.exit_code = None;
        Ok(execution)
    }
    pub async fn run_stream(&self, code: &str, options: RunCodeOptions) -> Result<ExecutionStream> {
        let body = code_body(code, options)?;
        let response = self
            .transport
            .streaming_request(Method::POST, self.transport.url("/code")?)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::Transport(error.without_url()))?;
        Ok(stream::events(
            Transport::checked_sensitive(response).await?,
        ))
    }
    pub async fn create_context(&self, language: &str) -> Result<CodeContext> {
        self.create_context_with_options(language, CreateContextOptions::default())
            .await
    }
    pub async fn create_context_with_options(
        &self,
        language: &str,
        options: CreateContextOptions,
    ) -> Result<CodeContext> {
        nonblank(language, "language")?;
        let mut body = CodeContextRequest {
            language: Some(language.into()),
            ..Default::default()
        };
        if let Some(cwd) = options.cwd {
            nonblank(&cwd, "cwd")?;
            body.additional_properties.insert("cwd".into(), cwd.into());
        }
        let context = api::Client::new(&self.transport)
            .create_code_context(&body)?
            .sensitive(
                "Sensitive API request failed",
                "Invalid code context response",
            )
            .json()
            .await?;
        validate_context(context)
    }
    pub async fn get_context(&self, context_id: &str) -> Result<CodeContext> {
        nonblank(context_id, "context_id")?;
        let context = api::Client::new(&self.transport)
            .get_context(api::GetContextParams { context_id })?
            .json()
            .await?;
        validate_context(context)
    }
    pub async fn list_contexts(&self, language: Option<&str>) -> Result<Vec<CodeContext>> {
        if let Some(language) = language {
            nonblank(language, "language")?;
        }
        // The spec requires this query parameter; execd treats an empty filter as all runtimes.
        let contexts = api::Client::new(&self.transport)
            .list_contexts(api::ListContextsParams {
                language: language.unwrap_or(""),
            })?
            .json()
            .await?;
        contexts.into_iter().map(validate_context).collect()
    }
    pub async fn delete_context(&self, context_id: &str) -> Result<()> {
        nonblank(context_id, "context_id")?;
        api::Client::new(&self.transport)
            .delete_context(api::DeleteContextParams { context_id })?
            .empty()
            .await
    }
    /// Delete all contexts for one runtime. Requires an explicit language.
    pub async fn delete_contexts(&self, language: &str) -> Result<()> {
        nonblank(language, "language")?;
        api::Client::new(&self.transport)
            .delete_contexts_by_language(api::DeleteContextsByLanguageParams { language })?
            .empty()
            .await
    }
    pub async fn interrupt(&self, context_id: &str) -> Result<()> {
        nonblank(context_id, "context_id")?;
        api::Client::new(&self.transport)
            .interrupt_code(api::InterruptCodeParams { id: context_id })?
            .empty()
            .await
    }
}
fn validate_context(context: CodeContext) -> Result<CodeContext> {
    if context.language.trim().is_empty()
        || context.language.contains('\0')
        || context
            .id
            .as_ref()
            .is_some_and(|id| id.trim().is_empty() || id.contains('\0'))
    {
        return Err(Error::Protocol("Invalid code context response".into()));
    }
    Ok(context)
}
fn code_body(code: &str, options: RunCodeOptions) -> Result<RunCodeRequest> {
    nonblank(code, "code")?;
    if options.context.is_some() && options.language.is_some() {
        return Err(Error::InvalidConfig(
            "Provide either context or language, not both".into(),
        ));
    }
    let context = options.context.unwrap_or_else(|| CodeContext {
        language: options.language.unwrap_or_else(|| "python".into()),
        ..Default::default()
    });
    nonblank(&context.language, "language")?;
    if let Some(id) = &context.id {
        nonblank(id, "context_id")?;
    }
    Ok(RunCodeRequest {
        code: code.into(),
        context: Some(context),
        ..Default::default()
    })
}

#[cfg(test)]
mod keepalive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::serve_once;
    fn response(content_type: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }
    #[test]
    fn options_are_validated() {
        assert_eq!(
            code_body("1", Default::default())
                .unwrap()
                .context
                .unwrap()
                .language,
            "python"
        );
        assert!(code_body(" ", Default::default()).is_err());
        assert!(code_body(
            "1",
            RunCodeOptions {
                context: Some(CodeContext::default()),
                language: Some("python".into())
            }
        )
        .is_err());
    }
    #[tokio::test]
    async fn kernel_error_and_premature_eof() {
        let body = "data: {\"type\":\"error\",\"error\":{\"ename\":\"ValueError\",\"evalue\":\"bad\",\"traceback\":[\"trace\"]}}\n\n";
        let (transport, task) = serve_once(response("text/event-stream", body)).await;
        let execution = Codes::new(transport)
            .run("raise ValueError()", Default::default())
            .await
            .unwrap();
        assert_eq!(execution.error.unwrap().traceback, vec!["trace"]);
        assert_eq!(execution.exit_code, None);
        task.await.unwrap();
        let (transport, task) = serve_once(response(
            "text/event-stream",
            "data: {\"type\":\"stdout\",\"text\":\"hi\"}\n\n",
        ))
        .await;
        assert!(Codes::new(transport)
            .run("print('hi')", Default::default())
            .await
            .is_err());
        task.await.unwrap();
    }
    #[tokio::test]
    async fn chunked_unicode_error_stream_preserves_output_and_terminal() {
        let body = concat!(
            "{\"type\":\"stdout\",\"text\":\"你好\"}\r\n\r\n",
            "data: {\"type\":\"stderr\",\"text\":\"stderr\"}\r\n\r\n",
            "data: {\"type\":\"error\",\"error\":{\"ename\":\"ValueError\",\"evalue\":\"bad\",\"traceback\":[\"trace\"]}}\r\n\r\n",
        );
        for chunk_size in [1, 2, 7, 37, body.len()] {
            let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
            for chunk in body.as_bytes().chunks(chunk_size) {
                bytes.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
                bytes.extend_from_slice(chunk);
                bytes.extend_from_slice(b"\r\n");
            }
            bytes.extend_from_slice(b"0\r\n\r\n");
            let (transport, task) = serve_once(bytes).await;
            let execution = Codes::new(transport)
                .run("cell", Default::default())
                .await
                .unwrap();
            assert_eq!(execution.logs.stdout[0].text, "你好");
            assert_eq!(execution.logs.stderr[0].text, "stderr");
            assert_eq!(execution.error.unwrap().traceback, vec!["trace"]);
            assert!(!execution.complete);
            assert_eq!(execution.exit_code, None);
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rich_results_survive_aggregation() {
        let body = "data: {\"type\":\"result\",\"results\":{\"text/plain\":\"42\",\"image/png\":\"abc\",\"application/json\":{\"answer\":42}}}\n\ndata: {\"type\":\"execution_complete\"}\n\n";
        let (transport, task) = serve_once(response("text/event-stream", body)).await;
        let execution = Codes::new(transport)
            .run("42", Default::default())
            .await
            .unwrap();
        assert_eq!(execution.result[0].raw["image/png"], "abc");
        assert_eq!(execution.result[0].raw["application/json"]["answer"], 42);
        assert_eq!(execution.exit_code, None);
        let request = String::from_utf8(task.await.unwrap()).unwrap();
        assert!(request.starts_with("POST /prefix/v1/code HTTP/1.1"));
        assert!(request.contains("x-endpoint-token: test-token"));
    }
    #[tokio::test]
    async fn unfiltered_context_list_preserves_all_runtime_semantics() {
        let body = r#"[{"id":"ctx","language":"python"}]"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let (transport, request) = crate::commands::tests::serve_once(response.into_bytes()).await;
        let contexts = Codes::new(transport).list_contexts(None).await.unwrap();
        assert_eq!(contexts[0].id.as_deref(), Some("ctx"));
        let request = String::from_utf8(request.await.unwrap()).unwrap();
        assert!(request.starts_with("GET /prefix/v1/code/contexts?language= HTTP/1.1"));
    }

    #[tokio::test]
    async fn context_get_delete_filter_and_interrupt() {
        let (transport, task) = serve_once(response(
            "application/json",
            "{\"id\":\"ctx/x\",\"language\":\"python\"}",
        ))
        .await;
        Codes::new(transport).get_context("ctx/x").await.unwrap();
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .starts_with("GET /prefix/v1/code/contexts/ctx%2Fx "));
        let (transport, task) = serve_once(response("text/plain", "")).await;
        Codes::new(transport).delete_context("ctx/x").await.unwrap();
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .starts_with("DELETE /prefix/v1/code/contexts/ctx%2Fx "));
        let (transport, task) = serve_once(response("text/plain", "")).await;
        Codes::new(transport).delete_contexts("c++").await.unwrap();
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .starts_with("DELETE /prefix/v1/code/contexts?language=c%2B%2B "));
        let (transport, task) = serve_once(response("text/plain", "")).await;
        Codes::new(transport).interrupt("ctx &x").await.unwrap();
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .starts_with("DELETE /prefix/v1/code?id=ctx+%26x "));
    }
    #[tokio::test]
    async fn cwd_mapping_and_sensitive_errors() {
        let (transport, task) = serve_once(response(
            "application/json",
            "{\"id\":\"ctx\",\"language\":\"python\"}",
        ))
        .await;
        Codes::new(transport)
            .create_context_with_options(
                "python",
                CreateContextOptions {
                    cwd: Some("/work dir".into()),
                },
            )
            .await
            .unwrap();
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .contains("\"cwd\":\"/work dir\""));
        for create in [true, false] {
            let body = "{\"message\":\"secret-code-cwd\",\"code\":\"secret-code-cwd\",\"requestId\":\"secret-code-cwd\"}";
            let bytes = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
            let (transport, task) = serve_once(bytes).await;
            let codes = Codes::new(transport);
            let error = if create {
                codes
                    .create_context_with_options(
                        "python",
                        CreateContextOptions {
                            cwd: Some("secret-code-cwd".into()),
                        },
                    )
                    .await
                    .err()
                    .unwrap()
            } else {
                codes
                    .run("secret-code-cwd", Default::default())
                    .await
                    .err()
                    .unwrap()
            };
            assert!(!format!("{error:?} {error}").contains("secret-code-cwd"));
            assert!(matches!(error, Error::Api { status: 400, .. }));
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn context_http_mapping() {
        let (transport, task) = serve_once(response(
            "application/json",
            "{\"id\":\"ctx\",\"language\":\"python\"}",
        ))
        .await;
        assert_eq!(
            Codes::new(transport)
                .create_context("python")
                .await
                .unwrap()
                .id
                .as_deref(),
            Some("ctx")
        );
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .starts_with("POST /prefix/v1/code/context "));
        let (transport, task) = serve_once(response("application/json", "[]")).await;
        assert!(Codes::new(transport)
            .list_contexts(Some("c++"))
            .await
            .unwrap()
            .is_empty());
        assert!(String::from_utf8(task.await.unwrap())
            .unwrap()
            .contains("language=c%2B%2B"));
    }
}
