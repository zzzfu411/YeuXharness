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
fn unknown_non_idempotent_invocation_can_be_reconciled_but_not_retried() {
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
    assert!(machine.transition(InvocationState::Started).is_err());
    assert!(machine.transition(InvocationState::Completed).is_err());
    machine
        .reconcile(InvocationReconciliationOutcome::Completed)
        .unwrap();
    assert_eq!(machine.state(), InvocationState::Completed);
}

#[test]
fn reconciliation_is_only_legal_from_unknown_and_never_resolves_to_cancelled() {
    let invocation_id = InvocationId::from_uuid(raw_id(4));
    let mut machine = InvocationMachine::proposed(invocation_id, Idempotency::NonIdempotent);
    assert!(machine
        .reconcile(InvocationReconciliationOutcome::Failed)
        .is_err());
    for state in [
        InvocationState::Approved,
        InvocationState::Prepared,
        InvocationState::Started,
        InvocationState::Unknown,
    ] {
        machine.transition(state).unwrap();
    }
    machine
        .reconcile(InvocationReconciliationOutcome::Failed)
        .unwrap();
    assert_eq!(machine.state(), InvocationState::Failed);
}

#[test]
fn unknown_idempotent_invocation_requires_an_explicit_retry_transition() {
    let invocation_id = InvocationId::from_uuid(raw_id(4));
    let mut machine = InvocationMachine::proposed(invocation_id, Idempotency::IdempotentWithKey);
    for state in [
        InvocationState::Approved,
        InvocationState::Prepared,
        InvocationState::Started,
        InvocationState::Unknown,
    ] {
        machine.transition(state).unwrap();
    }
    assert!(!InvocationState::Unknown.is_terminal());
    assert_eq!(
        machine.recovery_disposition(),
        RecoveryDisposition::RetryWithSameIdempotencyKey
    );
    machine.transition(InvocationState::Started).unwrap();
    machine.transition(InvocationState::Completed).unwrap();
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
            invocation_id: InvocationId::from_uuid(raw_id(4)),
            workspace_id,
            workspace_identity_digest: "workspace-digest".to_owned(),
            thread_id,
            turn_id: TurnId::from_uuid(raw_id(3)),
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

#[test]
fn approval_is_invalidated_by_effect_digest_changes() {
    let mut invocation = approved_invocation();
    invocation.approval.as_mut().unwrap().effect_digest = "different".to_owned();

    assert_eq!(
        validate_approval(&invocation, CapabilityMode::Build, at()),
        Err(ApprovalError::BindingMismatch("effect_digest"))
    );
}

#[test]
fn approval_is_scoped_to_one_invocation_and_turn() {
    let mut invocation = approved_invocation();
    invocation.invocation_id = InvocationId::from_uuid(raw_id(6));
    assert_eq!(
        validate_approval(&invocation, CapabilityMode::Build, at()),
        Err(ApprovalError::BindingMismatch("invocation_id"))
    );

    let mut invocation = approved_invocation();
    invocation.turn_id = TurnId::from_uuid(raw_id(7));
    assert_eq!(
        validate_approval(&invocation, CapabilityMode::Build, at()),
        Err(ApprovalError::BindingMismatch("turn_id"))
    );
}

#[test]
fn approval_rejects_every_mutable_security_binding() {
    let assert_mismatch = |invocation: PreparedInvocation, field| {
        assert_eq!(
            validate_approval(&invocation, CapabilityMode::Build, at()),
            Err(ApprovalError::BindingMismatch(field))
        );
    };

    let mut invocation = approved_invocation();
    invocation.workspace_id = WorkspaceId::from_uuid(raw_id(20));
    assert_mismatch(invocation, "workspace_id");

    let mut invocation = approved_invocation();
    invocation.workspace_identity_digest = "other-workspace".into();
    assert_mismatch(invocation, "workspace_identity_digest");

    let mut invocation = approved_invocation();
    invocation.thread_id = ThreadId::from_uuid(raw_id(21));
    assert_mismatch(invocation, "thread_id");

    let mut invocation = approved_invocation();
    invocation.agent_id = AgentId::from("other-agent");
    assert_mismatch(invocation, "agent_id");

    let invocation = approved_invocation();
    assert_eq!(
        validate_approval(&invocation, CapabilityMode::Operate, at()),
        Err(ApprovalError::BindingMismatch("mode"))
    );

    let mut invocation = approved_invocation();
    invocation.tool_id = "other.tool".into();
    assert_mismatch(invocation, "tool_id");

    let mut invocation = approved_invocation();
    invocation.tool_version = "2.0.0".into();
    assert_mismatch(invocation, "tool_version");

    let mut invocation = approved_invocation();
    invocation.normalized_arguments_digest = "other-arguments".into();
    assert_mismatch(invocation, "normalized_arguments_digest");

    let mut invocation = approved_invocation();
    invocation.effect_digest = "other-effects".into();
    assert_mismatch(invocation, "effect_digest");

    let mut invocation = approved_invocation();
    invocation.approval.as_mut().unwrap().granted_effects = EffectSet::default();
    assert_mismatch(invocation, "granted_effects");
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

fn invocation_proposal(invocation_id: InvocationId) -> Event {
    let effects = EffectSet {
        idempotency: Idempotency::NonIdempotent,
        reversibility: Reversibility::Unknown,
        ..EffectSet::default()
    };
    Event::InvocationProposed {
        invocation_id,
        call_id: "provider-call-1".to_owned(),
        tool_id: "workspace.apply_patch".to_owned(),
        tool_version: "1.0.0".to_owned(),
        normalized_arguments_digest: "arguments-digest".to_owned(),
        effect_digest: digest_serializable(&effects).unwrap(),
        idempotency: effects.idempotency,
        effects,
    }
}

#[test]
fn replay_projects_complete_invocation_proposal_evidence() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));

    let projection = replay(events.iter()).unwrap();
    let invocation = &projection.invocations[&invocation_id];
    assert_eq!(invocation.turn_id, turn_id);
    assert_eq!(invocation.agent_id, AgentId::from("root"));
    assert_eq!(invocation.call_id, "provider-call-1");
    assert_eq!(invocation.tool_id, "workspace.apply_patch");
    assert_eq!(invocation.tool_version, "1.0.0");
    assert_eq!(invocation.normalized_arguments_digest, "arguments-digest");
    assert_eq!(invocation.idempotency, Idempotency::NonIdempotent);
    assert!(!invocation.effect_digest.is_empty());
}

