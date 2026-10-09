// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    commands::{id_url, nonblank},
    models::egress::{NetworkPolicy, NetworkRule, PolicyStatus},
    transport::Transport,
    Error, Result,
};
use reqwest::{Method, Url};

#[derive(Clone)]
pub struct Egress {
    transport: Transport,
    sandbox_id: Option<String>,
}
impl Egress {
    pub(crate) fn new(transport: Transport) -> Self {
        Self {
            transport,
            sandbox_id: None,
        }
    }
    pub(crate) fn lifecycle(transport: Transport, id: String) -> Self {
        Self {
            transport,
            sandbox_id: Some(id),
        }
    }
    fn url(&self) -> Result<Url> {
        match &self.sandbox_id {
            Some(id) => id_url(&self.transport, "/sandboxes", id, Some("networkpolicy")),
            None => self.transport.url("/policy"),
        }
    }
    pub async fn get_policy_status(&self) -> Result<PolicyStatus> {
        self.transport
            .json(Method::GET, self.url()?, None::<&()>)
            .await
    }
    pub async fn get_policy(&self) -> Result<NetworkPolicy> {
        self.get_policy_status()
            .await?
            .policy
            .ok_or_else(|| Error::Protocol("Policy response omitted policy".into()))
    }
    /// Complete replacement is supported by lifecycle policy endpoints only.
    /// Direct sidecars expose rule mutation, not a public PUT contract.
    pub async fn replace_policy(&self, policy: &NetworkPolicy) -> Result<PolicyStatus> {
        if self.sandbox_id.is_none() {
            return Err(Error::InvalidConfig(
                "Full policy replacement requires a lifecycle policy endpoint".into(),
            ));
        }
        validate_rules(&policy.egress)?;
        self.transport
            .json(Method::PUT, self.url()?, Some(policy))
            .await
    }
    pub async fn patch_rules(&self, rules: &[NetworkRule]) -> Result<PolicyStatus> {
        if rules.is_empty() {
            return Err(Error::InvalidConfig("At least one rule is required".into()));
        }
        validate_rules(rules)?;
        self.transport
            .json(Method::PATCH, self.url()?, Some(&rules))
            .await
    }
    pub async fn delete_rules(&self, targets: &[String]) -> Result<PolicyStatus> {
        if targets.is_empty() {
            return Err(Error::InvalidConfig(
                "At least one target is required".into(),
            ));
        }
        for target in targets {
            nonblank(target, "target")?;
        }
        self.transport
            .json(Method::DELETE, self.url()?, Some(&targets))
            .await
    }
}
fn validate_rules(rules: &[NetworkRule]) -> Result<()> {
    for rule in rules {
        nonblank(&rule.target, "target")?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lifecycle_replacement_and_direct_rule_paths() {
        let body = r#"{"status":"ok","mode":"deny_all"}"#;
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes();
        let (transport, task) = crate::commands::tests::serve_once(response.clone()).await;
        Egress::lifecycle(transport, "a/b?c".into())
            .replace_policy(&NetworkPolicy::default())
            .await
            .unwrap();
        let request = String::from_utf8(task.await.unwrap()).unwrap();
        assert!(request.starts_with("PUT /prefix/v1/sandboxes/a%2Fb%3Fc/networkpolicy HTTP/1.1"));
        let (transport, task) = crate::commands::tests::serve_once(response).await;
        let egress = Egress::new(transport);
        assert!(egress
            .replace_policy(&NetworkPolicy::default())
            .await
            .is_err());
        egress.delete_rules(&["example.com".into()]).await.unwrap();
        let request = String::from_utf8(task.await.unwrap()).unwrap();
        assert!(request.starts_with("DELETE /prefix/v1/policy HTTP/1.1"));
        assert!(request.ends_with("[\"example.com\"]"));
    }
    #[test]
    fn mode_variations_and_missing_policy() {
        for mode in ["dns", "dns+nft", "future-mode"] {
            let status: PolicyStatus = serde_json::from_value(
                serde_json::json!({"mode":"deny_all","enforcementMode":mode}),
            )
            .unwrap();
            assert_eq!(status.enforcement_mode.as_deref(), Some(mode));
            assert!(status.policy.is_none());
        }
        let status: PolicyStatus =
            serde_json::from_str(r#"{"policy":{"defaultAction":"deny","egress":[]}}"#).unwrap();
        assert!(status.policy.is_some());
    }
}
