// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    api::{egress_client as sidecar, lifecycle_client as lifecycle},
    commands::nonblank,
    models::egress::{NetworkPolicy, NetworkRule, PolicyStatus},
    transport::Transport,
    Error, Result,
};

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
    /// Create the direct sidecar's credential vault. Secret values are write-only.
    pub async fn create(
        &self,
        request: &crate::api::egress::CredentialVaultCreateRequest,
    ) -> Result<crate::api::egress::CredentialVaultState> {
        self.vault_client()?
            .post_credential_vault(request)?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await
    }
    /// Read vault metadata, never credential values.
    pub async fn get(&self) -> Result<crate::api::egress::CredentialVaultState> {
        self.vault_client()?
            .get_credential_vault()?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await
    }
    /// Atomically mutate the vault, optionally guarded by expectedRevision.
    pub async fn patch(
        &self,
        request: &crate::api::egress::CredentialVaultMutationRequest,
    ) -> Result<crate::api::egress::CredentialVaultState> {
        self.vault_client()?
            .patch_credential_vault(request)?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await
    }
    /// Delete the vault. Template-backed sandboxes reject this locally.
    pub async fn delete(&self) -> Result<()> {
        self.vault_client()?
            .delete_credential_vault()?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .empty()
            .await
    }
    pub async fn list_credentials(&self) -> Result<Vec<crate::api::egress::CredentialMetadata>> {
        let response = self
            .vault_client()?
            .get_credential_vault_credentials()?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await?;
        Ok(response.credentials)
    }
    pub async fn get_credential(
        &self,
        name: &str,
    ) -> Result<crate::api::egress::CredentialMetadata> {
        nonblank(name, "name")?;
        self.vault_client()?
            .get_credential_vault_credentials_credential_name(
                sidecar::GetCredentialVaultCredentialsCredentialNameParams {
                    credential_name: name,
                },
            )?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await
    }
    pub async fn list_bindings(
        &self,
    ) -> Result<Vec<crate::api::egress::CredentialBindingMetadata>> {
        let response = self
            .vault_client()?
            .get_credential_vault_bindings()?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await?;
        Ok(response.bindings)
    }
    pub async fn get_binding(
        &self,
        name: &str,
    ) -> Result<crate::api::egress::CredentialBindingMetadata> {
        nonblank(name, "name")?;
        self.vault_client()?
            .get_credential_vault_bindings_binding_name(
                sidecar::GetCredentialVaultBindingsBindingNameParams { binding_name: name },
            )?
            .sensitive(
                "Credential Vault request failed",
                "Invalid Credential Vault response",
            )
            .json()
            .await
    }
    fn vault_client(&self) -> Result<sidecar::Client<'_>> {
        if self.sandbox_id.is_some() {
            return Err(Error::InvalidConfig(
                "Credential Vault is not supported for template-backed sandboxes".into(),
            ));
        }
        Ok(sidecar::Client::new(&self.transport))
    }
    pub async fn get_policy_status(&self) -> Result<PolicyStatus> {
        match self.sandbox_id.as_deref() {
            Some(sandbox_id) => {
                lifecycle::Client::new(&self.transport)
                    .get_sandbox_network_policy(lifecycle::GetSandboxNetworkPolicyParams {
                        sandbox_id,
                    })?
                    .json_as()
                    .await
            }
            None => {
                sidecar::Client::new(&self.transport)
                    .get_policy()?
                    .json_as()
                    .await
            }
        }
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
        let sandbox_id = self.sandbox_id.as_deref().ok_or_else(|| {
            Error::InvalidConfig(
                "Full policy replacement requires a lifecycle policy endpoint".into(),
            )
        })?;
        validate_rules(&policy.egress)?;
        lifecycle::Client::new(&self.transport)
            .replace_sandbox_network_policy(
                lifecycle::ReplaceSandboxNetworkPolicyParams { sandbox_id },
                &wire(policy)?,
            )?
            .json_as()
            .await
    }
    pub async fn patch_rules(&self, rules: &[NetworkRule]) -> Result<PolicyStatus> {
        if rules.is_empty() {
            return Err(Error::InvalidConfig("At least one rule is required".into()));
        }
        validate_rules(rules)?;
        match self.sandbox_id.as_deref() {
            Some(sandbox_id) => {
                let body: Vec<crate::api::lifecycle::NetworkRule> = wire(rules)?;
                lifecycle::Client::new(&self.transport)
                    .patch_sandbox_network_policy(
                        lifecycle::PatchSandboxNetworkPolicyParams { sandbox_id },
                        &body,
                    )?
                    .json_as()
                    .await
            }
            None => {
                let body: Vec<crate::api::egress::NetworkRule> = wire(rules)?;
                sidecar::Client::new(&self.transport)
                    .patch_policy(&body)?
                    .json_as()
                    .await
            }
        }
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
        match self.sandbox_id.as_deref() {
            Some(sandbox_id) => {
                lifecycle::Client::new(&self.transport)
                    .delete_sandbox_network_policy_rules(
                        lifecycle::DeleteSandboxNetworkPolicyRulesParams { sandbox_id },
                        targets,
                    )?
                    .json_as()
                    .await
            }
            None => {
                sidecar::Client::new(&self.transport)
                    .delete_policy(targets)?
                    .json_as()
                    .await
            }
        }
    }
}
fn wire<T: serde::de::DeserializeOwned>(value: &(impl serde::Serialize + ?Sized)) -> Result<T> {
    Ok(serde_json::from_value(serde_json::to_value(value)?)?)
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
