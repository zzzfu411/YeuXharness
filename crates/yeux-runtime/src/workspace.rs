//! Workspace path confinement and revision-checked atomic edits.
//!
//! Unix mutation paths are anchored to an opened workspace directory and use
//! component-wise `openat` plus `renameat`.  This closes path-based
//! intermediate-directory/symlink redirection and keeps temporary-file cleanup
//! in the originally opened parent.  POSIX does not expose a compare-and-swap
//! rename keyed by an inode or digest, so a non-cooperating process can still
//! replace the final target name between the last validation and `renameat`.
//! The implementation treats that as a documented residual boundary rather
//! than claiming a stronger guarantee than the platform provides.

#![allow(clippy::result_large_err)]

use std::{
    fmt,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use tempfile::NamedTempFile;
use walkdir::WalkDir;

#[cfg(unix)]
use {
    rustix::{
        fs::{openat, renameat, unlinkat, AtFlags, Mode, OFlags},
        io::Errno,
    },
    std::ffi::{OsStr, OsString},
};

/// An opened workspace root and the shared mutation gate used by all clones.
///
/// The root descriptor is deliberately retained for the lifetime of the
/// workspace.  Operations which can have side effects resolve every path
/// component relative to this descriptor instead of reopening an absolute
/// path after a `canonicalize` check.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    identity: String,
    root_identity: WorkspaceIdentitySnapshot,
    mutation_gate: Arc<Mutex<()>>,
    #[cfg(unix)]
    root_dir: Arc<File>,
}

impl Clone for Workspace {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            identity: self.identity.clone(),
            root_identity: self.root_identity.clone(),
            mutation_gate: Arc::clone(&self.mutation_gate),
            #[cfg(unix)]
            root_dir: Arc::clone(&self.root_dir),
        }
    }
}

/// A point-in-time identity for the canonical workspace root.
///
/// The digest is intentionally derived from the canonical path and the
/// platform file identity (device/inode on Unix).  Callers should persist the
/// whole snapshot when possible; the digest-only [`Workspace::identity`] API is
/// retained for protocol compatibility.  A snapshot is not a capability and
/// does not protect against a privileged/root actor replacing the host
/// filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentitySnapshot {
    /// Canonical root path observed when this snapshot was taken.
    pub canonical_root: PathBuf,
    /// BLAKE3 digest over `canonical_root` and available file identity fields.
    pub digest: String,
    /// Device number where the platform exposes one (Unix).
    pub device: Option<u64>,
    /// Inode/file-id where the platform exposes one (Unix).
    pub inode: Option<u64>,
}

impl fmt::Display for WorkspaceIdentitySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (digest={}, device={:?}, inode={:?})",
            self.canonical_root.display(),
            self.digest,
            self.device,
            self.inode
        )
    }
}

impl WorkspaceIdentitySnapshot {
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub const fn device(&self) -> Option<u64> {
        self.device
    }

    pub const fn inode(&self) -> Option<u64> {
        self.inode
    }
}

/// The revision and file identity observed from one validated file descriptor.
///
/// This is useful for a caller that prepares a mutation and wants to revalidate
/// both the content digest and the object identity immediately before commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevisionSnapshot {
    pub relative_path: PathBuf,
    pub revision: String,
    pub byte_length: u64,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

impl FileRevisionSnapshot {
    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    pub const fn device(&self) -> Option<u64> {
        self.device
    }

    pub const fn inode(&self) -> Option<u64> {
        self.inode
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionedFile {
    pub relative_path: PathBuf,
    pub revision: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchResult {
    pub relative_path: PathBuf,
    pub previous_revision: String,
    pub revision: String,
    pub bytes_written: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace root must be a directory: {0}")]
    NotDirectory(PathBuf),
    #[error("path must be relative and may not contain '..': {0}")]
    InvalidRelativePath(PathBuf),
    #[error("path escapes workspace {root}: {path}")]
    OutsideWorkspace { root: PathBuf, path: PathBuf },
    #[error("workspace root identity changed: expected {expected}, actual {actual}")]
    WorkspaceIdentityChanged {
        expected: WorkspaceIdentitySnapshot,
        actual: WorkspaceIdentitySnapshot,
    },
    #[error("workspace identity digest mismatch: expected {expected}, actual {actual}")]
    WorkspaceIdentityDigestMismatch { expected: String, actual: String },
    #[error("workspace file identity changed for {path}: expected device={expected_device:?}, inode={expected_inode:?}; actual device={actual_device:?}, inode={actual_inode:?}")]
    FileIdentityChanged {
        path: PathBuf,
        expected_device: Option<u64>,
        expected_inode: Option<u64>,
        actual_device: Option<u64>,
        actual_inode: Option<u64>,
    },
    #[error("workspace file changed while being read: {0}")]
    FileChangedDuringRead(PathBuf),
    #[error("workspace file revision changed for {path}: expected {expected}, actual {actual}")]
    RevisionChanged {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("resolved workspace path changed: expected {expected}, actual {actual}")]
    ResolvedPathChanged { expected: PathBuf, actual: PathBuf },
    #[error("path does not exist: {0}")]
    NotFound(PathBuf),
    #[error("path is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("file has multiple hard links and is not safe to access: {0}")]
    MultipleHardLinks(PathBuf),
    #[error("file exceeds the {limit}-byte read limit: {path}")]
    ReadLimitExceeded { path: PathBuf, limit: u64 },
    #[error("file is not valid UTF-8: {0}")]
    InvalidUtf8(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyPatchError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("stale base revision for {path}: expected {expected}, actual {actual}")]
    StaleRevision {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("failed to atomically publish replacement: {0}")]
    Persist(#[from] tempfile::PersistError),
    #[error("replacement for {path} was published but directory durability is unproven: {source}")]
    DurabilityUncertain {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type WorkspaceResult<T> = std::result::Result<T, WorkspaceError>;
pub type ApplyResult<T> = std::result::Result<T, ApplyPatchError>;

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> WorkspaceResult<Self> {
        let root = fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(WorkspaceError::NotDirectory(root));
        }
        #[cfg(unix)]
        let (root_dir, root_identity) = {
            // Open the canonical root with O_DIRECTORY|O_NOFOLLOW and derive
            // the identity from that descriptor.  A path-level stat taken
            // before/after this call could otherwise describe a different
            // directory if a concurrent actor replaced the root.
            let root_dir = open_directory_nofollow(&root).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    WorkspaceError::NotFound(root.clone())
                } else {
                    WorkspaceError::Io(error)
                }
            })?;
            let descriptor_identity = workspace_identity_snapshot_for_descriptor(&root, &root_dir)?;
            // Confirm that the canonical path still names the descriptor we
            // opened.  If it changed during open, fail closed instead of
            // silently binding the workspace to a detached tree.
            let path_identity = workspace_identity_snapshot_for_canonical_root(&root)?;
            if path_identity != descriptor_identity {
                return Err(WorkspaceError::WorkspaceIdentityChanged {
                    expected: descriptor_identity,
                    actual: path_identity,
                });
            }
            (root_dir, descriptor_identity)
        };
        #[cfg(not(unix))]
        let root_identity = workspace_identity_snapshot_for_canonical_root(&root)?;
        let identity = root_identity.digest.clone();
        Ok(Self {
            root,
            identity,
            root_identity,
            mutation_gate: Arc::new(Mutex::new(())),
            #[cfg(unix)]
            root_dir: Arc::new(root_dir),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable for the canonical root and its filesystem identity during this run.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Return the root identity captured when this workspace was opened.
    ///
    /// The returned value is a copy so callers can safely persist it as part
    /// of an invocation/approval binding without borrowing the workspace.
    pub fn identity_snapshot(&self) -> WorkspaceIdentitySnapshot {
        self.root_identity.clone()
    }

    /// Read the current root identity from the filesystem.
    ///
    /// This method intentionally does not compare against the opening
    /// snapshot.  It is useful for diagnostics and for callers that carry a
    /// separately persisted identity.  Use [`Workspace::revalidate_identity`]
    /// before authorizing or publishing a side effect.
    pub fn live_identity(&self) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
        let canonical = fs::canonicalize(&self.root).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NotFound(self.root.clone())
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        #[cfg(unix)]
        {
            // Resolve and stat the live path through one no-follow directory
            // descriptor.  This makes the identity sample atomic with
            // respect to replacement of the final root component.
            let descriptor = open_directory_nofollow(&canonical).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    WorkspaceError::NotFound(self.root.clone())
                } else {
                    WorkspaceError::Io(error)
                }
            })?;
            workspace_identity_snapshot_for_descriptor(&canonical, &descriptor)
        }
        #[cfg(not(unix))]
        {
            workspace_identity_snapshot_for_canonical_root(&canonical)
        }
    }

