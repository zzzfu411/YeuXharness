use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use yeux_protocol::{CapabilityGrant, CapabilityMode, EffectSet};

#[derive(Clone, Debug)]
pub struct PolicyInput {
    pub host_ceiling: CapabilityGrant,
    pub user_profile: CapabilityGrant,
    pub project_trust: CapabilityGrant,
    pub turn_override: CapabilityGrant,
    pub effects: EffectSet,
    pub now: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyDecision {
    Allow {
        effective_grant: CapabilityGrant,
        approval_required: bool,
        reasons: Vec<String>,
    },
    Deny {
        effective_grant: CapabilityGrant,
        reasons: Vec<String>,
    },
}

impl PolicyDecision {
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    pub fn effective_grant(&self) -> &CapabilityGrant {
        match self {
            Self::Allow {
                effective_grant, ..
            }
            | Self::Deny {
                effective_grant, ..
            } => effective_grant,
        }
    }
}

pub trait PolicyEngine: Send + Sync {
    fn evaluate(&self, input: PolicyInput) -> PolicyDecision;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StrictPolicy;

impl PolicyEngine for StrictPolicy {
    fn evaluate(&self, input: PolicyInput) -> PolicyDecision {
        evaluate_policy(input)
    }
}

fn intersect_scopes(left: &[String], right: &[String]) -> Vec<String> {
    let left: BTreeSet<_> = left.iter().cloned().collect();
    let right: BTreeSet<_> = right.iter().cloned().collect();
    if left.contains("*") {
        return right.into_iter().collect();
    }
    if right.contains("*") {
        return left.into_iter().collect();
    }
    left.intersection(&right).cloned().collect()
}

fn intersect_two(left: &CapabilityGrant, right: &CapabilityGrant) -> CapabilityGrant {
    CapabilityGrant {
        mode: left.mode.minimum(right.mode),
        filesystem_read: intersect_scopes(&left.filesystem_read, &right.filesystem_read),
        filesystem_write: intersect_scopes(&left.filesystem_write, &right.filesystem_write),
        filesystem_delete: intersect_scopes(&left.filesystem_delete, &right.filesystem_delete),
        process: left.process && right.process,
        network: intersect_scopes(&left.network, &right.network),
        secrets: intersect_scopes(&left.secrets, &right.secrets),
        external_write: intersect_scopes(&left.external_write, &right.external_write),
        expires_at: match (left.expires_at, right.expires_at) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        },
    }
}

/// Computes `host ∩ user ∩ project ∩ turn`. Empty input fails closed.
pub fn intersect_grants(grants: &[CapabilityGrant]) -> CapabilityGrant {
    grants
        .iter()
        .cloned()
        .reduce(|left, right| intersect_two(&left, &right))
        .unwrap_or_else(CapabilityGrant::observe)
}

fn scope_allows(grants: &[String], requested: &str) -> bool {
    grants
        .iter()
        .any(|grant| grant == "*" || grant == requested)
}

fn network_scope(scheme: &str, host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

pub fn evaluate_policy(input: PolicyInput) -> PolicyDecision {
    let effective = intersect_grants(&[
        input.host_ceiling,
        input.user_profile,
        input.project_trust,
        input.turn_override,
    ]);
    let mut denied = Vec::new();

    if effective
        .expires_at
        .is_some_and(|expiry| expiry <= input.now)
    {
        denied.push("effective capability grant is expired".to_owned());
    }

    for scope in input
        .effects
        .filesystem_read
        .iter()
        .chain(input.effects.filesystem_write.iter())
        .chain(input.effects.filesystem_delete.iter())
    {
        if !scope.resolved {
            denied.push(format!("filesystem scope is unresolved: {}", scope.path));
        }
    }

    for scope in &input.effects.filesystem_read {
        if !scope_allows(&effective.filesystem_read, &scope.path) {
            denied.push(format!(
                "filesystem read is outside the grant: {}",
                scope.path
            ));
        }
    }
    for scope in &input.effects.filesystem_write {
        if effective.mode < CapabilityMode::Build
            || !scope_allows(&effective.filesystem_write, &scope.path)
        {
            denied.push(format!(
                "filesystem write is outside the grant: {}",
                scope.path
            ));
        }
    }
    for scope in &input.effects.filesystem_delete {
        if effective.mode < CapabilityMode::Build
            || !scope_allows(&effective.filesystem_delete, &scope.path)
        {
            denied.push(format!(
                "filesystem delete is outside the grant: {}",
                scope.path
            ));
        }
    }
    if !input.effects.processes.is_empty()
        && (effective.mode < CapabilityMode::Build || !effective.process)
    {
        denied.push("process execution is not granted".to_owned());
    }
    for network in &input.effects.network {
        let requested = network_scope(&network.scheme, &network.host, network.port);
        if !scope_allows(&effective.network, &requested) {
            denied.push(format!("network access is outside the grant: {requested}"));
        }
    }
    for secret in &input.effects.secrets {
        if !scope_allows(&effective.secrets, &secret.name) {
            denied.push(format!(
                "secret access is outside the grant: {}",
                secret.name
            ));
        }
    }
    for external in &input.effects.external_writes {
        let requested = format!(
            "{}:{}:{}",
            external.system,
            external.operation,
            external.resource.as_deref().unwrap_or("*")
        );
        if effective.mode < CapabilityMode::Operate
            || !scope_allows(&effective.external_write, &requested)
        {
            denied.push(format!("external write is outside the grant: {requested}"));
        }
    }

    if !denied.is_empty() {
        return PolicyDecision::Deny {
            effective_grant: effective,
            reasons: denied,
        };
    }

    let approval_required = !input.effects.filesystem_write.is_empty()
        || !input.effects.filesystem_delete.is_empty()
        || !input.effects.processes.is_empty()
        || !input.effects.network.is_empty()
        || !input.effects.secrets.is_empty()
        || !input.effects.external_writes.is_empty();
    let reasons = if approval_required {
        vec!["the invocation requests a side effect or privileged capability".to_owned()]
    } else {
        vec!["all requested effects are structured and read-only".to_owned()]
    };

    PolicyDecision::Allow {
        effective_grant: effective,
        approval_required,
        reasons,
    }
}
