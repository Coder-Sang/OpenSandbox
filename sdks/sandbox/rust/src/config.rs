// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{Error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::{fmt, time::Duration};
use url::Url;

/// Connection settings. Build once and clone to share the HTTP connection pool.
#[derive(Clone)]
pub struct ConnectionConfig {
    /// Lifecycle API base URL, including `/v1/` and any deployment prefix.
    pub base_url: Url,
    pub request_timeout: Duration,
    pub ready_timeout: Duration,
    pub polling_interval: Duration,
    pub use_server_proxy: bool,
    /// Disable best-effort creation metrics. The environment opt-out also takes precedence.
    pub disable_metrics: bool,
    /// Enable optional warmup tracing through the application's global provider.
    pub enable_tracing: bool,
    pub(crate) headers: HeaderMap,
}

impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("base_url", &self.base_url)
            .field("request_timeout", &self.request_timeout)
            .field("ready_timeout", &self.ready_timeout)
            .field("polling_interval", &self.polling_interval)
            .field("use_server_proxy", &self.use_server_proxy)
            .field("disable_metrics", &self.disable_metrics)
            .field("enable_tracing", &self.enable_tracing)
            .finish_non_exhaustive()
    }
}

impl ConnectionConfig {
    /// Explicit builder values override `OPEN_SANDBOX_DOMAIN` and
    /// `OPEN_SANDBOX_API_KEY`. Defaults to `http://localhost:8080/v1/`.
    pub fn builder() -> ConnectionConfigBuilder {
        ConnectionConfigBuilder::default()
    }
}

#[derive(Default)]
pub struct ConnectionConfigBuilder {
    domain: Option<String>,
    protocol: Option<String>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    request_timeout: Option<Duration>,
    ready_timeout: Option<Duration>,
    polling_interval: Option<Duration>,
    use_server_proxy: bool,
    disable_metrics: bool,
    enable_tracing: bool,
}

impl ConnectionConfigBuilder {
    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn protocol(mut self, protocol: impl Into<String>) -> Self {
        self.protocol = Some(protocol.into());
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Header names are case insensitive. A custom API-key header takes
    /// precedence over the `api_key` option and environment variable.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = Some(timeout);
        self
    }

    pub fn polling_interval(mut self, interval: Duration) -> Self {
        self.polling_interval = Some(interval);
        self
    }

    pub fn use_server_proxy(mut self, enabled: bool) -> Self {
        self.use_server_proxy = enabled;
        self
    }

    pub fn disable_metrics(mut self, disabled: bool) -> Self {
        self.disable_metrics = disabled;
        self
    }

    pub fn enable_tracing(mut self, enabled: bool) -> Self {
        self.enable_tracing = enabled;
        self
    }

    pub fn build(self) -> Result<ConnectionConfig> {
        let domain = self
            .domain
            .or_else(|| {
                std::env::var("OPEN_SANDBOX_DOMAIN")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "localhost:8080".into());
        let protocol = self.protocol.as_deref().unwrap_or("http");
        if protocol != "http" && protocol != "https" {
            return Err(Error::InvalidConfig(
                "protocol must be http or https".into(),
            ));
        }
        let raw = if domain.contains("://") {
            domain
        } else {
            format!("{protocol}://{domain}")
        };
        let mut base_url = Url::parse(&raw).map_err(|_| {
            Error::InvalidConfig("domain must be a valid HTTP(S) URL or host[:port]".into())
        })?;
        validate_base_url(&base_url)?;
        let prefix = base_url.path().trim_end_matches('/');
        let prefix = prefix.strip_suffix("/v1").unwrap_or(prefix);
        let path = format!("{prefix}/v1/");
        base_url.set_path(&path);
        let mut headers = HeaderMap::new();
        for (name, value) in self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::InvalidConfig("invalid HTTP header name".into()))?;
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| Error::InvalidConfig("invalid HTTP header value".into()))?;
            // Avoid exposing any custom header values through Debug output.
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let api_key = self.api_key.or_else(|| {
            std::env::var("OPEN_SANDBOX_API_KEY")
                .ok()
                .filter(|v| !v.is_empty())
        });
        if !headers.contains_key("open-sandbox-api-key") {
            if let Some(api_key) = api_key {
                let mut value = HeaderValue::from_str(&api_key)
                    .map_err(|_| Error::InvalidConfig("invalid API key header value".into()))?;
                value.set_sensitive(true);
                headers.insert("open-sandbox-api-key", value);
            }
        }
        if !headers.contains_key("user-agent") {
            headers.insert(
                "user-agent",
                HeaderValue::from_str(&format!("OpenSandbox-Rust-SDK/{}", crate::VERSION))
                    .expect("valid SDK version"),
            );
        }
        let config = ConnectionConfig {
            base_url,
            request_timeout: self.request_timeout.unwrap_or(Duration::from_secs(30)),
            ready_timeout: self.ready_timeout.unwrap_or(Duration::from_secs(30)),
            polling_interval: self.polling_interval.unwrap_or(Duration::from_millis(200)),
            use_server_proxy: self.use_server_proxy,
            disable_metrics: self.disable_metrics || metrics_disabled_by_env(),
            enable_tracing: self.enable_tracing,
            headers,
        };
        config.validate()?;
        Ok(config)
    }
}