#[test]
fn replay_rejects_a_proposal_with_a_forged_effect_digest() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    let mut proposal = invocation_proposal(invocation_id);
    if let Event::InvocationProposed { effect_digest, .. } = &mut proposal {
        *effect_digest = "forged".into();
    }
    events.push(envelope(7, proposal, Some(turn_id)));

    assert!(matches!(
        replay(events.iter()),
        Err(ReplayError::InvocationEvidenceMismatch {
            invocation_id: found,
            field: "effect_digest",
        }) if found == invocation_id
    ));
}

#[test]
fn replay_rejects_invocation_state_from_another_thread() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));
    let mut projection = replay(events.iter()).unwrap();
    let other_thread = ThreadId::from_uuid(raw_id(9));
    let cross_thread = EventEnvelope::new(
        PROTOCOL_VERSION,
        EventId::from_uuid(raw_id(110)),
        other_thread,
        Some(turn_id),
        AgentId::from("root"),
        1,
        at(),
        None,
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Proposed,
            to: InvocationState::Failed,
            reason: Some("cross-thread mutation".to_owned()),
        },
    );

    assert_eq!(
        projection.apply(&cross_thread),
        Err(ReplayError::EnvelopeMismatch("invocation parent"))
    );
}

#[test]
fn replay_rejects_invocation_state_from_another_agent() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));
    let mut projection = replay(events.iter()).unwrap();
    let mut cross_agent = envelope(
        8,
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Proposed,
            to: InvocationState::Failed,
            reason: Some("cross-agent mutation".to_owned()),
        },
        Some(turn_id),
    );
    cross_agent.agent_id = AgentId::from("other-agent");

    assert_eq!(
        projection.apply(&cross_agent),
        Err(ReplayError::EnvelopeMismatch("invocation parent"))
    );
}

