use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use uuid::Uuid;
use yeux_core::*;
use yeux_protocol::*;

fn raw_id(number: u64) -> Uuid {
    Uuid::parse_str(&format!("01890f9d-0000-7000-8000-{number:012x}")).unwrap()
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-30T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn turn() -> Turn {
    Turn {
        id: TurnId::from_uuid(raw_id(3)),
        thread_id: ThreadId::from_uuid(raw_id(2)),
        agent_id: AgentId::from("root"),
        state: TurnState::Accepted,
        started_at: at(),
        ended_at: None,
        failure: None,
    }
}

#[test]
fn injected_ids_remain_v7_and_deterministic() {
    let values = [raw_id(1), raw_id(2)];
    let factory = IdFactory::new(SequenceIdGenerator::new(values).unwrap());
    assert_eq!(factory.thread().unwrap().into_uuid(), values[0]);
    assert_eq!(factory.turn().unwrap().into_uuid(), values[1]);
    assert_eq!(factory.event().unwrap_err(), IdError::Exhausted);
}

#[test]
fn turn_machine_accepts_normal_flow_and_rejects_state_skips() {
    let mut machine = AgentTurnMachine::new(turn());
    machine.apply(TurnAction::BeginContext, at()).unwrap();
    let error = machine.apply(TurnAction::ModelStreaming, at()).unwrap_err();
    assert_eq!(
        error,
        TurnError::InvalidTransition {
            from: TurnState::BuildingContext,
            to: TurnState::Streaming,
        }
    );
    machine.apply(TurnAction::ContextBuilt, at()).unwrap();
    machine.apply(TurnAction::ModelStreaming, at()).unwrap();
    machine.apply(TurnAction::Complete, at()).unwrap();
    assert_eq!(machine.turn().state, TurnState::Completed);
    assert_eq!(machine.turn().ended_at, Some(at()));
}

#[test]
fn steering_is_ordered_and_interrupt_finishes_explicitly() {
    let mut machine = AgentTurnMachine::new(turn());
    machine.steer("first").unwrap();
    machine.steer("second").unwrap();
    assert_eq!(machine.take_steering().as_deref(), Some("first"));
    machine.apply(TurnAction::Interrupt, at()).unwrap();
    machine
        .apply(TurnAction::CancellationFinished, at())
        .unwrap();
    assert_eq!(machine.turn().state, TurnState::Cancelled);
    assert!(machine.steer("too late").is_err());
}

#[test]
fn unknown_non_idempotent_invocation_never_auto_retries() {
    let invocation_id = InvocationId::from_uuid(raw_id(4));
    let mut machine = InvocationMachine::proposed(invocation_id, Idempotency::NonIdempotent);
    for state in [
        InvocationState::Approved,
        InvocationState::Prepared,
        InvocationState::Started,
        InvocationState::Unknown,
    ] {
        machine.transition(state).unwrap();
    }
    assert_eq!(
        machine.recovery_disposition(),
        RecoveryDisposition::ReconcileOnly
    );
    assert!(machine.transition(InvocationState::Completed).is_err());
}

fn grant(mode: CapabilityMode) -> CapabilityGrant {
    CapabilityGrant {
        mode,
        filesystem_read: vec!["/workspace".to_owned()],
        filesystem_write: vec!["/workspace/src/lib.rs".to_owned()],
        filesystem_delete: Vec::new(),
        process: true,
        network: vec!["https://api.example.com:443".to_owned()],
        secrets: vec!["provider-key".to_owned()],
        external_write: vec!["github:create_issue:repo".to_owned()],
        expires_at: Some(at() + Duration::hours(1)),
    }
}

#[test]
fn policy_intersection_never_inherits_a_parent_only_capability() {
    let mut turn_grant = grant(CapabilityMode::Build);
    turn_grant.process = false;
    let decision = evaluate_policy(PolicyInput {
        host_ceiling: grant(CapabilityMode::Operate),
        user_profile: grant(CapabilityMode::Operate),
        project_trust: grant(CapabilityMode::Build),
        turn_override: turn_grant,
        effects: EffectSet {
            processes: vec![ProcessEffect {
                executable: "cargo".to_owned(),
                argument_digest: None,
                may_spawn_children: true,
            }],
            ..EffectSet::default()
        },
        now: at(),
    });
    assert!(matches!(decision, PolicyDecision::Deny { .. }));
    assert_eq!(decision.effective_grant().mode, CapabilityMode::Build);
}

fn approved_invocation() -> PreparedInvocation {
    let effects = EffectSet {
        filesystem_write: vec![PathScope {
            path: "/workspace/src/lib.rs".to_owned(),
            recursive: false,
            resolved: true,
        }],
        idempotency: Idempotency::IdempotentWithKey,
        ..EffectSet::default()
    };
    let arguments = json!({"patch": "safe", "path": "src/lib.rs"});
    let effect_digest = digest_serializable(&effects).unwrap();
    let arguments_digest = digest_serializable(&arguments).unwrap();
    let workspace_id = WorkspaceId::from_uuid(raw_id(1));
    let thread_id = ThreadId::from_uuid(raw_id(2));
    let agent_id = AgentId::from("root");
    PreparedInvocation {
        invocation_id: InvocationId::from_uuid(raw_id(4)),
        tool_id: "workspace.apply_patch".to_owned(),
        tool_version: "1.0.0".to_owned(),
        workspace_id,
        workspace_identity_digest: "workspace-digest".to_owned(),
        thread_id,
        turn_id: TurnId::from_uuid(raw_id(3)),
        agent_id: agent_id.clone(),
        normalized_arguments: arguments,
        normalized_arguments_digest: arguments_digest.clone(),
        effects: effects.clone(),
        effect_digest: effect_digest.clone(),
        prepared_token: "opaque-token".to_owned(),
        prepared_at: at(),
        expires_at: at() + Duration::minutes(10),
        approval: Some(ApprovalBinding {
            approval_id: ApprovalId::from_uuid(raw_id(5)),
            workspace_id,
            workspace_identity_digest: "workspace-digest".to_owned(),
            thread_id,
            agent_id,
            mode: CapabilityMode::Build,
            tool_id: "workspace.apply_patch".to_owned(),
            tool_version: "1.0.0".to_owned(),
            normalized_arguments_digest: arguments_digest,
            effect_digest,
            granted_effects: effects,
            expires_at: at() + Duration::minutes(5),
        }),
    }
}

#[test]
fn approval_is_invalidated_by_argument_changes() {
    let mut invocation = approved_invocation();
    assert!(validate_approval(&invocation, CapabilityMode::Build, at()).is_ok());
    invocation.normalized_arguments["patch"] = json!("different");
    assert_eq!(
        validate_approval(&invocation, CapabilityMode::Build, at()),
        Err(ApprovalError::BindingMismatch(
            "normalized_arguments_content"
        ))
    );
}

fn envelope(seq: u64, event: Event, turn_id: Option<TurnId>) -> EventEnvelope {
    EventEnvelope::new(
        PROTOCOL_VERSION,
        EventId::from_uuid(raw_id(100 + seq)),
        ThreadId::from_uuid(raw_id(2)),
        turn_id,
        AgentId::from("root"),
        seq,
        at() + Duration::seconds(seq as i64),
        None,
        event,
    )
}

fn trace() -> Vec<EventEnvelope> {
    let workspace_id = WorkspaceId::from_uuid(raw_id(1));
    let thread_id = ThreadId::from_uuid(raw_id(2));
    let turn = turn();
    vec![
        envelope(
            1,
            Event::WorkspaceOpened {
                workspace: Workspace {
                    id: workspace_id,
                    root: "/workspace".to_owned(),
                    identity: WorkspaceIdentity {
                        canonical_root: "/workspace".to_owned(),
                        digest: "workspace-digest".to_owned(),
                        device: None,
                        inode: None,
                        git_common_dir: None,
                    },
                    trust: WorkspaceTrust::Trusted,
                    opened_at: at(),
                },
            },
            None,
        ),
        envelope(
            2,
            Event::ThreadStarted {
                thread: Thread {
                    id: thread_id,
                    workspace_id,
                    parent_thread_id: None,
                    parent_seq: None,
                    title: Some("test".to_owned()),
                    status: ThreadStatus::Idle,
                    created_at: at(),
                    updated_at: at(),
                    last_seq: 0,
                },
            },
            None,
        ),
        envelope(3, Event::TurnStarted { turn: turn.clone() }, Some(turn.id)),
        envelope(
            4,
            Event::TurnStateChanged {
                turn_id: turn.id,
                from: TurnState::Accepted,
                to: TurnState::BuildingContext,
                reason: None,
            },
            Some(turn.id),
        ),
        envelope(
            5,
            Event::TurnStateChanged {
                turn_id: turn.id,
                from: TurnState::BuildingContext,
                to: TurnState::RequestingModel,
                reason: None,
            },
            Some(turn.id),
        ),
        envelope(
            6,
            Event::TurnStateChanged {
                turn_id: turn.id,
                from: TurnState::RequestingModel,
                to: TurnState::Streaming,
                reason: None,
            },
            Some(turn.id),
        ),
        envelope(
            7,
            Event::TurnStateChanged {
                turn_id: turn.id,
                from: TurnState::Streaming,
                to: TurnState::Completed,
                reason: None,
            },
            Some(turn.id),
        ),
    ]
}

#[test]
fn replay_is_a_deterministic_pure_projection() {
    let events = trace();
    let first = replay(events.iter()).unwrap();
    let second = replay(events.iter()).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first.turns[&TurnId::from_uuid(raw_id(3))].state,
        TurnState::Completed
    );
    assert_eq!(first.last_seq_by_thread[&ThreadId::from_uuid(raw_id(2))], 7);
}

#[test]
fn replay_rejects_sequence_gaps_before_mutating_projection() {
    let mut events = trace();
    events[1].seq = 3;
    let error = replay(events.iter()).unwrap_err();
    assert!(matches!(
        error,
        ReplayError::SequenceGap {
            expected: 2,
            actual: 3,
            ..
        }
    ));
}