    /// Revalidate that the path still names the exact root opened by this
    /// instance.
    ///
    /// A changed canonical path, device, inode, or derived digest fails
    /// closed.  The live snapshot is returned on success so the caller can
    /// include the observed values in evidence without doing a second stat.
    pub fn revalidate_identity(&self) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
        let actual = self.live_identity()?;
        if actual != self.root_identity {
            return Err(WorkspaceError::WorkspaceIdentityChanged {
                expected: self.root_identity.clone(),
                actual,
            });
        }
        Ok(actual)
    }

    /// Revalidate against an externally persisted root snapshot.
    ///
    /// Both the persisted value and the value captured at [`Workspace::open`]
    /// must agree with the live filesystem.  Requiring both prevents a caller
    /// from accidentally authorizing a workspace that was reopened under a
    /// different root while retaining an old approval digest.
    pub fn revalidate_identity_against(
        &self,
        expected: &WorkspaceIdentitySnapshot,
    ) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
        let actual = self.live_identity()?;
        if actual != *expected {
            return Err(WorkspaceError::WorkspaceIdentityChanged {
                expected: expected.clone(),
                actual,
            });
        }
        if actual != self.root_identity {
            return Err(WorkspaceError::WorkspaceIdentityChanged {
                expected: self.root_identity.clone(),
                actual,
            });
        }
        Ok(actual)
    }

    /// Revalidate a persisted digest while also checking the cached root
    /// identity.  This is the digest-only counterpart to
    /// [`Workspace::revalidate_identity_against`].
    pub fn revalidate_identity_digest(
        &self,
        expected_digest: &str,
    ) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
        let actual = self.revalidate_identity()?;
        if actual.digest != expected_digest {
            return Err(WorkspaceError::WorkspaceIdentityDigestMismatch {
                expected: expected_digest.to_owned(),
                actual: actual.digest,
            });
        }
        Ok(actual)
    }

    /// Capture the current BLAKE3 revision and filesystem identity of one
    /// regular workspace file.  The path must name a direct canonical target;
    /// symlink aliases are rejected so the token can safely be reused for a
    /// later CAS operation.
    ///
    /// The file is opened with the same no-follow and single-hard-link checks
    /// used by [`Workspace::read`].  Metadata is sampled before and after the
    /// hash; a change during hashing is rejected instead of returning a
    /// potentially mixed revision.
    pub fn revision_snapshot(
        &self,
        relative: impl AsRef<Path>,
    ) -> WorkspaceResult<FileRevisionSnapshot> {
        let relative = validate_relative(relative.as_ref())?;
        // A revision token is an approval/CAS input, so bind it to the direct
        // canonical workspace-relative spelling rather than allowing a
        // symlink alias whose target could later be redirected.
        let absolute = self.resolve_expected(&relative)?;
        self.revision_snapshot_at(&absolute, &relative)
    }

    /// Revalidate a previously captured file revision and object identity.
    ///
    /// This is a compare-and-swap guard for callers that prepare work in one
    /// phase and publish it later.  A changed digest, device, inode, or
    /// canonical path fails closed.
    pub fn revalidate_revision(
        &self,
        expected: &FileRevisionSnapshot,
    ) -> WorkspaceResult<FileRevisionSnapshot> {
        let relative = validate_relative(&expected.relative_path)?;
        let absolute = self.resolve_expected(&relative)?;
        let actual = self.revision_snapshot_at(&absolute, &relative)?;
        if actual.relative_path != expected.relative_path
            || actual.byte_length != expected.byte_length
            || actual.device != expected.device
            || actual.inode != expected.inode
        {
            return Err(WorkspaceError::FileIdentityChanged {
                path: relative,
                expected_device: expected.device,
                expected_inode: expected.inode,
                actual_device: actual.device,
                actual_inode: actual.inode,
            });
        }
        if actual.revision != expected.revision {
            return Err(WorkspaceError::RevisionChanged {
                path: relative,
                expected: expected.revision.clone(),
                actual: actual.revision,
            });
        }
        Ok(actual)
    }

    /// Revalidate a base digest and return the richer live revision snapshot.
    ///
    /// This convenience method preserves the error shape used by
    /// [`Workspace::apply_patch`] while checking the live workspace root and
    /// returning the current file object identity.  A digest-only caller
    /// cannot detect a same-byte inode replacement; use
    /// [`Workspace::revalidate_revision`] when the original snapshot is
    /// available.
    pub fn revalidate_base_revision(
        &self,
        relative: impl AsRef<Path>,
        expected_revision: &str,
    ) -> ApplyResult<FileRevisionSnapshot> {
        let relative = validate_relative(relative.as_ref())?;
        let absolute = self.resolve_expected(&relative)?;
        let actual = self.revision_snapshot_at(&absolute, &relative)?;
        if actual.revision != expected_revision {
            return Err(ApplyPatchError::StaleRevision {
                path: relative,
                expected: expected_revision.to_owned(),
                actual: actual.revision,
            });
        }
        Ok(actual)
    }

    pub fn resolve_directory(&self, relative: impl AsRef<Path>) -> WorkspaceResult<PathBuf> {
        self.revalidate_identity()?;
        let relative = validate_relative_allow_empty(relative.as_ref())?;
        let directory = if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.resolve_existing(&relative)?
        };
        if !directory.is_dir() {
            return Err(WorkspaceError::NotDirectory(relative));
        }
        #[cfg(unix)]
        if !relative.as_os_str().is_empty() {
            // Validate every directory component through the anchored root
            // descriptor.  This is intentionally a second open after the
            // canonical path check: the returned path is only a descriptive
            // value, while callers that perform I/O should retain/use the fd.
            self.open_secure_directory(&relative)?;
        }
        self.revalidate_identity()?;
        Ok(directory)
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> WorkspaceResult<RevisionedFile> {
        let relative = validate_relative(relative.as_ref())?;
        #[cfg(unix)]
        let (mut file, metadata) = {
            // Canonicalize only to preserve the public alias semantics (a
            // read may name an in-workspace symlink), then reopen the
            // canonical spelling component-by-component from the root fd.
            // The descriptor, not the path, is what supplies the bytes.
            let absolute = self.resolve_existing(&relative)?;
            let canonical_relative = self.canonical_relative(&absolute)?;
            let opened = self.open_secure_regular(&canonical_relative)?;
            (opened.file, opened.metadata)
        };
        #[cfg(not(unix))]
        let (mut file, metadata) = {
            let absolute = self.resolve_existing(&relative)?;
            open_regular_single_link(&absolute, &relative)?
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure_file_identity_unchanged(&file, &metadata, &relative)?;
        self.revalidate_identity()?;
        Ok(RevisionedFile {
            relative_path: relative,
            revision: digest(&bytes),
            bytes,
        })
    }

    /// Read at most `limit` bytes without ever buffering an oversized file.
    ///
    /// This is crate-private because callers that expose workspace contents to
    /// a model must also enforce aggregate scan and serialized-output budgets.
    pub(crate) fn read_limited(
        &self,
        relative: impl AsRef<Path>,
        limit: u64,
    ) -> WorkspaceResult<RevisionedFile> {
        let relative = validate_relative(relative.as_ref())?;
        #[cfg(unix)]
        let (mut file, metadata) = {
            let absolute = self.resolve_existing(&relative)?;
            let canonical_relative = self.canonical_relative(&absolute)?;
            let opened = self.open_secure_regular(&canonical_relative)?;
            (opened.file, opened.metadata)
        };
        #[cfg(not(unix))]
        let (mut file, metadata) = {
            let absolute = self.resolve_existing(&relative)?;
            open_regular_single_link(&absolute, &relative)?
        };
        if metadata.len() > limit {
            return Err(WorkspaceError::ReadLimitExceeded {
                path: relative,
                limit,
            });
        }

        // Metadata can race with a writer. Take one extra byte so growth after
        // the metadata check is detected while allocation remains bounded.
        let mut bytes =
            Vec::with_capacity(usize::try_from(metadata.len().min(limit)).unwrap_or_default());
        Read::by_ref(&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(WorkspaceError::ReadLimitExceeded {
                path: relative,
                limit,
            });
        }
        ensure_file_identity_unchanged(&file, &metadata, &relative)?;
        self.revalidate_identity()?;
        Ok(RevisionedFile {
            relative_path: relative,
            revision: digest(&bytes),
            bytes,
        })
    }

    /// Read a path that has already been canonicalized relative to the
    /// workspace, rejecting any later symlink redirection.
    pub(crate) fn read_resolved_limited(
        &self,
        relative: impl AsRef<Path>,
        limit: u64,
    ) -> WorkspaceResult<RevisionedFile> {
        self.read_resolved_limited_with_snapshot(relative, limit)
            .map(|(file, _snapshot)| file)
    }

    /// Read a canonical workspace file and return the exact file identity
    /// observed by the same descriptor that produced its bytes.  Mutation
    /// preparation uses this pair to bind approval to both content and inode;
    /// a later same-byte replacement must therefore fail closed.
    pub(crate) fn read_resolved_limited_with_snapshot(
        &self,
        relative: impl AsRef<Path>,
        limit: u64,
    ) -> WorkspaceResult<(RevisionedFile, FileRevisionSnapshot)> {
        let relative = validate_relative(relative.as_ref())?;
        #[cfg(unix)]
        let (mut file, metadata) = {
            // `resolve_expected` performs the canonical direct-path check and
            // a no-follow walk.  Open once more through the root descriptor
            // and retain that descriptor for the actual read so an
            // intermediate directory swap cannot redirect the operation.
            self.resolve_expected(&relative)?;
            let opened = self.open_secure_regular(&relative)?;
            (opened.file, opened.metadata)
        };
        #[cfg(not(unix))]
        let (mut file, metadata) = {
            let absolute = self.resolve_expected(&relative)?;
            open_regular_single_link(&absolute, &relative)?
        };
        let identity = file_identity(&metadata);
        if metadata.len() > limit {
            return Err(WorkspaceError::ReadLimitExceeded {
                path: relative,
                limit,
            });
        }

        let mut bytes =
            Vec::with_capacity(usize::try_from(metadata.len().min(limit)).unwrap_or_default());
        Read::by_ref(&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
            return Err(WorkspaceError::ReadLimitExceeded {
                path: relative,
                limit,
            });
        }
        ensure_file_identity_unchanged(&file, &metadata, &relative)?;
        self.revalidate_identity()?;
        let revision = digest(&bytes);
        let revision_snapshot = revision_snapshot_from_parts(&relative, revision.clone(), identity);
        Ok((
            RevisionedFile {
                relative_path: relative,
                revision,
                bytes,
            },
            revision_snapshot,
        ))
    }

    /// Validate a regular file through the same no-follow/single-link path as
    /// [`Workspace::read`] and return its current byte length.
    pub(crate) fn regular_file_size(&self, relative: impl AsRef<Path>) -> WorkspaceResult<u64> {
        self.resolve_regular_file(relative)
            .map(|(_, byte_length)| byte_length)
    }

    /// Resolve and validate a regular file, returning the canonical path
    /// relative to this workspace together with its current byte length.
    pub(crate) fn resolve_regular_file(
        &self,
        relative: impl AsRef<Path>,
    ) -> WorkspaceResult<(PathBuf, u64)> {
        let relative = validate_relative(relative.as_ref())?;
        let absolute = self.resolve_existing(&relative)?;
        let canonical_relative = self.canonical_relative(&absolute)?;
        #[cfg(unix)]
        let metadata = self.open_secure_regular(&canonical_relative)?.metadata;
        #[cfg(not(unix))]
        let (_, metadata) = open_regular_single_link(&absolute, &relative)?;
        Ok((canonical_relative, metadata.len()))
    }

    pub fn read_text(&self, relative: impl AsRef<Path>) -> WorkspaceResult<(String, String)> {
        let file = self.read(relative)?;
        let text = String::from_utf8(file.bytes)
            .map_err(|_| WorkspaceError::InvalidUtf8(file.relative_path.clone()))?;
        Ok((text, file.revision))
    }

    /// Atomically replace a regular file if and only if its BLAKE3 base hash matches.
    ///
    /// The replacement is first durably written to the same directory. The base
    /// hash is checked again from one validated file descriptor immediately before
    /// an anchored, directory-relative atomic rename.  On Unix, all path
    /// components are opened with `O_NOFOLLOW` from the retained root fd.
    /// There is no portable inode/hash compare-and-swap rename: a hostile actor
    /// can still replace the final name after the last check, so callers must
    /// treat the operation as object-bound rather than an unqualified external
    /// CAS.
    pub fn apply_patch(
        &self,
        relative: impl AsRef<Path>,
        base_revision: &str,
        replacement: &[u8],
    ) -> ApplyResult<ApplyPatchResult> {
        let relative = validate_relative(relative.as_ref())?;
        self.apply_patch_inner(relative, base_revision, replacement, false)
    }

    /// Apply a replacement to an already-canonical workspace-relative path.
    ///
    /// Unlike [`Workspace::apply_patch`], this rejects later symlink
    /// redirection. It is the publication primitive used by prepared mutation
    /// tools whose approved effect is bound to one canonical path.
    pub(crate) fn apply_patch_resolved(
        &self,
        relative: impl AsRef<Path>,
        base_revision: &str,
        replacement: &[u8],
    ) -> ApplyResult<ApplyPatchResult> {
        let relative = validate_relative(relative.as_ref())?;
        self.apply_patch_inner(relative, base_revision, replacement, true)
    }

    fn apply_patch_inner(
        &self,
        relative: PathBuf,
        base_revision: &str,
        replacement: &[u8],
        require_resolved_path: bool,
    ) -> ApplyResult<ApplyPatchResult> {
        // Serialize mutations made through clones of one Workspace.  This
        // gives the prepare/check/publish sequence deterministic semantics for
        // cooperating callers; the descriptor-relative publication below is
        // still required for callers in other processes.
        let _mutation_guard = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Keep the mutation bound to the root captured at Workspace::open.
        // `resolve_existing` repeats this check, but the explicit boundary is
        // intentional: future changes must not accidentally resolve a path
        // before the live identity guard.
        self.revalidate_identity()?;

        #[cfg(unix)]
        {
            // `apply_patch` historically accepted an in-workspace symlink
            // alias.  Resolve that alias once, then use its canonical direct
            // spelling for every descriptor operation.  The prepared variant
            // requires the spelling to be direct and therefore rejects the
            // alias in `resolve_expected`.
            let operation_relative = if require_resolved_path {
                self.resolve_expected(&relative)?;
                relative.clone()
            } else {
                let absolute = self.resolve_existing(&relative)?;
                self.canonical_relative(&absolute)?
            };
            self.apply_patch_dirfd(&relative, &operation_relative, base_revision, replacement)
        }

        #[cfg(not(unix))]
        {
            let absolute = if require_resolved_path {
                self.resolve_expected(&relative)?
            } else {
                self.resolve_existing(&relative)?
            };
            let (mut original, metadata) = open_regular_single_link(&absolute, &relative)?;
            let first_revision = digest_open_file(&mut original)?;
            ensure_file_identity_unchanged(&original, &metadata, &relative)?;
            if first_revision != base_revision {
                return Err(ApplyPatchError::StaleRevision {
                    path: relative,
                    expected: base_revision.to_owned(),
                    actual: first_revision,
                });
            }
            let parent = absolute
                .parent()
                .expect("a workspace-contained file always has a parent");
            let mut temporary = NamedTempFile::new_in(parent)?;
            temporary.write_all(replacement)?;
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
            temporary.as_file().sync_all()?;
            self.revalidate_identity()?;
            let current_absolute = if require_resolved_path {
                self.resolve_expected(&relative)?
            } else {
                self.resolve_existing(&relative)?
            };
            if current_absolute != absolute {
                return Err(WorkspaceError::ResolvedPathChanged {
                    expected: relative.clone(),
                    actual: current_absolute,
                }
                .into());
            }
            let (mut current, current_metadata) =
                open_regular_single_link(&current_absolute, &relative)?;
            let first_identity = file_identity(&metadata);
            let current_identity = file_identity(&current_metadata);
            if !same_file_object(first_identity, current_identity) {
                return Err(WorkspaceError::FileIdentityChanged {
                    path: relative.clone(),
                    expected_device: first_identity.device,
                    expected_inode: first_identity.inode,
                    actual_device: current_identity.device,
                    actual_inode: current_identity.inode,
                }
                .into());
            }
            let current_revision = digest_open_file(&mut current)?;
            ensure_file_identity_unchanged(&current, &current_metadata, &relative)?;
            if current_revision != base_revision {
                return Err(ApplyPatchError::StaleRevision {
                    path: relative.clone(),
                    expected: base_revision.to_owned(),
                    actual: current_revision,
                });
            }
            self.revalidate_identity()?;
            temporary.persist(&absolute)?;
            sync_directory(parent).map_err(|source| ApplyPatchError::DurabilityUncertain {
                path: relative.clone(),
                source,
            })?;
            Ok(ApplyPatchResult {
                relative_path: relative,
                previous_revision: base_revision.to_owned(),
                revision: digest(replacement),
                bytes_written: replacement.len() as u64,
            })
        }
    }

    #[cfg(unix)]
    fn apply_patch_dirfd(
        &self,
        result_relative: &Path,
        operation_relative: &Path,
        base_revision: &str,
        replacement: &[u8],
    ) -> ApplyResult<ApplyPatchResult> {
        self.apply_patch_dirfd_observed(
            result_relative,
            operation_relative,
            base_revision,
            replacement,
            |_| {},
        )
    }

    /// Descriptor-relative patch implementation.  `observer` is intentionally
    /// private and is used by race/crash tests to mutate the namespace at
    /// transaction boundaries without adding a production hook surface.
    #[cfg(unix)]
    fn apply_patch_dirfd_observed<F>(
        &self,
        result_relative: &Path,
        operation_relative: &Path,
        base_revision: &str,
        replacement: &[u8],
        mut observer: F,
    ) -> ApplyResult<ApplyPatchResult>
    where
        F: FnMut(PatchStage),
    {
        let mut opened = self.open_secure_regular(operation_relative)?;
        let original_identity = file_identity(&opened.metadata);
        let first_revision = digest_open_file(&mut opened.file)?;
        ensure_file_identity_unchanged(&opened.file, &opened.metadata, operation_relative)?;
        if first_revision != base_revision {
            return Err(ApplyPatchError::StaleRevision {
                path: result_relative.to_owned(),
                expected: base_revision.to_owned(),
                actual: first_revision,
            });
        }
        observer(PatchStage::TargetValidated);

        let mut temporary = DirectoryTempFile::create(&opened.parent)?;
        temporary.write_all(replacement)?;
        temporary
            .file()
            .set_permissions(opened.metadata.permissions())?;
        temporary.sync_all()?;
        observer(PatchStage::ReplacementSynced);

        // The parent descriptor is held from the component walk through the
        // rename.  A replacement of an intermediate directory or a symlink
        // insertion therefore cannot redirect this publication to a different
        // pathname.  We still revalidate the root and target identity before
        // the rename; POSIX has no primitive for “rename iff this inode/hash is
        // still at the name”, so an independent hostile writer can race the
        // final name replacement (see the module docs and tests).
        self.revalidate_identity()?;
        self.verify_parent_descriptor(operation_relative, &opened.parent)?;
        let (mut current, current_metadata) =
            open_regular_at(&opened.parent, &opened.name, operation_relative)?;
        let current_identity = file_identity(&current_metadata);
        if !same_file_object(original_identity, current_identity) {
            return Err(WorkspaceError::FileIdentityChanged {
                path: result_relative.to_owned(),
                expected_device: original_identity.device,
                expected_inode: original_identity.inode,
                actual_device: current_identity.device,
                actual_inode: current_identity.inode,
            }
            .into());
        }
        let current_revision = digest_open_file(&mut current)?;
        ensure_file_identity_unchanged(&current, &current_metadata, operation_relative)?;
        if current_revision != base_revision {
            return Err(ApplyPatchError::StaleRevision {
                path: result_relative.to_owned(),
                expected: base_revision.to_owned(),
                actual: current_revision,
            });
        }

        observer(PatchStage::BeforePublish);
        self.revalidate_identity()?;
        temporary.publish(&opened.name)?;
        sync_directory_fd(&opened.parent).map_err(|source| {
            ApplyPatchError::DurabilityUncertain {
                path: result_relative.to_owned(),
                source,
            }
        })?;
        Ok(ApplyPatchResult {
            relative_path: result_relative.to_owned(),
            previous_revision: base_revision.to_owned(),
            revision: digest(replacement),
            bytes_written: replacement.len() as u64,
        })
    }

    /// Recursively list regular files without following symlinks.
    pub fn list(&self, relative: impl AsRef<Path>) -> WorkspaceResult<Vec<PathBuf>> {
        self.revalidate_identity()?;
        let relative = validate_relative_allow_empty(relative.as_ref())?;
        let start = if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.resolve_existing(&relative)?
        };
        let mut files = Vec::new();
        for entry in WalkDir::new(start).follow_links(false).sort_by_file_name() {
            let entry = entry.map_err(|error| {
                WorkspaceError::Io(
                    error
                        .into_io_error()
                        .unwrap_or_else(|| std::io::Error::other("failed to walk workspace")),
                )
            })?;
            if entry.file_type().is_file() {
                let absolute = fs::canonicalize(entry.path())?;
                self.ensure_contained(&absolute)?;
                let relative = absolute
                    .strip_prefix(&self.root)
                    .expect("containment checked")
                    .to_owned();
                ensure_single_link(&fs::metadata(&absolute)?, &relative)?;
                files.push(relative);
            }
        }
        self.revalidate_identity()?;
        Ok(files)
    }

    /// A deliberately small literal text search used by the M1 read-only loop.
    pub fn search_text(&self, needle: &str, limit: usize) -> WorkspaceResult<Vec<PathBuf>> {
        if needle.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut matches = Vec::new();
        for relative in self.list(Path::new(""))? {
            let bytes = self.read(&relative)?.bytes;
            if bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
            {
                matches.push(relative);
                if matches.len() == limit {
                    break;
                }
            }
        }
        Ok(matches)
    }

    fn resolve_existing(&self, relative: &Path) -> WorkspaceResult<PathBuf> {
        self.revalidate_identity()?;
        let candidate = self.root.join(relative);
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NotFound(relative.to_owned())
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        // Canonicalization can race a root replacement.  Revalidate before
        // exposing the result so a path resolved against a detached root is
        // not silently accepted.
        self.revalidate_identity()?;
        self.ensure_contained(&canonical)?;
        Ok(canonical)
    }

    fn canonical_relative(&self, absolute: &Path) -> WorkspaceResult<PathBuf> {
        let relative = absolute
            .strip_prefix(&self.root)
            .map_err(|_| WorkspaceError::OutsideWorkspace {
                root: self.root.clone(),
                path: absolute.to_owned(),
            })?
            .to_owned();
        validate_relative(&relative)
    }

    fn resolve_expected(&self, relative: &Path) -> WorkspaceResult<PathBuf> {
        let canonical = self.resolve_existing(relative)?;
        let actual = self.canonical_relative(&canonical)?;
        if actual != relative {
            return Err(WorkspaceError::ResolvedPathChanged {
                expected: relative.to_owned(),
                actual,
            });
        }
        #[cfg(unix)]
        {
            // Canonicalization above preserves the existing error taxonomy
            // for aliases, while this walk closes the intermediate-component
            // TOCTOU window.  No descriptor returned here is used for the
            // eventual side effect; callers reopen and retain the parent fd
            // for that transaction.
            self.open_secure_regular(relative).map(|_| ())?;
        }
        Ok(canonical)
    }

    #[cfg(unix)]
    fn open_secure_regular(&self, relative: &Path) -> WorkspaceResult<OpenedRelative> {
        self.revalidate_identity()?;
        let opened = open_relative_nofollow(&self.root_dir, relative)
            .map_err(|error| self.map_secure_path_error(relative, error))?;
        let metadata = opened.file.metadata()?;
        if !metadata.is_file() {
            return Err(WorkspaceError::NotAFile(relative.to_owned()));
        }
        ensure_single_link(&metadata, relative)?;
        self.revalidate_identity()?;
        Ok(OpenedRelative { metadata, ..opened })
    }

    #[cfg(unix)]
    fn open_secure_directory(&self, relative: &Path) -> WorkspaceResult<File> {
        self.revalidate_identity()?;
        let directory = open_directory_relative_nofollow(&self.root_dir, relative)
            .map_err(|error| self.map_secure_path_error(relative, error))?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::NotDirectory(relative.to_owned()));
        }
        self.revalidate_identity()?;
        Ok(directory)
    }

    #[cfg(unix)]
    fn verify_parent_descriptor(
        &self,
        relative: &Path,
        expected_parent: &File,
    ) -> WorkspaceResult<()> {
        let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
        let live_parent = open_directory_relative_nofollow(&self.root_dir, parent_relative)
            .map_err(|error| self.map_secure_path_error(parent_relative, error))?;
        let expected_identity = file_identity(&expected_parent.metadata()?);
        let actual_identity = file_identity(&live_parent.metadata()?);
        if !same_file_object(expected_identity, actual_identity) {
            return Err(WorkspaceError::FileIdentityChanged {
                path: relative.to_owned(),
                expected_device: expected_identity.device,
                expected_inode: expected_identity.inode,
                actual_device: actual_identity.device,
                actual_inode: actual_identity.inode,
            });
        }
        Ok(())
    }

    #[cfg(unix)]
    fn map_secure_path_error(&self, relative: &Path, error: std::io::Error) -> WorkspaceError {
        if error.kind() == std::io::ErrorKind::NotFound {
            return WorkspaceError::NotFound(relative.to_owned());
        }
        // O_NOFOLLOW reports ELOOP on some Unix implementations and
        // ENOTDIR on others when a symlink (or a non-directory) appears in an
        // intermediate component. Preserve the public path-change/escape
        // taxonomy by taking a best-effort diagnostic canonicalization. The
        // diagnostic result is never used to perform the operation.
        if error.raw_os_error() == Some(Errno::LOOP.raw_os_error())
            || error.kind() == std::io::ErrorKind::NotADirectory
        {
            let candidate = self.root.join(relative);
            if let Ok(canonical) = fs::canonicalize(&candidate) {
                if !canonical.starts_with(&self.root) {
                    return WorkspaceError::OutsideWorkspace {
                        root: self.root.clone(),
                        path: canonical,
                    };
                }
                if let Ok(actual) = self.canonical_relative(&canonical) {
                    return WorkspaceError::ResolvedPathChanged {
                        expected: relative.to_owned(),
                        actual,
                    };
                }
            }
            return WorkspaceError::ResolvedPathChanged {
                expected: relative.to_owned(),
                actual: relative.to_owned(),
            };
        }
        WorkspaceError::Io(error)
    }

    fn revision_snapshot_at(
        &self,
        _absolute: &Path,
        relative: &Path,
    ) -> WorkspaceResult<FileRevisionSnapshot> {
        // `absolute` is resolved by a caller after a live root check.  Keep a
        // second check here because this helper is also used by public
        // revalidation APIs and should remain safe if the call graph changes.
        self.revalidate_identity()?;
        #[cfg(unix)]
        let (mut file, metadata) = {
            // Ignore the path after validation and obtain the bytes and
            // identity from one root-dirfd-relative descriptor.
            let opened = self.open_secure_regular(relative)?;
            (opened.file, opened.metadata)
        };
        #[cfg(not(unix))]
        let (mut file, metadata) = open_regular_single_link(_absolute, relative)?;
        let identity = file_identity(&metadata);
        let revision = digest_open_file(&mut file)?;
        ensure_file_identity_unchanged(&file, &metadata, relative)?;
        self.revalidate_identity()?;
        Ok(revision_snapshot_from_parts(relative, revision, identity))
    }

    fn ensure_contained(&self, canonical: &Path) -> WorkspaceResult<()> {
        if canonical.starts_with(&self.root) {
            Ok(())
        } else {
            Err(WorkspaceError::OutsideWorkspace {
                root: self.root.clone(),
                path: canonical.to_owned(),
            })
        }
    }
}

