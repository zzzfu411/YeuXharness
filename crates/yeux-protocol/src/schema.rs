use crate::*;
use schemars::{schema::RootSchema, schema_for};
use std::collections::BTreeMap;

/// Generates the stable protocol schemas from the Rust source of truth.
pub fn stable_schema_bundle() -> BTreeMap<&'static str, RootSchema> {
    let mut schemas = BTreeMap::new();
    schemas.insert("CommandEnvelope", schema_for!(CommandEnvelope));
    schemas.insert("ResponseEnvelope", schema_for!(ResponseEnvelope));
    schemas.insert("EventEnvelope", schema_for!(EventEnvelope));
    schemas.insert("InitializeParams", schema_for!(InitializeParams));
    schemas.insert("InitializeResult", schema_for!(InitializeResult));
    schemas.insert("WorkspaceOpenParams", schema_for!(WorkspaceOpenParams));
    schemas.insert("WorkspaceOpenResult", schema_for!(WorkspaceOpenResult));
    schemas.insert("WorkspaceTrustParams", schema_for!(WorkspaceTrustParams));
    schemas.insert("WorkspaceStatusParams", schema_for!(WorkspaceStatusParams));
    schemas.insert("WorkspaceStatusResult", schema_for!(WorkspaceStatusResult));
    schemas.insert("ThreadStartParams", schema_for!(ThreadStartParams));
    schemas.insert("ThreadResult", schema_for!(ThreadResult));
    schemas.insert("ThreadResumeParams", schema_for!(ThreadResumeParams));
    schemas.insert("ThreadForkParams", schema_for!(ThreadForkParams));
    schemas.insert("ThreadReadParams", schema_for!(ThreadReadParams));
    schemas.insert("ThreadReadResult", schema_for!(ThreadReadResult));
    schemas.insert("ThreadListParams", schema_for!(ThreadListParams));
    schemas.insert("ThreadListResult", schema_for!(ThreadListResult));
    schemas.insert("ThreadArchiveParams", schema_for!(ThreadArchiveParams));
    schemas.insert("ThreadCompactParams", schema_for!(ThreadCompactParams));
    schemas.insert("ThreadCompactResult", schema_for!(ThreadCompactResult));
    schemas.insert("ThreadSubscribeParams", schema_for!(ThreadSubscribeParams));
    schemas.insert("ThreadSubscribeResult", schema_for!(ThreadSubscribeResult));
    schemas.insert("TurnStartParams", schema_for!(TurnStartParams));
    schemas.insert("TurnResult", schema_for!(TurnResult));
    schemas.insert("TurnSteerParams", schema_for!(TurnSteerParams));
    schemas.insert("TurnInterruptParams", schema_for!(TurnInterruptParams));
    schemas.insert("AcceptedResult", schema_for!(AcceptedResult));
    schemas.insert(
        "InvocationReconcileParams",
        schema_for!(InvocationReconcileParams),
    );
    schemas.insert(
        "InvocationReconcileResult",
        schema_for!(InvocationReconcileResult),
    );
    schemas.insert("ModelListParams", schema_for!(ModelListParams));
    schemas.insert("ModelListResult", schema_for!(ModelListResult));
    schemas.insert("SkillListParams", schema_for!(SkillListParams));
    schemas.insert("SkillListResult", schema_for!(SkillListResult));
    schemas.insert("McpStatusParams", schema_for!(McpStatusParams));
    schemas.insert("McpStatusResult", schema_for!(McpStatusResult));
    schemas.insert("PluginListParams", schema_for!(PluginListParams));
    schemas.insert("PluginListResult", schema_for!(PluginListResult));
    schemas.insert("JobCreateParams", schema_for!(JobCreateParams));
    schemas.insert("JobResult", schema_for!(JobResult));
    schemas.insert("JobListParams", schema_for!(JobListParams));
    schemas.insert("JobListResult", schema_for!(JobListResult));
    schemas.insert("JobIdParams", schema_for!(JobIdParams));
    schemas.insert("ApprovalRequestParams", schema_for!(ApprovalRequestParams));
    schemas.insert("ApprovalRequestResult", schema_for!(ApprovalRequestResult));
    schemas.insert("UserInputParams", schema_for!(UserInputParams));
    schemas.insert("UserInputResult", schema_for!(UserInputResult));
    schemas.insert("ModelRequest", schema_for!(ModelRequest));
    schemas.insert("ModelEvent", schema_for!(ModelEvent));
    schemas.insert("ToolSpec", schema_for!(ToolSpec));
    schemas.insert("EffectSet", schema_for!(EffectSet));
    schemas.insert("PreparedInvocation", schema_for!(PreparedInvocation));
    schemas.insert("CapabilityGrant", schema_for!(CapabilityGrant));
    schemas.insert("JobSpec", schema_for!(JobSpec));
    schemas.insert("AgentSpawnSpec", schema_for!(AgentSpawnSpec));
    schemas.insert("AgentResult", schema_for!(AgentResult));
    schemas
}
