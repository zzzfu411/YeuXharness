use crate::digest_serializable;
use chrono::{DateTime, Utc};
use thiserror::Error;
use yeux_protocol::{CapabilityMode, PreparedInvocation};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ApprovalError {
    #[error("the prepared invocation has expired")]
    PreparationExpired,
    #[error("the prepared token is empty")]
    EmptyPreparedToken,
    #[error("the invocation has no approval binding")]
    MissingApproval,
    #[error("the approval has expired")]
    ApprovalExpired,
    #[error("approval binding mismatch: {0}")]
    BindingMismatch(&'static str),
    #[error("cannot compute a security digest: {0}")]
    Digest(String),
}

/// Verifies that an approval still authorizes exactly this prepared invocation.
/// Call immediately before the sandboxed executor starts.
pub fn validate_approval(
    invocation: &PreparedInvocation,
    expected_mode: CapabilityMode,
    now: DateTime<Utc>,
) -> Result<(), ApprovalError> {
    if invocation.expires_at <= now {
        return Err(ApprovalError::PreparationExpired);
    }
    if invocation.prepared_token.is_empty() {
        return Err(ApprovalError::EmptyPreparedToken);
    }
    let approval = invocation
        .approval
        .as_ref()
        .ok_or(ApprovalError::MissingApproval)?;
    if approval.expires_at <= now {
        return Err(ApprovalError::ApprovalExpired);
    }

    macro_rules! require_equal {
        ($left:expr, $right:expr, $name:literal) => {
            if $left != $right {
                return Err(ApprovalError::BindingMismatch($name));
            }
        };
    }
    require_equal!(
        approval.workspace_id,
        invocation.workspace_id,
        "workspace_id"
    );
    require_equal!(
        approval.workspace_identity_digest,
        invocation.workspace_identity_digest,
        "workspace_identity_digest"
    );
    require_equal!(approval.thread_id, invocation.thread_id, "thread_id");
    require_equal!(
        approval.invocation_id,
        invocation.invocation_id,
        "invocation_id"
    );
    require_equal!(approval.turn_id, invocation.turn_id, "turn_id");
    require_equal!(approval.agent_id, invocation.agent_id, "agent_id");
    require_equal!(approval.mode, expected_mode, "mode");
    require_equal!(approval.tool_id, invocation.tool_id, "tool_id");
    require_equal!(
        approval.tool_version,
        invocation.tool_version,
        "tool_version"
    );
    require_equal!(
        approval.normalized_arguments_digest,
        invocation.normalized_arguments_digest,
        "normalized_arguments_digest"
    );
    require_equal!(
        approval.effect_digest,
        invocation.effect_digest,
        "effect_digest"
    );

    let arguments_digest = digest_serializable(&invocation.normalized_arguments)
        .map_err(|error| ApprovalError::Digest(error.to_string()))?;
    require_equal!(
        arguments_digest,
        invocation.normalized_arguments_digest,
        "normalized_arguments_content"
    );
    let effect_digest = digest_serializable(&invocation.effects)
        .map_err(|error| ApprovalError::Digest(error.to_string()))?;
    require_equal!(effect_digest, invocation.effect_digest, "effect_content");
    let approved_effect_digest = digest_serializable(&approval.granted_effects)
        .map_err(|error| ApprovalError::Digest(error.to_string()))?;
    require_equal!(
        approved_effect_digest,
        invocation.effect_digest,
        "granted_effects"
    );
    Ok(())
}
