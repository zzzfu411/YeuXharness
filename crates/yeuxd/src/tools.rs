//! Sealed daemon-owned registration and execution boundary for built-in tools.
//!
//! The protocol's serializable `PreparedInvocation` is evidence, not an
//! execution capability. This module deliberately keeps concrete adapters and
//! their prepared payloads private, and reserves an opaque, non-cloneable
//! [`ExecutionPermit`] for the future authority pipeline. A registry can be
//! cloned by the daemon, but a permit is consumed by value exactly once.

#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeMap,
    fmt, fs,
    future::Future,
    path::{Component, Path},
    pin::Pin,
    sync::Arc,
};

use serde_json::{Map, Value};
use thiserror::Error;
use yeux_core::digest_value;
use yeux_protocol::{
    ConcurrencyClass, EffectSet, Idempotency, PathScope, ProcessEffect, Reversibility, ToolSpec,
};
use yeux_runtime::{
    workspace_apply_patch_spec, workspace_list_spec, workspace_read_spec, workspace_search_spec,
    PreparedWorkspaceMutation, ProcessError, ProcessExecutor, ProcessRequest,
    WorkspaceSearchControl, WorkspaceToolError, WorkspaceTools,
};
pub use yeux_runtime::{
    WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_LIST_TOOL_ID, WORKSPACE_READ_TOOL_ID,
    WORKSPACE_SEARCH_TOOL_ID, WORKSPACE_TOOL_VERSION,
};

pub const PROCESS_RUN_TOOL_ID: &str = "process.run";
pub const PROCESS_TOOL_VERSION: &str = "1";

/// Hard daemon ceiling for one sealed registry.
pub const MAX_REGISTERED_TOOLS: usize = 128;
/// Tool identifiers are provider-visible and therefore intentionally compact.
pub const MAX_TOOL_ID_BYTES: usize = 128;
/// Version selectors are exact, bounded strings rather than semver ranges.
pub const MAX_TOOL_VERSION_BYTES: usize = 64;
/// Descriptions are sent to providers on every model request.
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Each input or output schema is bounded independently.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 256 * 1024;
/// Built-ins may narrow this timeout but never raise it.
pub const MAX_TOOL_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
/// Large output belongs in artifacts; inline output remains bounded.
pub const MAX_TOOL_INLINE_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EFFECT_ENTRIES: usize = 128;
const MAX_EFFECT_STRING_BYTES: usize = 4 * 1024;

/// Explicit opt-in switches for daemon-owned built-ins.
///
/// Mutation registration is disabled by default. Enabling it only makes the
/// adapter resolvable by exact id/version; it does not advertise the mutation
/// to a provider. Advertising remains gated on the complete P1 authority path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuiltInToolRegistryConfig {
    register_hidden_workspace_mutations: bool,
    register_hidden_process: bool,
    advertise_workspace_mutations: bool,
    advertise_process: bool,
}

impl BuiltInToolRegistryConfig {
    pub const fn read_only() -> Self {
        Self {
            register_hidden_workspace_mutations: false,
            register_hidden_process: false,
            advertise_workspace_mutations: false,
            advertise_process: false,
        }
    }

    pub const fn with_hidden_workspace_mutations(mut self) -> Self {
        self.register_hidden_workspace_mutations = true;
        self
    }

    pub const fn with_hidden_process(mut self) -> Self {
        self.register_hidden_process = true;
        self
    }

    /// Advertise the mutation adapter to the provider. Callers must only set
    /// this after the daemon has confirmed the complete policy/sandbox path.
    pub const fn with_advertised_workspace_mutations(mut self) -> Self {
        self.register_hidden_workspace_mutations = true;
        self.advertise_workspace_mutations = true;
        self
    }

    /// Advertise the process adapter to the provider. Callers must only set
    /// this after the daemon has confirmed the complete policy/sandbox path.
    pub const fn with_advertised_process(mut self) -> Self {
        self.register_hidden_process = true;
        self.advertise_process = true;
        self
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ToolKey {
    id: String,
    version: String,
}

impl ToolKey {
    fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
        }
    }

    fn from_spec(spec: &ToolSpec) -> Self {
        Self::new(spec.id.clone(), spec.version.clone())
    }
}

/// Stable registry failures suitable for daemon diagnostics.
#[derive(Debug, Error)]
pub enum ToolRegistryError {
    #[error("a tool registry must contain at least one registration")]
    EmptyRegistry,
    #[error("tool registry contains {actual} registrations; maximum is {limit}")]
    RegistrationLimit { actual: usize, limit: usize },
    #[error("tool registration {index} has an empty {field}")]
    EmptySpecField { index: usize, field: &'static str },
    #[error("tool registration {index} has an invalid {field}: {value}")]
    InvalidSpecIdentifier {
        index: usize,
        field: &'static str,
        value: String,
    },
    #[error("tool registration {index} exceeds {field} limit: value {actual}, maximum {limit}")]
    SpecLimit {
        index: usize,
        field: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("tool registration {index} has an invalid {field}: {message}")]
    InvalidSpec {
        index: usize,
        field: &'static str,
        message: String,
    },
    #[error("duplicate tool registration {tool_id}@{tool_version}")]
    DuplicateTool {
        tool_id: String,
        tool_version: String,
    },
    #[error("tool adapter does not implement {tool_id}@{tool_version}")]
    AdapterIdentityMismatch {
        tool_id: String,
        tool_version: String,
    },
    #[error("unknown registered tool {tool_id}@{tool_version}")]
    UnknownTool {
        tool_id: String,
        tool_version: String,
    },
    #[error("tool plan belongs to another registry")]
    ForeignPlan,
    #[error("tool execution permit belongs to another registry")]
    ForeignExecutionPermit,
    #[error("tool plan changed during revalidation: {field}")]
    PlanChanged { field: &'static str },
    #[error("tool {tool_id}@{tool_version} concrete effects exceed its registered template")]
    EffectEscalation {
        tool_id: String,
        tool_version: String,
    },
    #[error("tool adapter payload does not match {tool_id}@{tool_version}")]
    AdapterPayloadMismatch {
        tool_id: String,
        tool_version: String,
    },
    #[error("tool {tool_id}@{tool_version} failed: {source}")]
    WorkspaceTool {
        tool_id: String,
        tool_version: String,
        #[source]
        source: WorkspaceToolError,
    },
    #[error("process tool {tool_id}@{tool_version} failed: {source}")]
    Process {
        tool_id: String,
        tool_version: String,
        #[source]
        source: ProcessError,
    },
    #[error("process tool arguments are invalid: {0}")]
    InvalidProcessArguments(String),
    #[error("process tool requires the async execution boundary")]
    ProcessRequiresAsync,
    #[error("tool authority pipeline rejected the invocation: {0}")]
    Authority(String),
}

impl ToolRegistryError {
    /// Stable machine-readable error code for persistence and provider results.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyRegistry => "tool_registry_empty",
            Self::RegistrationLimit { .. } => "tool_registry_registration_limit",
            Self::EmptySpecField { .. } => "tool_registry_empty_spec_field",
            Self::InvalidSpecIdentifier { .. } => "tool_registry_invalid_identifier",
            Self::SpecLimit { .. } => "tool_registry_spec_limit",
            Self::InvalidSpec { .. } => "tool_registry_invalid_spec",
            Self::DuplicateTool { .. } => "tool_registry_duplicate_tool",
            Self::AdapterIdentityMismatch { .. } => "tool_registry_adapter_identity_mismatch",
            Self::UnknownTool { .. } => "tool_registry_unknown_tool",
            Self::ForeignPlan => "tool_registry_foreign_plan",
            Self::ForeignExecutionPermit => "tool_registry_foreign_execution_permit",
            Self::PlanChanged { .. } => "tool_registry_plan_changed",
            Self::EffectEscalation { .. } => "tool_registry_effect_escalation",
            Self::AdapterPayloadMismatch { .. } => "tool_registry_adapter_payload_mismatch",
            Self::WorkspaceTool { source, .. } => source.code(),
            Self::Process { .. } => "process_execution_failed",
            Self::InvalidProcessArguments(_) => "process_invalid_arguments",
            Self::ProcessRequiresAsync => "process_async_required",
            Self::Authority(_) => "tool_authority_rejected",
        }
    }

    /// Compatibility code for model-visible tool results.  The registry keeps
    /// its own diagnostic namespace, while the pre-registry workspace API
    /// exposed `workspace_unknown_tool`; retain that stable wire code for an
    /// unknown provider call so existing clients do not need a migration.
    pub const fn provider_code(&self) -> &'static str {
        match self {
            Self::UnknownTool { .. } => "workspace_unknown_tool",
            _ => self.code(),
        }
    }
}

#[derive(Clone)]
struct RegisteredTool {
    spec: ToolSpec,
    advertised: bool,
    adapter: Arc<dyn SealedToolAdapter>,
}

impl fmt::Debug for RegisteredTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegisteredTool")
            .field("id", &self.spec.id)
            .field("version", &self.spec.version)
            .field("advertised", &self.advertised)
            .finish_non_exhaustive()
    }
}