fn validate_relative(path: &Path) -> WorkspaceResult<PathBuf> {
    let path = validate_relative_allow_empty(path)?;
    if path.as_os_str().is_empty() {
        return Err(WorkspaceError::InvalidRelativePath(path));
    }
    Ok(path)
}

fn validate_relative_allow_empty(path: &Path) -> WorkspaceResult<PathBuf> {
    if path.as_os_str().as_encoded_bytes().contains(&0)
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(WorkspaceError::InvalidRelativePath(path.to_owned()));
    }
    // Remove harmless `.` components without interpreting user input as an absolute path.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(value) = component {
            normalized.push(value);
        }
    }
    Ok(normalized)
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn digest_open_file(file: &mut File) -> WorkspaceResult<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: Option<u64>,
    inode: Option<u64>,
    byte_length: u64,
    mode: Option<u32>,
    link_count: Option<u64>,
    modified: Option<(i64, u32)>,
    changed: Option<(i64, u32)>,
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            device: Some(metadata.dev()),
            inode: Some(metadata.ino()),
            byte_length: metadata.len(),
            mode: Some(metadata.mode()),
            link_count: Some(metadata.nlink()),
            modified: Some((metadata.mtime(), metadata.mtime_nsec() as u32)),
            changed: Some((metadata.ctime(), metadata.ctime_nsec() as u32)),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            device: None,
            inode: None,
            byte_length: metadata.len(),
            mode: None,
            link_count: None,
            modified: None,
            changed: None,
        }
    }
}

