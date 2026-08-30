use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::BTreeSet;
use uuid::Uuid;
use yeux_protocol::*;

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-30T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn command_has_jsonrpc_id_and_independent_command_id() {
    let command = CommandEnvelope::new(
        "rpc-1",
        CommandId::from_uuid(uuid("01890f9d-0000-7000-8000-000000000001")),
        method::THREAD_READ,
        ThreadReadParams {
            thread_id: ThreadId::from_uuid(uuid("01890f9d-0000-7000-8000-000000000002")),
            after_seq: 41,
            limit: 100,
        },
    );

    let wire = serde_json::to_value(command).unwrap();
    assert_eq!(wire["jsonrpc"], "2.0");
    assert_eq!(wire["id"], "rpc-1");
    assert_eq!(wire["method"], "thread/read");
    assert_eq!(wire["params"]["afterSeq"], 41);
    assert!(wire.get("command_id").is_some());
}

#[test]
fn command_requires_the_jsonrpc_version_field() {
    let wire = json!({
        "id": "rpc-1",
        "command_id": "01890f9d-0000-7000-8000-000000000001",
        "method": "thread/read",
        "params": {
            "threadId": "01890f9d-0000-7000-8000-000000000002",
            "afterSeq": 0,
            "limit": 100
        }
    });

    assert!(serde_json::from_value::<CommandEnvelope>(wire).is_err());
}

#[test]
fn event_envelope_flattens_kind_and_payload() {
    let thread_id = ThreadId::from_uuid(uuid("01890f9d-0000-7000-8000-000000000010"));
    let event = EventEnvelope::new(
        PROTOCOL_VERSION,
        EventId::from_uuid(uuid("01890f9d-0000-7000-8000-000000000011")),
        thread_id,
        None,
        AgentId::from("root"),
        1,
        at(),
        Some(CausationId::from("rpc-1")),
        Event::RuntimeDiagnostic {
            code: "sandbox.available".to_owned(),
            message: "seatbelt".to_owned(),
            recoverable: true,
        },
    );

    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["kind"], "runtime/diagnostic");
    assert_eq!(wire["payload"]["code"], "sandbox.available");
    assert_eq!(wire["thread_id"], thread_id.to_string());
    assert!(wire.get("event").is_none());
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(wire).unwrap(),
        event
    );
}

#[test]
fn response_never_serializes_result_and_error_together() {
    let success =
        serde_json::to_value(ResponseEnvelope::success(1_i64, json!({"ok": true}))).unwrap();
    assert!(success.get("result").is_some());
    assert!(success.get("error").is_none());

    let failure = serde_json::to_value(ResponseEnvelope::<serde_json::Value>::failure(
        1_i64,
        RpcError {
            code: RpcError::INCOMPATIBLE_PROTOCOL,
            message: "major version mismatch".to_owned(),
            data: None,
        },
    ))
    .unwrap();
    assert!(failure.get("error").is_some());
    assert!(failure.get("result").is_none());
}

#[test]
fn stable_schema_bundle_contains_wire_and_security_types() {
    let schemas = stable_schema_bundle();
    for name in [
        "CommandEnvelope",
        "EventEnvelope",
        "ThreadReadResult",
        "PreparedInvocation",
        "CapabilityGrant",
        "AgentSpawnSpec",
    ] {
        assert!(schemas.contains_key(name), "missing schema {name}");
    }
}

#[test]
fn committed_stable_schemas_match_the_rust_source() {
    let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("spec/schema");
    let documents = stable_schema_bundle()
        .into_iter()
        .map(|(name, schema)| {
            let mut json = serde_json::to_string_pretty(&schema).unwrap();
            json.push('\n');
            (format!("{name}.schema.json"), json)
        })
        .collect::<Vec<_>>();
    let expected_names: BTreeSet<_> = documents.iter().map(|(name, _)| name.clone()).collect();
    let actual_names: BTreeSet<_> = std::fs::read_dir(&schema_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".schema.json"))
        .collect();
    assert_eq!(
        actual_names, expected_names,
        "stable schema file set drifted"
    );

    for (name, expected) in documents {
        let actual = std::fs::read_to_string(schema_dir.join(&name)).unwrap();
        assert_eq!(actual, expected, "committed schema drifted: {name}");
    }
}