impl RegisteredTool {
    fn advertised(spec: ToolSpec, adapter: Arc<dyn SealedToolAdapter>) -> Self {
        Self {
            spec,
            advertised: true,
            adapter,
        }
    }

    fn hidden(spec: ToolSpec, adapter: Arc<dyn SealedToolAdapter>) -> Self {
        Self {
            spec,
            advertised: false,
            adapter,
        }
    }
}

/// A daemon-owned, exact-version registry of sealed built-in adapters.
///
/// Registrations are indexed by `id@version` in a `BTreeMap`; advertised
/// specs are cached in the same stable lexical order regardless of
/// construction order.
#[derive(Clone)]
pub struct ToolRegistry {
    seal: Arc<RegistrySeal>,
    tools: BTreeMap<ToolKey, RegisteredTool>,
    advertised_specs: Vec<ToolSpec>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("registered_tools", &self.tools.len())
            .field("advertised_tools", &self.advertised_specs.len())
            .finish()
    }
}

#[derive(Debug)]
struct RegistrySeal;

impl ToolRegistry {
    /// Register the three bounded read-only workspace tools.
    pub fn workspace_built_ins(tools: WorkspaceTools) -> Result<Self, ToolRegistryError> {
        Self::workspace_built_ins_with_config(tools, BuiltInToolRegistryConfig::read_only())
    }

    /// Register built-ins under an explicit daemon configuration.
    ///
    /// `workspace.apply_patch` can be registered hidden for pipeline work, but
    /// it is never included in [`ToolRegistry::advertised_specs`] at this stage.
    pub fn workspace_built_ins_with_config(
        tools: WorkspaceTools,
        config: BuiltInToolRegistryConfig,
    ) -> Result<Self, ToolRegistryError> {
        let tools = Arc::new(tools);
        Self::workspace_built_ins_with_config_and_process(tools, config, None)
    }

    /// Register built-ins with an optional daemon-owned process executor.
    /// `process.run` is never visible in the provider tool list at registration
    /// time; the M2 pipeline is the only component that can execute it.
    pub fn workspace_built_ins_with_config_and_process(
        tools: Arc<WorkspaceTools>,
        config: BuiltInToolRegistryConfig,
        process_executor: Option<Arc<ProcessExecutor>>,
    ) -> Result<Self, ToolRegistryError> {
        let mut registrations = vec![
            RegisteredTool::advertised(
                workspace_list_spec(),
                Arc::new(WorkspaceReadAdapter::new(
                    Arc::clone(&tools),
                    WorkspaceReadOperation::List,
                )),
            ),
            RegisteredTool::advertised(
                workspace_read_spec(),
                Arc::new(WorkspaceReadAdapter::new(
                    Arc::clone(&tools),
                    WorkspaceReadOperation::Read,
                )),
            ),
            RegisteredTool::advertised(
                workspace_search_spec(),
                Arc::new(WorkspaceReadAdapter::new(
                    Arc::clone(&tools),
                    WorkspaceReadOperation::Search,
                )),
            ),
        ];
        if config.register_hidden_workspace_mutations {
            let spec = workspace_apply_patch_spec();
            let adapter = Arc::new(WorkspaceMutationAdapter::new(Arc::clone(&tools)));
            registrations.push(if config.advertise_workspace_mutations {
                RegisteredTool::advertised(spec, adapter)
            } else {
                RegisteredTool::hidden(spec, adapter)
            });
        }
        if config.register_hidden_process {
            let executor = process_executor.unwrap_or_else(|| Arc::new(ProcessExecutor::detect()));
            let spec = process_run_spec();
            let adapter = Arc::new(ProcessAdapter::new(Arc::clone(&tools), executor));
            registrations.push(if config.advertise_process {
                RegisteredTool::advertised(spec, adapter)
            } else {
                RegisteredTool::hidden(spec, adapter)
            });
        }
        Self::try_new(registrations)
    }

    pub fn workspace_built_ins_with_process(
        tools: WorkspaceTools,
        process_executor: Arc<ProcessExecutor>,
    ) -> Result<Self, ToolRegistryError> {
        Self::workspace_built_ins_with_config_and_process(
            Arc::new(tools),
            BuiltInToolRegistryConfig::read_only()
                .with_hidden_workspace_mutations()
                .with_hidden_process(),
            Some(process_executor),
        )
    }

    fn try_new(registrations: Vec<RegisteredTool>) -> Result<Self, ToolRegistryError> {
        if registrations.is_empty() {
            return Err(ToolRegistryError::EmptyRegistry);
        }
        if registrations.len() > MAX_REGISTERED_TOOLS {
            return Err(ToolRegistryError::RegistrationLimit {
                actual: registrations.len(),
                limit: MAX_REGISTERED_TOOLS,
            });
        }

        let mut tools = BTreeMap::new();
        for (index, registration) in registrations.into_iter().enumerate() {
            validate_spec(index, &registration.spec)?;
            let key = ToolKey::from_spec(&registration.spec);
            if !registration.adapter.supports(&key) {
                return Err(ToolRegistryError::AdapterIdentityMismatch {
                    tool_id: key.id,
                    tool_version: key.version,
                });
            }
            if tools.insert(key.clone(), registration).is_some() {
                return Err(ToolRegistryError::DuplicateTool {
                    tool_id: key.id,
                    tool_version: key.version,
                });
            }
        }

        let advertised_specs = tools
            .values()
            .filter(|registration| registration.advertised)
            .map(|registration| registration.spec.clone())
            .collect();
        Ok(Self {
            seal: Arc::new(RegistrySeal),
            tools,
            advertised_specs,
        })
    }

    /// Provider-visible specs in stable `id`, then `version`, lexical order.
    pub fn advertised_specs(&self) -> &[ToolSpec] {
        &self.advertised_specs
    }

    pub fn registered_len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_registered(&self, tool_id: &str, tool_version: &str) -> bool {
        self.tools
            .contains_key(&ToolKey::new(tool_id, tool_version))
    }

    pub fn advertised_len(&self) -> usize {
        self.advertised_specs.len()
    }

    /// Return the exact version advertised for a provider-visible tool.
    ///
    /// Provider tool calls carry an identifier but not a version in the
    /// current protocol.  Resolving the version from the sealed advertised
    /// set keeps that compatibility detail in the authority boundary and
    /// prevents callers from silently selecting a newer registration.
    pub fn advertised_version(&self, tool_id: &str) -> Option<&str> {
        self.advertised_specs
            .iter()
            .find(|spec| spec.id == tool_id)
            .map(|spec| spec.version.as_str())
    }

    /// Execute one provider-visible read-only invocation through the sealed
    /// plan/revalidate/permit path.
    ///
    /// This is intentionally the only convenience executor exposed to the
    /// M1 runner.  It refuses any effect-bearing registration and mints an
    /// opaque, single-use permit only after the adapter has been revalidated.
    /// Side-effecting registrations therefore remain unavailable until the
    /// full policy/approval/sandbox pipeline supplies its own permit.
    pub fn execute_read_only(
        &self,
        tool_id: &str,
        tool_version: &str,
        arguments: Value,
    ) -> Result<Value, ToolRegistryError> {
        self.execute_read_only_with_control(tool_id, tool_version, arguments, None)
    }

    /// Execute a read-only invocation while binding execution to an already
    /// persisted proposal.
    ///
    /// The proposal fields are evidence, not authority: the registry plans and
    /// revalidates the supplied normalized arguments against the live adapter,
    /// then compares all three approval-bound fields (workspace identity,
    /// normalized arguments, and concrete effects) with the persisted values.
    /// No execution permit is minted until every comparison succeeds.
    pub fn execute_read_only_bound(
        &self,
        tool_id: &str,
        tool_version: &str,
        expected_workspace_identity: &str,
        expected_normalized_arguments: &Value,
        expected_effects: &EffectSet,
    ) -> Result<Value, ToolRegistryError> {
        self.execute_read_only_bound_with_control(
            tool_id,
            tool_version,
            expected_workspace_identity,
            expected_normalized_arguments,
            expected_effects,
            None,
        )
    }