impl ConnectionConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_base_url(&self.base_url)?;
        for (name, value) in [
            ("request_timeout", self.request_timeout),
            ("ready_timeout", self.ready_timeout),
            ("polling_interval", self.polling_interval),
        ] {
            if value.is_zero() || std::time::Instant::now().checked_add(value).is_none() {
                return Err(Error::InvalidConfig(format!(
                    "{name} must be a positive finite duration"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn metrics_disabled_by_env() -> bool {
    std::env::var("OPENSANDBOX_DISABLE_METRICS").is_ok_and(|value| value.trim() == "1")
}

fn validate_base_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::InvalidConfig("base URL must use HTTP(S), have a host, and contain no credentials, query, or fragment".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_defaults_and_environment_override() {
        if std::env::var("OPENSANDBOX_METRICS_ENV_TEST").is_ok() {
            let config = ConnectionConfig::builder()
                .disable_metrics(false)
                .build()
                .unwrap();
            assert!(config.disable_metrics);
            assert!(!config.enable_tracing);
            return;
        }
        assert!(!ConnectionConfig::builder().build().unwrap().enable_tracing);
        assert!(
            ConnectionConfig::builder()
                .disable_metrics(true)
                .build()
                .unwrap()
                .disable_metrics
        );
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "config::tests::telemetry_defaults_and_environment_override",
            ])
            .env("OPENSANDBOX_DISABLE_METRICS", " 1 ")
            .env("OPENSANDBOX_METRICS_ENV_TEST", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn preserves_prefix_and_normalizes_v1() {
        for domain in [
            "https://example.com/deployment",
            "https://example.com/deployment/v1/",
            "https://example.com/deployment/v1",
        ] {
            let config = ConnectionConfig::builder()
                .domain(domain)
                .api_key("secret")
                .build()
                .unwrap();
            assert_eq!(
                config.base_url.as_str(),
                "https://example.com/deployment/v1/"
            );
            assert!(!format!("{config:?}").contains("secret"));
        }
    }

    #[test]
    fn explicit_header_has_priority_case_insensitively() {
        let config = ConnectionConfig::builder()
            .domain("localhost:8080")
            .api_key("configured")
            .header("Open-Sandbox-Api-Key", "override")
            .build()
            .unwrap();
        assert_eq!(config.headers["open-sandbox-api-key"], "override");
        assert!(!format!("{:?}", config.headers).contains("override"));
    }

    #[test]
    fn rejects_unsafe_urls_headers_and_zero_timeout() {
        for url in [
            "ftp://example.com",
            "http://user:pass@example.com",
            "http://example.com?secret=x",
            "http://example.com#frag",
        ] {
            assert!(ConnectionConfig::builder().domain(url).build().is_err());
        }
        assert!(ConnectionConfig::builder()
            .domain("localhost:8080")
            .header("x-token", "a\r\nb")
            .build()
            .is_err());
        assert!(ConnectionConfig::builder()
            .domain("localhost:8080")
            .request_timeout(Duration::ZERO)
            .build()
            .is_err());
    }
}