#[test]
fn replay_rejects_non_idempotent_unknown_retry() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));
    let transitions = [
        (InvocationState::Proposed, InvocationState::Approved),
        (InvocationState::Approved, InvocationState::Prepared),
        (InvocationState::Prepared, InvocationState::Started),
        (InvocationState::Started, InvocationState::Unknown),
    ];
    for (offset, (from, to)) in transitions.into_iter().enumerate() {
        events.push(envelope(
            8 + offset as u64,
            Event::InvocationStateChanged {
                invocation_id,
                from,
                to,
                reason: None,
            },
            Some(turn_id),
        ));
    }
    let mut projection = replay(events.iter()).unwrap();
    let retry = envelope(
        12,
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Unknown,
            to: InvocationState::Started,
            reason: Some("explicit retry".to_owned()),
        },
        Some(turn_id),
    );

    assert_eq!(
        projection.apply(&retry),
        Err(ReplayError::InvalidInvocationTransition {
            from: InvocationState::Unknown,
            to: InvocationState::Started,
        })
    );
}

#[test]
fn replay_requires_an_explicit_evidenced_reconciliation_event() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));
    for (offset, (from, to)) in [
        (InvocationState::Proposed, InvocationState::Approved),
        (InvocationState::Approved, InvocationState::Prepared),
        (InvocationState::Prepared, InvocationState::Started),
        (InvocationState::Started, InvocationState::Unknown),
    ]
    .into_iter()
    .enumerate()
    {
        events.push(envelope(
            8 + offset as u64,
            Event::InvocationStateChanged {
                invocation_id,
                from,
                to,
                reason: None,
            },
            Some(turn_id),
        ));
    }
    let mut projection = replay(events.iter()).unwrap();
    let ordinary_terminal = envelope(
        12,
        Event::InvocationStateChanged {
            invocation_id,
            from: InvocationState::Unknown,
            to: InvocationState::Completed,
            reason: Some("not explicit reconciliation".into()),
        },
        Some(turn_id),
    );
    assert_eq!(
        projection.apply(&ordinary_terminal),
        Err(ReplayError::InvalidInvocationTransition {
            from: InvocationState::Unknown,
            to: InvocationState::Completed,
        })
    );

    let reconciled = envelope(
        12,
        Event::InvocationReconciled {
            invocation_id,
            outcome: InvocationReconciliationOutcome::Completed,
            evidence: InvocationReconciliationEvidence {
                source: "executor_receipt".into(),
                summary: "receipt proves the write committed".into(),
                artifact_uri: Some("artifact://sha256/example".into()),
            },
        },
        Some(turn_id),
    );
    projection.apply(&reconciled).unwrap();
    let invocation = &projection.invocations[&invocation_id];
    assert_eq!(invocation.state, InvocationState::Completed);
    assert_eq!(
        invocation.reconciliation.as_ref().unwrap().source,
        "executor_receipt"
    );
}

#[test]
fn replay_rejects_reconciliation_before_unknown_without_mutating_state() {
    let mut events = trace();
    events.pop();
    let invocation_id = InvocationId::from_uuid(raw_id(8));
    let turn_id = TurnId::from_uuid(raw_id(3));
    events.push(envelope(
        7,
        invocation_proposal(invocation_id),
        Some(turn_id),
    ));
    for (offset, (from, to)) in [
        (InvocationState::Proposed, InvocationState::Approved),
        (InvocationState::Approved, InvocationState::Prepared),
        (InvocationState::Prepared, InvocationState::Started),
    ]
    .into_iter()
    .enumerate()
    {
        events.push(envelope(
            8 + offset as u64,
            Event::InvocationStateChanged {
                invocation_id,
                from,
                to,
                reason: None,
            },
            Some(turn_id),
        ));
    }

    let mut projection = replay(events.iter()).unwrap();
    let reconciled = envelope(
        11,
        Event::InvocationReconciled {
            invocation_id,
            outcome: InvocationReconciliationOutcome::Failed,
            evidence: InvocationReconciliationEvidence {
                source: "executor_receipt".into(),
                summary: "receipt is available".into(),
                artifact_uri: None,
            },
        },
        Some(turn_id),
    );
    assert_eq!(
        projection.apply(&reconciled),
        Err(ReplayError::InvalidInvocationTransition {
            from: InvocationState::Started,
            to: InvocationState::Failed,
        })
    );
    assert_eq!(
        projection.invocations[&invocation_id].state,
        InvocationState::Started
    );
}