    /// [`ToolRegistry::execute_read_only_bound`] with cooperative search
    /// controls propagated to the runtime adapter.
    pub fn execute_read_only_bound_with_control(
        &self,
        tool_id: &str,
        tool_version: &str,
        expected_workspace_identity: &str,
        expected_normalized_arguments: &Value,
        expected_effects: &EffectSet,
        control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError> {
        // Use the normalized proposal arguments as the sole plan input.  A
        // caller cannot substitute a second, unbound argument value between
        // proposal verification and execution.
        let plan = self.plan(tool_id, tool_version, expected_normalized_arguments.clone())?;
        let revalidated = self.revalidate(plan)?;

        // Compare each persisted binding field explicitly.  Keep these checks
        // before `into_execution_permit`: a mismatch must not consume or mint
        // an execution capability, even if a later caller retries safely.
        if revalidated.workspace_identity() != expected_workspace_identity {
            return Err(ToolRegistryError::PlanChanged {
                field: "workspace_identity",
            });
        }
        if revalidated.normalized_arguments() != expected_normalized_arguments {
            return Err(ToolRegistryError::PlanChanged {
                field: "normalized_arguments",
            });
        }
        if revalidated.effects() != expected_effects {
            return Err(ToolRegistryError::PlanChanged { field: "effects" });
        }
        if !revalidated.effects().is_read_only() {
            return Err(ToolRegistryError::EffectEscalation {
                tool_id: tool_id.to_owned(),
                tool_version: tool_version.to_owned(),
            });
        }

        self.execute_with_control(revalidated.into_execution_permit(), control)
            .map(ToolExecutionOutput::into_value)
    }

    /// Execute one read-only invocation while propagating cooperative search
    /// controls into the runtime adapter. The control is borrowed only for
    /// this synchronous call and is never persisted or treated as authority.
    pub fn execute_read_only_with_control(
        &self,
        tool_id: &str,
        tool_version: &str,
        arguments: Value,
        control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError> {
        let plan = self.plan(tool_id, tool_version, arguments)?;
        if !plan.effects().is_read_only() {
            return Err(ToolRegistryError::EffectEscalation {
                tool_id: tool_id.to_owned(),
                tool_version: tool_version.to_owned(),
            });
        }
        let revalidated = self.revalidate(plan)?;
        if !revalidated.effects().is_read_only() {
            return Err(ToolRegistryError::EffectEscalation {
                tool_id: tool_id.to_owned(),
                tool_version: tool_version.to_owned(),
            });
        }
        self.execute_with_control(revalidated.into_execution_permit(), control)
            .map(ToolExecutionOutput::into_value)
    }

    /// Resolve one exact registration without a latest-version fallback.
    pub fn resolve_exact(
        &self,
        tool_id: &str,
        tool_version: &str,
    ) -> Result<&ToolSpec, ToolRegistryError> {
        self.registration(tool_id, tool_version)
            .map(|registration| &registration.spec)
    }

    /// Validate and normalize arguments, resolve concrete effects, and retain
    /// a sealed adapter payload. Planning never grants execution authority.
    pub fn plan(
        &self,
        tool_id: &str,
        tool_version: &str,
        arguments: Value,
    ) -> Result<ToolPlan, ToolRegistryError> {
        let key = ToolKey::new(tool_id, tool_version);
        let registration = self.registration(&key.id, &key.version)?;
        let adapter_plan = registration.adapter.plan(arguments)?;
        verify_effect_subset(&registration.spec, &adapter_plan.effects)?;
        Ok(ToolPlan {
            registry_seal: Arc::clone(&self.seal),
            key,
            workspace_identity: adapter_plan.workspace_identity,
            normalized_arguments: adapter_plan.normalized_arguments,
            effects: adapter_plan.effects,
            payload: adapter_plan.payload,
        })
    }

    /// Re-run path, argument, effect, and payload preparation against current
    /// workspace state. The plan is consumed so one caller cannot revalidate
    /// the same sealed payload twice.
    pub fn revalidate(&self, plan: ToolPlan) -> Result<RevalidatedToolPlan, ToolRegistryError> {
        let ToolPlan {
            registry_seal,
            key,
            workspace_identity,
            normalized_arguments,
            effects,
            payload,
        } = plan;
        if !Arc::ptr_eq(&self.seal, &registry_seal) {
            return Err(ToolRegistryError::ForeignPlan);
        }

        let registration = self.registration(&key.id, &key.version)?;
        let current = registration.adapter.revalidate(
            &workspace_identity,
            &normalized_arguments,
            &effects,
            payload,
        )?;
        if current.workspace_identity != workspace_identity {
            return Err(ToolRegistryError::PlanChanged {
                field: "workspace_identity",
            });
        }
        if current.normalized_arguments != normalized_arguments {
            return Err(ToolRegistryError::PlanChanged {
                field: "normalized_arguments",
            });
        }
        if current.effects != effects {
            return Err(ToolRegistryError::PlanChanged { field: "effects" });
        }
        verify_effect_subset(&registration.spec, &current.effects)?;

        Ok(RevalidatedToolPlan {
            registry_seal,
            key,
            workspace_identity: current.workspace_identity,
            normalized_arguments: current.normalized_arguments,
            effects: current.effects,
            payload: current.payload,
        })
    }

    /// Consume one opaque authority permit and execute its sealed adapter.
    ///
    /// There is intentionally no public constructor for [`ExecutionPermit`].
    /// [`crate::pipeline::InvocationPipeline`] mints it only after policy,
    /// approval, sandbox capability, and revalidation checks have succeeded.
    pub fn execute(
        &self,
        permit: ExecutionPermit,
    ) -> Result<ToolExecutionOutput, ToolRegistryError> {
        self.execute_with_control(permit, None)
    }

    /// Async counterpart used by `process.run`. Read and mutation adapters
    /// retain their synchronous implementation; process execution awaits the
    /// runtime supervisor without creating a nested Tokio runtime.
    pub async fn execute_async(
        &self,
        permit: ExecutionPermit,
    ) -> Result<ToolExecutionOutput, ToolRegistryError> {
        self.execute_async_with_control(permit).await
    }

    async fn execute_async_with_control(
        &self,
        permit: ExecutionPermit,
    ) -> Result<ToolExecutionOutput, ToolRegistryError> {
        let ExecutionPermit {
            registry_seal,
            key,
            workspace_identity: _,
            normalized_arguments,
            effects,
            payload,
        } = permit;
        if !Arc::ptr_eq(&self.seal, &registry_seal) {
            return Err(ToolRegistryError::ForeignExecutionPermit);
        }
        let registration = self.registration(&key.id, &key.version)?;
        verify_effect_subset(&registration.spec, &effects)?;
        let value = registration
            .adapter
            .execute_async(normalized_arguments, payload)
            .await?;
        Ok(ToolExecutionOutput { value })
    }

    fn execute_with_control(
        &self,
        permit: ExecutionPermit,
        control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<ToolExecutionOutput, ToolRegistryError> {
        let ExecutionPermit {
            registry_seal,
            key,
            workspace_identity: _,
            normalized_arguments,
            effects,
            payload,
        } = permit;
        if !Arc::ptr_eq(&self.seal, &registry_seal) {
            return Err(ToolRegistryError::ForeignExecutionPermit);
        }
        let registration = self.registration(&key.id, &key.version)?;
        verify_effect_subset(&registration.spec, &effects)?;
        let value = registration
            .adapter
            .execute(normalized_arguments, payload, control)?;
        Ok(ToolExecutionOutput { value })
    }

    fn registration(
        &self,
        tool_id: &str,
        tool_version: &str,
    ) -> Result<&RegisteredTool, ToolRegistryError> {
        self.tools
            .get(&ToolKey::new(tool_id, tool_version))
            .ok_or_else(|| ToolRegistryError::UnknownTool {
                tool_id: tool_id.to_owned(),
                tool_version: tool_version.to_owned(),
            })
    }
}

/// A sealed preparation result. Its arguments and concrete effects are
/// inspectable by the authority pipeline, while the executor payload remains
/// private to this module.
pub struct ToolPlan {
    registry_seal: Arc<RegistrySeal>,
    key: ToolKey,
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    payload: PlannedPayload,
}

impl ToolPlan {
    pub fn tool_id(&self) -> &str {
        &self.key.id
    }

    pub fn tool_version(&self) -> &str {
        &self.key.version
    }

    pub fn workspace_identity(&self) -> &str {
        &self.workspace_identity
    }

    pub fn normalized_arguments(&self) -> &Value {
        &self.normalized_arguments
    }

    pub fn effects(&self) -> &EffectSet {
        &self.effects
    }
}

impl fmt::Debug for ToolPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPlan")
            .field("tool_id", &self.key.id)
            .field("tool_version", &self.key.version)
            .field("workspace_identity", &self.workspace_identity)
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}

/// Fresh preparation evidence returned immediately before authority checks
/// mint an execution permit. It is intentionally non-cloneable.
pub struct RevalidatedToolPlan {
    registry_seal: Arc<RegistrySeal>,
    key: ToolKey,
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    payload: ExecutionPayload,
}

impl RevalidatedToolPlan {
    pub fn tool_id(&self) -> &str {
        &self.key.id
    }

    pub fn tool_version(&self) -> &str {
        &self.key.version
    }

    pub fn workspace_identity(&self) -> &str {
        &self.workspace_identity
    }

    pub fn normalized_arguments(&self) -> &Value {
        &self.normalized_arguments
    }

    pub fn effects(&self) -> &EffectSet {
        &self.effects
    }

    /// Reserved for `InvocationPipeline`: this remains private so registry
    /// consumers cannot turn preparation evidence into execution authority.
    #[allow(dead_code)]
    pub(crate) fn into_execution_permit(self) -> ExecutionPermit {
        ExecutionPermit {
            registry_seal: self.registry_seal,
            key: self.key,
            workspace_identity: self.workspace_identity,
            normalized_arguments: self.normalized_arguments,
            effects: self.effects,
            payload: self.payload,
        }
    }
}

impl fmt::Debug for RevalidatedToolPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RevalidatedToolPlan")
            .field("tool_id", &self.key.id)
            .field("tool_version", &self.key.version)
            .field("workspace_identity", &self.workspace_identity)
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}

/// Opaque, by-value, non-cloneable execution authority.
///
/// It cannot be deserialized, built from `PreparedInvocation`, or constructed
/// outside this module. Its eventual production constructor belongs beside
/// `InvocationPipeline`, after policy and approval validation.
///
/// ```compile_fail
/// use yeuxd::tools::ExecutionPermit;
///
/// fn duplicate(permit: ExecutionPermit) {
///     let _first = permit;
///     let _second = permit;
/// }
/// ```
pub struct ExecutionPermit {
    registry_seal: Arc<RegistrySeal>,
    key: ToolKey,
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    payload: ExecutionPayload,
}

impl ExecutionPermit {
    pub fn tool_id(&self) -> &str {
        &self.key.id
    }

