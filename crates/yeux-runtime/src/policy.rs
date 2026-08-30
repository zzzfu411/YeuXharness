//! Runtime policy evaluation at the side-effect boundary.

use std::{collections::BTreeSet, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use yeux_protocol::{
    AgentId, ApprovalBinding, CapabilityGrant, CapabilityMode, EffectSet, ThreadId, WorkspaceId,
};

#[derive(Debug, Clone)]
pub struct PolicyRequest {
    pub effects: EffectSet,
    pub host_ceiling: CapabilityGrant,
    pub user_profile: CapabilityGrant,
    pub project_trust: CapabilityGrant,
    pub turn_override: CapabilityGrant,
    pub workspace_id: WorkspaceId,
    pub workspace_identity_digest: String,
    pub thread_id: ThreadId,
    pub agent_id: AgentId,
    pub tool_id: String,
    pub tool_version: String,
    pub normalized_arguments_digest: String,
    pub effect_digest: String,
    pub approval: Option<ApprovalBinding>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow {
        effective_grant: CapabilityGrant,
    },
    Ask {
        effective_grant: CapabilityGrant,
        effect_digest: String,
        reason: String,
    },
    Deny {
        reason: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct PolicyEvaluator;

impl PolicyEvaluator {
    pub fn evaluate(&self, request: &PolicyRequest) -> PolicyDecision {
        let effective = intersect_all([
            &request.host_ceiling,
            &request.user_profile,
            &request.project_trust,
            &request.turn_override,
        ]);

        if let Some(expiry) = effective.expires_at {
            if expiry <= request.now {
                return PolicyDecision::Deny {
                    reason: "effective capability grant has expired".into(),
                };
            }
        }
        if request
            .effects
            .filesystem_read
            .iter()
            .chain(&request.effects.filesystem_write)
            .chain(&request.effects.filesystem_delete)
            .any(|scope| !scope.resolved)
        {
            return PolicyDecision::Deny {
                reason: "unresolved filesystem scope cannot be authorized".into(),
            };
        }
        if effective.mode == CapabilityMode::Observe && !request.effects.is_read_only() {
            return PolicyDecision::Deny {
                reason: "observe mode is technically read-only".into(),
            };
        }
        if effective.mode != CapabilityMode::Operate && !request.effects.external_writes.is_empty()
        {
            return PolicyDecision::Deny {
                reason: "external writes require operate mode".into(),
            };
        }
        if let Some(reason) = exceeds_grant(&request.effects, &effective) {
            return PolicyDecision::Deny { reason };
        }

        if request.effects.is_read_only() {
            return PolicyDecision::Allow {
                effective_grant: effective,
            };
        }

        match request.approval.as_ref() {
            Some(approval) if approval_matches(request, approval) => PolicyDecision::Allow {
                effective_grant: effective,
            },
            Some(_) => PolicyDecision::Ask {
                effective_grant: effective,
                effect_digest: request.effect_digest.clone(),
                reason: "approval does not bind the exact prepared invocation".into(),
            },
            None => PolicyDecision::Ask {
                effective_grant: effective,
                effect_digest: request.effect_digest.clone(),
                reason: "side effects require an invocation-bound approval".into(),
            },
        }
    }
}

fn intersect_all<'a>(grants: impl IntoIterator<Item = &'a CapabilityGrant>) -> CapabilityGrant {
    let mut grants = grants.into_iter();
    let first = grants
        .next()
        .cloned()
        .unwrap_or_else(CapabilityGrant::observe);
    grants.fold(first, intersect_two)
}

fn intersect_two(mut left: CapabilityGrant, right: &CapabilityGrant) -> CapabilityGrant {
    left.mode = left.mode.minimum(right.mode);
    left.filesystem_read = intersect_strings(&left.filesystem_read, &right.filesystem_read);
    left.filesystem_write = intersect_strings(&left.filesystem_write, &right.filesystem_write);
    left.filesystem_delete = intersect_strings(&left.filesystem_delete, &right.filesystem_delete);
    left.process &= right.process;
    left.network = intersect_strings(&left.network, &right.network);
    left.secrets = intersect_strings(&left.secrets, &right.secrets);
    left.external_write = intersect_strings(&left.external_write, &right.external_write);
    left.expires_at = match (left.expires_at, right.expires_at) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    left
}

fn intersect_strings(left: &[String], right: &[String]) -> Vec<String> {
    if left.iter().any(|value| value == "*") {
        return sorted_unique(right);
    }
    if right.iter().any(|value| value == "*") {
        return sorted_unique(left);
    }
    let right: BTreeSet<_> = right.iter().collect();
    let mut values: Vec<_> = left
        .iter()
        .filter(|value| right.contains(value))
        .cloned()
        .collect();
    values.sort();
    values.dedup();
    values
}

fn sorted_unique(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

fn exceeds_grant(effects: &EffectSet, grant: &CapabilityGrant) -> Option<String> {
    for scope in &effects.filesystem_read {
        if !path_allowed(&scope.path, &grant.filesystem_read) {
            return Some(format!(
                "filesystem read is outside effective grant: {}",
                scope.path
            ));
        }
    }
    for scope in &effects.filesystem_write {
        if !path_allowed(&scope.path, &grant.filesystem_write) {
            return Some(format!(
                "filesystem write is outside effective grant: {}",
                scope.path
            ));
        }
    }
    for scope in &effects.filesystem_delete {
        if !path_allowed(&scope.path, &grant.filesystem_delete) {
            return Some(format!(
                "filesystem delete is outside effective grant: {}",
                scope.path
            ));
        }
    }
    if !effects.processes.is_empty() && !grant.process {
        return Some("process execution is outside effective grant".into());
    }
    for network in &effects.network {
        let authority = match network.port {
            Some(port) => format!("{}://{}:{port}", network.scheme, network.host),
            None => format!("{}://{}", network.scheme, network.host),
        };
        if !string_allowed(&authority, &grant.network)
            && !string_allowed(&network.host, &grant.network)
        {
            return Some(format!(
                "network endpoint is outside effective grant: {authority}"
            ));
        }
    }
    for secret in &effects.secrets {
        if !string_allowed(&secret.name, &grant.secrets) {
            return Some(format!(
                "secret is outside effective grant: {}",
                secret.name
            ));
        }
    }
    for external in &effects.external_writes {
        let operation = format!("{}:{}", external.system, external.operation);
        if !string_allowed(&operation, &grant.external_write) {
            return Some(format!(
                "external write is outside effective grant: {operation}"
            ));
        }
    }
    None
}

fn path_allowed(candidate: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|scope| scope == "*" || Path::new(candidate).starts_with(Path::new(scope)))
}

fn string_allowed(candidate: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|value| value == "*" || value == candidate)
}

fn approval_matches(request: &PolicyRequest, approval: &ApprovalBinding) -> bool {
    approval.expires_at > request.now
        && approval.workspace_id == request.workspace_id
        && approval.workspace_identity_digest == request.workspace_identity_digest
        && approval.thread_id == request.thread_id
        && approval.agent_id == request.agent_id
        && approval.mode
            == request
                .host_ceiling
                .mode
                .minimum(request.user_profile.mode)
                .minimum(request.project_trust.mode)
                .minimum(request.turn_override.mode)
        && approval.tool_id == request.tool_id
        && approval.tool_version == request.tool_version
        && approval.normalized_arguments_digest == request.normalized_arguments_digest
        && approval.effect_digest == request.effect_digest
        && approval.granted_effects == request.effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use uuid::Uuid;
    use yeux_protocol::{ApprovalId, Idempotency, PathScope, Reversibility, SecretHandle};

    fn grant(mode: CapabilityMode) -> CapabilityGrant {
        CapabilityGrant {
            mode,
            filesystem_read: vec!["/workspace".into()],
            filesystem_write: vec!["/workspace".into()],
            filesystem_delete: vec![],
            process: true,
            network: vec!["api.example.test".into()],
            secrets: vec!["provider-key".into()],
            external_write: vec![],
            expires_at: None,
        }
    }

    fn request(effects: EffectSet, mode: CapabilityMode) -> PolicyRequest {
        let workspace_id = WorkspaceId::from_uuid(Uuid::now_v7());
        let thread_id = ThreadId::from_uuid(Uuid::now_v7());
        PolicyRequest {
            effects,
            host_ceiling: grant(mode),
            user_profile: grant(mode),
            project_trust: grant(mode),
            turn_override: grant(mode),
            workspace_id,
            workspace_identity_digest: "workspace".into(),
            thread_id,
            agent_id: AgentId::new("root"),
            tool_id: "workspace.apply_patch".into(),
            tool_version: "1".into(),
            normalized_arguments_digest: "args".into(),
            effect_digest: "effects".into(),
            approval: None,
            now: Utc::now(),
        }
    }

    #[test]
    fn observe_mode_denies_mutation_even_if_scope_is_listed() {
        let effects = EffectSet {
            filesystem_write: vec![PathScope {
                path: "/workspace/a".into(),
                recursive: false,
                resolved: true,
            }],
            idempotency: Idempotency::Idempotent,
            reversibility: Reversibility::Reversible,
            ..EffectSet::default()
        };
        assert!(matches!(
            PolicyEvaluator.evaluate(&request(effects, CapabilityMode::Observe)),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn unresolved_path_fails_closed() {
        let effects = EffectSet {
            filesystem_read: vec![PathScope {
                path: "/workspace/a".into(),
                recursive: false,
                resolved: false,
            }],
            ..EffectSet::default()
        };
        assert!(matches!(
            PolicyEvaluator.evaluate(&request(effects, CapabilityMode::Build)),
            PolicyDecision::Deny { .. }
        ));
    }

    #[test]
    fn exact_approval_allows_side_effect() {
        let effects = EffectSet {
            filesystem_write: vec![PathScope {
                path: "/workspace/a".into(),
                recursive: false,
                resolved: true,
            }],
            ..EffectSet::default()
        };
        let mut request = request(effects.clone(), CapabilityMode::Build);
        request.approval = Some(ApprovalBinding {
            approval_id: ApprovalId::from_uuid(Uuid::now_v7()),
            workspace_id: request.workspace_id,
            workspace_identity_digest: request.workspace_identity_digest.clone(),
            thread_id: request.thread_id,
            agent_id: request.agent_id.clone(),
            mode: CapabilityMode::Build,
            tool_id: request.tool_id.clone(),
            tool_version: request.tool_version.clone(),
            normalized_arguments_digest: request.normalized_arguments_digest.clone(),
            effect_digest: request.effect_digest.clone(),
            granted_effects: effects,
            expires_at: request.now + Duration::minutes(1),
        });
        assert!(matches!(
            PolicyEvaluator.evaluate(&request),
            PolicyDecision::Allow { .. }
        ));
    }

    #[test]
    fn changed_digest_invalidates_approval() {
        let effects = EffectSet {
            secrets: vec![SecretHandle {
                name: "provider-key".into(),
            }],
            ..EffectSet::default()
        };
        let mut request = request(effects.clone(), CapabilityMode::Build);
        request.approval = Some(ApprovalBinding {
            approval_id: ApprovalId::from_uuid(Uuid::now_v7()),
            workspace_id: request.workspace_id,
            workspace_identity_digest: request.workspace_identity_digest.clone(),
            thread_id: request.thread_id,
            agent_id: request.agent_id.clone(),
            mode: CapabilityMode::Build,
            tool_id: request.tool_id.clone(),
            tool_version: request.tool_version.clone(),
            normalized_arguments_digest: request.normalized_arguments_digest.clone(),
            effect_digest: "old".into(),
            granted_effects: effects,
            expires_at: request.now + Duration::minutes(1),
        });
        assert!(matches!(
            PolicyEvaluator.evaluate(&request),
            PolicyDecision::Ask { .. }
        ));
    }
}
