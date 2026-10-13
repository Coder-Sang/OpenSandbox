// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    api::{diagnostic as api, diagnostic_client as client},
    lifecycle::SandboxManager,
    Error, Result,
};
use chrono::{DateTime, Utc};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    Logs,
    Events,
}

/// Describes delivery only. The SDK never downloads external URLs or forwards credentials.
#[derive(Clone, PartialEq, Eq)]
pub enum DiagnosticPayload {
    Inline {
        content: String,
    },
    Url {
        content_url: Url,
        expires_at: DateTime<Utc>,
    },
}
impl std::fmt::Debug for DiagnosticPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inline { .. } => "Inline(<redacted>)",
            Self::Url { .. } => "Url(<redacted>)",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticContent {
    pub sandbox_id: String,
    pub kind: DiagnosticKind,
    pub scope: String,
    pub content_type: String,
    pub payload: DiagnosticPayload,
    pub content_length: Option<u64>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}
fn malformed(message: &str) -> Error {
    Error::Protocol(format!("invalid diagnostic descriptor: {message}"))
}
impl TryFrom<api::DiagnosticContentResponse> for DiagnosticContent {
    type Error = Error;
    fn try_from(wire: api::DiagnosticContentResponse) -> Result<Self> {
        let kind = match wire.kind {
            api::DiagnosticContentResponseKind::Logs => DiagnosticKind::Logs,
            api::DiagnosticContentResponseKind::Events => DiagnosticKind::Events,
            _ => return Err(malformed("unknown diagnostic kind")),
        };
        let payload = match wire.delivery {
            api::DiagnosticContentResponseDelivery::Inline => {
                if wire.content_url.is_some() || wire.expires_at.is_some() {
                    return Err(malformed("inline delivery must omit URL and expiry"));
                }
                DiagnosticPayload::Inline {
                    content: wire
                        .content
                        .ok_or_else(|| malformed("inline content is missing"))?,
                }
            }
            api::DiagnosticContentResponseDelivery::Url => {
                if wire.content.is_some() {
                    return Err(malformed("URL delivery must omit inline content"));
                }
                let url = Url::parse(
                    &wire
                        .content_url
                        .ok_or_else(|| malformed("download URL is missing"))?,
                )
                .map_err(|_| malformed("download URL is invalid"))?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(malformed(
                        "download URL must be HTTP(S) without credentials or fragment",
                    ));
                }
                let expires_at = DateTime::parse_from_rfc3339(
                    &wire
                        .expires_at
                        .ok_or_else(|| malformed("download expiry is missing"))?,
                )
                .map_err(|_| malformed("download expiry is not RFC3339"))?
                .with_timezone(&Utc);
                DiagnosticPayload::Url {
                    content_url: url,
                    expires_at,
                }
            }
            _ => return Err(malformed("unknown delivery mode")),
        };
        let content_length = wire
            .content_length
            .map(|length| {
                u64::try_from(length).map_err(|_| malformed("content length must be nonnegative"))
            })
            .transpose()?;
        Ok(Self {
            sandbox_id: wire.sandbox_id,
            kind,
            scope: wire.scope,
            content_type: wire.content_type,
            payload,
            content_length,
            truncated: wire.truncated,
            warnings: wire.warnings.unwrap_or_default(),
        })
    }
}
impl SandboxManager {
    pub async fn diagnostic_logs(&self, id: &str, scope: &str) -> Result<DiagnosticContent> {
        self.diagnostic(id, scope, "logs").await
    }
    pub async fn diagnostic_events(&self, id: &str, scope: &str) -> Result<DiagnosticContent> {
        self.diagnostic(id, scope, "events").await
    }
    async fn diagnostic(&self, id: &str, scope: &str, kind: &str) -> Result<DiagnosticContent> {
        if scope.trim().is_empty() {
            return Err(Error::InvalidConfig(
                "diagnostic scope must not be blank".into(),
            ));
        }
        let client = client::Client::new(&self.transport);
        let response = match kind {
            "logs" => {
                client
                    .get_sandboxes_sandbox_id_diagnostics_logs(
                        client::GetSandboxesSandboxIdDiagnosticsLogsParams {
                            sandbox_id: id,
                            scope,
                        },
                    )?
                    .json()
                    .await?
            }
            _ => {
                client
                    .get_sandboxes_sandbox_id_diagnostics_events(
                        client::GetSandboxesSandboxIdDiagnosticsEventsParams {
                            sandbox_id: id,
                            scope,
                        },
                    )?
                    .json()
                    .await?
            }
        };
        response.try_into()
    }
}
