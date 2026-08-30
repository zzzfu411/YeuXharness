use crate::{AgentId, ApprovalId, EffectSet, ThreadId, WorkspaceId};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    Observe,
    Build,
    Operate,
}

impl CapabilityMode {
    pub const fn minimum(self, other: Self) -> Self {
        if (self as u8) <= (other as u8) {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapabilityGrant {
    pub mode: CapabilityMode,
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    #[serde(default)]
    pub filesystem_delete: Vec<String>,
    #[serde(default)]
    pub process: bool,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub external_write: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl CapabilityGrant {
    pub fn observe() -> Self {
        Self {
            mode: CapabilityMode::Observe,
            filesystem_read: Vec::new(),
            filesystem_write: Vec::new(),
            filesystem_delete: Vec::new(),
            process: false,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalBinding {
    pub approval_id: ApprovalId,
    pub workspace_id: WorkspaceId,
    pub workspace_identity_digest: String,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
    pub mode: CapabilityMode,
    pub tool_id: String,
    pub tool_version: String,
    pub normalized_arguments_digest: String,
    pub effect_digest: String,
    pub granted_effects: EffectSet,
    pub expires_at: DateTime<Utc>,
}
