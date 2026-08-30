use thiserror::Error;
use yeux_protocol::{Idempotency, InvocationId, InvocationState};

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
    if from == to || from.is_terminal() {
        return false;
    }
    matches!(
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
        if !can_transition_invocation(from, to) {
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