    pub fn tool_version(&self) -> &str {
        &self.key.version
    }

    pub fn workspace_identity(&self) -> &str {
        &self.workspace_identity
    }

    pub fn normalized_arguments(&self) -> &Value {
        &self.normalized_arguments
    }

    pub fn effects(&self) -> &EffectSet {
        &self.effects
    }
}

impl fmt::Debug for ExecutionPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionPermit")
            .field("tool_id", &self.key.id)
            .field("tool_version", &self.key.version)
            .field("workspace_identity", &self.workspace_identity)
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct ToolExecutionOutput {
    value: Value,
}

impl ToolExecutionOutput {
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }
}

struct AdapterPlan {
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    payload: PlannedPayload,
}

struct AdapterRevalidation {
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    payload: ExecutionPayload,
}

enum PlannedPayload {
    WorkspaceRead,
    WorkspaceMutation(Box<PreparedWorkspaceMutation>),
    Process(Box<ProcessRequest>),
    #[cfg(test)]
    Test,
}

enum ExecutionPayload {
    WorkspaceRead,
    WorkspaceMutation(Box<PreparedWorkspaceMutation>),
    Process(Box<ProcessRequest>),
    #[cfg(test)]
    Test,
}

/// Private trait: only daemon-owned adapters in this module can be registered.
trait SealedToolAdapter: Send + Sync {
    fn supports(&self, key: &ToolKey) -> bool;
    fn plan(&self, arguments: Value) -> Result<AdapterPlan, ToolRegistryError>;
    fn revalidate(
        &self,
        workspace_identity: &str,
        normalized_arguments: &Value,
        effects: &EffectSet,
        payload: PlannedPayload,
    ) -> Result<AdapterRevalidation, ToolRegistryError>;
    fn execute(
        &self,
        normalized_arguments: Value,
        payload: ExecutionPayload,
        control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError>;

    fn execute_async<'a>(
        &'a self,
        normalized_arguments: Value,
        payload: ExecutionPayload,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolRegistryError>> + Send + 'a>> {
        Box::pin(async move { self.execute(normalized_arguments, payload, None) })
    }
}

#[derive(Clone, Copy, Debug)]
enum WorkspaceReadOperation {
    List,
    Read,
    Search,
}

impl WorkspaceReadOperation {
    const fn tool_id(self) -> &'static str {
        match self {
            Self::List => WORKSPACE_LIST_TOOL_ID,
            Self::Read => WORKSPACE_READ_TOOL_ID,
            Self::Search => WORKSPACE_SEARCH_TOOL_ID,
        }
    }
}

#[derive(Debug)]
struct WorkspaceReadAdapter {
    tools: Arc<WorkspaceTools>,
    operation: WorkspaceReadOperation,
}

impl WorkspaceReadAdapter {
    fn new(tools: Arc<WorkspaceTools>, operation: WorkspaceReadOperation) -> Self {
        Self { tools, operation }
    }

    fn error(&self, source: WorkspaceToolError) -> ToolRegistryError {
        ToolRegistryError::WorkspaceTool {
            tool_id: self.operation.tool_id().to_owned(),
            tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            source,
        }
    }
}

impl SealedToolAdapter for WorkspaceReadAdapter {
    fn supports(&self, key: &ToolKey) -> bool {
        key.id == self.operation.tool_id() && key.version == WORKSPACE_TOOL_VERSION
    }

    fn plan(&self, arguments: Value) -> Result<AdapterPlan, ToolRegistryError> {
        let effects = self
            .tools
            .prepare_effects(self.operation.tool_id(), &arguments)
            .map_err(|error| self.error(error))?;
        let normalized_arguments = normalize_workspace_read_arguments(arguments, &effects);
        Ok(AdapterPlan {
            workspace_identity: self.tools.workspace().identity().to_owned(),
            normalized_arguments,
            effects,
            payload: PlannedPayload::WorkspaceRead,
        })
    }

    fn revalidate(
        &self,
        workspace_identity: &str,
        normalized_arguments: &Value,
        _effects: &EffectSet,
        payload: PlannedPayload,
    ) -> Result<AdapterRevalidation, ToolRegistryError> {
        if !matches!(payload, PlannedPayload::WorkspaceRead) {
            return Err(ToolRegistryError::AdapterPayloadMismatch {
                tool_id: self.operation.tool_id().to_owned(),
                tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            });
        }
        if workspace_identity != self.tools.workspace().identity() {
            return Err(ToolRegistryError::PlanChanged {
                field: "workspace_identity",
            });
        }
        let effects = self
            .tools
            .prepare_effects(self.operation.tool_id(), normalized_arguments)
            .map_err(|error| self.error(error))?;
        Ok(AdapterRevalidation {
            workspace_identity: self.tools.workspace().identity().to_owned(),
            normalized_arguments: normalized_arguments.clone(),
            effects,
            payload: ExecutionPayload::WorkspaceRead,
        })
    }

    fn execute(
        &self,
        normalized_arguments: Value,
        payload: ExecutionPayload,
        control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError> {
        if !matches!(payload, ExecutionPayload::WorkspaceRead) {
            return Err(ToolRegistryError::AdapterPayloadMismatch {
                tool_id: self.operation.tool_id().to_owned(),
                tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            });
        }
        let result = match control {
            Some(control) => self.tools.execute_with_control(
                self.operation.tool_id(),
                normalized_arguments,
                control,
            ),
            None => self
                .tools
                .execute(self.operation.tool_id(), normalized_arguments),
        };
        result.map_err(|error| self.error(error))
    }
}

#[derive(Debug)]
struct WorkspaceMutationAdapter {
    tools: Arc<WorkspaceTools>,
}

impl WorkspaceMutationAdapter {
    fn new(tools: Arc<WorkspaceTools>) -> Self {
        Self { tools }
    }

    fn error(source: WorkspaceToolError) -> ToolRegistryError {
        ToolRegistryError::WorkspaceTool {
            tool_id: WORKSPACE_APPLY_PATCH_TOOL_ID.to_owned(),
            tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            source,
        }
    }
}

impl SealedToolAdapter for WorkspaceMutationAdapter {
    fn supports(&self, key: &ToolKey) -> bool {
        key.id == WORKSPACE_APPLY_PATCH_TOOL_ID && key.version == WORKSPACE_TOOL_VERSION
    }

    fn plan(&self, arguments: Value) -> Result<AdapterPlan, ToolRegistryError> {
        let prepared = self
            .tools
            .prepare_mutation(WORKSPACE_APPLY_PATCH_TOOL_ID, &arguments)
            .map_err(Self::error)?;
        Ok(AdapterPlan {
            workspace_identity: prepared.workspace_identity().to_owned(),
            normalized_arguments: prepared.normalized_arguments().clone(),
            effects: prepared.effects().clone(),
            payload: PlannedPayload::WorkspaceMutation(Box::new(prepared)),
        })
    }

    fn revalidate(
        &self,
        workspace_identity: &str,
        normalized_arguments: &Value,
        _effects: &EffectSet,
        payload: PlannedPayload,
    ) -> Result<AdapterRevalidation, ToolRegistryError> {
        let PlannedPayload::WorkspaceMutation(previous) = payload else {
            return Err(ToolRegistryError::AdapterPayloadMismatch {
                tool_id: WORKSPACE_APPLY_PATCH_TOOL_ID.to_owned(),
                tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            });
        };
        if workspace_identity != self.tools.workspace().identity() {
            return Err(ToolRegistryError::PlanChanged {
                field: "workspace_identity",
            });
        }
        let prepared = self
            .tools
            .prepare_mutation(WORKSPACE_APPLY_PATCH_TOOL_ID, normalized_arguments)
            .map_err(Self::error)?;
        if prepared.diff_summary() != previous.diff_summary() {
            return Err(ToolRegistryError::PlanChanged {
                field: "mutation_summary",
            });
        }
        Ok(AdapterRevalidation {
            workspace_identity: prepared.workspace_identity().to_owned(),
            normalized_arguments: prepared.normalized_arguments().clone(),
            effects: prepared.effects().clone(),
            payload: ExecutionPayload::WorkspaceMutation(Box::new(prepared)),
        })
    }

