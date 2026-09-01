use thiserror::Error;
use yeux_protocol::{Idempotency, InvocationId, InvocationReconciliationOutcome, InvocationState};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDisposition {
    ResumePreparation,
    RetryWithSameIdempotencyKey,
    ReconcileOnly,
    Terminal,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InvocationError {
    #[error("invalid invocation transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: InvocationState,
        to: InvocationState,
    },
}

pub fn can_transition_invocation(from: InvocationState, to: InvocationState) -> bool {
    // The state-only helper cannot prove that a retry is safe, so it uses the
    // fail-closed non-idempotent contract. Callers that possess the persisted
    // idempotency classification must use the explicit helper below.
    can_transition_invocation_with_idempotency(from, to, Idempotency::NonIdempotent)
}

/// Validates an invocation transition with the idempotency contract required
/// for recovery from an unknown outcome.
///
/// `Unknown -> Started` is an explicit retry and is therefore available only
/// to idempotent invocations. Resolving an unknown outcome is deliberately not
/// an ordinary transition; callers must use [`can_reconcile_invocation`] and
/// [`InvocationMachine::reconcile`] so recovery cannot be confused with a
/// fresh execution result.
pub fn can_transition_invocation_with_idempotency(
    from: InvocationState,
    to: InvocationState,
    idempotency: Idempotency,
) -> bool {
    if from == to || from.is_terminal() {
        return false;
    }
    let ordinary = matches!(
        (from, to),
        (InvocationState::Proposed, InvocationState::Approved)
            | (InvocationState::Approved, InvocationState::Prepared)
            | (InvocationState::Prepared, InvocationState::Started)
            | (InvocationState::Started, InvocationState::Completed)
            | (InvocationState::Started, InvocationState::Failed)
            | (InvocationState::Started, InvocationState::Cancelled)
            | (InvocationState::Started, InvocationState::Unknown)
            | (InvocationState::Proposed, InvocationState::Failed)
            | (InvocationState::Proposed, InvocationState::Cancelled)
            | (InvocationState::Approved, InvocationState::Failed)
            | (InvocationState::Approved, InvocationState::Cancelled)
            | (InvocationState::Prepared, InvocationState::Failed)
            | (InvocationState::Prepared, InvocationState::Cancelled)
    );
    ordinary
        || (from == InvocationState::Unknown
            && to == InvocationState::Started
            && matches!(
                idempotency,
                Idempotency::Idempotent | Idempotency::IdempotentWithKey
            ))
}

/// Returns whether durable reconciliation evidence may resolve the current
/// invocation without executing it again.
pub fn can_reconcile_invocation(
    from: InvocationState,
    outcome: InvocationReconciliationOutcome,
) -> bool {
    from == InvocationState::Unknown
        && matches!(
            outcome,
            InvocationReconciliationOutcome::Completed | InvocationReconciliationOutcome::Failed
        )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationMachine {
    id: InvocationId,
    state: InvocationState,
    idempotency: Idempotency,
}

impl InvocationMachine {
    pub fn proposed(id: InvocationId, idempotency: Idempotency) -> Self {
        Self {
            id,
            state: InvocationState::Proposed,
            idempotency,
        }
    }

    pub fn from_parts(id: InvocationId, state: InvocationState, idempotency: Idempotency) -> Self {
        Self {
            id,
            state,
            idempotency,
        }
    }

    pub fn id(&self) -> InvocationId {
        self.id
    }

    pub fn state(&self) -> InvocationState {
        self.state
    }

    pub fn transition(&mut self, to: InvocationState) -> Result<(), InvocationError> {
        let from = self.state;
        if !can_transition_invocation_with_idempotency(from, to, self.idempotency) {
            return Err(InvocationError::InvalidTransition { from, to });
        }
        self.state = to;
        Ok(())
    }

    /// Resolve an unknown outcome from external evidence without retrying the
    /// invocation. The caller must persist the corresponding reconciliation
    /// evidence alongside the state change.
    pub fn reconcile(
        &mut self,
        outcome: InvocationReconciliationOutcome,
    ) -> Result<(), InvocationError> {
        let from = self.state;
        let to = outcome.state();
        if !can_reconcile_invocation(from, outcome) {
            return Err(InvocationError::InvalidTransition { from, to });
        }
        self.state = to;
        Ok(())
    }

    pub fn recovery_disposition(&self) -> RecoveryDisposition {
        match self.state {
            InvocationState::Proposed | InvocationState::Approved | InvocationState::Prepared => {
                RecoveryDisposition::ResumePreparation
            }
            InvocationState::Started | InvocationState::Unknown => match self.idempotency {
                Idempotency::Idempotent | Idempotency::IdempotentWithKey => {
                    RecoveryDisposition::RetryWithSameIdempotencyKey
                }
                Idempotency::NonIdempotent | Idempotency::Unknown => {
                    RecoveryDisposition::ReconcileOnly
                }
            },
            InvocationState::Completed | InvocationState::Failed | InvocationState::Cancelled => {
                RecoveryDisposition::Terminal
            }
        }
    }
}
