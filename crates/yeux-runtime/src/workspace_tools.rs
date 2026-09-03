//! Bounded, structured tools for an opened [`Workspace`].
//!
//! These tools intentionally do not expose a generic filesystem or process
//! primitive. Every invocation is schema-shaped, confined to the canonical
//! workspace, deterministically ordered, and subject to fixed resource caps.
//! Mutation registration and execution remain explicitly separate from the
//! current auto-approved read-only tool set.

#![allow(clippy::result_large_err)]

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use walkdir::WalkDir;
use yeux_protocol::{ConcurrencyClass, EffectSet, Idempotency, PathScope, Reversibility, ToolSpec};

use crate::{ApplyPatchError, FileRevisionSnapshot, Workspace, WorkspaceError};

pub const WORKSPACE_LIST_TOOL_ID: &str = "workspace.list";
pub const WORKSPACE_READ_TOOL_ID: &str = "workspace.read";
pub const WORKSPACE_SEARCH_TOOL_ID: &str = "workspace.search";
pub const WORKSPACE_APPLY_PATCH_TOOL_ID: &str = "workspace.apply_patch";
pub const WORKSPACE_TOOL_VERSION: &str = "1";

const MAX_QUERY_BYTES: usize = 4 * 1024;

/// The matcher checks its cancellation/deadline hook at most this many byte
/// operations apart.  Keeping the interval small bounds cancellation
/// latency without making an atomic flag or clock read part of every byte's
/// hot path.
const SEARCH_CONTROL_CHECK_INTERVAL: u64 = 1024;

/// KMP performs at most a small constant number of comparisons per haystack
/// byte.  This multiplier leaves room for prefix-table fallback comparisons
/// while still giving every search a finite CPU budget derived from its byte
/// budget.  Callers may narrow it through [`WorkspaceSearchControl`], never
/// widen it past this derived ceiling.
const SEARCH_OPERATION_MULTIPLIER: u64 = 4;

/// Hard upper bound for a shared, per-turn search operation budget.
///
/// One invocation is already bounded by the scan limit below.  Keeping the
/// aggregate default at that same ceiling means a turn cannot multiply the
/// worst-case matcher work by its (larger) tool-call budget.  Callers may
/// choose a smaller value, but [`SearchOperationBudget::new`] defensively
/// clamps values supplied by an untrusted adapter to this ceiling.
pub const WORKSPACE_SEARCH_HARD_OPERATION_LIMIT: u64 = WORKSPACE_TOOL_HARD_LIMITS
    .max_scan_bytes
    .saturating_mul(SEARCH_OPERATION_MULTIPLIER)
    .saturating_add((MAX_QUERY_BYTES as u64).saturating_mul(2));

/// Default aggregate operation allowance for one agent turn.
pub const WORKSPACE_SEARCH_DEFAULT_OPERATION_BUDGET: u64 = WORKSPACE_SEARCH_HARD_OPERATION_LIMIT;

/// Absolute per-invocation ceilings for the built-in workspace tools.
///
/// An executor may use [`WorkspaceToolLimits::try_new`] to choose smaller
/// limits, but it cannot raise any field above these values.
pub const WORKSPACE_TOOL_HARD_LIMITS: WorkspaceToolLimits = WorkspaceToolLimits {
    max_files: 10_000,
    max_depth: 32,
    max_file_bytes: 1024 * 1024,
    max_scan_bytes: 32 * 1024 * 1024,
    max_matches: 1_000,
    max_output_bytes: 8 * 1024 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceToolLimits {
    max_files: usize,
    max_depth: usize,
    max_file_bytes: u64,
    max_scan_bytes: u64,
    max_matches: usize,
    max_output_bytes: usize,
}

/// Compact, deterministic mutation evidence suitable for an approval surface
/// and append-only persistence. Compact stats PLUS a bounded unified diff so
/// the approval surface can show which lines changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiffSummary {
    pub changed: bool,
    pub previous_bytes: u64,
    pub replacement_bytes: u64,
    pub byte_delta: i64,
    pub previous_lines: u64,
    pub replacement_lines: u64,
    pub line_delta: i64,
    pub common_prefix_bytes: u64,
    pub common_suffix_bytes: u64,
    pub removed_bytes: u64,
    pub inserted_bytes: u64,
    pub first_changed_line: Option<u64>,
    pub unified_diff: String,
}

/// An indivisible preparation result for one structured workspace mutation.
///
/// The canonical arguments, concrete effects, and exact base-file identity are
/// kept together so a caller can bind all of them into one approval.
/// Construction is private to this module; execution revalidates the snapshot
/// before publishing any bytes.
#[derive(Clone, Serialize)]
pub struct PreparedWorkspaceMutation {
    tool_id: String,
    tool_version: String,
    workspace_identity: String,
    normalized_arguments: Value,
    effects: EffectSet,
    base_revision: FileRevisionSnapshot,
    diff_summary: WorkspaceDiffSummary,
}

impl std::fmt::Debug for PreparedWorkspaceMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedWorkspaceMutation")
            .field("tool_id", &self.tool_id)
            .field("tool_version", &self.tool_version)
            .field("workspace_identity", &self.workspace_identity)
            .field("effects", &self.effects)
            .field("base_revision", &self.base_revision)
            .field("diff_summary", &self.diff_summary)
            .finish_non_exhaustive()
    }
}

impl PreparedWorkspaceMutation {
    pub fn tool_id(&self) -> &str {
        &self.tool_id
    }

    pub fn tool_version(&self) -> &str {
        &self.tool_version
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

    /// Exact file identity captured while preparing the mutation.  This is
    /// authority evidence, not a capability; callers must revalidate it
    /// immediately before publishing bytes.
    pub fn base_revision(&self) -> &FileRevisionSnapshot {
        &self.base_revision
    }

    pub fn diff_summary(&self) -> &WorkspaceDiffSummary {
        &self.diff_summary
    }
}

impl Default for WorkspaceToolLimits {
    fn default() -> Self {
        WORKSPACE_TOOL_HARD_LIMITS
    }
}

impl WorkspaceToolLimits {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        max_files: usize,
        max_depth: usize,
        max_file_bytes: u64,
        max_scan_bytes: u64,
        max_matches: usize,
        max_output_bytes: usize,
    ) -> Result<Self, WorkspaceToolError> {
        validate_limit(
            "max_files",
            max_files as u64,
            WORKSPACE_TOOL_HARD_LIMITS.max_files as u64,
        )?;
        validate_limit(
            "max_depth",
            max_depth as u64,
            WORKSPACE_TOOL_HARD_LIMITS.max_depth as u64,
        )?;
        validate_limit(
            "max_file_bytes",
            max_file_bytes,
            WORKSPACE_TOOL_HARD_LIMITS.max_file_bytes,
        )?;
        validate_limit(
            "max_scan_bytes",
            max_scan_bytes,
            WORKSPACE_TOOL_HARD_LIMITS.max_scan_bytes,
        )?;
        validate_limit(
            "max_matches",
            max_matches as u64,
            WORKSPACE_TOOL_HARD_LIMITS.max_matches as u64,
        )?;
        validate_limit(
            "max_output_bytes",
            max_output_bytes as u64,
            WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64,
        )?;
        Ok(Self {
            max_files,
            max_depth,
            max_file_bytes,
            max_scan_bytes,
            max_matches,
            max_output_bytes,
        })
    }

    pub const fn max_files(self) -> usize {
        self.max_files
    }

    pub const fn max_depth(self) -> usize {
        self.max_depth
    }

    pub const fn max_file_bytes(self) -> u64 {
        self.max_file_bytes
    }

    pub const fn max_scan_bytes(self) -> u64 {
        self.max_scan_bytes
    }

    pub const fn max_matches(self) -> usize {
        self.max_matches
    }

    pub const fn max_output_bytes(self) -> usize {
        self.max_output_bytes
    }
}

/// A small cancellation port used by [`WorkspaceTools::execute_with_control`].
///
/// The runtime deliberately does not depend on the daemon's turn registry.
/// A caller can implement this trait with an atomic flag (or use a closure;
/// closures with the required bounds are supported below) and pass a borrowed
/// view into a search invocation.  The matcher samples the port at bounded
/// intervals, so a cancellation request is cooperative and never claims that
/// an already-running synchronous worker was forcibly terminated.
pub trait SearchCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

/// Allow daemon workers to share a cheap cancellation flag without coupling
/// the runtime crate to a particular command/turn implementation.
impl SearchCancellation for AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(Ordering::Acquire)
    }
}

impl<F> SearchCancellation for F
where
    F: Fn() -> bool + Send + Sync,
{
    fn is_cancelled(&self) -> bool {
        self()
    }
}

/// A thread-safe operation counter shared by all searches in one agent turn.
///
/// The counter is intentionally separate from the per-invocation derived
/// limit: a turn may issue many searches, so each worker must charge the same
/// aggregate budget.  `new` clamps the requested value to the runtime hard
/// ceiling; callers can therefore only narrow the allowance.
#[derive(Debug)]
pub struct SearchOperationBudget {
    limit: u64,
    operations: AtomicU64,
}

