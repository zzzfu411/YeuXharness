//! The single authority path for side-effecting tools.
//!
//! A protocol `PreparedInvocation` is evidence only.  This module is the sole
//! daemon-owned code that can turn that evidence into the sealed registry
//! permit consumed by a mutation or process adapter.

#![allow(clippy::result_large_err)]

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use thiserror::Error;
use yeux_core::{digest_value, evaluate_policy, validate_approval, PolicyDecision, PolicyInput};
use yeux_protocol::{
    AgentId, ApprovalBinding, ApprovalId, ApprovalRequestParams, CapabilityGrant, CapabilityMode,
    EffectSet, InvocationId, PreparedInvocation, ThreadId, TurnId, WorkspaceId,
};
use yeux_runtime::{
    CredentialBroker, CredentialError, CredentialLease, SandboxBackend, SandboxError,
    SandboxRequirement,
};

use crate::tools::{
    ExecutionPermit, ToolRegistry, ToolRegistryError, PROCESS_RUN_TOOL_ID, PROCESS_TOOL_VERSION,
    WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_LIST_TOOL_ID, WORKSPACE_READ_TOOL_ID,
    WORKSPACE_TOOL_VERSION,
};

pub const DEFAULT_PREPARATION_TTL_SECONDS: i64 = 60;

/// The four independent capability layers supplied to policy evaluation.
#[derive(Clone, Debug)]
pub struct PipelineGrants {
    pub host_ceiling: CapabilityGrant,
    pub user_profile: CapabilityGrant,
    pub project_trust: CapabilityGrant,
    pub turn_override: CapabilityGrant,
}

impl Default for PipelineGrants {
    fn default() -> Self {
        let observe = CapabilityGrant::observe();
        Self {
            host_ceiling: observe.clone(),
            user_profile: observe.clone(),
            project_trust: observe.clone(),
            turn_override: observe,
        }
    }
}

/// Stable identity facts bound into every prepared invocation.
#[derive(Clone, Debug)]
pub struct InvocationContext {
    pub invocation_id: InvocationId,
    pub workspace_id: WorkspaceId,
    pub workspace_identity_digest: String,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub agent_id: AgentId,
    pub grants: PipelineGrants,
    pub now: DateTime<Utc>,
    pub preparation_ttl: Duration,
}