    fn execute(
        &self,
        _normalized_arguments: Value,
        payload: ExecutionPayload,
        _control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError> {
        let ExecutionPayload::WorkspaceMutation(prepared) = payload else {
            return Err(ToolRegistryError::AdapterPayloadMismatch {
                tool_id: WORKSPACE_APPLY_PATCH_TOOL_ID.to_owned(),
                tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
            });
        };
        self.tools
            .execute_prepared_mutation(&prepared)
            .map_err(Self::error)
    }
}

/// Environment and stdin are intentionally absent from the schema. They
/// are broker/policy capabilities, never provider-controlled fields.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessArguments {
    executable: String,
    #[serde(default)]
    arguments: Vec<String>,
    #[serde(default = "default_process_cwd")]
    cwd: String,
}

fn default_process_cwd() -> String {
    ".".into()
}

#[derive(Debug)]
struct ProcessAdapter {
    tools: Arc<WorkspaceTools>,
    executor: Arc<ProcessExecutor>,
}

impl ProcessAdapter {
    fn new(tools: Arc<WorkspaceTools>, executor: Arc<ProcessExecutor>) -> Self {
        Self { tools, executor }
    }

    fn invalid(message: impl Into<String>) -> ToolRegistryError {
        ToolRegistryError::InvalidProcessArguments(message.into())
    }

    fn parse(
        &self,
        arguments: Value,
    ) -> Result<(Value, ProcessRequest, EffectSet), ToolRegistryError> {
        let parsed: ProcessArguments =
            serde_json::from_value(arguments).map_err(|error| Self::invalid(error.to_string()))?;
        if parsed.executable.is_empty() {
            return Err(Self::invalid("executable must not be empty"));
        }
        if !Path::new(&parsed.executable).is_absolute() {
            return Err(Self::invalid("executable must be absolute"));
        }
        if parsed.arguments.len() > 128 {
            return Err(Self::invalid("argument count exceeds 128"));
        }
        let argument_bytes = parsed.arguments.iter().map(String::len).sum::<usize>();
        if argument_bytes > 256 * 1024 {
            return Err(Self::invalid("serialized arguments exceed 262144 bytes"));
        }
        if parsed
            .arguments
            .iter()
            .any(|argument| argument.len() > 64 * 1024)
        {
            return Err(Self::invalid("an argument exceeds 65536 bytes"));
        }
        let executable = fs::canonicalize(&parsed.executable)
            .map_err(|error| Self::invalid(format!("executable is unavailable: {error}")))?;
        if !fs::metadata(&executable)
            .map_err(|error| Self::invalid(error.to_string()))?
            .is_file()
        {
            return Err(Self::invalid("executable is not a regular file"));
        }
        let cwd = self
            .tools
            .workspace()
            .resolve_directory(&parsed.cwd)
            .map_err(|error| Self::invalid(error.to_string()))?;
        let relative_cwd = cwd
            .strip_prefix(self.tools.workspace().root())
            .map_err(|_| Self::invalid("cwd escapes the workspace"))?;
        let cwd_value = if relative_cwd.as_os_str().is_empty() {
            ".".to_owned()
        } else {
            relative_cwd.to_string_lossy().into_owned()
        };
        let args_value = Value::Array(
            parsed
                .arguments
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        );
        let argument_digest = digest_value(&args_value);
        let effects = EffectSet {
            processes: vec![ProcessEffect {
                executable: executable.to_string_lossy().into_owned(),
                argument_digest: Some(argument_digest),
                may_spawn_children: true,
            }],
            idempotency: Idempotency::Unknown,
            reversibility: Reversibility::Unknown,
            ..EffectSet::default()
        };
        let executable_string = executable.to_string_lossy().into_owned();
        let normalized = serde_json::json!({
            "executable": executable_string,
            "arguments": args_value,
            "cwd": cwd_value,
        });
        let mut request = ProcessRequest::new(executable.clone());
        request.arguments = parsed.arguments;
        request.cwd = Path::new(normalized["cwd"].as_str().unwrap_or(".")).to_owned();
        request.timeout = std::time::Duration::from_secs(5 * 60);
        request.output_limit_bytes = 8 * 1024 * 1024;
        // The daemon, not the provider, chooses the sandbox profile. Process
        // requests are always network-disabled and read-only at this adapter.
        request.sandbox.allow_network = false;
        request.sandbox.allow_workspace_write = false;
        Ok((normalized, request, effects))
    }

    fn process_error(error: ProcessError) -> ToolRegistryError {
        ToolRegistryError::Process {
            tool_id: PROCESS_RUN_TOOL_ID.into(),
            tool_version: PROCESS_TOOL_VERSION.into(),
            source: error,
        }
    }
}

impl SealedToolAdapter for ProcessAdapter {
    fn supports(&self, key: &ToolKey) -> bool {
        key.id == PROCESS_RUN_TOOL_ID && key.version == PROCESS_TOOL_VERSION
    }

    fn plan(&self, arguments: Value) -> Result<AdapterPlan, ToolRegistryError> {
        let (normalized_arguments, request, effects) = self.parse(arguments)?;
        Ok(AdapterPlan {
            workspace_identity: self.tools.workspace().identity().to_owned(),
            normalized_arguments,
            effects,
            payload: PlannedPayload::Process(Box::new(request)),
        })
    }

    fn revalidate(
        &self,
        workspace_identity: &str,
        normalized_arguments: &Value,
        effects: &EffectSet,
        payload: PlannedPayload,
    ) -> Result<AdapterRevalidation, ToolRegistryError> {
        let PlannedPayload::Process(previous) = payload else {
            return Err(ToolRegistryError::AdapterPayloadMismatch {
                tool_id: PROCESS_RUN_TOOL_ID.into(),
                tool_version: PROCESS_TOOL_VERSION.into(),
            });
        };
        if workspace_identity != self.tools.workspace().identity() {
            return Err(ToolRegistryError::PlanChanged {
                field: "workspace_identity",
            });
        }
        let (current_arguments, current_request, current_effects) =
            self.parse(normalized_arguments.clone())?;
        if current_arguments != *normalized_arguments || current_effects != *effects {
            return Err(ToolRegistryError::PlanChanged {
                field: "process_binding",
            });
        }
        if previous.executable != current_request.executable
            || previous.arguments != current_request.arguments
            || previous.cwd != current_request.cwd
        {
            return Err(ToolRegistryError::PlanChanged {
                field: "process_request",
            });
        }
        Ok(AdapterRevalidation {
            workspace_identity: self.tools.workspace().identity().to_owned(),
            normalized_arguments: current_arguments,
            effects: current_effects,
            payload: ExecutionPayload::Process(Box::new(current_request)),
        })
    }

    fn execute(
        &self,
        _normalized_arguments: Value,
        _payload: ExecutionPayload,
        _control: Option<&WorkspaceSearchControl<'_>>,
    ) -> Result<Value, ToolRegistryError> {
        Err(ToolRegistryError::ProcessRequiresAsync)
    }

    fn execute_async<'a>(
        &'a self,
        _normalized_arguments: Value,
        payload: ExecutionPayload,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolRegistryError>> + Send + 'a>> {
        let workspace = self.tools.workspace().clone();
        let executor = Arc::clone(&self.executor);
        Box::pin(async move {
            let ExecutionPayload::Process(request) = payload else {
                return Err(ToolRegistryError::AdapterPayloadMismatch {
                    tool_id: PROCESS_RUN_TOOL_ID.into(),
                    tool_version: PROCESS_TOOL_VERSION.into(),
                });
            };
            let output = executor
                .execute(&workspace, *request)
                .await
                .map_err(Self::process_error)?;
            Ok(process_output(output))
        })
    }
}

fn process_output(output: yeux_runtime::ProcessOutput) -> Value {
    serde_json::json!({
        "exit_code": output.exit_code,
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "stdout_truncated": output.stdout_truncated,
        "stderr_truncated": output.stderr_truncated,
        "timed_out": output.timed_out,
        "duration_ms": output.duration.as_millis(),
    })
}

pub fn process_run_spec() -> ToolSpec {
    ToolSpec {
        id: PROCESS_RUN_TOOL_ID.into(),
        version: PROCESS_TOOL_VERSION.into(),
        description: "Run one absolute executable inside the daemon OS sandbox".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["executable"],
            "properties": {
                "executable": {"type": "string", "minLength": 1},
                "arguments": {
                    "type": "array",
                    "maxItems": 128,
                    "items": {"type": "string", "maxLength": 65536}
                },
                "cwd": {"type": "string", "default": "."}
            }
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "exit_code": {"type": ["integer", "null"]},
                "stdout": {"type": "string"},
                "stderr": {"type": "string"},
                "stdout_truncated": {"type": "boolean"},
                "stderr_truncated": {"type": "boolean"},
                "timed_out": {"type": "boolean"},
                "duration_ms": {"type": "integer"}
            }
        }),
        effect_template: EffectSet {
            processes: vec![ProcessEffect {
                executable: "*".into(),
                argument_digest: None,
                may_spawn_children: true,
            }],
            idempotency: Idempotency::Unknown,
            reversibility: Reversibility::Unknown,
            ..EffectSet::default()
        },
        concurrency: ConcurrencyClass::SerialProcess,
        timeout_ms: 5 * 60 * 1_000,
        inline_output_budget_bytes: 8 * 1024 * 1024,
    }
}

