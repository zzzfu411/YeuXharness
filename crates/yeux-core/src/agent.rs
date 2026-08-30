use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use thiserror::Error;
use yeux_protocol::{Turn, TurnState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnAction {
    BeginContext,
    ContextBuilt,
    ModelAccepted,
    ModelStreaming,
    ToolsProposed,
    ApprovalRequired,
    ApprovalGranted,
    Scheduled,
    ExecutionStarted,
    ResultsIntegrated { continue_model: bool },
    InputRequired,
    InputReceived,
    Complete,
    Interrupt,
    CancellationFinished,
    Fail(String),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TurnError {
    #[error("invalid turn transition from {from:?} to {to:?}")]
    InvalidTransition { from: TurnState, to: TurnState },
    #[error("cannot steer a terminal turn in state {0:?}")]
    TerminalSteering(TurnState),
}

pub fn can_transition_turn(from: TurnState, to: TurnState) -> bool {
    if from == to || from.is_terminal() {
        return false;
    }
    if matches!(to, TurnState::Cancelling | TurnState::Failed) {
        return true;
    }

    matches!(
        (from, to),
        (TurnState::Accepted, TurnState::BuildingContext)
            | (TurnState::BuildingContext, TurnState::RequestingModel)
            | (TurnState::BuildingContext, TurnState::WaitingForInput)
            | (TurnState::RequestingModel, TurnState::Streaming)
            | (TurnState::Streaming, TurnState::ProposedTools)
            | (TurnState::Streaming, TurnState::Completed)
            | (TurnState::ProposedTools, TurnState::WaitingForApproval)
            | (TurnState::ProposedTools, TurnState::Authorizing)
            | (TurnState::WaitingForApproval, TurnState::Authorizing)
            | (TurnState::Authorizing, TurnState::WaitingForApproval)
            | (TurnState::Authorizing, TurnState::Scheduling)
            | (TurnState::Scheduling, TurnState::Executing)
            | (TurnState::Executing, TurnState::IntegratingResults)
            | (TurnState::IntegratingResults, TurnState::RequestingModel)
            | (TurnState::IntegratingResults, TurnState::Completed)
            | (TurnState::IntegratingResults, TurnState::WaitingForInput)
            | (TurnState::WaitingForInput, TurnState::BuildingContext)
            | (TurnState::Cancelling, TurnState::Cancelled)
    )
}

#[derive(Clone, Debug)]
pub struct AgentTurnMachine {
    turn: Turn,
    steering: VecDeque<String>,
}

impl AgentTurnMachine {
    pub fn new(turn: Turn) -> Self {
        Self {
            turn,
            steering: VecDeque::new(),
        }
    }

    pub fn turn(&self) -> &Turn {
        &self.turn
    }

    pub fn into_turn(self) -> Turn {
        self.turn
    }

    pub fn pending_steering(&self) -> usize {
        self.steering.len()
    }

    pub fn steer(&mut self, message: impl Into<String>) -> Result<(), TurnError> {
        if self.turn.state.is_terminal() {
            return Err(TurnError::TerminalSteering(self.turn.state));
        }
        self.steering.push_back(message.into());
        Ok(())
    }

    pub fn take_steering(&mut self) -> Option<String> {
        self.steering.pop_front()
    }

    pub fn apply(&mut self, action: TurnAction, at: DateTime<Utc>) -> Result<TurnState, TurnError> {
        let target = match action {
            TurnAction::BeginContext | TurnAction::InputReceived => TurnState::BuildingContext,
            TurnAction::ContextBuilt | TurnAction::ModelAccepted => TurnState::RequestingModel,
            TurnAction::ModelStreaming => TurnState::Streaming,
            TurnAction::ToolsProposed => TurnState::ProposedTools,
            TurnAction::ApprovalRequired => TurnState::WaitingForApproval,
            TurnAction::ApprovalGranted => TurnState::Authorizing,
            TurnAction::Scheduled => TurnState::Scheduling,
            TurnAction::ExecutionStarted => TurnState::Executing,
            TurnAction::ResultsIntegrated {
                continue_model: true,
            } => TurnState::RequestingModel,
            TurnAction::ResultsIntegrated {
                continue_model: false,
            }
            | TurnAction::Complete => TurnState::Completed,
            TurnAction::InputRequired => TurnState::WaitingForInput,
            TurnAction::Interrupt => TurnState::Cancelling,
            TurnAction::CancellationFinished => TurnState::Cancelled,
            TurnAction::Fail(_) => TurnState::Failed,
        };

        self.transition(
            target,
            at,
            match action {
                TurnAction::Fail(message) => Some(message),
                _ => None,
            },
        )?;
        Ok(target)
    }

    pub fn transition(
        &mut self,
        to: TurnState,
        at: DateTime<Utc>,
        reason: Option<String>,
    ) -> Result<(), TurnError> {
        let from = self.turn.state;
        if !can_transition_turn(from, to) {
            return Err(TurnError::InvalidTransition { from, to });
        }
        self.turn.state = to;
        if to == TurnState::Failed {
            self.turn.failure = reason;
        }
        if to.is_terminal() {
            self.turn.ended_at = Some(at);
        }
        Ok(())
    }
}