fn same_file_object(left: FileIdentity, right: FileIdentity) -> bool {
    // On platforms with stable device/inode values, object identity is the
    // primary CAS key.  On platforms without them, the content digest check
    // remains authoritative and this function conservatively compares the
    // observed length.
    match (left.device, left.inode, right.device, right.inode) {
        (Some(left_device), Some(left_inode), Some(right_device), Some(right_inode)) => {
            left_device == right_device && left_inode == right_inode && left.mode == right.mode
        }
        _ => left.byte_length == right.byte_length,
    }
}

fn ensure_file_identity_unchanged(
    file: &File,
    before: &fs::Metadata,
    relative: &Path,
) -> WorkspaceResult<()> {
    let after = file.metadata()?;
    let before_identity = file_identity(before);
    let after_identity = file_identity(&after);
    if !same_file_object(before_identity, after_identity)
        || before_identity.byte_length != after_identity.byte_length
        || before_identity.link_count != after_identity.link_count
        || before_identity.modified != after_identity.modified
        || before_identity.changed != after_identity.changed
    {
        return Err(WorkspaceError::FileChangedDuringRead(relative.to_owned()));
    }
    // A hard link can be added while the descriptor is being read.  Check the
    // post-read metadata as well as the initial metadata so the caller never
    // returns bytes from an object that became externally reachable during the
    // operation.
    ensure_single_link(&after, relative)?;
    Ok(())
}