impl SearchOperationBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit: limit.min(WORKSPACE_SEARCH_HARD_OPERATION_LIMIT),
            operations: AtomicU64::new(0),
        }
    }

    pub const fn limit(&self) -> u64 {
        self.limit
    }

    pub fn operations(&self) -> u64 {
        self.operations.load(Ordering::Relaxed)
    }

    fn try_consume(&self) -> bool {
        let mut current = self.operations.load(Ordering::Relaxed);
        loop {
            if current >= self.limit {
                return false;
            }
            match self.operations.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Optional cooperative controls for a workspace tool invocation.
///
/// Controls are intentionally borrowed and non-owning: the daemon remains the
/// owner of cancellation state, while this crate only observes it.  A caller
/// supplied operation budget can narrow the hard, byte-derived search budget;
/// it can never widen that budget.
pub struct WorkspaceSearchControl<'a> {
    cancellation: Option<&'a dyn SearchCancellation>,
    deadline: Option<Instant>,
    max_operations: Option<u64>,
    shared_operation_budget: Option<&'a SearchOperationBudget>,
}

/// Short alias useful at call sites that already know the operation is a
/// workspace search.
pub type SearchControl<'a> = WorkspaceSearchControl<'a>;

impl<'a> WorkspaceSearchControl<'a> {
    pub const fn new() -> Self {
        Self {
            cancellation: None,
            deadline: None,
            max_operations: None,
            shared_operation_budget: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: &'a dyn SearchCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub const fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Set a relative deadline using the monotonic clock.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        let now = Instant::now();
        let deadline = now.checked_add(timeout).unwrap_or(now);
        self.with_deadline(deadline)
    }

    /// Narrow the derived operation budget.  Zero intentionally means that
    /// no matcher operation is allowed and therefore fails closed.
    pub const fn with_operation_budget(mut self, max_operations: u64) -> Self {
        self.max_operations = Some(max_operations);
        self
    }

    /// Charge matcher operations to a budget shared by a turn.  The budget is
    /// borrowed for the duration of this synchronous invocation and is never
    /// persisted as authority or evidence.
    pub fn with_shared_operation_budget(mut self, budget: &'a SearchOperationBudget) -> Self {
        self.shared_operation_budget = Some(budget);
        self
    }

    pub const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub const fn max_operations(&self) -> Option<u64> {
        self.max_operations
    }

    fn check_now(&self) -> Result<(), WorkspaceToolError> {
        if self
            .cancellation
            .is_some_and(|cancellation| cancellation.is_cancelled())
        {
            return Err(WorkspaceToolError::SearchCancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(WorkspaceToolError::SearchDeadlineExceeded);
        }
        Ok(())
    }

    fn check(&self, operations: u64, operation_limit: u64) -> Result<(), WorkspaceToolError> {
        self.check_now()?;
        if operations > operation_limit {
            return Err(WorkspaceToolError::SearchBudgetExceeded {
                limit: operation_limit,
            });
        }
        Ok(())
    }

    fn consume_shared_operation(&self) -> Result<(), WorkspaceToolError> {
        let Some(budget) = self.shared_operation_budget else {
            return Ok(());
        };
        if budget.try_consume() {
            return Ok(());
        }
        // Preserve cancellation/deadline precedence when a request races the
        // final aggregate operation slot.
        self.check_now()?;
        Err(WorkspaceToolError::SearchBudgetExceeded {
            limit: budget.limit(),
        })
    }
}

impl Default for WorkspaceSearchControl<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for WorkspaceSearchControl<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceSearchControl")
            .field("cancellation", &self.cancellation.is_some())
            .field("deadline", &self.deadline)
            .field("max_operations", &self.max_operations)
            .field(
                "shared_operation_budget",
                &self.shared_operation_budget.is_some(),
            )
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceToolError {
    #[error("unknown built-in workspace tool: {tool_id}")]
    UnknownTool { tool_id: String },
    #[error("invalid arguments for {tool_id}: {message}")]
    InvalidArguments { tool_id: String, message: String },
    #[error("invalid workspace tool limit {field}={value}; allowed range is 1..={hard_max}")]
    InvalidLimits {
        field: &'static str,
        value: u64,
        hard_max: u64,
    },
    #[error("invalid base revision for {tool_id}; expected 64 lowercase hexadecimal characters")]
    InvalidBaseRevision { tool_id: String },
    #[error("workspace replacement exceeds the {limit}-byte limit")]
    ReplacementBytesLimit { limit: u64 },
    #[error("prepared mutation belongs to workspace {expected}, not {actual}")]
    WorkspaceIdentityMismatch { expected: String, actual: String },
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    ApplyPatch(#[from] ApplyPatchError),
    #[error("workspace path is not valid UTF-8: {0}")]
    InvalidPathEncoding(PathBuf),
    #[error("workspace traversal exceeded the {limit}-entry limit")]
    FileCountLimit { limit: usize },
    #[error("workspace traversal exceeded depth {limit} at {path}")]
    DepthLimit { limit: usize, path: PathBuf },
    #[error("workspace file exceeds the {limit}-byte tool limit: {path}")]
    FileBytesLimit { path: String, limit: u64 },
    #[error("workspace search exceeds the {limit}-byte aggregate scan limit")]
    ScanBytesLimit { limit: u64 },
    #[error("workspace search query exceeds the {limit}-byte limit")]
    QueryBytesLimit { limit: usize },
    #[error("workspace search exceeds the {limit}-match limit")]
    MatchLimit { limit: usize },
    #[error("workspace search was cancelled")]
    SearchCancelled,
    #[error("workspace search deadline exceeded")]
    SearchDeadlineExceeded,
    #[error("workspace search operation budget exceeded ({limit} operations)")]
    SearchBudgetExceeded { limit: u64 },
    #[error("workspace tool output exceeds the {limit}-byte serialized limit")]
    OutputBytesLimit { limit: usize },
    #[error("failed to serialize workspace tool output: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl WorkspaceToolError {
    /// Stable machine-readable code suitable for protocol diagnostics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownTool { .. } => "workspace_unknown_tool",
            Self::InvalidArguments { .. } => "workspace_invalid_arguments",
            Self::InvalidLimits { .. } => "workspace_invalid_limits",
            Self::InvalidBaseRevision { .. } => "workspace_invalid_base_revision",
            Self::ReplacementBytesLimit { .. } => "workspace_replacement_bytes_limit",
            Self::WorkspaceIdentityMismatch { .. } => "workspace_identity_mismatch",
            Self::Workspace(error) => match error {
                WorkspaceError::Io(_) => "workspace_io",
                WorkspaceError::NotDirectory(_) => "workspace_not_directory",
                WorkspaceError::InvalidRelativePath(_) => "workspace_invalid_path",
                WorkspaceError::OutsideWorkspace { .. } => "workspace_path_escape",
                WorkspaceError::WorkspaceIdentityChanged { .. } => "workspace_identity_changed",
                WorkspaceError::WorkspaceIdentityDigestMismatch { .. } => {
                    "workspace_identity_digest_mismatch"
                }
                WorkspaceError::FileIdentityChanged { .. } => "workspace_file_identity_changed",
                WorkspaceError::FileChangedDuringRead(_) => "workspace_file_changed_during_read",
                WorkspaceError::RevisionChanged { .. } => "workspace_revision_changed",
                WorkspaceError::ResolvedPathChanged { .. } => "workspace_path_changed",
                WorkspaceError::NotFound(_) => "workspace_not_found",
                WorkspaceError::NotAFile(_) => "workspace_not_file",
                WorkspaceError::MultipleHardLinks(_) => "workspace_multiple_hard_links",
                WorkspaceError::ReadLimitExceeded { .. } => "workspace_file_bytes_limit",
                WorkspaceError::InvalidUtf8(_) => "workspace_invalid_utf8",
            },
            Self::ApplyPatch(error) => match error {
                ApplyPatchError::Workspace(error) => match error {
                    WorkspaceError::Io(_) => "workspace_io",
                    WorkspaceError::NotDirectory(_) => "workspace_not_directory",
                    WorkspaceError::InvalidRelativePath(_) => "workspace_invalid_path",
                    WorkspaceError::OutsideWorkspace { .. } => "workspace_path_escape",
                    WorkspaceError::WorkspaceIdentityChanged { .. } => "workspace_identity_changed",
                    WorkspaceError::WorkspaceIdentityDigestMismatch { .. } => {
                        "workspace_identity_digest_mismatch"
                    }
                    WorkspaceError::FileIdentityChanged { .. } => "workspace_file_identity_changed",
                    WorkspaceError::FileChangedDuringRead(_) => {
                        "workspace_file_changed_during_read"
                    }
                    WorkspaceError::RevisionChanged { .. } => "workspace_revision_changed",
                    WorkspaceError::ResolvedPathChanged { .. } => "workspace_path_changed",
                    WorkspaceError::NotFound(_) => "workspace_not_found",
                    WorkspaceError::NotAFile(_) => "workspace_not_file",
                    WorkspaceError::MultipleHardLinks(_) => "workspace_multiple_hard_links",
                    WorkspaceError::ReadLimitExceeded { .. } => "workspace_file_bytes_limit",
                    WorkspaceError::InvalidUtf8(_) => "workspace_invalid_utf8",
                },
                ApplyPatchError::Io(_) => "workspace_io",
                ApplyPatchError::StaleRevision { .. } => "workspace_stale_revision",
                ApplyPatchError::Persist(_) => "workspace_publish_failed",
            },
            Self::InvalidPathEncoding(_) => "workspace_path_not_utf8",
            Self::FileCountLimit { .. } => "workspace_file_count_limit",
            Self::DepthLimit { .. } => "workspace_depth_limit",
            Self::FileBytesLimit { .. } => "workspace_file_bytes_limit",
            Self::ScanBytesLimit { .. } => "workspace_scan_bytes_limit",
            Self::QueryBytesLimit { .. } => "workspace_query_bytes_limit",
            Self::MatchLimit { .. } => "workspace_match_limit",
            Self::SearchCancelled => "workspace_search_cancelled",
            Self::SearchDeadlineExceeded => "workspace_search_deadline_exceeded",
            Self::SearchBudgetExceeded { .. } => "workspace_search_budget_exceeded",
            Self::OutputBytesLimit { .. } => "workspace_output_bytes_limit",
            Self::Serialization(_) => "workspace_output_serialization",
        }
    }

    pub const fn retryable(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceTools {
    workspace: Workspace,
    limits: WorkspaceToolLimits,
}

impl WorkspaceTools {
    pub fn new(workspace: Workspace) -> Self {
        Self::with_limits(workspace, WorkspaceToolLimits::default())
    }

    /// Construct an executor with limits previously validated by
    /// [`WorkspaceToolLimits::try_new`]. Limits can only narrow the hard caps.
    pub fn with_limits(workspace: Workspace, limits: WorkspaceToolLimits) -> Self {
        Self { workspace, limits }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub const fn limits(&self) -> WorkspaceToolLimits {
        self.limits
    }

    pub fn execute(&self, tool_id: &str, arguments: Value) -> Result<Value, WorkspaceToolError> {
        self.execute_with_control(tool_id, arguments, &WorkspaceSearchControl::default())
    }

    /// Execute a workspace tool while observing optional cooperative search
    /// controls.  The controls are checked before dispatch and after the tool
    /// returns; `workspace.search` additionally samples them while traversing
    /// and matching bytes.  List/read retain their existing semantics and do
    /// not consume the search operation budget.
    pub fn execute_with_control(
        &self,
        tool_id: &str,
        arguments: Value,
        control: &WorkspaceSearchControl<'_>,
    ) -> Result<Value, WorkspaceToolError> {
        control.check_now()?;
        let output = match tool_id {
            WORKSPACE_LIST_TOOL_ID => self.list(parse_arguments(tool_id, arguments)?)?,
            WORKSPACE_READ_TOOL_ID => self.read(parse_arguments(tool_id, arguments)?)?,
            WORKSPACE_SEARCH_TOOL_ID => {
                self.search_with_control(parse_arguments(tool_id, arguments)?, control)?
            }
            _ => {
                return Err(WorkspaceToolError::UnknownTool {
                    tool_id: tool_id.to_owned(),
                });
            }
        };
        control.check_now()?;
        self.enforce_output_limit(output)
    }

    /// Explicitly named alias for callers that want to make the search
    /// cancellation contract visible at the call site.
    pub fn execute_with_search_control(
        &self,
        tool_id: &str,
        arguments: Value,
        control: &WorkspaceSearchControl<'_>,
    ) -> Result<Value, WorkspaceToolError> {
        self.execute_with_control(tool_id, arguments, control)
    }

    /// Validate one invocation without reading file contents and return the
    /// concrete, workspace-relative read effect recorded by the daemon.
    pub fn prepare_effects(
        &self,
        tool_id: &str,
        arguments: &Value,
    ) -> Result<EffectSet, WorkspaceToolError> {
        let (path, recursive) = match tool_id {
            WORKSPACE_LIST_TOOL_ID => {
                let arguments: ListArguments = parse_arguments(tool_id, arguments.clone())?;
                let directory = self.workspace.resolve_directory(&arguments.path)?;
                let relative = directory.strip_prefix(self.workspace.root()).map_err(|_| {
                    WorkspaceError::OutsideWorkspace {
                        root: self.workspace.root().to_owned(),
                        path: directory.clone(),
                    }
                })?;
                (effect_path(relative)?, true)
            }
            WORKSPACE_READ_TOOL_ID => {
                let arguments: ReadArguments = parse_arguments(tool_id, arguments.clone())?;
                let (relative, bytes) = self.workspace.resolve_regular_file(&arguments.path)?;
                if bytes > self.limits.max_file_bytes {
                    return Err(WorkspaceToolError::FileBytesLimit {
                        path: arguments.path,
                        limit: self.limits.max_file_bytes,
                    });
                }
                (effect_path(&relative)?, false)
            }
            WORKSPACE_SEARCH_TOOL_ID => {
                let arguments: SearchArguments = parse_arguments(tool_id, arguments.clone())?;
                validate_query(&arguments.query)?;
                let directory = self.workspace.resolve_directory(&arguments.path)?;
                let relative = directory.strip_prefix(self.workspace.root()).map_err(|_| {
                    WorkspaceError::OutsideWorkspace {
                        root: self.workspace.root().to_owned(),
                        path: directory.clone(),
                    }
                })?;
                (effect_path(relative)?, true)
            }
            _ => {
                return Err(WorkspaceToolError::UnknownTool {
                    tool_id: tool_id.to_owned(),
                });
            }
        };
        Ok(EffectSet {
            filesystem_read: vec![PathScope {
                path,
                recursive,
                resolved: true,
            }],
            idempotency: Idempotency::Idempotent,
            reversibility: Reversibility::Reversible,
            ..EffectSet::default()
        })
    }

    /// Normalize and validate a structured mutation before policy evaluation.
    ///
    /// This method performs no write. It requires a direct canonical
    /// workspace-relative target (symlink redirection is rejected), verifies
    /// UTF-8, the base revision, and size limits, then binds the normalized
    /// arguments to the exact effects.
    pub fn prepare_mutation(
        &self,
        tool_id: &str,
        arguments: &Value,
    ) -> Result<PreparedWorkspaceMutation, WorkspaceToolError> {
        match tool_id {
            WORKSPACE_APPLY_PATCH_TOOL_ID => {
                let arguments: ApplyPatchArguments = parse_arguments(tool_id, arguments.clone())?;
                validate_base_revision(tool_id, &arguments.base_revision)?;
                validate_replacement_size(arguments.replacement.as_bytes(), self.limits)?;

                let (before, base_revision) = self.workspace.read_resolved_limited_with_snapshot(
                    &arguments.path,
                    self.limits.max_file_bytes,
                )?;
                if before.revision != arguments.base_revision {
                    return Err(ApplyPatchError::StaleRevision {
                        path: before.relative_path,
                        expected: arguments.base_revision,
                        actual: before.revision,
                    }
                    .into());
                }
                validate_patch_target_utf8(&before.relative_path, &before.bytes)?;

                let path = tool_path(&before.relative_path)?;
                let diff_summary = WorkspaceDiffSummary::for_path(
                    &path,
                    &before.bytes,
                    arguments.replacement.as_bytes(),
                );
                let effects = mutation_effects(&path);
                let normalized_arguments = json!({
                    "path": path,
                    "base_revision": before.revision,
                    "replacement": arguments.replacement,
                });
                Ok(PreparedWorkspaceMutation {
                    tool_id: tool_id.to_owned(),
                    tool_version: WORKSPACE_TOOL_VERSION.to_owned(),
                    workspace_identity: self.workspace.identity().to_owned(),
                    normalized_arguments,
                    effects,
                    base_revision,
                    diff_summary,
                })
            }
            _ => Err(WorkspaceToolError::UnknownTool {
                tool_id: tool_id.to_owned(),
            }),
        }
    }

    /// Execute a mutation that was prepared by [`WorkspaceTools::prepare_mutation`].
    ///
    /// Callers must persist and authorize the plan's normalized arguments and
    /// effects before calling this method. Direct mutation execution is not
    /// exposed, keeping callers inside the daemon policy/approval pipeline.
    pub fn execute_prepared_mutation(
        &self,
        prepared: &PreparedWorkspaceMutation,
    ) -> Result<Value, WorkspaceToolError> {
        if prepared.workspace_identity != self.workspace.identity() {
            return Err(WorkspaceToolError::WorkspaceIdentityMismatch {
                expected: prepared.workspace_identity.clone(),
                actual: self.workspace.identity().to_owned(),
            });
        }
        if prepared.tool_id != WORKSPACE_APPLY_PATCH_TOOL_ID
            || prepared.tool_version != WORKSPACE_TOOL_VERSION
        {
            return Err(WorkspaceToolError::UnknownTool {
                tool_id: prepared.tool_id.clone(),
            });
        }

        let arguments: ApplyPatchArguments = parse_arguments(
            WORKSPACE_APPLY_PATCH_TOOL_ID,
            prepared.normalized_arguments.clone(),
        )?;
        validate_base_revision(WORKSPACE_APPLY_PATCH_TOOL_ID, &arguments.base_revision)?;
        validate_replacement_size(arguments.replacement.as_bytes(), self.limits)?;

        let actual_revision = self
            .workspace
            .revision_snapshot(&prepared.base_revision.relative_path)?;
        if actual_revision.device != prepared.base_revision.device
            || actual_revision.inode != prepared.base_revision.inode
        {
            return Err(WorkspaceError::FileIdentityChanged {
                path: prepared.base_revision.relative_path.clone(),
                expected_device: prepared.base_revision.device,
                expected_inode: prepared.base_revision.inode,
                actual_device: actual_revision.device,
                actual_inode: actual_revision.inode,
            }
            .into());
        }
        if actual_revision.revision != prepared.base_revision.revision {
            return Err(ApplyPatchError::StaleRevision {
                path: prepared.base_revision.relative_path.clone(),
                expected: prepared.base_revision.revision.clone(),
                actual: actual_revision.revision,
            }
            .into());
        }
        if actual_revision.byte_length != prepared.base_revision.byte_length {
            return Err(ApplyPatchError::StaleRevision {
                path: prepared.base_revision.relative_path.clone(),
                expected: prepared.base_revision.revision.clone(),
                actual: actual_revision.revision,
            }
            .into());
        }

        let before = self
            .workspace
            .read_resolved_limited(&arguments.path, self.limits.max_file_bytes)?;
        if before.revision != arguments.base_revision {
            return Err(ApplyPatchError::StaleRevision {
                path: before.relative_path,
                expected: arguments.base_revision,
                actual: before.revision,
            }
            .into());
        }
        validate_patch_target_utf8(&before.relative_path, &before.bytes)?;
        let path = tool_path(&before.relative_path)?;
        let diff_summary =
            WorkspaceDiffSummary::for_path(&path, &before.bytes, arguments.replacement.as_bytes());
        debug_assert_eq!(diff_summary, prepared.diff_summary);

        // The result is deterministic from the validated base and replacement.
        // Serialize and budget it before publishing so an output-budget failure
        // can never turn a successful filesystem mutation into a reported
        // failure with no durable result.
        let expected_revision = blake3::hash(arguments.replacement.as_bytes())
            .to_hex()
            .to_string();
        let output = self.enforce_output_limit(json!({
            "path": path.clone(),
            "previous_revision": arguments.base_revision.clone(),
            "revision": expected_revision.clone(),
            "bytes_written": arguments.replacement.len(),
            "diff_summary": diff_summary,
        }))?;

        let result = self.workspace.apply_patch_resolved(
            &arguments.path,
            &arguments.base_revision,
            arguments.replacement.as_bytes(),
        )?;
        debug_assert_eq!(result.relative_path, PathBuf::from(&path));
        debug_assert_eq!(result.previous_revision, arguments.base_revision);
        debug_assert_eq!(result.revision, expected_revision);
        debug_assert_eq!(result.bytes_written as usize, arguments.replacement.len());
        Ok(output)
    }

    fn list(&self, arguments: ListArguments) -> Result<Value, WorkspaceToolError> {
        let collection = self.collect_files(&arguments.path)?;
        let files = collection
            .files
            .into_iter()
            .map(|file| json!({"path": file.path, "bytes": file.bytes}))
            .collect::<Vec<_>>();
        Ok(json!({
            "path": collection.scope,
            "files": files,
        }))
    }

    fn read(&self, arguments: ReadArguments) -> Result<Value, WorkspaceToolError> {
        let file = self
            .workspace
            .read_limited(&arguments.path, self.limits.max_file_bytes)?;
        let path = tool_path(&file.relative_path)?;
        let byte_count = file.bytes.len();
        let content = String::from_utf8(file.bytes)
            .map_err(|_| WorkspaceError::InvalidUtf8(file.relative_path))?;
        Ok(json!({
            "path": path,
            "revision": file.revision,
            "bytes": byte_count,
            "content": content,
        }))
    }

    fn search_with_control(
        &self,
        arguments: SearchArguments,
        control: &WorkspaceSearchControl<'_>,
    ) -> Result<Value, WorkspaceToolError> {
        validate_query(&arguments.query)?;
        let operation_limit = derived_search_operation_limit(
            self.limits,
            arguments.query.len(),
            control.max_operations(),
        );
        let mut progress = SearchProgress::new(control, operation_limit);
        progress.check()?;
        let matcher = LiteralMatcher::new(arguments.query.as_bytes(), &mut progress)?;

        let collection = self.collect_files_with_control(&arguments.path, control)?;
        let mut matches = Vec::new();
        let mut files_scanned = 0_usize;
        let mut bytes_scanned = 0_u64;
        for entry in collection.files {
            control.check_now()?;
            if entry.bytes > self.limits.max_file_bytes {
                return Err(WorkspaceToolError::FileBytesLimit {
                    path: entry.path,
                    limit: self.limits.max_file_bytes,
                });
            }
            ensure_scan_budget(bytes_scanned, entry.bytes, self.limits.max_scan_bytes)?;
            let file = self
                .workspace
                .read_limited(&entry.relative_path, self.limits.max_file_bytes)?;
            let actual_bytes = u64::try_from(file.bytes.len()).unwrap_or(u64::MAX);
            ensure_scan_budget(bytes_scanned, actual_bytes, self.limits.max_scan_bytes)?;
            bytes_scanned += actual_bytes;
            files_scanned += 1;
            matcher.collect(
                &file.bytes,
                &entry.path,
                &mut matches,
                self.limits.max_matches,
                &mut progress,
            )?;
        }
        progress.check()?;

        Ok(json!({
            "path": collection.scope,
            "query": arguments.query,
            "matches": matches,
            "files_scanned": files_scanned,
            "bytes_scanned": bytes_scanned,
        }))
    }

    fn collect_files(&self, requested: &str) -> Result<FileCollection, WorkspaceToolError> {
        self.collect_files_with_control(requested, &WorkspaceSearchControl::default())
    }

    fn collect_files_with_control(
        &self,
        requested: &str,
        control: &WorkspaceSearchControl<'_>,
    ) -> Result<FileCollection, WorkspaceToolError> {
        let start = self.workspace.resolve_directory(requested)?;
        let scope_relative = start.strip_prefix(self.workspace.root()).map_err(|_| {
            WorkspaceError::OutsideWorkspace {
                root: self.workspace.root().to_owned(),
                path: start.clone(),
            }
        })?;
        let scope = tool_path(scope_relative)?;
        let mut files = Vec::new();
        let mut visited = 0_usize;
        let walk_depth = self.limits.max_depth.saturating_add(1);
        for entry in WalkDir::new(&start)
            .follow_links(false)
            .max_depth(walk_depth)
            .sort_by_file_name()
        {
            control.check_now()?;
            let entry = entry.map_err(walk_error)?;
            if entry.depth() == 0 {
                continue;
            }
            if entry.depth() > self.limits.max_depth {
                let relative = entry
                    .path()
                    .strip_prefix(self.workspace.root())
                    .unwrap_or_else(|_| entry.path())
                    .to_owned();
                return Err(WorkspaceToolError::DepthLimit {
                    limit: self.limits.max_depth,
                    path: relative,
                });
            }
            visited = visited.saturating_add(1);
            if visited > self.limits.max_files {
                return Err(WorkspaceToolError::FileCountLimit {
                    limit: self.limits.max_files,
                });
            }

            // Symlinks are intentionally not followed or surfaced as regular
            // files. Each regular file is re-opened through Workspace below,
            // which also rejects hard links and post-walk symlink swaps.
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(self.workspace.root())
                .map_err(|_| WorkspaceError::OutsideWorkspace {
                    root: self.workspace.root().to_owned(),
                    path: entry.path().to_owned(),
                })?
                .to_owned();
            let path = tool_path(&relative)?;
            let bytes = self.workspace.regular_file_size(&relative)?;
            files.push(FileEntry {
                relative_path: relative,
                path,
                bytes,
            });
        }
        control.check_now()?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(FileCollection { scope, files })
    }

    fn enforce_output_limit(&self, output: Value) -> Result<Value, WorkspaceToolError> {
        let encoded = serde_json::to_vec(&output)?;
        if encoded.len() > self.limits.max_output_bytes {
            return Err(WorkspaceToolError::OutputBytesLimit {
                limit: self.limits.max_output_bytes,
            });
        }
        Ok(output)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    #[serde(default)]
    path: String,
    query: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArguments {
    path: String,
    base_revision: String,
    replacement: String,
}

#[derive(Debug)]
struct FileEntry {
    relative_path: PathBuf,
    path: String,
    bytes: u64,
}

#[derive(Debug)]
struct FileCollection {
    scope: String,
    files: Vec<FileEntry>,
}

impl WorkspaceDiffSummary {
    fn for_path(path: &str, previous: &[u8], replacement: &[u8]) -> Self {
        let changed = previous != replacement;
        let common_prefix = common_prefix_len(previous, replacement);
        let common_suffix = if changed {
            common_suffix_len(&previous[common_prefix..], &replacement[common_prefix..])
        } else {
            0
        };
        let previous_bytes = previous.len() as u64;
        let replacement_bytes = replacement.len() as u64;
        let previous_lines = line_count(previous);
        let replacement_lines = line_count(replacement);
        Self {
            changed,
            previous_bytes,
            replacement_bytes,
            byte_delta: replacement_bytes as i64 - previous_bytes as i64,
            previous_lines,
            replacement_lines,
            line_delta: replacement_lines as i64 - previous_lines as i64,
            common_prefix_bytes: common_prefix as u64,
            common_suffix_bytes: common_suffix as u64,
            removed_bytes: previous.len().saturating_sub(common_prefix + common_suffix) as u64,
            inserted_bytes: replacement
                .len()
                .saturating_sub(common_prefix + common_suffix) as u64,
            first_changed_line: changed.then(|| {
                previous[..common_prefix]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64
                    + 1
            }),
            unified_diff: unified_diff_for_path(path, previous, replacement),
        }
    }
}

const UNIFIED_DIFF_CONTEXT: usize = 3;

fn split_unified_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if bytes.ends_with(b"\n") {
        lines.pop();
    }
    lines
}

fn unified_diff_for_path(path: &str, previous: &[u8], replacement: &[u8]) -> String {
    let old_lines = split_unified_lines(previous);
    let new_lines = split_unified_lines(replacement);
    let mut diff = format!("--- a/{path}\n+++ b/{path}\n");
    if old_lines == new_lines {
        return diff;
    }

    let mut prefix = 0;
    let min_len = old_lines.len().min(new_lines.len());
    while prefix < min_len && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }

    let mut suffix = 0;
    let old_rest = old_lines.len() - prefix;
    let new_rest = new_lines.len() - prefix;
    while suffix < old_rest
        && suffix < new_rest
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let prefix_context = prefix.min(UNIFIED_DIFF_CONTEXT);
    let suffix_context = suffix.min(UNIFIED_DIFF_CONTEXT);
    let old_start_idx = prefix - prefix_context;
    let old_end_idx = old_lines.len() - suffix + suffix_context;
    let new_end_idx = new_lines.len() - suffix + suffix_context;
    let old_count = old_end_idx - old_start_idx;
    let new_count = new_end_idx - old_start_idx;
    let old_start = if old_count == 0 { 0 } else { old_start_idx + 1 };
    let new_start = if new_count == 0 { 0 } else { old_start_idx + 1 };

    diff.push_str(&format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    ));
    for line in &old_lines[old_start_idx..prefix] {
        diff.push(' ');
        diff.push_str(&String::from_utf8_lossy(line));
        diff.push('\n');
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        diff.push('-');
        diff.push_str(&String::from_utf8_lossy(line));
        diff.push('\n');
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        diff.push('+');
        diff.push_str(&String::from_utf8_lossy(line));
        diff.push('\n');
    }
    for line in &old_lines[old_lines.len() - suffix..old_end_idx] {
        diff.push(' ');
        diff.push_str(&String::from_utf8_lossy(line));
        diff.push('\n');
    }
    diff
}

fn parse_arguments<T: DeserializeOwned>(
    tool_id: &str,
    arguments: Value,
) -> Result<T, WorkspaceToolError> {
    serde_json::from_value(arguments).map_err(|error| WorkspaceToolError::InvalidArguments {
        tool_id: tool_id.to_owned(),
        message: error.to_string(),
    })
}

fn invalid_arguments(tool_id: &str, message: impl Into<String>) -> WorkspaceToolError {
    WorkspaceToolError::InvalidArguments {
        tool_id: tool_id.to_owned(),
        message: message.into(),
    }
}

fn validate_limit(
    field: &'static str,
    value: u64,
    hard_max: u64,
) -> Result<(), WorkspaceToolError> {
    if value == 0 || value > hard_max {
        Err(WorkspaceToolError::InvalidLimits {
            field,
            value,
            hard_max,
        })
    } else {
        Ok(())
    }
}

fn validate_base_revision(tool_id: &str, revision: &str) -> Result<(), WorkspaceToolError> {
    if revision.len() == 64
        && revision
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(WorkspaceToolError::InvalidBaseRevision {
            tool_id: tool_id.to_owned(),
        })
    }
}

fn validate_replacement_size(
    replacement: &[u8],
    limits: WorkspaceToolLimits,
) -> Result<(), WorkspaceToolError> {
    if u64::try_from(replacement.len()).unwrap_or(u64::MAX) > limits.max_file_bytes {
        Err(WorkspaceToolError::ReplacementBytesLimit {
            limit: limits.max_file_bytes,
        })
    } else {
        Ok(())
    }
}

fn validate_patch_target_utf8(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceToolError> {
    std::str::from_utf8(bytes)
        .map(|_| ())
        .map_err(|_| WorkspaceError::InvalidUtf8(path.to_owned()).into())
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn common_suffix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
}

fn line_count(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() as u64
        + u64::from(bytes.last() != Some(&b'\n'))
}

fn ensure_scan_budget(scanned: u64, additional: u64, limit: u64) -> Result<(), WorkspaceToolError> {
    if scanned
        .checked_add(additional)
        .is_none_or(|total| total > limit)
    {
        Err(WorkspaceToolError::ScanBytesLimit { limit })
    } else {
        Ok(())
    }
}

fn walk_error(error: walkdir::Error) -> WorkspaceToolError {
    WorkspaceError::Io(
        error
            .into_io_error()
            .unwrap_or_else(|| std::io::Error::other("failed to walk workspace")),
    )
    .into()
}

fn tool_path(path: &Path) -> Result<String, WorkspaceToolError> {
    let mut result = String::new();
    for component in path.components() {
        if let std::path::Component::Normal(value) = component {
            let value = value
                .to_str()
                .ok_or_else(|| WorkspaceToolError::InvalidPathEncoding(path.to_owned()))?;
            if !result.is_empty() {
                result.push('/');
            }
            result.push_str(value);
        }
    }
    Ok(result)
}

fn effect_path(path: &Path) -> Result<String, WorkspaceToolError> {
    let path = tool_path(path)?;
    Ok(if path.is_empty() { ".".into() } else { path })
}

fn validate_query(query: &str) -> Result<(), WorkspaceToolError> {
    if query.is_empty() {
        return Err(invalid_arguments(
            WORKSPACE_SEARCH_TOOL_ID,
            "query must not be empty",
        ));
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(WorkspaceToolError::QueryBytesLimit {
            limit: MAX_QUERY_BYTES,
        });
    }
    Ok(())
}

/// Derive a finite matcher-operation ceiling from the existing byte ceiling.
///
/// The KMP matcher below is linear, but retaining a separate operation budget
/// protects the daemon if the implementation changes or if a caller supplies
/// a cancellation/deadline adapter that is delayed.  A caller-requested value
/// can only narrow this hard-derived limit.
fn derived_search_operation_limit(
    limits: WorkspaceToolLimits,
    query_bytes: usize,
    requested: Option<u64>,
) -> u64 {
    let query_bytes = u64::try_from(query_bytes).unwrap_or(u64::MAX);
    let derived = limits
        .max_scan_bytes
        .saturating_mul(SEARCH_OPERATION_MULTIPLIER)
        // Prefix-table construction can perform up to roughly two
        // comparisons per query byte, even when every file is shorter than
        // the query.  Budget that fixed overhead explicitly.
        .saturating_add(query_bytes.saturating_mul(2))
        .max(1);
    requested.map_or(derived, |requested| requested.min(derived))
}

/// Shared progress tracker for prefix construction and haystack scanning.
/// Checks are amortized over a bounded number of operations, while an
/// operation-limit boundary is always checked on the next operation.
struct SearchProgress<'control, 'cancel> {
    control: &'control WorkspaceSearchControl<'cancel>,
    operation_limit: u64,
    operations: u64,
    next_check: u64,
}

impl<'control, 'cancel> SearchProgress<'control, 'cancel> {
    fn new(control: &'control WorkspaceSearchControl<'cancel>, operation_limit: u64) -> Self {
        Self {
            control,
            operation_limit,
            operations: 0,
            next_check: 0,
        }
    }

    fn check(&self) -> Result<(), WorkspaceToolError> {
        self.control.check(self.operations, self.operation_limit)
    }

    fn step(&mut self) -> Result<(), WorkspaceToolError> {
        self.operations = self.operations.saturating_add(1);
        // Enforce the local derived ceiling before charging the shared turn
        // budget.  This avoids consuming aggregate allowance for an
        // operation that this invocation was never permitted to perform.
        if self.operations > self.operation_limit {
            self.control.check_now()?;
            return Err(WorkspaceToolError::SearchBudgetExceeded {
                limit: self.operation_limit,
            });
        }
        self.control.consume_shared_operation()?;
        if self.operations >= self.next_check {
            self.check()?;
            let next = self
                .operations
                .saturating_add(SEARCH_CONTROL_CHECK_INTERVAL);
            self.next_check = next.min(self.operation_limit.saturating_add(1));
        }
        Ok(())
    }
}

/// A byte-oriented Knuth–Morris–Pratt matcher.
///
/// KMP gives a strict `O(needle + haystack)` bound and, unlike
/// `memmem::find_iter`, preserves the old workspace.search behavior of
/// reporting overlapping matches (for example, `aaa` in `aaaaa` at offsets
/// 0, 1, and 2).
struct LiteralMatcher<'a> {
    needle: &'a [u8],
    prefix: Vec<usize>,
}

impl<'a> LiteralMatcher<'a> {
    fn new(
        needle: &'a [u8],
        progress: &mut SearchProgress<'_, '_>,
    ) -> Result<Self, WorkspaceToolError> {
        debug_assert!(!needle.is_empty());
        let mut prefix = vec![0; needle.len()];
        let mut index = 1_usize;
        let mut matched = 0_usize;
        while index < needle.len() {
            progress.step()?;
            if needle[index] == needle[matched] {
                matched += 1;
                prefix[index] = matched;
                index += 1;
            } else if matched > 0 {
                // No index advance here: the fallback is part of the KMP
                // prefix computation and remains amortized linear.
                matched = prefix[matched - 1];
            } else {
                index += 1;
            }
        }
        Ok(Self { needle, prefix })
    }

    fn collect(
        &self,
        haystack: &[u8],
        path: &str,
        output: &mut Vec<Value>,
        limit: usize,
        progress: &mut SearchProgress<'_, '_>,
    ) -> Result<(), WorkspaceToolError> {
        if self.needle.is_empty() {
            return Ok(());
        }
        if haystack.len() < self.needle.len() {
            // Still sample cancellation/deadline for a file that was read but
            // is too short to contain the query.
            progress.check()?;
            return Ok(());
        }

        let mut matched = 0_usize;
        // Match starts are monotonically increasing.  Advance this cursor
        // only as far as the next match so line/column accounting remains
        // linear even when the query has many overlapping matches.
        let mut position_cursor = 0_usize;
        let mut match_line = 1_u64;
        let mut match_line_start = 0_usize;
        for (offset, &byte) in haystack.iter().enumerate() {
            progress.step()?;

            while matched > 0 && byte != self.needle[matched] {
                progress.step()?;
                matched = self.prefix[matched - 1];
            }
            if byte == self.needle[matched] {
                matched += 1;
                if matched == self.needle.len() {
                    let match_offset = offset + 1 - self.needle.len();
                    // Give a newly requested cancellation/deadline priority
                    // over reporting a match-budget failure.  This check is
                    // intentionally immediately before the limit branch so
                    // a dense-match input cannot mask cooperative shutdown.
                    progress.check()?;
                    if output.len() >= limit {
                        return Err(WorkspaceToolError::MatchLimit { limit });
                    }
                    while position_cursor < match_offset {
                        progress.step()?;
                        if haystack[position_cursor] == b'\n' {
                            match_line = match_line.saturating_add(1);
                            match_line_start = position_cursor + 1;
                        }
                        position_cursor += 1;
                    }
                    output.push(json!({
                        "path": path,
                        "byte_offset": match_offset,
                        "line": match_line,
                        "column": match_offset - match_line_start + 1,
                    }));
                    // Fall back rather than clearing the state so overlapping
                    // matches retain the legacy semantics.
                    matched = self.prefix[matched - 1];
                }
            }
        }
        Ok(())
    }
}

/// Fixed registration specs for the three M1 built-in read-only tools.
pub fn workspace_tool_specs() -> Vec<ToolSpec> {
    vec![
        workspace_list_spec(),
        workspace_read_spec(),
        workspace_search_spec(),
    ]
}

/// Mutation specs are intentionally separate from [`workspace_tool_specs`].
/// The latter is consumed by the current auto-approved read-only runner; a
/// caller must explicitly opt into this list after policy and approval are in
/// the execution path.
pub fn workspace_mutation_tool_specs() -> Vec<ToolSpec> {
    vec![workspace_apply_patch_spec()]
}

pub fn workspace_apply_patch_spec() -> ToolSpec {
    ToolSpec {
        id: WORKSPACE_APPLY_PATCH_TOOL_ID.into(),
        version: WORKSPACE_TOOL_VERSION.into(),
        description: "Atomically replace one direct UTF-8 workspace file when its BLAKE3 base revision still matches.".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "base_revision", "replacement"],
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "base_revision": {
                    "type": "string",
                    "pattern": "^[0-9a-f]{64}$",
                    "description": "Revision returned by workspace.read."
                },
                "replacement": {
                    "type": "string",
                    "maxLength": WORKSPACE_TOOL_HARD_LIMITS.max_file_bytes,
                    "description": "Complete UTF-8 replacement content; the runtime enforces a byte limit."
                }
            }
        }),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "path",
                "previous_revision",
                "revision",
                "bytes_written",
                "diff_summary"
            ],
            "properties": {
                "path": {"type": "string"},
                "previous_revision": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                "revision": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                "bytes_written": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": WORKSPACE_TOOL_HARD_LIMITS.max_file_bytes
                },
                "diff_summary": diff_summary_schema()
            }
        }),
        effect_template: mutation_effect_template(),
        concurrency: ConcurrencyClass::SerialMutation,
        timeout_ms: 5_000,
        inline_output_budget_bytes: WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64,
    }
}

