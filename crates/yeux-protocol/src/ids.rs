use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Serialize,
            Deserialize,
            JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn into_uuid(self) -> Uuid {
                self.0
            }

            pub const fn nil() -> Self {
                Self(Uuid::nil())
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(CommandId);
uuid_id!(EventId);
uuid_id!(WorkspaceId);
uuid_id!(ThreadId);
uuid_id!(TurnId);
uuid_id!(ItemId);
uuid_id!(InvocationId);
uuid_id!(JobId);
uuid_id!(AgentRunId);
uuid_id!(ModelRequestId);
uuid_id!(ApprovalId);
uuid_id!(ArtifactId);

/// Agent names are stable human-readable identifiers (`root`, `research-1`, …).
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<&str> for AgentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for AgentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Links an event to the command, model call, invocation, or parent event that caused it.
#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct CausationId(pub String);

impl CausationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for CausationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<&str> for CausationId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for CausationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<CommandId> for CausationId {
    fn from(value: CommandId) -> Self {
        Self(value.to_string())
    }
}

impl From<EventId> for CausationId {
    fn from(value: EventId) -> Self {
        Self(value.to_string())
    }
}

impl From<InvocationId> for CausationId {
    fn from(value: InvocationId) -> Self {
        Self(value.to_string())
    }
}

impl From<ModelRequestId> for CausationId {
    fn from(value: ModelRequestId) -> Self {
        Self(value.to_string())
    }
}