fn revision_snapshot_from_parts(
    relative: &Path,
    revision: String,
    identity: FileIdentity,
) -> FileRevisionSnapshot {
    FileRevisionSnapshot {
        relative_path: relative.to_owned(),
        revision,
        byte_length: identity.byte_length,
        device: identity.device,
        inode: identity.inode,
    }
}

#[cfg(not(unix))]
fn open_regular_single_link(
    absolute: &Path,
    display_path: &Path,
) -> WorkspaceResult<(File, fs::Metadata)> {
    let file = open_readonly_nofollow(absolute)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(WorkspaceError::NotAFile(display_path.to_owned()));
    }
    ensure_single_link(&metadata, display_path)?;
    Ok((file, metadata))
}

#[cfg(not(unix))]
fn open_readonly_nofollow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn ensure_single_link(metadata: &fs::Metadata, path: &Path) -> WorkspaceResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(WorkspaceError::MultipleHardLinks(path.to_owned()));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, path);
    }
    Ok(())
}

fn workspace_identity_snapshot_for_canonical_root(
    canonical_root: &Path,
) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
    let metadata = fs::metadata(canonical_root)?;
    if !metadata.is_dir() {
        return Err(WorkspaceError::NotDirectory(canonical_root.to_owned()));
    }

    let (device, inode) = directory_identity(&metadata);
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical_root.as_os_str().as_encoded_bytes());
    #[cfg(unix)]
    {
        hasher.update(&device.unwrap_or_default().to_le_bytes());
        hasher.update(&inode.unwrap_or_default().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(&metadata.len().to_le_bytes());
    }
    Ok(WorkspaceIdentitySnapshot {
        canonical_root: canonical_root.to_owned(),
        digest: hasher.finalize().to_hex().to_string(),
        device,
        inode,
    })
}