pub fn workspace_list_spec() -> ToolSpec {
    ToolSpec {
        id: WORKSPACE_LIST_TOOL_ID.into(),
        version: WORKSPACE_TOOL_VERSION.into(),
        description:
            "List regular files below a workspace-relative directory in stable path order.".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "default": "",
                    "description": "Workspace-relative directory; empty selects the workspace root."
                }
            }
        }),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "files"],
            "properties": {
                "path": {"type": "string"},
                "files": {
                    "type": "array",
                    "maxItems": WORKSPACE_TOOL_HARD_LIMITS.max_files,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["path", "bytes"],
                        "properties": {
                            "path": {"type": "string"},
                            "bytes": {"type": "integer", "minimum": 0}
                        }
                    }
                }
            }
        }),
        effect_template: read_effect_template(),
        concurrency: ConcurrencyClass::StructuredReadOnly,
        timeout_ms: 5_000,
        inline_output_budget_bytes: WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64,
    }
}

pub fn workspace_read_spec() -> ToolSpec {
    ToolSpec {
        id: WORKSPACE_READ_TOOL_ID.into(),
        version: WORKSPACE_TOOL_VERSION.into(),
        description: "Read one UTF-8 regular file from a workspace-relative path.".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "minLength": 1}
            }
        }),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "revision", "bytes", "content"],
            "properties": {
                "path": {"type": "string"},
                "revision": {"type": "string", "pattern": "^[0-9a-f]{64}$"},
                "bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": WORKSPACE_TOOL_HARD_LIMITS.max_file_bytes
                },
                "content": {"type": "string"}
            }
        }),
        effect_template: read_effect_template(),
        concurrency: ConcurrencyClass::StructuredReadOnly,
        timeout_ms: 5_000,
        inline_output_budget_bytes: WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64,
    }
}