impl InvocationContext {
    pub fn with_ids(
        invocation_id: InvocationId,
        workspace_id: WorkspaceId,
        workspace_identity_digest: impl Into<String>,
        thread_id: ThreadId,
        turn_id: TurnId,
        agent_id: AgentId,
        grants: PipelineGrants,
    ) -> Self {
        Self {
            invocation_id,
            workspace_id,
            workspace_identity_digest: workspace_identity_digest.into(),
            thread_id,
            turn_id,
            agent_id,
            grants,
            now: Utc::now(),
            preparation_ttl: Duration::seconds(DEFAULT_PREPARATION_TTL_SECONDS),
        }
    }
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Tool(#[from] ToolRegistryError),
    #[error("policy denied invocation: {reasons:?}")]
    PolicyDenied { reasons: Vec<String> },
    #[error("approval denied the invocation")]
    ApprovalDenied,
    #[error("approval is required before execution")]
    ApprovalRequired,
    #[error("approval response cannot supply a binding; the daemon mints it")]
    ClientApprovalBinding,
    #[error("sandbox is required for side-effecting tools: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("prepared invocation binding mismatch: {0}")]
    BindingMismatch(&'static str),
    #[error("prepared invocation has an invalid mode")]
    InvalidMode,
    #[error("prepared invocation token was not minted by this pipeline")]
    UnknownPreparedToken,
    #[error("prepared invocation token has already been consumed")]
    TokenConsumed,
    #[error("prepared invocation token has expired")]
    PreparationExpired,
    #[error(transparent)]
    Approval(#[from] yeux_core::ApprovalError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

impl PipelineError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Tool(error) => error.code(),
            Self::PolicyDenied { .. } => "policy_denied",
            Self::ApprovalDenied => "approval_denied",
            Self::ApprovalRequired => "approval_required",
            Self::ClientApprovalBinding => "client_approval_binding_rejected",
            Self::Sandbox(_) => "sandbox_unavailable",
            Self::BindingMismatch(_) => "invocation_binding_mismatch",
            Self::InvalidMode => "invalid_approval_mode",
            Self::UnknownPreparedToken => "unknown_prepared_token",
            Self::TokenConsumed => "prepared_token_consumed",
            Self::PreparationExpired => "prepared_token_expired",
            Self::Approval(error) => match error {
                yeux_core::ApprovalError::MissingApproval => "approval_required",
                _ => "approval_invalid",
            },
            Self::Credential(_) => "credential_unavailable",
        }
    }
}

/// Authority pipeline shared by `workspace.apply_patch` and `process.run`.
#[derive(Clone)]
pub struct InvocationPipeline {
    registry: Arc<ToolRegistry>,
    sandbox: SandboxBackend,
    credentials: Arc<dyn CredentialBroker>,
    issued_tokens: Arc<Mutex<BTreeMap<String, String>>>,
    consumed_tokens: Arc<Mutex<BTreeSet<String>>>,
}

impl std::fmt::Debug for InvocationPipeline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InvocationPipeline")
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

impl InvocationPipeline {
    pub fn new(
        registry: Arc<ToolRegistry>,
        sandbox: SandboxBackend,
        credentials: Arc<dyn CredentialBroker>,
    ) -> Self {
        Self {
            registry,
            sandbox,
            credentials,
            issued_tokens: Arc::new(Mutex::new(BTreeMap::new())),
            consumed_tokens: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn sandbox(&self) -> &SandboxBackend {
        &self.sandbox
    }

    /// A build-mode client may only advertise itself when both side-effecting
    /// registrations exist and the host can start its required OS sandbox.
    pub fn write_tools_available(&self) -> bool {
        self.registry
            .is_registered(WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_TOOL_VERSION)
            && self
                .sandbox
                .ensure(sandbox_requirement(true, false))
                .is_ok()
    }

    pub fn process_tools_available(&self) -> bool {
        self.registry
            .is_registered(PROCESS_RUN_TOOL_ID, PROCESS_TOOL_VERSION)
            && self
                .sandbox
                .ensure(sandbox_requirement(false, true))
                .is_ok()
    }

    /// Resolve a secret only for a runtime-owned provider/network adapter.
    /// No pipeline tool receives this lease or a raw value.
    pub async fn resolve_secret(&self, handle: &str) -> Result<CredentialLease, PipelineError> {
        Ok(self.credentials.resolve(handle).await?)
    }

    /// Plan, normalize, policy-check, and bind one invocation. No side effect
    /// happens here. A side-effecting invocation is returned without approval
    /// so the caller can present it through the interactive gate.
    pub fn prepare(
        &self,
        tool_id: &str,
        tool_version: &str,
        arguments: Value,
        context: &InvocationContext,
    ) -> Result<PreparedInvocation, PipelineError> {
        let plan = self.registry.plan(tool_id, tool_version, arguments)?;
        if plan.workspace_identity() != context.workspace_identity_digest {
            return Err(PipelineError::BindingMismatch("workspace_identity_digest"));
        }
        let effects = plan.effects().clone();
        let decision = evaluate_policy(PolicyInput {
            host_ceiling: context.grants.host_ceiling.clone(),
            user_profile: context.grants.user_profile.clone(),
            project_trust: context.grants.project_trust.clone(),
            turn_override: context.grants.turn_override.clone(),
            effects: effects.clone(),
            now: context.now,
        });
        let effective_mode = match decision {
            PolicyDecision::Allow {
                effective_grant,
                ..
            } => effective_grant.mode,
            PolicyDecision::Deny { reasons, .. } => {
                return Err(PipelineError::PolicyDenied { reasons });
            }
        };
        if !effects.is_read_only() {
            self.sandbox.ensure(sandbox_requirement(
                !effects.filesystem_write.is_empty() || !effects.filesystem_delete.is_empty(),
                !effects.processes.is_empty(),
            ))?;
        }
        let normalized_arguments = plan.normalized_arguments().clone();
        let prepared_at = context.now;
        let expires_at = prepared_at + context.preparation_ttl;
        let invocation = PreparedInvocation {
            invocation_id: context.invocation_id,
            tool_id: tool_id.to_owned(),
            tool_version: tool_version.to_owned(),
            workspace_id: context.workspace_id,
            workspace_identity_digest: context.workspace_identity_digest.clone(),
            thread_id: context.thread_id,
            turn_id: context.turn_id,
            agent_id: context.agent_id.clone(),
            normalized_arguments: normalized_arguments.clone(),
            normalized_arguments_digest: digest_value(&normalized_arguments),
            effects: effects.clone(),
            effect_digest: digest_effects(&effects),
            prepared_token: uuid::Uuid::now_v7().to_string(),
            prepared_at,
            expires_at,
            approval: if effective_mode < CapabilityMode::Build && !effects.is_read_only() {
                return Err(PipelineError::InvalidMode);
            } else {
                None
            },
        };
        self.issued_tokens
            .lock()
            .map_err(|_| PipelineError::UnknownPreparedToken)?
            .insert(
                invocation.prepared_token.clone(),
                prepared_binding_digest(&invocation),
            );
        Ok(invocation)
    }

    pub fn requires_approval(invocation: &PreparedInvocation) -> bool {
        !invocation.effects.is_read_only()
    }

    pub fn approval_request(
        &self,
        invocation: &PreparedInvocation,
        explanation: impl Into<String>,
    ) -> ApprovalRequestParams {
        ApprovalRequestParams {
            invocation: invocation.clone(),
            explanation: explanation.into(),
        }
    }

    /// Consume a human decision and mint the exact binding locally. A client
    /// cannot provide or alter an `ApprovalBinding`.
    pub fn approve_once(
        &self,
        mut invocation: PreparedInvocation,
        approved: bool,
    ) -> Result<PreparedInvocation, PipelineError> {
        self.ensure_issued(&invocation)?;
        if invocation.expires_at < Utc::now() {
            return Err(PipelineError::PreparationExpired);
        }
        if !Self::requires_approval(&invocation) {
            return Ok(invocation);
        }
        if !approved {
            return Err(PipelineError::ApprovalDenied);
        }
        let mode = required_mode(&invocation.effects)?;
        invocation.approval = Some(ApprovalBinding {
            approval_id: ApprovalId::from_uuid(uuid::Uuid::now_v7()),
            invocation_id: invocation.invocation_id,
            workspace_id: invocation.workspace_id,
            workspace_identity_digest: invocation.workspace_identity_digest.clone(),
            thread_id: invocation.thread_id,
            turn_id: invocation.turn_id,
            agent_id: invocation.agent_id.clone(),
            mode,
            tool_id: invocation.tool_id.clone(),
            tool_version: invocation.tool_version.clone(),
            normalized_arguments_digest: invocation.normalized_arguments_digest.clone(),
            effect_digest: invocation.effect_digest.clone(),
            granted_effects: invocation.effects.clone(),
            expires_at: invocation.expires_at,
        });
        Ok(invocation)
    }

    /// Boundary used by JSON-RPC approval adapters. The optional binding is
    /// intentionally rejected: only the daemon can mint an
    /// `ApprovalBinding` from the original prepared evidence.
    pub fn accept_approval_response(
        &self,
        invocation: PreparedInvocation,
        approved: bool,
        supplied_binding: Option<ApprovalBinding>,
    ) -> Result<PreparedInvocation, PipelineError> {
        if supplied_binding.is_some() {
            return Err(PipelineError::ClientApprovalBinding);
        }
        self.approve_once(invocation, approved)
    }

    /// Run the interactive approval boundary and then execute the same
    /// prepared invocation. Read-only calls auto-approve without invoking the
    /// gate; side effects always cross it exactly once.
    pub async fn approve_and_execute<F, Fut>(
        &self,
        invocation: PreparedInvocation,
        gate: F,
    ) -> Result<Value, PipelineError>
    where
        F: FnOnce(ApprovalRequestParams) -> Fut,
        Fut: Future<Output = bool>,
    {
        let invocation = if Self::requires_approval(&invocation) {
            let request = self.approval_request(
                &invocation,
                "side-effecting tool requires approval",
            );
            self.approve_once(invocation, gate(request).await)?
        } else {
            invocation
        };
        self.execute(invocation).await
    }

    /// Execute only after exact binding validation and a fresh plan/revalidate
    /// pass. This is the only public path that obtains an execution permit.
    pub async fn execute(
        &self,
        invocation: PreparedInvocation,
    ) -> Result<Value, PipelineError> {
        self.ensure_issued(&invocation)?;
        if invocation.expires_at < Utc::now() {
            return Err(PipelineError::PreparationExpired);
        }
        let mode = if Self::requires_approval(&invocation) {
            required_mode(&invocation.effects)?
        } else {
            CapabilityMode::Observe
        };
        if Self::requires_approval(&invocation) && invocation.approval.is_none() {
            return Err(PipelineError::ApprovalRequired);
        }
        validate_approval(&invocation, mode, Utc::now())
            .or_else(|error| {
                if !Self::requires_approval(&invocation)
                    && matches!(error, yeux_core::ApprovalError::MissingApproval)
                {
                    Ok(())
                } else {
                    Err(error)
                }
            })?;
        if !invocation.effects.is_read_only() {
            self.sandbox.ensure(sandbox_requirement(
                !invocation.effects.filesystem_write.is_empty()
                    || !invocation.effects.filesystem_delete.is_empty(),
                !invocation.effects.processes.is_empty(),
            ))?;
        }
        let plan = self
            .registry
            .plan(
                &invocation.tool_id,
                &invocation.tool_version,
                invocation.normalized_arguments.clone(),
            )?;
        let revalidated = self.registry.revalidate(plan)?;
        if revalidated.workspace_identity() != invocation.workspace_identity_digest {
            return Err(PipelineError::BindingMismatch("workspace_identity_digest"));
        }
        if revalidated.normalized_arguments() != &invocation.normalized_arguments {
            return Err(PipelineError::BindingMismatch("normalized_arguments"));
        }
        if revalidated.effects() != &invocation.effects {
            return Err(PipelineError::BindingMismatch("effects"));
        }
        if digest_value(revalidated.normalized_arguments())
            != invocation.normalized_arguments_digest
        {
            return Err(PipelineError::BindingMismatch("normalized_arguments_digest"));
        }
        if digest_effects(revalidated.effects()) != invocation.effect_digest {
            return Err(PipelineError::BindingMismatch("effect_digest"));
        }
        {
            let mut consumed = self
                .consumed_tokens
                .lock()
                .map_err(|_| PipelineError::TokenConsumed)?;
            if !consumed.insert(invocation.prepared_token.clone()) {
                return Err(PipelineError::TokenConsumed);
            }
        }
        let permit: ExecutionPermit = revalidated.into_execution_permit();
        Ok(self.registry.execute_async(permit).await?.into_value())
    }

    fn ensure_issued(&self, invocation: &PreparedInvocation) -> Result<(), PipelineError> {
        let issued = self
            .issued_tokens
            .lock()
            .map_err(|_| PipelineError::UnknownPreparedToken)?;
        let Some(binding) = issued.get(&invocation.prepared_token) else {
            return Err(PipelineError::UnknownPreparedToken);
        };
        if binding != &prepared_binding_digest(invocation) {
            return Err(PipelineError::BindingMismatch("prepared_token_binding"));
        }
        Ok(())
    }
}

fn prepared_binding_digest(invocation: &PreparedInvocation) -> String {
    digest_value(&serde_json::json!({
        "invocation_id": invocation.invocation_id,
        "tool_id": invocation.tool_id,
        "tool_version": invocation.tool_version,
        "workspace_id": invocation.workspace_id,
        "workspace_identity_digest": invocation.workspace_identity_digest,
        "thread_id": invocation.thread_id,
        "turn_id": invocation.turn_id,
        "agent_id": invocation.agent_id,
        "normalized_arguments": invocation.normalized_arguments,
        "normalized_arguments_digest": invocation.normalized_arguments_digest,
        "effects": invocation.effects,
        "effect_digest": invocation.effect_digest,
        "prepared_at": invocation.prepared_at,
        "expires_at": invocation.expires_at,
    }))
}

fn digest_effects(effects: &EffectSet) -> String {
    digest_value(
        &serde_json::to_value(effects)
            .expect("protocol effect sets contain only serializable fields"),
    )
}

fn required_mode(effects: &EffectSet) -> Result<CapabilityMode, PipelineError> {
    if effects.filesystem_write.is_empty()
        && effects.filesystem_delete.is_empty()
        && effects.processes.is_empty()
        && effects.network.is_empty()
        && effects.secrets.is_empty()
        && effects.external_writes.is_empty()
    {
        return Ok(CapabilityMode::Observe);
    }
    if !effects.external_writes.is_empty() || !effects.network.is_empty() {
        return Ok(CapabilityMode::Operate);
    }
    Ok(CapabilityMode::Build)
}

fn sandbox_requirement(allow_workspace_write: bool, process: bool) -> SandboxRequirement {
    SandboxRequirement {
        filesystem_isolation: true,
        process_isolation: process || allow_workspace_write,
        network_isolation: true,
        allow_workspace_write,
        allow_network: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use yeux_runtime::{NoCredentialBroker, Workspace, WorkspaceTools};

    fn grants() -> PipelineGrants {
        let build = CapabilityGrant {
            mode: CapabilityMode::Build,
            filesystem_read: vec!["*".into()],
            filesystem_write: vec!["*".into()],
            filesystem_delete: vec!["*".into()],
            process: true,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        };
        PipelineGrants {
            host_ceiling: build.clone(),
            user_profile: build.clone(),
            project_trust: build.clone(),
            turn_override: build,
        }
    }

    fn context(workspace: &Workspace) -> InvocationContext {
        InvocationContext::with_ids(
            InvocationId::from_uuid(uuid::Uuid::now_v7()),
            WorkspaceId::from_uuid(uuid::Uuid::now_v7()),
            workspace.identity(),
            ThreadId::from_uuid(uuid::Uuid::now_v7()),
            TurnId::from_uuid(uuid::Uuid::now_v7()),
            AgentId::new("root"),
            grants(),
        )
    }

    #[test]
    fn mutation_cannot_start_when_sandbox_is_unavailable() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tools = WorkspaceTools::new(workspace.clone());
        let registry = Arc::new(
            ToolRegistry::workspace_built_ins_with_config(
                tools,
                crate::tools::BuiltInToolRegistryConfig::read_only()
                    .with_hidden_workspace_mutations(),
            )
            .unwrap(),
        );
        let pipeline = InvocationPipeline::new(
            registry,
            SandboxBackend::Unavailable {
                reason: "fixture sandbox missing".into(),
            },
            Arc::new(NoCredentialBroker),
        );
        let base = blake3::hash(b"before\n").to_hex().to_string();
        let result = pipeline.prepare(
            WORKSPACE_APPLY_PATCH_TOOL_ID,
            WORKSPACE_TOOL_VERSION,
            serde_json::json!({
                "path": "hello.txt",
                "base_revision": base,
                "replacement": "after\n"
            }),
            &context(&workspace),
        );
        assert!(matches!(result, Err(PipelineError::Sandbox(_))));
        assert_eq!(fs::read_to_string(directory.path().join("hello.txt")).unwrap(), "before\n");
    }

    #[test]
    fn approval_binding_is_daemon_minted_and_client_binding_is_rejected() {
        let invocation = PreparedInvocation {
            invocation_id: InvocationId::from_uuid(uuid::Uuid::now_v7()),
            tool_id: WORKSPACE_APPLY_PATCH_TOOL_ID.into(),
            tool_version: WORKSPACE_TOOL_VERSION.into(),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::now_v7()),
            workspace_identity_digest: "workspace".into(),
            thread_id: ThreadId::from_uuid(uuid::Uuid::now_v7()),
            turn_id: TurnId::from_uuid(uuid::Uuid::now_v7()),
            agent_id: AgentId::new("root"),
            normalized_arguments: serde_json::json!({"path":"x"}),
            normalized_arguments_digest: "args".into(),
            effects: EffectSet {
                filesystem_write: vec![yeux_protocol::PathScope {
                    path: "x".into(),
                    recursive: false,
                    resolved: true,
                }],
                ..EffectSet::default()
            },
            effect_digest: "effects".into(),
            prepared_token: "opaque".into(),
            prepared_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(1),
            approval: None,
        };
        let registry = Arc::new(
            ToolRegistry::workspace_built_ins_with_config(
                WorkspaceTools::new(Workspace::open(tempdir().unwrap().path()).unwrap()),
                crate::tools::BuiltInToolRegistryConfig::read_only(),
            )
            .unwrap(),
        );
        let pipeline = InvocationPipeline::new(
            registry,
            SandboxBackend::Unavailable { reason: "test".into() },
            Arc::new(NoCredentialBroker),
        );
        assert!(matches!(
            pipeline.accept_approval_response(invocation, true, Some(unsafe_binding())),
            Err(PipelineError::ClientApprovalBinding)
        ));
    }

    #[tokio::test]
    async fn prepared_token_cannot_be_replayed_after_one_execution() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = Arc::new(
            ToolRegistry::workspace_built_ins_with_config(
                WorkspaceTools::new(workspace.clone()),
                crate::tools::BuiltInToolRegistryConfig::read_only(),
            )
            .unwrap(),
        );
        let pipeline = InvocationPipeline::new(
            registry,
            SandboxBackend::Unavailable {
                reason: "read-only replay test does not need a sandbox".into(),
            },
            Arc::new(NoCredentialBroker),
        );
        let prepared = pipeline
            .prepare(
                WORKSPACE_LIST_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                serde_json::json!({}),
                &context(&workspace),
            )
            .unwrap();
        let replay = prepared.clone();
        pipeline.execute(prepared).await.unwrap();
        assert!(matches!(
            pipeline.execute(replay).await,
            Err(PipelineError::TokenConsumed)
        ));
    }