fn directory_identity(metadata: &fs::Metadata) -> (Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (Some(metadata.dev()), Some(metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        (None, None)
    }
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    let _ = path;
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
struct OpenedRelative {
    parent: File,
    name: OsString,
    file: File,
    metadata: fs::Metadata,
}

/// Internal transaction points used only by deterministic race tests.  The
/// production path always supplies a no-op observer.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchStage {
    TargetValidated,
    ReplacementSynced,
    BeforePublish,
}

/// A temporary file created relative to a pinned parent directory.
///
/// `NamedTempFile::new_in(parent_path)` is intentionally not used on Unix:
/// the parent path can be renamed or replaced after it has been checked.  An
/// O_EXCL `openat` keeps creation and cleanup in the same directory object that
/// will receive the final `renameat`.
#[cfg(unix)]
#[derive(Debug)]
struct DirectoryTempFile {
    parent: File,
    name: OsString,
    file: File,
    published: bool,
}

#[cfg(unix)]
impl DirectoryTempFile {
    fn create(parent: &File) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
        let parent = parent.try_clone()?;
        let mode = Mode::from_raw_mode(0o600);
        for _ in 0..64 {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(".yeux-tmp-{}-{}", std::process::id(), sequence));
            match openat(
                &parent,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                mode,
            ) {
                Ok(descriptor) => {
                    return Ok(Self {
                        parent,
                        name,
                        file: descriptor.into(),
                        published: false,
                    });
                }
                Err(error) if error == Errno::EXIST => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "unable to allocate a unique workspace temporary name",
        ))
    }

    fn file(&self) -> &File {
        &self.file
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync_all(&self) -> std::io::Result<()> {
        self.file.sync_all()
    }

    fn publish(mut self, target: &OsStr) -> std::io::Result<()> {
        renameat(&self.parent, &self.name, &self.parent, target)?;
        self.published = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for DirectoryTempFile {
    fn drop(&mut self) {
        if !self.published {
            // The descriptor remains valid even if the parent was renamed;
            // unlinkat therefore cleans up the exact directory in which the
            // temporary was created, never a path-resolved substitute.
            let _ = unlinkat(&self.parent, &self.name, AtFlags::empty());
        }
    }
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    Ok(descriptor.into())
}

#[cfg(unix)]
fn open_directory_relative_nofollow(root: &File, relative: &Path) -> std::io::Result<File> {
    let mut current = root.try_clone()?;
    for component in relative.components() {
        let descriptor = openat(
            &current,
            component.as_os_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        current = descriptor.into();
    }
    Ok(current)
}

#[cfg(unix)]
fn open_relative_nofollow(root: &File, relative: &Path) -> std::io::Result<OpenedRelative> {
    let mut components = relative
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect::<Vec<_>>();
    let name = components.pop().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace relative path must contain a file name",
        )
    })?;
    let mut parent = root.try_clone()?;
    for component in components {
        let descriptor = openat(
            &parent,
            &component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        parent = descriptor.into();
    }
    let descriptor = openat(
        &parent,
        &name,
        // `O_NONBLOCK` is essential before the metadata check: opening a
        // FIFO read-only without a writer blocks in the kernel, so a hostile
        // workspace entry could wedge planning before the daemon's normal
        // cancellation/timeout boundary is reached.  Regular files are
        // unaffected by the flag; the subsequent fstat still rejects every
        // special file before bytes are read.
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let file: File = descriptor.into();
    let metadata = file.metadata()?;
    Ok(OpenedRelative {
        parent,
        name,
        file,
        metadata,
    })
}

#[cfg(unix)]
fn open_regular_at(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> WorkspaceResult<(File, fs::Metadata)> {
    let descriptor = openat(
        parent,
        name,
        // Keep the revalidation open non-blocking as well.  This path runs
        // after the first descriptor check and must not reintroduce a FIFO
        // denial-of-service window during the final identity comparison.
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| WorkspaceError::Io(error.into()))?;
    let file: File = descriptor.into();
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(WorkspaceError::NotAFile(display_path.to_owned()));
    }
    ensure_single_link(&metadata, display_path)?;
    Ok((file, metadata))
}

#[cfg(unix)]
fn sync_directory_fd(directory: &File) -> std::io::Result<()> {
    directory.sync_all()
}

#[cfg(unix)]
fn workspace_identity_snapshot_for_descriptor(
    canonical_root: &Path,
    descriptor: &File,
) -> WorkspaceResult<WorkspaceIdentitySnapshot> {
    let metadata = descriptor.metadata()?;
    if !metadata.is_dir() {
        return Err(WorkspaceError::NotDirectory(canonical_root.to_owned()));
    }
    let (device, inode) = directory_identity(&metadata);
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical_root.as_os_str().as_encoded_bytes());
    hasher.update(&device.unwrap_or_default().to_le_bytes());
    hasher.update(&inode.unwrap_or_default().to_le_bytes());
    Ok(WorkspaceIdentitySnapshot {
        canonical_root: canonical_root.to_owned(),
        digest: hasher.finalize().to_hex().to_string(),
        device,
        inode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("hello.txt"), b"old").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        (directory, workspace)
    }

    #[test]
    fn reads_with_blake3_revision() {
        let (_directory, workspace) = setup();
        let file = workspace.read("hello.txt").unwrap();
        assert_eq!(file.bytes, b"old");
        assert_eq!(file.revision, blake3::hash(b"old").to_hex().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn special_file_read_is_rejected_without_blocking() {
        use std::{sync::mpsc, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("unreadable-fifo");
        let mkfifo = ["/usr/bin/mkfifo", "/bin/mkfifo"]
            .into_iter()
            .find(|path| std::path::Path::new(path).is_file())
            .expect("test host has mkfifo");
        let status = std::process::Command::new(mkfifo)
            .arg(&fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success());
        let workspace = Workspace::open(directory.path()).unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = workspace.read("unreadable-fifo").map(|_| ());
            let _ = sender.send(result.map_err(|error| error.to_string()));
        });

        let result = receiver
            .recv_timeout(Duration::from_millis(500))
            .expect("special-file validation must not block waiting for a FIFO writer");
        assert!(matches!(result, Err(message) if message.contains("regular file")));
    }

    #[test]
    fn captures_and_revalidates_live_root_identity() {
        let (_directory, workspace) = setup();
        let snapshot = workspace.identity_snapshot();
        assert_eq!(snapshot.digest, workspace.identity());
        assert_eq!(snapshot.canonical_root, workspace.root());
        assert_eq!(workspace.live_identity().unwrap(), snapshot);
        assert_eq!(workspace.revalidate_identity().unwrap(), snapshot);
        assert_eq!(
            workspace
                .revalidate_identity_digest(workspace.identity())
                .unwrap(),
            snapshot
        );
        assert_eq!(
            workspace.revalidate_identity_against(&snapshot).unwrap(),
            snapshot
        );
        assert!(matches!(
            workspace.revalidate_identity_digest("0"),
            Err(WorkspaceError::WorkspaceIdentityDigestMismatch { .. })
        ));

        #[cfg(unix)]
        {
            assert!(snapshot.device.is_some());
            assert!(snapshot.inode.is_some());
        }
    }

    #[test]
    fn root_replacement_fails_closed_for_revalidation_and_reads() {
        let parent = tempfile::tempdir().unwrap();
        let root_path = parent.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("hello.txt"), b"old").unwrap();
        let workspace = Workspace::open(&root_path).unwrap();

        let detached = parent.path().join("detached-root");
        fs::rename(&root_path, &detached).unwrap();
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("hello.txt"), b"attacker content").unwrap();

        let error = workspace.revalidate_identity().unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::WorkspaceIdentityChanged { .. }
        ));
        assert!(matches!(
            workspace.read("hello.txt"),
            Err(WorkspaceError::WorkspaceIdentityChanged { .. })
        ));
        // The old detached tree remains untouched; the path replacement is
        // never silently accepted as the original workspace.
        assert_eq!(fs::read(detached.join("hello.txt")).unwrap(), b"old");
    }

    #[test]
    fn revision_snapshot_revalidates_digest_and_file_identity() {
        let (_directory, workspace) = setup();
        let snapshot = workspace.revision_snapshot("hello.txt").unwrap();
        assert_eq!(snapshot.revision, blake3::hash(b"old").to_hex().to_string());
        assert_eq!(workspace.revalidate_revision(&snapshot).unwrap(), snapshot);

        let mut forged_length = snapshot.clone();
        forged_length.byte_length = forged_length.byte_length.saturating_add(1);
        assert!(matches!(
            workspace.revalidate_revision(&forged_length),
            Err(WorkspaceError::FileIdentityChanged { .. })
        ));

        fs::write(workspace.root().join("hello.txt"), b"new").unwrap();
        assert!(matches!(
            workspace.revalidate_revision(&snapshot),
            Err(WorkspaceError::RevisionChanged { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn revision_revalidation_rejects_same_bytes_from_a_new_inode() {
        let (_directory, workspace) = setup();
        let snapshot = workspace.revision_snapshot("hello.txt").unwrap();
        let replacement = tempfile::NamedTempFile::new_in(workspace.root()).unwrap();
        fs::write(replacement.path(), b"old").unwrap();
        fs::rename(replacement.path(), workspace.root().join("hello.txt")).unwrap();

        let error = workspace.revalidate_revision(&snapshot).unwrap_err();
        assert!(matches!(error, WorkspaceError::FileIdentityChanged { .. }));
        assert_eq!(
            fs::read(workspace.root().join("hello.txt")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn apply_patch_revalidates_root_before_publishing() {
        let parent = tempfile::tempdir().unwrap();
        let root_path = parent.path().join("root");
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("hello.txt"), b"old").unwrap();
        let workspace = Workspace::open(&root_path).unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();

        fs::rename(&root_path, parent.path().join("detached-root")).unwrap();
        fs::create_dir(&root_path).unwrap();
        fs::write(root_path.join("hello.txt"), b"attacker content").unwrap();

        let error = workspace
            .apply_patch("hello.txt", &base, b"new")
            .unwrap_err();
        assert!(matches!(
            error,
            ApplyPatchError::Workspace(WorkspaceError::WorkspaceIdentityChanged { .. })
        ));
        assert_eq!(
            fs::read(root_path.join("hello.txt")).unwrap(),
            b"attacker content"
        );
    }

    #[test]
    fn rejects_absolute_and_parent_paths() {
        let (_directory, workspace) = setup();
        assert!(matches!(
            workspace.read("../hello.txt"),
            Err(WorkspaceError::InvalidRelativePath(_))
        ));
        assert!(matches!(
            workspace.read(std::env::temp_dir().join("anything")),
            Err(WorkspaceError::InvalidRelativePath(_))
        ));
        assert!(matches!(
            workspace.read("nul\0file"),
            Err(WorkspaceError::InvalidRelativePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (directory, workspace) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), directory.path().join("escape")).unwrap();
        assert!(matches!(
            workspace.read("escape"),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_link_reads_and_searches() {
        let (directory, workspace) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"outside secret").unwrap();
        fs::hard_link(outside.path(), directory.path().join("linked-secret")).unwrap();

        assert!(matches!(
            workspace.read("linked-secret"),
            Err(WorkspaceError::MultipleHardLinks(_))
        ));
        assert!(matches!(
            workspace.search_text("outside secret", 10),
            Err(WorkspaceError::MultipleHardLinks(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_link_patch_without_changing_external_inode() {
        let (directory, workspace) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"outside secret").unwrap();
        let linked = directory.path().join("linked-secret");
        fs::hard_link(outside.path(), &linked).unwrap();
        let base = blake3::hash(b"outside secret").to_hex().to_string();

        assert!(matches!(
            workspace.apply_patch("linked-secret", &base, b"replacement"),
            Err(ApplyPatchError::Workspace(
                WorkspaceError::MultipleHardLinks(_)
            ))
        ));
        assert_eq!(fs::read(outside.path()).unwrap(), b"outside secret");
        assert_eq!(fs::read(linked).unwrap(), b"outside secret");
    }

    #[test]
    fn stale_patch_is_rejected_without_modifying_file() {
        let (directory, workspace) = setup();
        let error = workspace
            .apply_patch("hello.txt", "wrong", b"new")
            .unwrap_err();
        assert!(matches!(error, ApplyPatchError::StaleRevision { .. }));
        assert_eq!(
            fs::read(directory.path().join("hello.txt")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn patch_is_atomic_and_returns_new_revision() {
        let (directory, workspace) = setup();
        let base = workspace.read("hello.txt").unwrap().revision;
        let result = workspace.apply_patch("hello.txt", &base, b"new").unwrap();
        assert_eq!(
            fs::read(directory.path().join("hello.txt")).unwrap(),
            b"new"
        );
        assert_eq!(result.revision, blake3::hash(b"new").to_hex().to_string());
        assert_eq!(result.previous_revision, base);
    }

    #[cfg(unix)]
    #[test]
    fn dirfd_patch_rejects_intermediate_directory_replacement_before_publish() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let source = root.join("src");
        let detached = parent.path().join("detached-src");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("target.txt"), b"old").unwrap();
        fs::write(outside.path().join("target.txt"), b"outside").unwrap();
        let workspace = Workspace::open(&root).unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();

        let result = workspace.apply_patch_dirfd_observed(
            Path::new("src/target.txt"),
            Path::new("src/target.txt"),
            &base,
            b"new",
            |stage| {
                if stage == PatchStage::ReplacementSynced {
                    fs::rename(&source, &detached).unwrap();
                    symlink(outside.path(), &source).unwrap();
                }
            },
        );

        assert!(matches!(
            result,
            Err(ApplyPatchError::Workspace(
                WorkspaceError::OutsideWorkspace { .. }
            )) | Err(ApplyPatchError::Workspace(
                WorkspaceError::ResolvedPathChanged { .. }
            ))
        ));
        assert_eq!(fs::read(detached.join("target.txt")).unwrap(), b"old");
        assert_eq!(
            fs::read(outside.path().join("target.txt")).unwrap(),
            b"outside"
        );
        // The temporary was created through the detached parent descriptor and
        // is cleaned there; no path-resolved temporary leaks into the attacker
        // replacement.
        assert!(fs::read_dir(&detached)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".yeux-tmp-")));
    }

    #[cfg(unix)]
    #[test]
    fn dirfd_patch_rejects_parent_replacement_with_another_in_workspace_directory() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let source = root.join("src");
        let detached = parent.path().join("detached-src");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("target.txt"), b"old").unwrap();
        let workspace = Workspace::open(&root).unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();

        let result = workspace.apply_patch_dirfd_observed(
            Path::new("src/target.txt"),
            Path::new("src/target.txt"),
            &base,
            b"new",
            |stage| {
                if stage == PatchStage::ReplacementSynced {
                    fs::rename(&source, &detached).unwrap();
                    fs::create_dir(&source).unwrap();
                    fs::write(source.join("target.txt"), b"attacker").unwrap();
                }
            },
        );

        assert!(matches!(
            result,
            Err(ApplyPatchError::Workspace(
                WorkspaceError::FileIdentityChanged { .. }
            ))
        ));
        assert_eq!(fs::read(detached.join("target.txt")).unwrap(), b"old");
        assert_eq!(fs::read(source.join("target.txt")).unwrap(), b"attacker");
    }

    #[cfg(unix)]
    #[test]
    fn dirfd_publish_does_not_follow_a_final_target_symlink_race() {
        use std::os::unix::fs::symlink;

        let (directory, workspace) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), b"outside").unwrap();
        let base = blake3::hash(b"old").to_hex().to_string();
        let result = workspace.apply_patch_dirfd_observed(
            Path::new("hello.txt"),
            Path::new("hello.txt"),
            &base,
            b"new",
            |stage| {
                if stage == PatchStage::BeforePublish {
                    fs::remove_file(directory.path().join("hello.txt")).unwrap();
                    symlink(outside.path(), directory.path().join("hello.txt")).unwrap();
                }
            },
        );

        // renameat operates on the pinned root directory and replaces the
        // symlink entry itself; it never follows it to the outside file.
        assert!(result.is_ok());
        assert_eq!(
            fs::read(directory.path().join("hello.txt")).unwrap(),
            b"new"
        );
        assert_eq!(fs::read(outside.path()).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn mutations_through_workspace_clones_are_serialized() {
        use std::{
            sync::{Arc, Barrier},
            thread,
        };

        let (_directory, workspace) = setup();
        let base = workspace.read("hello.txt").unwrap().revision;
        let workspace = Arc::new(workspace);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for replacement in [b"one".as_slice(), b"two".as_slice()] {
            let workspace = Arc::clone(&workspace);
            let barrier = Arc::clone(&barrier);
            let base = base.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                workspace.apply_patch("hello.txt", &base, replacement)
            }));
        }
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| { matches!(result, Err(ApplyPatchError::StaleRevision { .. })) })
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn detects_same_length_content_change_during_descriptor_read() {
        let (directory, _workspace) = setup();
        let path = directory.path().join("hello.txt");
        let file = File::open(&path).unwrap();
        let metadata = file.metadata().unwrap();
        fs::write(&path, b"new").unwrap();
        let error =
            ensure_file_identity_unchanged(&file, &metadata, Path::new("hello.txt")).unwrap_err();
        assert!(matches!(error, WorkspaceError::FileChangedDuringRead(_)));
    }

    #[cfg(unix)]
    #[test]
    fn patch_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, workspace) = setup();
        let path = directory.path().join("hello.txt");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let base = workspace.read("hello.txt").unwrap().revision;
        workspace.apply_patch("hello.txt", &base, b"new").unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn lists_and_searches_without_following_links() {
        let (directory, workspace) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/lib.rs"), "needle").unwrap();
        assert_eq!(
            workspace.search_text("needle", 10).unwrap(),
            vec![PathBuf::from("src/lib.rs")]
        );
    }
}
