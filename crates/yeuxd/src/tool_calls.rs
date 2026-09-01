//! Deterministic assembly of fragmented provider tool-call deltas.
//!
//! Provider adapters emit arguments as arbitrary JSON string fragments.  This
//! module keeps that transport concern out of the turn runner and enforces a
//! second, daemon-side resource boundary before any tool input is decoded.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;
use yeux_protocol::ModelEvent;

pub const DEFAULT_MAX_TOOL_CALLS_PER_ROUND: usize = 32;
pub const DEFAULT_MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
pub const DEFAULT_MAX_TOOL_ARGUMENT_BYTES_PER_ROUND: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolCallLimits {
    pub max_calls: usize,
    pub max_argument_bytes_per_call: usize,
    pub max_argument_bytes_per_round: usize,
}

impl Default for ToolCallLimits {
    fn default() -> Self {
        Self {
            max_calls: DEFAULT_MAX_TOOL_CALLS_PER_ROUND,
            max_argument_bytes_per_call: DEFAULT_MAX_TOOL_ARGUMENT_BYTES,
            max_argument_bytes_per_round: DEFAULT_MAX_TOOL_ARGUMENT_BYTES_PER_ROUND,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssembledToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ToolCallAssemblyError {
    #[error("provider emitted a tool-call delta without a call id")]
    MissingCallId,
    #[error("provider emitted more than {limit} tool calls in one model round")]
    TooManyCalls { limit: usize },
    #[error("tool call {call_id} changed name from {expected} to {actual}")]
    NameChanged {
        call_id: String,
        expected: String,
        actual: String,
    },
    #[error("tool call {call_id} did not include a tool name")]
    MissingName { call_id: String },
    #[error("tool call {call_id} arguments exceed {limit} bytes")]
    CallArgumentsTooLarge { call_id: String, limit: usize },
    #[error("tool-call arguments exceed {limit} bytes in one model round")]
    RoundArgumentsTooLarge { limit: usize },
    #[error("tool call {call_id} arguments are invalid JSON: {message}")]
    InvalidJson { call_id: String, message: String },
    #[error("tool call {call_id} arguments must decode to a JSON object")]
    ArgumentsNotObject { call_id: String },
}

#[derive(Debug)]
struct PendingToolCall {
    order: usize,
    name: String,
    arguments: String,
}

#[derive(Debug)]
pub struct ToolCallAssembler {
    limits: ToolCallLimits,
    calls: BTreeMap<String, PendingToolCall>,
    next_order: usize,
    total_argument_bytes: usize,
}

impl Default for ToolCallAssembler {
    fn default() -> Self {
        Self::new(ToolCallLimits::default())
    }
}

impl ToolCallAssembler {
    pub fn new(limits: ToolCallLimits) -> Self {
        Self {
            limits,
            calls: BTreeMap::new(),
            next_order: 0,
            total_argument_bytes: 0,
        }
    }

    pub fn push(&mut self, event: &ModelEvent) -> Result<(), ToolCallAssemblyError> {
        let ModelEvent::ToolCallDelta {
            call_id,
            name,
            json_delta,
        } = event
        else {
            return Ok(());
        };

        if call_id.is_empty() {
            return Err(ToolCallAssemblyError::MissingCallId);
        }

        if !self.calls.contains_key(call_id) {
            if self.calls.len() >= self.limits.max_calls {
                return Err(ToolCallAssemblyError::TooManyCalls {
                    limit: self.limits.max_calls,
                });
            }
            let order = self.next_order;
            self.next_order += 1;
            self.calls.insert(
                call_id.clone(),
                PendingToolCall {
                    order,
                    name: name.clone(),
                    arguments: String::new(),
                },
            );
        }

        let pending = self.calls.get_mut(call_id).expect("inserted above");
        if !name.is_empty() {
            if pending.name.is_empty() {
                pending.name.clone_from(name);
            } else if pending.name != *name {
                return Err(ToolCallAssemblyError::NameChanged {
                    call_id: call_id.clone(),
                    expected: pending.name.clone(),
                    actual: name.clone(),
                });
            }
        }

        let delta_bytes = json_delta.len();
        if pending.arguments.len().saturating_add(delta_bytes)
            > self.limits.max_argument_bytes_per_call
        {
            return Err(ToolCallAssemblyError::CallArgumentsTooLarge {
                call_id: call_id.clone(),
                limit: self.limits.max_argument_bytes_per_call,
            });
        }
        if self.total_argument_bytes.saturating_add(delta_bytes)
            > self.limits.max_argument_bytes_per_round
        {
            return Err(ToolCallAssemblyError::RoundArgumentsTooLarge {
                limit: self.limits.max_argument_bytes_per_round,
            });
        }
        pending.arguments.push_str(json_delta);
        self.total_argument_bytes += delta_bytes;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn finish(self) -> Result<Vec<AssembledToolCall>, ToolCallAssemblyError> {
        let mut calls: Vec<_> = self.calls.into_iter().collect();
        calls.sort_by_key(|(_, pending)| pending.order);
        calls
            .into_iter()
            .map(|(call_id, pending)| {
                if pending.name.is_empty() {
                    return Err(ToolCallAssemblyError::MissingName { call_id });
                }
                let arguments = if pending.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str::<Value>(&pending.arguments).map_err(|error| {
                        ToolCallAssemblyError::InvalidJson {
                            call_id: call_id.clone(),
                            message: error.to_string(),
                        }
                    })?
                };
                if !arguments.is_object() {
                    return Err(ToolCallAssemblyError::ArgumentsNotObject { call_id });
                }
                Ok(AssembledToolCall {
                    call_id,
                    name: pending.name,
                    arguments,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(call_id: &str, name: &str, json_delta: &str) -> ModelEvent {
        ModelEvent::ToolCallDelta {
            call_id: call_id.into(),
            name: name.into(),
            json_delta: json_delta.into(),
        }
    }

    #[test]
    fn assembles_fragmented_calls_in_first_seen_order() {
        let mut assembler = ToolCallAssembler::default();
        assembler
            .push(&delta("call-b", "workspace.search", "{\"query\":"))
            .unwrap();
        assembler
            .push(&delta("call-a", "workspace.read", "{\"path\":\"a.rs\"}"))
            .unwrap();
        assembler
            .push(&delta("call-b", "workspace.search", "\"needle\"}"))
            .unwrap();

        let calls = assembler.finish().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, "call-b");
        assert_eq!(calls[0].arguments, serde_json::json!({"query": "needle"}));
        assert_eq!(calls[1].call_id, "call-a");
    }

    #[test]
    fn accepts_empty_arguments_as_an_empty_object() {
        let mut assembler = ToolCallAssembler::default();
        assembler
            .push(&delta("call", "workspace.list", ""))
            .unwrap();
        assert_eq!(
            assembler.finish().unwrap()[0].arguments,
            serde_json::json!({})
        );
    }

    #[test]
    fn rejects_missing_identity_name_changes_and_invalid_json() {
        let mut missing_id = ToolCallAssembler::default();
        assert_eq!(
            missing_id.push(&delta("", "workspace.read", "{}")),
            Err(ToolCallAssemblyError::MissingCallId)
        );

        let mut changed = ToolCallAssembler::default();
        changed
            .push(&delta("call", "workspace.read", "{}"))
            .unwrap();
        assert!(matches!(
            changed.push(&delta("call", "workspace.list", "")),
            Err(ToolCallAssemblyError::NameChanged { .. })
        ));

        let mut invalid = ToolCallAssembler::default();
        invalid.push(&delta("call", "workspace.read", "{")).unwrap();
        assert!(matches!(
            invalid.finish(),
            Err(ToolCallAssemblyError::InvalidJson { .. })
        ));
    }

    #[test]
    fn enforces_call_and_argument_budgets() {
        let limits = ToolCallLimits {
            max_calls: 1,
            max_argument_bytes_per_call: 4,
            max_argument_bytes_per_round: 4,
        };
        let mut too_many = ToolCallAssembler::new(limits);
        too_many.push(&delta("one", "a", "{}")).unwrap();
        assert_eq!(
            too_many.push(&delta("two", "b", "{}")),
            Err(ToolCallAssemblyError::TooManyCalls { limit: 1 })
        );

        let mut too_large = ToolCallAssembler::new(limits);
        assert!(matches!(
            too_large.push(&delta("one", "a", "12345")),
            Err(ToolCallAssemblyError::CallArgumentsTooLarge { .. })
        ));
    }
}