fn normalize_workspace_read_arguments(arguments: Value, effects: &EffectSet) -> Value {
    let mut object = arguments.as_object().cloned().unwrap_or_else(Map::new);
    if let Some(scope) = effects.filesystem_read.first() {
        // Execute the same canonical workspace-relative path that was placed
        // in the concrete effect set, rather than retaining an alias supplied
        // by the provider.
        object.insert("path".to_owned(), Value::String(scope.path.clone()));
    }
    Value::Object(object)
}

fn validate_spec(index: usize, spec: &ToolSpec) -> Result<(), ToolRegistryError> {
    validate_identifier(
        index,
        "id",
        &spec.id,
        MAX_TOOL_ID_BYTES,
        is_tool_id_character,
    )?;
    validate_identifier(
        index,
        "version",
        &spec.version,
        MAX_TOOL_VERSION_BYTES,
        is_tool_version_character,
    )?;
    validate_nonempty_bounded(
        index,
        "description",
        &spec.description,
        MAX_TOOL_DESCRIPTION_BYTES,
    )?;
    validate_schema(index, "input_schema", &spec.input_schema)?;
    validate_schema(index, "output_schema", &spec.output_schema)?;
    validate_nonzero_limit(index, "timeout_ms", spec.timeout_ms, MAX_TOOL_TIMEOUT_MS)?;
    validate_nonzero_limit(
        index,
        "inline_output_budget_bytes",
        spec.inline_output_budget_bytes,
        MAX_TOOL_INLINE_OUTPUT_BYTES,
    )?;
    validate_effect_template(index, spec)?;
    Ok(())
}