pub fn workspace_search_spec() -> ToolSpec {
    ToolSpec {
        id: WORKSPACE_SEARCH_TOOL_ID.into(),
        version: WORKSPACE_TOOL_VERSION.into(),
        description:
            "Search literal UTF-8 query bytes across regular workspace files in stable path order."
                .into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": {
                "path": {
                    "type": "string",
                    "default": "",
                    "description": "Workspace-relative directory; empty selects the workspace root."
                },
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES}
            }
        }),
        output_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "query", "matches", "files_scanned", "bytes_scanned"],
            "properties": {
                "path": {"type": "string"},
                "query": {"type": "string"},
                "matches": {
                    "type": "array",
                    "maxItems": WORKSPACE_TOOL_HARD_LIMITS.max_matches,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["path", "byte_offset", "line", "column"],
                        "properties": {
                            "path": {"type": "string"},
                            "byte_offset": {"type": "integer", "minimum": 0},
                            "line": {"type": "integer", "minimum": 1},
                            "column": {"type": "integer", "minimum": 1}
                        }
                    }
                },
                "files_scanned": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": WORKSPACE_TOOL_HARD_LIMITS.max_files
                },
                "bytes_scanned": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": WORKSPACE_TOOL_HARD_LIMITS.max_scan_bytes
                }
            }
        }),
        effect_template: read_effect_template(),
        concurrency: ConcurrencyClass::StructuredReadOnly,
        timeout_ms: 5_000,
        inline_output_budget_bytes: WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64,
    }
}

