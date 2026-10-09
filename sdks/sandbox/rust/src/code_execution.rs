// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared safe code execution service for the independent interpreter SDK.

pub use crate::api::execd::CodeContext;
use crate::{
    api::execd::{CodeContextRequest, RunCodeRequest},
    commands::{consume, id_url, nonblank},
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
        let response = self
            .transport
            .request(Method::POST, self.transport.url("/code/context")?)
            .json(&body)
            .send()
            .await
            .map_err(|error| Error::Transport(error.without_url()))?;
        let response = Transport::checked_sensitive(response).await?;
        let context = response
            .json()
            .await
            .map_err(|_| Error::Protocol("Invalid code context response".into()))?;
        validate_context(context)
    }
    pub async fn get_context(&self, context_id: &str) -> Result<CodeContext> {
        let context = self
            .transport
            .json(
                Method::GET,
                id_url(&self.transport, "/code/contexts", context_id, None)?,
                None::<&()>,
            )
            .await?;
        validate_context(context)
    }
    pub async fn list_contexts(&self, language: Option<&str>) -> Result<Vec<CodeContext>> {
        let contexts: Vec<CodeContext> = self
            .transport
            .json(Method::GET, self.contexts_url(language)?, None::<&()>)
            .await?;
        contexts.into_iter().map(validate_context).collect()
    }
    pub async fn delete_context(&self, context_id: &str) -> Result<()> {
        self.transport
            .empty(
                Method::DELETE,
                id_url(&self.transport, "/code/contexts", context_id, None)?,
                None::<&()>,
            )
            .await
    }
    /// Delete all contexts for one runtime. Requires an explicit language.
    pub async fn delete_contexts(&self, language: &str) -> Result<()> {
        self.transport
            .empty(
                Method::DELETE,
                self.contexts_url(Some(language))?,
                None::<&()>,
            )
            .await
    }
    pub async fn interrupt(&self, context_id: &str) -> Result<()> {
        nonblank(context_id, "context_id")?;
        let mut url = self.transport.url("/code")?;
        url.query_pairs_mut().append_pair("id", context_id);
        self.transport.empty(Method::DELETE, url, None::<&()>).await
    }
    fn contexts_url(&self, language: Option<&str>) -> Result<reqwest::Url> {
        let mut url = self.transport.url("/code/contexts")?;
        if let Some(language) = language {
            nonblank(language, "language")?;
            url.query_pairs_mut().append_pair("language", language);
        }
        Ok(url)
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
