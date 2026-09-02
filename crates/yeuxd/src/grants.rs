//! Capability-layer construction for the daemon-owned invocation pipeline.
//!
//! The policy engine computes the actual intersection. This module only maps
//! daemon configuration and durable workspace/turn facts into the four grants
//! supplied to that engine. A missing optional layer is represented by an
//! identity grant, never by `observe`, so absence cannot accidentally collapse
//! every requested capability.

use yeux_protocol::{CapabilityGrant, CapabilityMode, WorkspaceTrust};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GrantLayers {
    pub host_ceiling: CapabilityGrant,
    pub user_profile: CapabilityGrant,
    pub project_trust: CapabilityGrant,
    pub turn_override: CapabilityGrant,
}

pub(crate) fn resolve_grant_layers(
    host_mode: CapabilityMode,
    workspace_trust: WorkspaceTrust,
    user_profile: Option<&CapabilityGrant>,
    turn_override: Option<&CapabilityGrant>,
) -> GrantLayers {
    GrantLayers {
        host_ceiling: host_ceiling(host_mode),
        user_profile: user_profile.cloned().unwrap_or_else(identity_grant),
        project_trust: project_grant(workspace_trust),
        turn_override: turn_override.cloned().unwrap_or_else(identity_grant),
    }
}

fn host_ceiling(mode: CapabilityMode) -> CapabilityGrant {
    match mode {
        CapabilityMode::Observe => CapabilityGrant {
            mode,
            filesystem_read: vec!["*".into()],
            filesystem_write: Vec::new(),
            filesystem_delete: Vec::new(),
            process: false,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        },
        CapabilityMode::Build | CapabilityMode::Operate => CapabilityGrant {
            mode,
            filesystem_read: vec!["*".into()],
            filesystem_write: vec!["*".into()],
            filesystem_delete: vec!["*".into()],
            process: true,
            // P1 does not infer endpoint, secret, or external-write authority
            // from a coarse mode flag. Those capabilities remain unavailable
            // until an explicit profile supplies scopes and the corresponding
            // broker/proxy is implemented.
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        },
    }
}

fn project_grant(trust: WorkspaceTrust) -> CapabilityGrant {
    match trust {
        WorkspaceTrust::Untrusted => CapabilityGrant {
            mode: CapabilityMode::Observe,
            filesystem_read: vec!["*".into()],
            filesystem_write: Vec::new(),
            filesystem_delete: Vec::new(),
            process: false,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        },
        WorkspaceTrust::Trusted => CapabilityGrant {
            mode: CapabilityMode::Build,
            filesystem_read: vec!["*".into()],
            filesystem_write: vec!["*".into()],
            filesystem_delete: vec!["*".into()],
            process: true,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        },
    }
}

fn identity_grant() -> CapabilityGrant {
    CapabilityGrant {
        mode: CapabilityMode::Operate,
        filesystem_read: vec!["*".into()],
        filesystem_write: vec!["*".into()],
        filesystem_delete: vec!["*".into()],
        process: true,
        network: vec!["*".into()],
        secrets: vec!["*".into()],
        external_write: vec!["*".into()],
        expires_at: None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use yeux_core::{evaluate_policy, PolicyDecision, PolicyInput};
    use yeux_protocol::{EffectSet, PathScope};

    use super::*;

    fn write_effect() -> EffectSet {
        EffectSet {
            filesystem_write: vec![PathScope {
                path: "src/lib.rs".into(),
                recursive: false,
                resolved: true,
            }],
            ..EffectSet::default()
        }
    }

    #[test]
    fn missing_optional_layers_are_identity_not_observe() {
        let layers =
            resolve_grant_layers(CapabilityMode::Build, WorkspaceTrust::Trusted, None, None);
        let decision = evaluate_policy(PolicyInput {
            host_ceiling: layers.host_ceiling,
            user_profile: layers.user_profile,
            project_trust: layers.project_trust,
            turn_override: layers.turn_override,
            effects: write_effect(),
            now: Utc::now(),
        });
        assert!(matches!(
            decision,
            PolicyDecision::Allow {
                approval_required: true,
                ..
            }
        ));
    }

    #[test]
    fn untrusted_workspace_forces_observe_and_disables_process() {
        let layers = resolve_grant_layers(
            CapabilityMode::Operate,
            WorkspaceTrust::Untrusted,
            None,
            None,
        );
        assert_eq!(layers.project_trust.mode, CapabilityMode::Observe);
        assert!(!layers.project_trust.process);
        let decision = evaluate_policy(PolicyInput {
            host_ceiling: layers.host_ceiling,
            user_profile: layers.user_profile,
            project_trust: layers.project_trust,
            turn_override: layers.turn_override,
            effects: write_effect(),
            now: Utc::now(),
        });
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[test]
    fn explicit_turn_override_can_only_narrow() {
        let override_grant = CapabilityGrant {
            mode: CapabilityMode::Observe,
            filesystem_read: vec!["src".into()],
            filesystem_write: Vec::new(),
            filesystem_delete: Vec::new(),
            process: false,
            network: Vec::new(),
            secrets: Vec::new(),
            external_write: Vec::new(),
            expires_at: None,
        };
        let layers = resolve_grant_layers(
            CapabilityMode::Operate,
            WorkspaceTrust::Trusted,
            None,
            Some(&override_grant),
        );
        let decision = evaluate_policy(PolicyInput {
            host_ceiling: layers.host_ceiling,
            user_profile: layers.user_profile,
            project_trust: layers.project_trust,
            turn_override: layers.turn_override,
            effects: write_effect(),
            now: Utc::now(),
        });
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[test]
    fn expired_user_profile_fails_closed() {
        let mut profile = identity_grant();
        profile.expires_at = Some(Utc::now() - Duration::seconds(1));
        let layers = resolve_grant_layers(
            CapabilityMode::Build,
            WorkspaceTrust::Trusted,
            Some(&profile),
            None,
        );
        let decision = evaluate_policy(PolicyInput {
            host_ceiling: layers.host_ceiling,
            user_profile: layers.user_profile,
            project_trust: layers.project_trust,
            turn_override: layers.turn_override,
            effects: EffectSet::default(),
            now: Utc::now(),
        });
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }
}