fn read_effect_template() -> EffectSet {
    EffectSet {
        filesystem_read: vec![PathScope {
            path: ".".into(),
            recursive: true,
            resolved: false,
        }],
        idempotency: Idempotency::Idempotent,
        reversibility: Reversibility::Reversible,
        ..EffectSet::default()
    }
}

fn mutation_effects(path: &str) -> EffectSet {
    let scope = PathScope {
        path: path.into(),
        recursive: false,
        resolved: true,
    };
    EffectSet {
        filesystem_read: vec![scope.clone()],
        filesystem_write: vec![scope],
        idempotency: Idempotency::IdempotentWithKey,
        reversibility: Reversibility::Unknown,
        ..EffectSet::default()
    }
}

fn mutation_effect_template() -> EffectSet {
    let scope = PathScope {
        path: ".".into(),
        recursive: true,
        resolved: false,
    };
    EffectSet {
        filesystem_read: vec![scope.clone()],
        filesystem_write: vec![scope],
        idempotency: Idempotency::IdempotentWithKey,
        reversibility: Reversibility::Unknown,
        ..EffectSet::default()
    }
}

fn diff_summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "changed",
            "previous_bytes",
            "replacement_bytes",
            "byte_delta",
            "previous_lines",
            "replacement_lines",
            "line_delta",
            "common_prefix_bytes",
            "common_suffix_bytes",
            "removed_bytes",
            "inserted_bytes",
            "first_changed_line",
            "unified_diff"
        ],
        "properties": {
            "changed": {"type": "boolean"},
            "previous_bytes": {"type": "integer", "minimum": 0},
            "replacement_bytes": {"type": "integer", "minimum": 0},
            "byte_delta": {"type": "integer"},
            "previous_lines": {"type": "integer", "minimum": 0},
            "replacement_lines": {"type": "integer", "minimum": 0},
            "line_delta": {"type": "integer"},
            "common_prefix_bytes": {"type": "integer", "minimum": 0},
            "common_suffix_bytes": {"type": "integer", "minimum": 0},
            "removed_bytes": {"type": "integer", "minimum": 0},
            "inserted_bytes": {"type": "integer", "minimum": 0},
            "first_changed_line": {
                "anyOf": [
                    {"type": "integer", "minimum": 1},
                    {"type": "null"}
                ]
            },
            "unified_diff": {"type": "string"}
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use super::*;

    fn executor(limits: WorkspaceToolLimits) -> (tempfile::TempDir, WorkspaceTools) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        (directory, WorkspaceTools::with_limits(workspace, limits))
    }

    fn limits(
        max_files: usize,
        max_depth: usize,
        max_file_bytes: u64,
        max_scan_bytes: u64,
        max_matches: usize,
        max_output_bytes: usize,
    ) -> WorkspaceToolLimits {
        WorkspaceToolLimits::try_new(
            max_files,
            max_depth,
            max_file_bytes,
            max_scan_bytes,
            max_matches,
            max_output_bytes,
        )
        .unwrap()
    }

    fn generous_test_limits() -> WorkspaceToolLimits {
        limits(100, 8, 1024, 4096, 100, 16 * 1024)
    }

    #[test]
    fn exposes_exact_read_only_specs_in_stable_order() {
        let specs = workspace_tool_specs();
        assert_eq!(
            specs
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>(),
            [
                WORKSPACE_LIST_TOOL_ID,
                WORKSPACE_READ_TOOL_ID,
                WORKSPACE_SEARCH_TOOL_ID
            ]
        );
        for spec in specs {
            assert_eq!(spec.version, WORKSPACE_TOOL_VERSION);
            assert_eq!(spec.concurrency, ConcurrencyClass::StructuredReadOnly);
            assert!(spec.effect_template.is_read_only());
            assert_eq!(spec.effect_template.filesystem_read.len(), 1);
            assert_eq!(
                spec.inline_output_budget_bytes,
                WORKSPACE_TOOL_HARD_LIMITS.max_output_bytes as u64
            );
            assert_eq!(spec.input_schema["additionalProperties"], false);
        }
    }

    #[test]
    fn exposes_mutation_spec_only_through_explicit_opt_in() {
        assert!(workspace_tool_specs()
            .iter()
            .all(|spec| spec.id != WORKSPACE_APPLY_PATCH_TOOL_ID));

        let specs = workspace_mutation_tool_specs();
        assert_eq!(specs.len(), 1);
        let spec = &specs[0];
        assert_eq!(spec.id, WORKSPACE_APPLY_PATCH_TOOL_ID);
        assert_eq!(spec.version, WORKSPACE_TOOL_VERSION);
        assert_eq!(spec.concurrency, ConcurrencyClass::SerialMutation);
        assert_eq!(spec.input_schema["additionalProperties"], false);
        assert_eq!(
            spec.input_schema["required"],
            json!(["path", "base_revision", "replacement"])
        );
        assert_eq!(
            spec.input_schema["properties"]["replacement"]["maxLength"],
            WORKSPACE_TOOL_HARD_LIMITS.max_file_bytes
        );
        assert_eq!(spec.effect_template.filesystem_read.len(), 1);
        assert_eq!(spec.effect_template.filesystem_write.len(), 1);
        assert!(!spec.effect_template.filesystem_read[0].resolved);
        assert!(!spec.effect_template.filesystem_write[0].resolved);
        assert!(spec.effect_template.filesystem_read[0].recursive);
        assert!(spec.effect_template.filesystem_write[0].recursive);
        assert!(!spec.effect_template.is_read_only());
        assert_eq!(
            spec.effect_template.idempotency,
            Idempotency::IdempotentWithKey
        );
        assert_eq!(spec.effect_template.reversibility, Reversibility::Unknown);
    }

    #[test]
    fn patch_arguments_are_strict_and_base_revision_is_validated() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("hello.txt"), b"old").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        let invalid_argument_cases = [
            Value::Null,
            json!({}),
            json!({"path": "hello.txt", "base_revision": base}),
            json!({"path": "hello.txt", "base_revision": base, "replacement": "new", "extra": true}),
            json!({"path": 7, "base_revision": base, "replacement": "new"}),
            json!({"path": "hello.txt", "base_revision": base, "replacement": 7}),
        ];
        for arguments in invalid_argument_cases {
            assert_eq!(
                tools
                    .prepare_mutation(WORKSPACE_APPLY_PATCH_TOOL_ID, &arguments)
                    .unwrap_err()
                    .code(),
                "workspace_invalid_arguments"
            );
        }

        for revision in ["", "a", &"A".repeat(64), &"g".repeat(64), &"0".repeat(63)] {
            let error = tools
                .prepare_mutation(
                    WORKSPACE_APPLY_PATCH_TOOL_ID,
                    &json!({
                        "path": "hello.txt",
                        "base_revision": revision,
                        "replacement": "new"
                    }),
                )
                .unwrap_err();
            assert_eq!(error.code(), "workspace_invalid_base_revision");
        }
        assert_eq!(
            tools
                .prepare_mutation("workspace.nope", &json!({}))
                .unwrap_err()
                .code(),
            "workspace_unknown_tool"
        );
        assert_eq!(
            tools
                .execute(
                    WORKSPACE_APPLY_PATCH_TOOL_ID,
                    json!({"path": "hello.txt", "base_revision": base, "replacement": "new"})
                )
                .unwrap_err()
                .code(),
            "workspace_unknown_tool"
        );
    }

    #[test]
    fn prepared_patch_has_canonical_effects_and_persistable_diff_summary() {
        let (directory, tools) = executor(generous_test_limits());
        fs::create_dir(directory.path().join("src")).unwrap();
        let previous = b"alpha\nbeta\n";
        fs::write(directory.path().join("src/hello.txt"), previous).unwrap();
        let base = blake3::hash(previous).to_hex().to_string();
        let replacement = "alpha\ngamma\nextra\n";

        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "./src/hello.txt",
                    "base_revision": base,
                    "replacement": replacement
                }),
            )
            .unwrap();
        assert_eq!(prepared.tool_id(), WORKSPACE_APPLY_PATCH_TOOL_ID);
        assert_eq!(prepared.tool_version(), WORKSPACE_TOOL_VERSION);
        assert_eq!(prepared.workspace_identity(), tools.workspace().identity());
        assert_eq!(prepared.normalized_arguments()["path"], "src/hello.txt");
        assert_eq!(prepared.normalized_arguments()["base_revision"], base);
        assert_eq!(prepared.effects().filesystem_read.len(), 1);
        assert_eq!(prepared.effects().filesystem_write.len(), 1);
        for scope in prepared
            .effects()
            .filesystem_read
            .iter()
            .chain(&prepared.effects().filesystem_write)
        {
            assert_eq!(scope.path, "src/hello.txt");
            assert!(!scope.recursive);
            assert!(scope.resolved);
        }
        let summary = prepared.diff_summary();
        assert!(summary.changed);
        assert_eq!(summary.previous_bytes, 11);
        assert_eq!(summary.replacement_bytes, 18);
        assert_eq!(summary.byte_delta, 7);
        assert_eq!(summary.previous_lines, 2);
        assert_eq!(summary.replacement_lines, 3);
        assert_eq!(summary.line_delta, 1);
        assert_eq!(summary.common_prefix_bytes, 6);
        assert_eq!(summary.common_suffix_bytes, 2);
        assert_eq!(summary.removed_bytes, 3);
        assert_eq!(summary.inserted_bytes, 10);
        assert_eq!(summary.first_changed_line, Some(2));
        assert!(summary.unified_diff.contains("--- a/src/hello.txt"));
        assert!(summary.unified_diff.contains("+++ b/src/hello.txt"));
        assert!(summary.unified_diff.contains("-beta"));
        assert!(summary.unified_diff.contains("+gamma"));

        let output = tools.execute_prepared_mutation(&prepared).unwrap();
        assert_eq!(output["path"], "src/hello.txt");
        assert_eq!(output["previous_revision"], base);
        assert_eq!(
            output["revision"],
            blake3::hash(replacement.as_bytes()).to_hex().to_string()
        );
        assert_eq!(output["bytes_written"], replacement.len());
        let persisted: WorkspaceDiffSummary =
            serde_json::from_value(output["diff_summary"].clone()).unwrap();
        assert_eq!(persisted, *prepared.diff_summary());
        assert_eq!(
            fs::read_to_string(directory.path().join("src/hello.txt")).unwrap(),
            replacement
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepared_patch_rejects_same_byte_inode_replacement() {
        let (directory, tools) = executor(generous_test_limits());
        let path = directory.path().join("hello.txt");
        fs::write(&path, b"before\n").unwrap();
        let base = blake3::hash(b"before\n").to_hex().to_string();
        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "hello.txt",
                    "base_revision": base,
                    "replacement": "after\n"
                }),
            )
            .unwrap();

        let replacement = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        fs::write(replacement.path(), b"before\n").unwrap();
        fs::rename(replacement.path(), &path).unwrap();

        let error = tools.execute_prepared_mutation(&prepared).unwrap_err();
        assert!(matches!(
            error,
            WorkspaceToolError::Workspace(WorkspaceError::FileIdentityChanged { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"before\n");
    }

    #[test]
    fn diff_summary_is_stable_for_noop_empty_and_unicode_edits() {
        assert_eq!(
            WorkspaceDiffSummary::for_path("file", b"", b""),
            WorkspaceDiffSummary {
                changed: false,
                previous_bytes: 0,
                replacement_bytes: 0,
                byte_delta: 0,
                previous_lines: 0,
                replacement_lines: 0,
                line_delta: 0,
                common_prefix_bytes: 0,
                common_suffix_bytes: 0,
                removed_bytes: 0,
                inserted_bytes: 0,
                first_changed_line: None,
                unified_diff: String::from("--- a/file\n+++ b/file\n"),
            }
        );
        let summary =
            WorkspaceDiffSummary::for_path("file", "甲\n乙".as_bytes(), "甲\n丙".as_bytes());
        assert!(summary.changed);
        assert_eq!(summary.previous_lines, 2);
        assert_eq!(summary.replacement_lines, 2);
        assert_eq!(summary.first_changed_line, Some(2));
        assert!(summary.unified_diff.contains("-乙"));
        assert!(summary.unified_diff.contains("+丙"));
    }

    #[test]
    fn diff_summary_unified_diff_contains_hunk_markers() {
        let summary = WorkspaceDiffSummary::for_path("file", b"alpha\nbeta\n", b"alpha\ngamma\n");
        assert!(summary.unified_diff.contains("@@"));
        assert!(summary.unified_diff.contains("-beta"));
        assert!(summary.unified_diff.contains("+gamma"));
    }

    #[test]
    fn limits_can_only_narrow_nonzero_hard_caps() {
        let error = WorkspaceToolLimits::try_new(0, 1, 1, 1, 1, 1).unwrap_err();
        assert_eq!(error.code(), "workspace_invalid_limits");
        let error = WorkspaceToolLimits::try_new(10_001, 1, 1, 1, 1, 1).unwrap_err();
        assert_eq!(error.code(), "workspace_invalid_limits");
        assert_eq!(WorkspaceToolLimits::default(), WORKSPACE_TOOL_HARD_LIMITS);
    }

    #[test]
    fn rejects_unknown_tools_and_non_object_unknown_or_wrong_arguments() {
        let (_directory, tools) = executor(generous_test_limits());
        let cases = [
            ("workspace.nope", json!({}), "workspace_unknown_tool"),
            (
                WORKSPACE_LIST_TOOL_ID,
                Value::Null,
                "workspace_invalid_arguments",
            ),
            (
                WORKSPACE_LIST_TOOL_ID,
                json!({"path": 7}),
                "workspace_invalid_arguments",
            ),
            (
                WORKSPACE_LIST_TOOL_ID,
                json!({"unexpected": true}),
                "workspace_invalid_arguments",
            ),
            (
                WORKSPACE_READ_TOOL_ID,
                json!({}),
                "workspace_invalid_arguments",
            ),
            (
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "x", "limit": 1}),
                "workspace_invalid_arguments",
            ),
        ];
        for (tool_id, arguments, code) in cases {
            assert_eq!(tools.execute(tool_id, arguments).unwrap_err().code(), code);
        }
    }

    #[test]
    fn list_is_normalized_and_deterministically_sorted() {
        let (directory, tools) = executor(generous_test_limits());
        fs::create_dir_all(directory.path().join("src/nested")).unwrap();
        fs::write(directory.path().join("z.txt"), b"z").unwrap();
        fs::write(directory.path().join("a.txt"), b"aa").unwrap();
        fs::write(directory.path().join("src/nested/lib.rs"), b"rust").unwrap();

        let first = tools
            .execute(WORKSPACE_LIST_TOOL_ID, json!({"path": "./"}))
            .unwrap();
        let second = tools
            .execute(WORKSPACE_LIST_TOOL_ID, json!({"path": ""}))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first["path"], "");
        let paths = first["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["a.txt", "src/nested/lib.rs", "z.txt"]);
        assert_eq!(first["files"][0]["bytes"], 2);
    }

    #[test]
    fn read_returns_utf8_content_revision_and_exact_byte_count() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("hello.txt"), "héllo").unwrap();
        let output = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "./hello.txt"}))
            .unwrap();
        assert_eq!(output["path"], "hello.txt");
        assert_eq!(output["content"], "héllo");
        assert_eq!(output["bytes"], "héllo".len());
        assert_eq!(
            output["revision"],
            blake3::hash("héllo".as_bytes()).to_hex().to_string()
        );
    }

    #[test]
    fn search_is_literal_ordered_and_reports_byte_positions() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("b.txt"), b"needle").unwrap();
        fs::write(directory.path().join("a.txt"), b"one needle\ntwo needle").unwrap();
        let output = tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "needle"}))
            .unwrap();
        let matches = output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(
            matches[0],
            json!({
                "path": "a.txt", "byte_offset": 4, "line": 1, "column": 5
            })
        );
        assert_eq!(
            matches[1],
            json!({
                "path": "a.txt", "byte_offset": 15, "line": 2, "column": 5
            })
        );
        assert_eq!(matches[2]["path"], "b.txt");
        assert_eq!(output["files_scanned"], 2);
        assert_eq!(output["bytes_scanned"], 27);
    }

    #[test]
    fn search_preserves_overlapping_matches_and_line_columns() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("overlap.txt"), b"aaaa\naaa").unwrap();
        let output = tools
            .execute(
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"path": "", "query": "aaa"}),
            )
            .unwrap();
        assert_eq!(
            output["matches"],
            json!([
                {"path": "overlap.txt", "byte_offset": 0, "line": 1, "column": 1},
                {"path": "overlap.txt", "byte_offset": 1, "line": 1, "column": 2},
                {"path": "overlap.txt", "byte_offset": 5, "line": 2, "column": 1}
            ])
        );
    }

    #[test]
    fn long_common_prefix_search_stays_within_linear_operation_budget() {
        let haystack = vec![b'a'; 64 * 1024];
        let query = format!("{}b", "a".repeat(255));
        let limits = limits(
            10,
            2,
            haystack.len() as u64,
            haystack.len() as u64,
            10,
            4096,
        );
        let (directory, tools) = executor(limits);
        fs::write(directory.path().join("repeated"), &haystack).unwrap();

        // This is deliberately below the old implementation's conceptual
        // comparison count (roughly haystack × query), while leaving enough
        // room for KMP's prefix table, fallback comparisons and line tracking.
        let control = WorkspaceSearchControl::new()
            .with_operation_budget((haystack.len() as u64).saturating_mul(4));
        let output = tools
            .execute_with_control(WORKSPACE_SEARCH_TOOL_ID, json!({"query": query}), &control)
            .unwrap();
        assert!(output["matches"].as_array().unwrap().is_empty());
        assert_eq!(output["bytes_scanned"], haystack.len());
    }

    #[test]
    fn search_control_reports_expired_deadline_and_pre_cancelled_invocation() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("file"), b"content").unwrap();

        let expired =
            WorkspaceSearchControl::new().with_deadline(Instant::now() - Duration::from_millis(1));
        let error = tools
            .execute_with_control(
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "content"}),
                &expired,
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_search_deadline_exceeded");

        let cancelled = AtomicBool::new(true);
        let cancellation = || cancelled.load(Ordering::Acquire);
        let control = WorkspaceSearchControl::new().with_cancellation(&cancellation);
        let error = tools
            .execute_with_control(
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "content"}),
                &control,
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_search_cancelled");
    }

    #[test]
    fn search_control_can_stop_a_running_matcher_cooperatively() {
        struct CancelAfter {
            checks: AtomicUsize,
            threshold: usize,
        }

        impl SearchCancellation for CancelAfter {
            fn is_cancelled(&self) -> bool {
                self.checks.fetch_add(1, Ordering::AcqRel) >= self.threshold
            }
        }

        let haystack = vec![b'x'; 32 * 1024];
        let limits = limits(
            10,
            2,
            haystack.len() as u64,
            haystack.len() as u64,
            10,
            4096,
        );
        let (directory, tools) = executor(limits);
        fs::write(directory.path().join("repeated"), &haystack).unwrap();
        let cancellation = CancelAfter {
            checks: AtomicUsize::new(0),
            // Account for dispatch, the initial walk and the first matcher
            // sample; this threshold deterministically trips during scanning.
            threshold: 9,
        };
        let control = WorkspaceSearchControl::new().with_cancellation(&cancellation);
        let error = tools
            .execute_with_control(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "x"}), &control)
            .unwrap_err();
        assert_eq!(error.code(), "workspace_search_cancelled");
    }

    #[test]
    fn zero_search_operation_budget_fails_closed() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("file"), b"content").unwrap();
        let control = WorkspaceSearchControl::new().with_operation_budget(0);
        let error = tools
            .execute_with_control(
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "content"}),
                &control,
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_search_budget_exceeded");
    }

    #[test]
    fn shared_search_operation_budget_accumulates_across_invocations() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("file"), b"abc").unwrap();
        let budget = Arc::new(SearchOperationBudget::new(5));
        let control = WorkspaceSearchControl::new().with_shared_operation_budget(budget.as_ref());

        // A one-byte query over this three-byte file consumes exactly three
        // matcher steps. The second invocation can consume only two more
        // steps before the shared per-turn allowance fails closed.
        tools
            .execute_with_control(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "z"}), &control)
            .unwrap();
        assert_eq!(budget.operations(), 3);
        let error = tools
            .execute_with_control(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "z"}), &control)
            .unwrap_err();
        assert_eq!(error.code(), "workspace_search_budget_exceeded");
        assert_eq!(budget.operations(), budget.limit());
    }

    #[test]
    fn rejects_absolute_parent_empty_and_oversized_queries_with_stable_codes() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("hello.txt"), b"hello").unwrap();
        assert!(tools
            .execute(
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "x".repeat(MAX_QUERY_BYTES)})
            )
            .is_ok());
        let absolute = directory.path().join("hello.txt").display().to_string();
        let cases = [
            (
                WORKSPACE_READ_TOOL_ID,
                json!({"path": "../hello.txt"}),
                "workspace_invalid_path",
            ),
            (
                WORKSPACE_READ_TOOL_ID,
                json!({"path": absolute}),
                "workspace_invalid_path",
            ),
            (
                WORKSPACE_READ_TOOL_ID,
                json!({"path": ""}),
                "workspace_invalid_path",
            ),
            (
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": ""}),
                "workspace_invalid_arguments",
            ),
            (
                WORKSPACE_SEARCH_TOOL_ID,
                json!({"query": "x".repeat(MAX_QUERY_BYTES + 1)}),
                "workspace_query_bytes_limit",
            ),
        ];
        for (tool_id, arguments, code) in cases {
            assert_eq!(tools.execute(tool_id, arguments).unwrap_err().code(), code);
        }
    }

    #[test]
    fn file_count_budget_accepts_boundary_and_rejects_next_entry() {
        let exact = limits(2, 2, 32, 64, 10, 1024);
        let (directory, tools) = executor(exact);
        fs::write(directory.path().join("a"), b"a").unwrap();
        fs::write(directory.path().join("b"), b"b").unwrap();
        assert!(tools.execute(WORKSPACE_LIST_TOOL_ID, json!({})).is_ok());
        fs::write(directory.path().join("c"), b"c").unwrap();
        let error = tools
            .execute(WORKSPACE_LIST_TOOL_ID, json!({}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_file_count_limit");
    }

    #[test]
    fn depth_budget_accepts_boundary_and_rejects_deeper_entries() {
        let shallow = limits(10, 1, 32, 64, 10, 1024);
        let (directory, tools) = executor(shallow);
        fs::write(directory.path().join("root.txt"), b"ok").unwrap();
        assert!(tools.execute(WORKSPACE_LIST_TOOL_ID, json!({})).is_ok());
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::write(directory.path().join("nested/deep.txt"), b"no").unwrap();
        let error = tools
            .execute(WORKSPACE_LIST_TOOL_ID, json!({}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_depth_limit");
    }

    #[test]
    fn single_file_budget_is_checked_before_unbounded_reading() {
        let tiny = limits(10, 2, 3, 64, 10, 1024);
        let (directory, tools) = executor(tiny);
        fs::write(directory.path().join("exact"), b"123").unwrap();
        fs::write(directory.path().join("large"), b"1234").unwrap();
        assert!(tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "exact"}))
            .is_ok());
        let error = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "large"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_file_bytes_limit");
        let error = tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "1"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_file_bytes_limit");
    }

    #[test]
    fn aggregate_scan_budget_accepts_boundary_and_rejects_overflow() {
        let (directory, exact_tools) = executor(limits(10, 2, 8, 6, 10, 1024));
        fs::write(directory.path().join("a"), b"abc").unwrap();
        fs::write(directory.path().join("b"), b"def").unwrap();
        assert!(exact_tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "z"}))
            .is_ok());

        let workspace = Workspace::open(directory.path()).unwrap();
        let over_tools = WorkspaceTools::with_limits(workspace, limits(10, 2, 8, 5, 10, 1024));
        let error = over_tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "z"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_scan_bytes_limit");
    }

    #[test]
    fn match_budget_accepts_boundary_and_rejects_one_more() {
        let exact = limits(10, 2, 8, 8, 3, 1024);
        let (directory, tools) = executor(exact);
        fs::write(directory.path().join("matches"), b"aaa").unwrap();
        assert_eq!(
            tools
                .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "a"}))
                .unwrap()["matches"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        fs::write(directory.path().join("matches"), b"aaaa").unwrap();
        let error = tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "a"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_match_limit");
    }

    #[test]
    fn serialized_output_budget_accepts_exact_size_and_rejects_one_byte_less() {
        let (directory, baseline_tools) = executor(generous_test_limits());
        fs::write(directory.path().join("a"), b"a").unwrap();
        let output = baseline_tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "a"}))
            .unwrap();
        let encoded_len = serde_json::to_vec(&output).unwrap().len();

        let workspace = Workspace::open(directory.path()).unwrap();
        let exact_tools =
            WorkspaceTools::with_limits(workspace.clone(), limits(10, 2, 32, 64, 10, encoded_len));
        assert_eq!(
            exact_tools
                .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "a"}))
                .unwrap(),
            output
        );
        let too_small_tools =
            WorkspaceTools::with_limits(workspace, limits(10, 2, 32, 64, 10, encoded_len - 1));
        let error = too_small_tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "a"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_output_bytes_limit");
    }

    #[test]
    fn invalid_utf8_content_has_a_stable_error_code() {
        let (directory, tools) = executor(generous_test_limits());
        fs::write(directory.path().join("binary"), [0xff, 0xfe]).unwrap();
        let error = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "binary"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_invalid_utf8");
        // Literal byte search remains well-defined for binary workspace files.
        assert!(tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "absent"}))
            .is_ok());
    }

    #[test]
    fn stale_patch_is_rejected_before_and_after_preparation_without_overwrite() {
        let (directory, tools) = executor(generous_test_limits());
        let path = directory.path().join("hello.txt");
        fs::write(&path, b"old").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();

        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "hello.txt",
                    "base_revision": "0".repeat(64),
                    "replacement": "new"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_stale_revision");

        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "hello.txt",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap();
        fs::write(&path, b"user edit").unwrap();
        let error = tools.execute_prepared_mutation(&prepared).unwrap_err();
        assert_eq!(error.code(), "workspace_stale_revision");
        assert_eq!(fs::read(&path).unwrap(), b"user edit");
    }

    #[test]
    fn patch_rejects_path_escape_and_replacement_byte_overflow() {
        let tiny = limits(10, 2, 3, 64, 10, 4096);
        let (directory, tools) = executor(tiny);
        fs::write(directory.path().join("file"), b"old").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        for path in ["../file", "/file", "", "nul\0file"] {
            let error = tools
                .prepare_mutation(
                    WORKSPACE_APPLY_PATCH_TOOL_ID,
                    &json!({
                        "path": path,
                        "base_revision": base,
                        "replacement": "new"
                    }),
                )
                .unwrap_err();
            assert_eq!(error.code(), "workspace_invalid_path");
        }
        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "file",
                    "base_revision": base,
                    "replacement": "éé"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_replacement_bytes_limit");
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"old");
    }

    #[test]
    fn patch_output_budget_is_preflighted_before_the_write() {
        let constrained = limits(10, 2, 32, 64, 10, 1);
        let (directory, tools) = executor(constrained);
        let path = directory.path().join("file");
        fs::write(&path, b"old").unwrap();
        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "file",
                    "base_revision": blake3::hash(b"old").to_hex().to_string(),
                    "replacement": "new"
                }),
            )
            .unwrap();
        let error = tools.execute_prepared_mutation(&prepared).unwrap_err();
        assert_eq!(error.code(), "workspace_output_bytes_limit");
        assert_eq!(fs::read(path).unwrap(), b"old");
    }

    #[test]
    fn prepared_patch_is_bound_to_the_originating_workspace_identity() {
        let (first_directory, first_tools) = executor(generous_test_limits());
        fs::write(first_directory.path().join("file"), b"old").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        let prepared = first_tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "file",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap();

        let (second_directory, second_tools) = executor(generous_test_limits());
        fs::write(second_directory.path().join("file"), b"old").unwrap();
        let error = second_tools
            .execute_prepared_mutation(&prepared)
            .unwrap_err();
        assert_eq!(error.code(), "workspace_identity_mismatch");
        assert_eq!(
            fs::read(second_directory.path().join("file")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn unicode_workspace_paths_are_preserved_exactly() {
        let (directory, tools) = executor(generous_test_limits());
        let relative = "源/你好.rs";
        fs::create_dir(directory.path().join("源")).unwrap();
        fs::write(directory.path().join(relative), "旧\n").unwrap();
        let base = blake3::hash("旧\n".as_bytes()).to_hex().to_string();
        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": relative,
                    "base_revision": base,
                    "replacement": "新\n"
                }),
            )
            .unwrap();
        assert_eq!(prepared.normalized_arguments()["path"], relative);
        assert_eq!(prepared.effects().filesystem_write[0].path, relative);
        tools.execute_prepared_mutation(&prepared).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join(relative)).unwrap(),
            "新\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_for_read_and_never_walked() {
        use std::os::unix::fs::symlink;

        let (directory, tools) = executor(generous_test_limits());
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"needle").unwrap();
        symlink(
            outside.path().join("secret"),
            directory.path().join("escape"),
        )
        .unwrap();

        let error = tools
            .execute(WORKSPACE_READ_TOOL_ID, json!({"path": "escape"}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_path_escape");
        let listed = tools.execute(WORKSPACE_LIST_TOOL_ID, json!({})).unwrap();
        assert!(listed["files"].as_array().unwrap().is_empty());
        let searched = tools
            .execute(WORKSPACE_SEARCH_TOOL_ID, json!({"query": "needle"}))
            .unwrap();
        assert!(searched["matches"].as_array().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn patch_preparation_rejects_internal_symlinks_and_escape() {
        use std::os::unix::fs::symlink;

        let (directory, tools) = executor(generous_test_limits());
        fs::create_dir(directory.path().join("actual")).unwrap();
        fs::write(directory.path().join("actual/file"), b"old").unwrap();
        symlink("actual/file", directory.path().join("alias")).unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "alias",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_path_changed");
        assert_eq!(
            fs::read(directory.path().join("actual/file")).unwrap(),
            b"old"
        );

        symlink("actual", directory.path().join("alias-dir")).unwrap();
        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "alias-dir/file",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_path_changed");
        assert_eq!(
            fs::read(directory.path().join("actual/file")).unwrap(),
            b"old"
        );

        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"outside").unwrap();
        symlink(outside.path(), directory.path().join("escape-patch")).unwrap();
        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "escape-patch",
                    "base_revision": blake3::hash(b"outside").to_hex().to_string(),
                    "replacement": "bad"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_path_escape");
        assert_eq!(fs::read(outside.path()).unwrap(), b"outside");
    }

    #[test]
    fn patch_rejects_non_utf8_targets_even_with_a_matching_revision() {
        let (directory, tools) = executor(generous_test_limits());
        let path = directory.path().join("binary");
        let content = [0xff, 0xfe, 0xfd];
        fs::write(&path, content).unwrap();

        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "binary",
                    "base_revision": blake3::hash(&content).to_hex().to_string(),
                    "replacement": "text"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_invalid_utf8");
        assert_eq!(fs::read(path).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn prepared_patch_rejects_later_symlink_redirection() {
        use std::os::unix::fs::symlink;

        let (directory, tools) = executor(generous_test_limits());
        let target = directory.path().join("target");
        let other = directory.path().join("other");
        fs::write(&target, b"same").unwrap();
        fs::write(&other, b"same").unwrap();
        let base = blake3::hash(b"same").to_hex().to_string();
        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "target",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap();

        fs::remove_file(&target).unwrap();
        symlink("other", &target).unwrap();
        let error = tools.execute_prepared_mutation(&prepared).unwrap_err();
        assert_eq!(error.code(), "workspace_path_changed");
        assert_eq!(fs::read(&other).unwrap(), b"same");
        assert!(fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_patch_rejects_a_hard_link_added_after_approval() {
        let (directory, tools) = executor(generous_test_limits());
        let target = directory.path().join("target");
        fs::write(&target, b"old").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        let prepared = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "target",
                    "base_revision": base,
                    "replacement": "new"
                }),
            )
            .unwrap();

        let outside = tempfile::tempdir().unwrap();
        let linked = outside.path().join("linked");
        fs::hard_link(&target, &linked).unwrap();
        let error = tools.execute_prepared_mutation(&prepared).unwrap_err();
        assert_eq!(error.code(), "workspace_multiple_hard_links");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read(&linked).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_read_effect_uses_the_canonical_in_workspace_target() {
        use std::os::unix::fs::symlink;

        let (directory, tools) = executor(generous_test_limits());
        fs::create_dir(directory.path().join("actual")).unwrap();
        fs::write(directory.path().join("actual/data.txt"), b"data").unwrap();
        symlink("actual/data.txt", directory.path().join("alias.txt")).unwrap();

        let effects = tools
            .prepare_effects(WORKSPACE_READ_TOOL_ID, &json!({"path": "alias.txt"}))
            .unwrap();
        assert_eq!(effects.filesystem_read.len(), 1);
        assert_eq!(effects.filesystem_read[0].path, "actual/data.txt");
        assert!(!effects.filesystem_read[0].recursive);
        assert!(effects.filesystem_read[0].resolved);
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_rejected_by_all_tools_that_surface_them() {
        let (directory, tools) = executor(generous_test_limits());
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"secret").unwrap();
        fs::hard_link(outside.path(), directory.path().join("linked")).unwrap();

        for (tool_id, arguments) in [
            (WORKSPACE_LIST_TOOL_ID, json!({})),
            (WORKSPACE_READ_TOOL_ID, json!({"path": "linked"})),
            (WORKSPACE_SEARCH_TOOL_ID, json!({"query": "secret"})),
        ] {
            let error = tools.execute(tool_id, arguments).unwrap_err();
            assert_eq!(error.code(), "workspace_multiple_hard_links");
        }

        let error = tools
            .prepare_mutation(
                WORKSPACE_APPLY_PATCH_TOOL_ID,
                &json!({
                    "path": "linked",
                    "base_revision": blake3::hash(b"secret").to_hex().to_string(),
                    "replacement": "replacement"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code(), "workspace_multiple_hard_links");
        assert_eq!(fs::read(outside.path()).unwrap(), b"secret");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_paths_are_not_lossily_exposed() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let (directory, tools) = executor(generous_test_limits());
        let name = OsString::from_vec(vec![b'b', 0xff]);
        fs::write(directory.path().join(name), b"data").unwrap();
        let error = tools
            .execute(WORKSPACE_LIST_TOOL_ID, json!({}))
            .unwrap_err();
        assert_eq!(error.code(), "workspace_path_not_utf8");
    }
}