    #[test]
    fn prepared_token_cannot_be_rebound_to_different_invocation_evidence() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = Arc::new(
            ToolRegistry::workspace_built_ins_with_config(
                WorkspaceTools::new(workspace.clone()),
                crate::tools::BuiltInToolRegistryConfig::read_only(),
            )
            .unwrap(),
        );
        let pipeline = InvocationPipeline::new(
            registry,
            SandboxBackend::Unavailable {
                reason: "forged-token test is read-only".into(),
            },
            Arc::new(NoCredentialBroker),
        );
        let prepared = pipeline
            .prepare(
                WORKSPACE_LIST_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                serde_json::json!({}),
                &context(&workspace),
            )
            .unwrap();
        let mut forged = prepared.clone();
        forged.tool_id = WORKSPACE_READ_TOOL_ID.into();
        assert!(matches!(
            pipeline.approve_once(forged, true),
            Err(PipelineError::BindingMismatch("prepared_token_binding"))
        ));
    }

    fn unsafe_binding() -> ApprovalBinding {
        // This value models an untrusted JSON-RPC response. The pipeline must
        // reject it before any fields are inspected or used as authority.
        ApprovalBinding {
            approval_id: ApprovalId::from_uuid(uuid::Uuid::now_v7()),
            invocation_id: InvocationId::from_uuid(uuid::Uuid::now_v7()),
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::now_v7()),
            workspace_identity_digest: "attacker".into(),
            thread_id: ThreadId::from_uuid(uuid::Uuid::now_v7()),
            turn_id: TurnId::from_uuid(uuid::Uuid::now_v7()),
            agent_id: AgentId::new("attacker"),
            mode: CapabilityMode::Operate,
            tool_id: "process.run".into(),
            tool_version: "1".into(),
            normalized_arguments_digest: "attacker".into(),
            effect_digest: "attacker".into(),
            granted_effects: EffectSet::default(),
            expires_at: Utc::now() + Duration::minutes(1),
        }
    }
}
