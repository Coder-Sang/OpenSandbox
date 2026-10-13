// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{models::lifecycle::Endpoint, ConnectionConfig, Error, Result};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, RequestBuilder, Response,
};
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub(crate) struct Transport {
    client: Client,
    base_url: Url,
    headers: HeaderMap,
    connection_headers: HeaderMap,
    request_timeout: Duration,
    use_server_proxy: bool,
    protocol: String,
}

impl Transport {
    pub(crate) fn new(config: &ConnectionConfig) -> Result<Self> {
        config.validate()?;
        // Never redirect requests carrying endpoint credentials to an
        // unexpected host. The SDK owns this policy for every shared client.
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.request_timeout)
            .build()?;
        Self::with_client(config, client)
    }

    pub(crate) fn with_client(config: &ConnectionConfig, client: Client) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            client,
            base_url: config.base_url.clone(),
            headers: config.headers.clone(),
            connection_headers: config.headers.clone(),
            request_timeout: config.request_timeout,
            use_server_proxy: config.use_server_proxy,
            protocol: config.base_url.scheme().to_owned(),
        })
    }

    pub(crate) fn lifecycle_url(&self, path: &str) -> Result<Url> {
        self.url(path)
    }

    /// Append already-separated generated path segments without losing a prefix.
    pub(crate) fn operation_url(&self, parts: &[(&str, bool)]) -> Result<Url> {
        let mut url = self.url("")?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::InvalidConfig("API URL cannot be a base".into()))?;
        segments.pop_if_empty();
        for (part, parameter) in parts {
            if *parameter
                && (part.trim().is_empty() || matches!(*part, "." | "..") || part.contains('\0'))
            {
                return Err(Error::InvalidConfig("Invalid API path parameter".into()));
            }
            segments.push(part);
        }
        drop(segments);
        Ok(url)
    }

    pub(crate) fn url(&self, path: &str) -> Result<Url> {
        // Do not use Url::join with a leading slash: it discards deployment
        // and server-proxy path prefixes.
        if path.contains("://") || path.starts_with("//") {
            return Err(Error::InvalidConfig(
                "request path must be relative to the API base".into(),
            ));
        }
        let url = format!(
            "{}/{}",
            self.base_url.as_str().trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        Url::parse(&url).map_err(|_| Error::InvalidConfig("invalid API request path".into()))
    }

    pub(crate) fn data_plane(&self, endpoint: &Endpoint) -> Result<Self> {
        let address = if endpoint.endpoint.contains("://") {
            endpoint.endpoint.clone()
        } else {
            format!("{}://{}", self.protocol, endpoint.endpoint)
        };
        let mut base_url = Url::parse(&address)
            .map_err(|_| Error::Protocol("server returned an invalid endpoint".into()))?;
        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.host_str().is_none()
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(Error::Protocol(
                "server endpoint must be HTTP(S) with no URL credentials, query or fragment".into(),
            ));
        }
        let path = format!("{}/", base_url.path().trim_end_matches('/'));
        base_url.set_path(&path);
        let mut headers = self.connection_headers.clone();
        headers.remove("traceparent");
        headers.remove("tracestate");
        if !self.use_server_proxy {
            headers.remove("open-sandbox-api-key");
        }
        for (name, value) in &endpoint.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Protocol("endpoint contains an invalid header name".into()))?;
            // Never forward control-plane API keys to direct data-plane routes,
            // including when the endpoint response happens to contain one.
            if !self.use_server_proxy && name == "open-sandbox-api-key" {
                continue;
            }
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| Error::Protocol("endpoint contains an invalid header value".into()))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(Self {
            base_url,
            headers,
            ..self.clone()
        })
    }

    pub(crate) fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.streaming_request(method, url)
            .timeout(self.request_timeout)
    }

    pub(crate) fn streaming_request(&self, method: Method, url: Url) -> RequestBuilder {
        self.client
            .request(method, url)
            .headers(self.headers.clone())
    }

    /// Sensitive request payloads may be echoed by an untrusted server in any
    /// response field. Preserve the HTTP status without decoding error metadata.
    pub(crate) async fn checked_sensitive(response: Response) -> Result<Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        Err(Error::Api {
            status: response.status().as_u16(),
            code: None,
            message: "Sensitive API request failed".into(),
            request_id: None,
        })
    }

    pub(crate) async fn checked(mut response: Response) -> Result<Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // Bound untrusted error responses. Preserve structured error messages
        // but do not embed arbitrary HTML or raw credentials in errors.
        const MAX_ERROR_BYTES: usize = 64 * 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            let length = chunk.len().min(MAX_ERROR_BYTES - body.len());
            body.extend_from_slice(&chunk[..length]);
            if body.len() == MAX_ERROR_BYTES {
                break;
            }
        }
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let value = value
            .get("error")
            .filter(|v| v.is_object())
            .unwrap_or(&value);
        let code = value
            .get("code")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("unexpected HTTP response {status}"));
        Err(Error::Api {
            status,
            code,
            message,
            request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(proxy: bool) -> ConnectionConfig {
        ConnectionConfig::builder()
            .domain("https://control.example/prefix")
            .api_key("control-secret")
            .header("x-custom", "custom")
            .use_server_proxy(proxy)
            .build()
            .unwrap()
    }

    #[test]
    fn signed_route_debug_and_control_trace_headers_are_redacted() {
        let config = ConnectionConfig::builder()
            .domain("https://control.example/prefix")
            .header("traceparent", "00-control-trace-secret")
            .header("tracestate", "control-state-secret")
            .build()
            .unwrap();
        let transport = Transport::new(&config).unwrap();
        let endpoint = Endpoint {
            endpoint: "worker.example/signed-route-secret".into(),
            headers: BTreeMap::new(),
            origin: None,
        };
        assert!(!format!("{endpoint:?}").contains("signed-route-secret"));
        let data = transport.data_plane(&endpoint).unwrap();
        assert!(!data.headers.contains_key("traceparent"));
        assert!(!data.headers.contains_key("tracestate"));
        assert!(transport.headers.contains_key("traceparent"));
    }

    #[test]
    fn direct_headers_never_carry_control_key() {
        let transport = Transport::new(&config(false)).unwrap();
        let endpoint = Endpoint {
            endpoint: "worker.example/proxy/sandbox".into(),
            headers: BTreeMap::from([
                ("X-EXECD-ACCESS-TOKEN".into(), "token".into()),
                ("OPEN-SANDBOX-API-KEY".into(), "endpoint-key".into()),
            ]),
            origin: None,
        };
        assert!(!format!("{endpoint:?}").contains("token"));
        assert!(!format!("{endpoint:?}").contains("endpoint-key"));
        let data = transport.data_plane(&endpoint).unwrap();
        assert!(!data.headers.contains_key("open-sandbox-api-key"));
        assert_eq!(data.headers["x-execd-access-token"], "token");
        assert_eq!(data.headers["x-custom"], "custom");
        assert_eq!(
            data.url("/files/info").unwrap().as_str(),
            "https://worker.example/proxy/sandbox/files/info"
        );
    }

    #[test]
    fn proxy_endpoint_key_overrides_custom_case_insensitively() {
        let transport = Transport::new(&config(true)).unwrap();
        let endpoint = Endpoint {
            endpoint: "control.example/prefix/v1/sandboxes/id/proxy".into(),
            headers: BTreeMap::from([("Open-Sandbox-Api-Key".into(), "endpoint-secret".into())]),
            origin: None,
        };
        let data = transport.data_plane(&endpoint).unwrap();
        assert_eq!(data.headers["open-sandbox-api-key"], "endpoint-secret");
        assert_eq!(
            transport.url("/sandboxes").unwrap().path(),
            "/prefix/v1/sandboxes"
        );
    }

    #[test]
    fn escapes_path_identifiers() {
        assert!(Transport::new(&config(false))
            .unwrap()
            .url("https://other.example")
            .is_err());
    }
}
