// Copyright 2026 The OpenSandbox Authors
// SPDX-License-Identifier: Apache-2.0

pub use crate::models::lifecycle::{NetworkPolicy, NetworkRule, NetworkRuleAction};
use serde::{Deserialize, Serialize};

/// Mode strings are intentionally open-ended to preserve new server modes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyStatus {
    pub status: Option<String>,
    pub mode: Option<String>,
    pub enforcement_mode: Option<String>,
    pub reason: Option<String>,
    pub policy: Option<NetworkPolicy>,
}