fn validate_identifier(
    index: usize,
    field: &'static str,
    value: &str,
    limit: usize,
    allowed: fn(char) -> bool,
) -> Result<(), ToolRegistryError> {
    validate_nonempty_bounded(index, field, value, limit)?;
    if value.trim() != value
        || !value.is_ascii()
        || !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !value.chars().all(allowed)
    {
        return Err(ToolRegistryError::InvalidSpecIdentifier {
            index,
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn is_tool_id_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

fn is_tool_version_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '+')
}

fn validate_nonempty_bounded(
    index: usize,
    field: &'static str,
    value: &str,
    limit: usize,
) -> Result<(), ToolRegistryError> {
    if value.trim().is_empty() {
        return Err(ToolRegistryError::EmptySpecField { index, field });
    }
    if value.len() > limit {
        return Err(ToolRegistryError::SpecLimit {
            index,
            field,
            actual: value.len() as u64,
            limit: limit as u64,
        });
    }
    Ok(())
}

fn validate_schema(
    index: usize,
    field: &'static str,
    schema: &Value,
) -> Result<(), ToolRegistryError> {
    let encoded = serde_json::to_vec(schema).map_err(|error| ToolRegistryError::InvalidSpec {
        index,
        field,
        message: error.to_string(),
    })?;
    if encoded.len() > MAX_TOOL_SCHEMA_BYTES {
        return Err(ToolRegistryError::SpecLimit {
            index,
            field,
            actual: encoded.len() as u64,
            limit: MAX_TOOL_SCHEMA_BYTES as u64,
        });
    }
    let object = schema
        .as_object()
        .ok_or_else(|| ToolRegistryError::InvalidSpec {
            index,
            field,
            message: "schema root must be an object".to_owned(),
        })?;
    if object.get("type") != Some(&Value::String("object".to_owned())) {
        return Err(ToolRegistryError::InvalidSpec {
            index,
            field,
            message: "schema root type must be object".to_owned(),
        });
    }
    if let Some(properties) = object.get("properties") {
        if !properties.is_object() {
            return Err(ToolRegistryError::InvalidSpec {
                index,
                field,
                message: "properties must be an object".to_owned(),
            });
        }
    }
    if let Some(required) = object.get("required") {
        let Some(required) = required.as_array() else {
            return Err(ToolRegistryError::InvalidSpec {
                index,
                field,
                message: "required must be an array".to_owned(),
            });
        };
        if required.iter().any(|entry| !entry.is_string()) {
            return Err(ToolRegistryError::InvalidSpec {
                index,
                field,
                message: "required entries must be strings".to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_nonzero_limit(
    index: usize,
    field: &'static str,
    value: u64,
    limit: u64,
) -> Result<(), ToolRegistryError> {
    if value == 0 || value > limit {
        return Err(ToolRegistryError::SpecLimit {
            index,
            field,
            actual: value,
            limit,
        });
    }
    Ok(())
}

fn validate_effect_template(index: usize, spec: &ToolSpec) -> Result<(), ToolRegistryError> {
    let effects = &spec.effect_template;
    let entry_count = effects
        .filesystem_read
        .len()
        .saturating_add(effects.filesystem_write.len())
        .saturating_add(effects.filesystem_delete.len())
        .saturating_add(effects.processes.len())
        .saturating_add(effects.network.len())
        .saturating_add(effects.secrets.len())
        .saturating_add(effects.external_writes.len());
    if entry_count > MAX_EFFECT_ENTRIES {
        return Err(ToolRegistryError::SpecLimit {
            index,
            field: "effect_template_entries",
            actual: entry_count as u64,
            limit: MAX_EFFECT_ENTRIES as u64,
        });
    }
    if matches!(spec.concurrency, ConcurrencyClass::StructuredReadOnly) && !effects.is_read_only() {
        return Err(ToolRegistryError::InvalidSpec {
            index,
            field: "effect_template",
            message: "structured_read_only tools must have read-only effect templates".to_owned(),
        });
    }
    for scope in effects
        .filesystem_read
        .iter()
        .chain(&effects.filesystem_write)
        .chain(&effects.filesystem_delete)
    {
        validate_template_path(index, scope)?;
    }
    for value in effect_strings(effects) {
        if value.len() > MAX_EFFECT_STRING_BYTES {
            return Err(ToolRegistryError::SpecLimit {
                index,
                field: "effect_template_string",
                actual: value.len() as u64,
                limit: MAX_EFFECT_STRING_BYTES as u64,
            });
        }
    }
    Ok(())
}

fn validate_template_path(index: usize, scope: &PathScope) -> Result<(), ToolRegistryError> {
    let path = Path::new(&scope.path);
    if scope.path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err(ToolRegistryError::InvalidSpec {
            index,
            field: "effect_template",
            message: format!("invalid workspace-relative path scope: {}", scope.path),
        });
    }
    Ok(())
}

fn effect_strings(effects: &EffectSet) -> impl Iterator<Item = &str> {
    effects
        .filesystem_read
        .iter()
        .chain(&effects.filesystem_write)
        .chain(&effects.filesystem_delete)
        .map(|scope| scope.path.as_str())
        .chain(effects.processes.iter().flat_map(|effect| {
            std::iter::once(effect.executable.as_str()).chain(effect.argument_digest.as_deref())
        }))
        .chain(effects.network.iter().flat_map(|effect| {
            std::iter::once(effect.scheme.as_str()).chain(std::iter::once(effect.host.as_str()))
        }))
        .chain(effects.secrets.iter().map(|effect| effect.name.as_str()))
        .chain(effects.external_writes.iter().flat_map(|effect| {
            std::iter::once(effect.system.as_str())
                .chain(std::iter::once(effect.operation.as_str()))
                .chain(effect.resource.as_deref())
        }))
}

fn verify_effect_subset(spec: &ToolSpec, concrete: &EffectSet) -> Result<(), ToolRegistryError> {
    let template = &spec.effect_template;
    let scopes_allowed = scopes_are_subset(&concrete.filesystem_read, &template.filesystem_read)
        && scopes_are_subset(&concrete.filesystem_write, &template.filesystem_write)
        && scopes_are_subset(&concrete.filesystem_delete, &template.filesystem_delete);
    let entries_allowed = concrete.processes.iter().all(|effect| {
        template.processes.iter().any(|allowed| {
            (allowed.executable == "*" || allowed.executable == effect.executable)
                && (allowed.argument_digest.is_none()
                    || allowed.argument_digest == effect.argument_digest)
                && (!effect.may_spawn_children || allowed.may_spawn_children)
        })
    }) && concrete
        .network
        .iter()
        .all(|effect| template.network.contains(effect))
        && concrete
            .secrets
            .iter()
            .all(|effect| template.secrets.contains(effect))
        && concrete
            .external_writes
            .iter()
            .all(|effect| template.external_writes.contains(effect));
    if !scopes_allowed
        || !entries_allowed
        || concrete.idempotency != template.idempotency
        || concrete.reversibility != template.reversibility
    {
        return Err(ToolRegistryError::EffectEscalation {
            tool_id: spec.id.clone(),
            tool_version: spec.version.clone(),
        });
    }
    Ok(())
}

fn scopes_are_subset(concrete: &[PathScope], templates: &[PathScope]) -> bool {
    concrete.iter().all(|scope| {
        scope.resolved
            && is_safe_concrete_path(&scope.path)
            && templates
                .iter()
                .any(|template| path_scope_contains(template, scope))
    })
}

fn is_safe_concrete_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
}

fn path_scope_contains(template: &PathScope, concrete: &PathScope) -> bool {
    let template_path = Path::new(&template.path);
    let concrete_path = Path::new(&concrete.path);
    if template.recursive {
        template_path == Path::new(".") || concrete_path.starts_with(template_path)
    } else {
        template_path == concrete_path && !concrete.recursive
    }
}

#[cfg(test)]
fn mint_test_permit(plan: RevalidatedToolPlan) -> ExecutionPermit {
    plan.into_execution_permit()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;
    use yeux_protocol::{Idempotency, Reversibility};
    use yeux_runtime::Workspace;

    use super::*;

    #[derive(Debug)]
    struct TestAdapter;

    impl SealedToolAdapter for TestAdapter {
        fn supports(&self, _key: &ToolKey) -> bool {
            true
        }

        fn plan(&self, arguments: Value) -> Result<AdapterPlan, ToolRegistryError> {
            Ok(AdapterPlan {
                workspace_identity: "test-workspace".to_owned(),
                normalized_arguments: arguments,
                effects: EffectSet::default(),
                payload: PlannedPayload::Test,
            })
        }

        fn revalidate(
            &self,
            workspace_identity: &str,
            normalized_arguments: &Value,
            effects: &EffectSet,
            payload: PlannedPayload,
        ) -> Result<AdapterRevalidation, ToolRegistryError> {
            assert!(matches!(payload, PlannedPayload::Test));
            Ok(AdapterRevalidation {
                workspace_identity: workspace_identity.to_owned(),
                normalized_arguments: normalized_arguments.clone(),
                effects: effects.clone(),
                payload: ExecutionPayload::Test,
            })
        }

        fn execute(
            &self,
            normalized_arguments: Value,
            payload: ExecutionPayload,
            _control: Option<&WorkspaceSearchControl<'_>>,
        ) -> Result<Value, ToolRegistryError> {
            assert!(matches!(payload, ExecutionPayload::Test));
            Ok(normalized_arguments)
        }
    }

    fn test_spec(id: &str, version: &str) -> ToolSpec {
        ToolSpec {
            id: id.to_owned(),
            version: version.to_owned(),
            description: "test tool".to_owned(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            effect_template: EffectSet::default(),
            concurrency: ConcurrencyClass::StructuredReadOnly,
            timeout_ms: 1_000,
            inline_output_budget_bytes: 1_024,
        }
    }

    fn test_registration(id: &str, version: &str, advertised: bool) -> RegisteredTool {
        let spec = test_spec(id, version);
        if advertised {
            RegisteredTool::advertised(spec, Arc::new(TestAdapter))
        } else {
            RegisteredTool::hidden(spec, Arc::new(TestAdapter))
        }
    }

    fn workspace_registry() -> (TempDir, ToolRegistry) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), "hello registry\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = ToolRegistry::workspace_built_ins(WorkspaceTools::new(workspace)).unwrap();
        (directory, registry)
    }

    #[test]
    fn empty_registry_is_rejected() {
        let error = ToolRegistry::try_new(Vec::new()).unwrap_err();
        assert_eq!(error.code(), "tool_registry_empty");
    }

    #[test]
    fn registration_count_is_bounded() {
        let registrations = (0..=MAX_REGISTERED_TOOLS)
            .map(|index| test_registration(&format!("tool.{index}"), "1", true))
            .collect();
        let error = ToolRegistry::try_new(registrations).unwrap_err();
        assert_eq!(error.code(), "tool_registry_registration_limit");
    }

    #[test]
    fn exact_duplicate_is_rejected_but_distinct_versions_are_allowed() {
        let duplicate = ToolRegistry::try_new(vec![
            test_registration("alpha", "1", true),
            test_registration("alpha", "1", true),
        ])
        .unwrap_err();
        assert_eq!(duplicate.code(), "tool_registry_duplicate_tool");

        let registry = ToolRegistry::try_new(vec![
            test_registration("alpha", "2", true),
            test_registration("alpha", "1", true),
        ])
        .unwrap();
        assert_eq!(registry.resolve_exact("alpha", "1").unwrap().version, "1");
        assert_eq!(registry.resolve_exact("alpha", "2").unwrap().version, "2");
        assert_eq!(
            registry.resolve_exact("alpha", "3").unwrap_err().code(),
            "tool_registry_unknown_tool"
        );
    }

    #[test]
    fn advertised_specs_are_stably_sorted_and_hidden_specs_stay_hidden() {
        let registry = ToolRegistry::try_new(vec![
            test_registration("zeta", "1", true),
            test_registration("alpha", "2", true),
            test_registration("hidden", "1", false),
            test_registration("alpha", "1", true),
        ])
        .unwrap();
        let keys = registry
            .advertised_specs()
            .iter()
            .map(|spec| format!("{}@{}", spec.id, spec.version))
            .collect::<Vec<_>>();
        assert_eq!(keys, ["alpha@1", "alpha@2", "zeta@1"]);
        assert!(registry.resolve_exact("hidden", "1").is_ok());
    }

    #[test]
    fn side_effecting_builtins_are_registered_only_as_hidden_adapters() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), "hello\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = ToolRegistry::workspace_built_ins_with_config_and_process(
            Arc::new(WorkspaceTools::new(workspace)),
            BuiltInToolRegistryConfig::read_only()
                .with_hidden_workspace_mutations()
                .with_hidden_process(),
            Some(Arc::new(ProcessExecutor::detect())),
        )
        .unwrap();
        assert!(registry.is_registered(WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_TOOL_VERSION));
        assert!(registry.is_registered(PROCESS_RUN_TOOL_ID, PROCESS_TOOL_VERSION));
        assert!(
            registry
                .advertised_specs()
                .iter()
                .all(|spec| spec.id != WORKSPACE_APPLY_PATCH_TOOL_ID
                    && spec.id != PROCESS_RUN_TOOL_ID)
        );
    }

    #[test]
    fn read_only_convenience_executor_cannot_bypass_the_side_effect_gate() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("hello.txt"), "hello\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = ToolRegistry::workspace_built_ins_with_config(
            WorkspaceTools::new(workspace),
            BuiltInToolRegistryConfig::read_only().with_hidden_workspace_mutations(),
        )
        .unwrap();
        let base = blake3::hash(b"hello\n").to_hex().to_string();
        let error = registry
            .execute_read_only(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({"path":"hello.txt", "base_revision":base, "replacement":"changed\n"}),
            )
            .unwrap_err();
        assert_eq!(error.code(), "tool_registry_effect_escalation");
    }

    #[test]
    fn empty_and_invalid_identifiers_are_rejected() {
        for (id, version, code) in [
            ("", "1", "tool_registry_empty_spec_field"),
            (" tool", "1", "tool_registry_invalid_identifier"),
            ("tool/name", "1", "tool_registry_invalid_identifier"),
            ("tool", "", "tool_registry_empty_spec_field"),
            ("tool", "v 1", "tool_registry_invalid_identifier"),
        ] {
            let error =
                ToolRegistry::try_new(vec![test_registration(id, version, true)]).unwrap_err();
            assert_eq!(error.code(), code, "{id}@{version}");
        }
    }

    #[test]
    fn empty_description_and_invalid_schemas_are_rejected() {
        let mut empty_description = test_spec("alpha", "1");
        empty_description.description = "  ".to_owned();
        let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
            empty_description,
            Arc::new(TestAdapter),
        )])
        .unwrap_err();
        assert_eq!(error.code(), "tool_registry_empty_spec_field");

        for schema in [json!(null), json!({}), json!({"type": "array"})] {
            let mut spec = test_spec("alpha", "1");
            spec.input_schema = schema;
            let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
                spec,
                Arc::new(TestAdapter),
            )])
            .unwrap_err();
            assert_eq!(error.code(), "tool_registry_invalid_spec");
        }
    }

    #[test]
    fn timeout_inline_output_and_schema_limits_are_fail_closed() {
        for (field, value) in [
            ("timeout_ms", 0),
            ("timeout_ms", MAX_TOOL_TIMEOUT_MS + 1),
            ("inline_output_budget_bytes", 0),
            (
                "inline_output_budget_bytes",
                MAX_TOOL_INLINE_OUTPUT_BYTES + 1,
            ),
        ] {
            let mut spec = test_spec("alpha", "1");
            match field {
                "timeout_ms" => spec.timeout_ms = value,
                _ => spec.inline_output_budget_bytes = value,
            }
            let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
                spec,
                Arc::new(TestAdapter),
            )])
            .unwrap_err();
            assert_eq!(error.code(), "tool_registry_spec_limit");
        }

        let mut spec = test_spec("alpha", "1");
        spec.input_schema = json!({
            "type": "object",
            "description": "x".repeat(MAX_TOOL_SCHEMA_BYTES)
        });
        let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
            spec,
            Arc::new(TestAdapter),
        )])
        .unwrap_err();
        assert_eq!(error.code(), "tool_registry_spec_limit");
    }

    #[test]
    fn identifier_and_description_byte_limits_are_enforced() {
        let mut cases = Vec::new();

        let mut id = test_spec("alpha", "1");
        id.id = format!("a{}", "x".repeat(MAX_TOOL_ID_BYTES));
        cases.push(id);

        let mut version = test_spec("alpha", "1");
        version.version = format!("1{}", "x".repeat(MAX_TOOL_VERSION_BYTES));
        cases.push(version);

        let mut description = test_spec("alpha", "1");
        description.description = "x".repeat(MAX_TOOL_DESCRIPTION_BYTES + 1);
        cases.push(description);

        for spec in cases {
            let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
                spec,
                Arc::new(TestAdapter),
            )])
            .unwrap_err();
            assert_eq!(error.code(), "tool_registry_spec_limit");
        }
    }

    #[test]
    fn read_only_concurrency_cannot_hide_write_effects() {
        let mut spec = test_spec("alpha", "1");
        spec.effect_template.filesystem_write.push(PathScope {
            path: ".".to_owned(),
            recursive: true,
            resolved: false,
        });
        let error = ToolRegistry::try_new(vec![RegisteredTool::advertised(
            spec,
            Arc::new(TestAdapter),
        )])
        .unwrap_err();
        assert_eq!(error.code(), "tool_registry_invalid_spec");
    }

    #[test]
    fn default_workspace_registry_only_resolves_and_advertises_reads() {
        let (_directory, registry) = workspace_registry();
        let advertised = registry
            .advertised_specs()
            .iter()
            .map(|spec| spec.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            advertised,
            [
                WORKSPACE_LIST_TOOL_ID,
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_SEARCH_TOOL_ID
            ]
        );
        assert_eq!(registry.registered_len(), 3);
        assert_eq!(
            registry
                .resolve_exact(WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_TOOL_VERSION)
                .unwrap_err()
                .code(),
            "tool_registry_unknown_tool"
        );
    }

    #[test]
    fn mutation_registration_is_explicit_and_remains_unadvertised() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let registry = ToolRegistry::workspace_built_ins_with_config(
            WorkspaceTools::new(workspace),
            BuiltInToolRegistryConfig::read_only().with_hidden_workspace_mutations(),
        )
        .unwrap();

        assert_eq!(registry.registered_len(), 4);
        assert_eq!(registry.advertised_len(), 3);
        assert!(registry
            .resolve_exact(WORKSPACE_APPLY_PATCH_TOOL_ID, WORKSPACE_TOOL_VERSION)
            .is_ok());
        assert!(registry
            .advertised_specs()
            .iter()
            .all(|spec| spec.id != WORKSPACE_APPLY_PATCH_TOOL_ID));
    }

    #[test]
    fn read_plan_normalizes_defaults_and_revalidates_exact_effects() {
        let (_directory, registry) = workspace_registry();
        let plan = registry
            .plan(WORKSPACE_LIST_TOOL_ID, WORKSPACE_TOOL_VERSION, json!({}))
            .unwrap();
        assert_eq!(plan.normalized_arguments(), &json!({"path": "."}));
        assert!(plan.effects().is_read_only());
        assert!(plan
            .effects()
            .filesystem_read
            .iter()
            .all(|scope| scope.resolved));

        let revalidated = registry.revalidate(plan).unwrap();
        assert_eq!(revalidated.tool_id(), WORKSPACE_LIST_TOOL_ID);
        assert_eq!(revalidated.normalized_arguments(), &json!({"path": "."}));
    }

    #[test]
    fn read_plan_replaces_provider_path_alias_with_resolved_effect_path() {
        let (_directory, registry) = workspace_registry();
        let plan = registry
            .plan(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({"path": "./hello.txt"}),
            )
            .unwrap();
        assert_eq!(plan.normalized_arguments(), &json!({"path": "hello.txt"}));
        assert_eq!(plan.effects().filesystem_read[0].path, "hello.txt");
    }

    #[test]
    fn permit_is_consumed_by_value_and_read_execution_uses_sealed_payload() {
        let (_directory, registry) = workspace_registry();
        let plan = registry
            .plan(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({"path": "hello.txt"}),
            )
            .unwrap();
        let revalidated = registry.revalidate(plan).unwrap();
        let permit = mint_test_permit(revalidated);
        let output = registry.execute(permit).unwrap().into_value();
        assert_eq!(output["content"], "hello registry\n");
    }

    #[test]
    fn bound_read_execution_requires_the_entire_proposal_binding() {
        let (_directory, registry) = workspace_registry();
        let plan = registry
            .plan(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({"path": "hello.txt"}),
            )
            .unwrap();
        let expected_workspace_identity = plan.workspace_identity().to_owned();
        let expected_normalized_arguments = plan.normalized_arguments().clone();
        let expected_effects = plan.effects().clone();

        let output = registry
            .execute_read_only_bound(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                &expected_workspace_identity,
                &expected_normalized_arguments,
                &expected_effects,
            )
            .unwrap();
        assert_eq!(output["content"], "hello registry\n");

        let error = registry
            .execute_read_only_bound(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                "different-workspace",
                &expected_normalized_arguments,
                &expected_effects,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ToolRegistryError::PlanChanged {
                field: "workspace_identity"
            }
        ));

        // A provider spelling that normalizes to the same target is still not
        // the persisted normalized proposal and must fail closed.
        let error = registry
            .execute_read_only_bound(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                &expected_workspace_identity,
                &json!({"path": "./hello.txt"}),
                &expected_effects,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ToolRegistryError::PlanChanged {
                field: "normalized_arguments"
            }
        ));

        let mut changed_effects = expected_effects.clone();
        changed_effects.filesystem_read[0].path = "other.txt".to_owned();
        let error = registry
            .execute_read_only_bound(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                &expected_workspace_identity,
                &expected_normalized_arguments,
                &changed_effects,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ToolRegistryError::PlanChanged { field: "effects" }
        ));

        // A failed binding check must not poison the registry or consume a
        // capability needed by a later correctly bound invocation.
        let output = registry
            .execute_read_only_bound(
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                &expected_workspace_identity,
                &expected_normalized_arguments,
                &expected_effects,
            )
            .unwrap();
        assert_eq!(output["content"], "hello registry\n");
    }

    #[test]
    fn plans_and_permits_are_bound_to_the_originating_registry() {
        let (_left_directory, left) = workspace_registry();
        let (_right_directory, right) = workspace_registry();
        let plan = left
            .plan(WORKSPACE_LIST_TOOL_ID, WORKSPACE_TOOL_VERSION, json!({}))
            .unwrap();
        assert_eq!(
            right.revalidate(plan).unwrap_err().code(),
            "tool_registry_foreign_plan"
        );

        let plan = left
            .plan(WORKSPACE_LIST_TOOL_ID, WORKSPACE_TOOL_VERSION, json!({}))
            .unwrap();
        let permit = mint_test_permit(left.revalidate(plan).unwrap());
        assert_eq!(
            right.execute(permit).unwrap_err().code(),
            "tool_registry_foreign_execution_permit"
        );
    }

    #[test]
    fn mutation_plan_revalidation_detects_stale_revision_before_permit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hello.txt");
        fs::write(&path, "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tools = WorkspaceTools::new(workspace);
        let read = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "hello.txt"}))
            .unwrap();
        let registry = ToolRegistry::workspace_built_ins_with_config(
            tools,
            BuiltInToolRegistryConfig::read_only().with_hidden_workspace_mutations(),
        )
        .unwrap();
        let plan = registry
            .plan(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({
                    "path": "hello.txt",
                    "base_revision": read["revision"],
                    "replacement": "after\n"
                }),
            )
            .unwrap();

        fs::write(&path, "external change\n").unwrap();
        let error = registry.revalidate(plan).unwrap_err();
        assert_eq!(error.code(), "workspace_stale_revision");
        assert_eq!(fs::read_to_string(path).unwrap(), "external change\n");
    }

    #[test]
    fn hidden_mutation_can_only_execute_through_an_execution_permit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hello.txt");
        fs::write(&path, "before\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let tools = WorkspaceTools::new(workspace);
        let read = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "hello.txt"}))
            .unwrap();
        let registry = ToolRegistry::workspace_built_ins_with_config(
            tools,
            BuiltInToolRegistryConfig::read_only().with_hidden_workspace_mutations(),
        )
        .unwrap();
        let plan = registry
            .plan(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                WORKSPACE_TOOL_VERSION,
                json!({
                    "path": "hello.txt",
                    "base_revision": read["revision"],
                    "replacement": "after\n"
                }),
            )
            .unwrap();
        assert_eq!(plan.effects().idempotency, Idempotency::IdempotentWithKey);
        assert_eq!(plan.effects().reversibility, Reversibility::Unknown);
        assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");

        let permit = mint_test_permit(registry.revalidate(plan).unwrap());
        let output = registry.execute(permit).unwrap();
        assert_eq!(output.value()["path"], "hello.txt");
        assert_eq!(fs::read_to_string(path).unwrap(), "after\n");
    }
}
