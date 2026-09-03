//! Workspace path confinement and revision-checked atomic edits.

#![allow(clippy::result_large_err)]

use std::{
    fmt,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    identity: String,
    root_identity: WorkspaceIdentitySnapshot,
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
}

pub type WorkspaceResult<T> = std::result::Result<T, WorkspaceError>;
pub type ApplyResult<T> = std::result::Result<T, ApplyPatchError>;

impl Workspace {
    pub fn open(root: impl AsRef<Path>) -> WorkspaceResult<Self> {
        let root = fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(WorkspaceError::NotDirectory(root));
        }
        let root_identity = workspace_identity_snapshot_for_canonical_root(&root)?;
        let identity = root_identity.digest.clone();
        Ok(Self {
            root,
            identity,
            root_identity,
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
        workspace_identity_snapshot_for_canonical_root(&canonical)
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
        self.revalidate_identity()?;
        Ok(directory)
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> WorkspaceResult<RevisionedFile> {
        let relative = validate_relative(relative.as_ref())?;
        let absolute = self.resolve_existing(&relative)?;
        let (mut file, metadata) = open_regular_single_link(&absolute, &relative)?;
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
        let absolute = self.resolve_existing(&relative)?;
        let (mut file, metadata) = open_regular_single_link(&absolute, &relative)?;
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
        let absolute = self.resolve_expected(&relative)?;
        let (mut file, metadata) = open_regular_single_link(&absolute, &relative)?;
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
        let (_, metadata) = open_regular_single_link(&absolute, &relative)?;
        let canonical_relative = absolute
            .strip_prefix(&self.root)
            .expect("resolve_existing enforces workspace containment")
            .to_owned();
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
    /// the atomic rename. Full dirfd-relative publication is still an M2 boundary.
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
        // Keep the mutation bound to the root captured at Workspace::open.
        // `resolve_existing` repeats this check, but the explicit boundary is
        // intentional: future changes must not accidentally resolve a path
        // before the live identity guard.
        self.revalidate_identity()?;
        let absolute = if require_resolved_path {
            self.resolve_expected(&relative)?
        } else {
            self.resolve_existing(&relative)?
        };
        let (mut original, metadata) = open_regular_single_link(&absolute, &relative)?;
        let original_identity = file_identity(&metadata);
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

        // Revalidate both the root and the canonical target before publication.
        // This rejects a same-byte replacement with a different device/inode.
        // A dirfd-relative rename is still required to make the final
        // check-to-rename interval race-free on a hostile shared filesystem;
        // see docs/ARCHITECTURE.md for this remaining boundary.
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
        let current_identity = file_identity(&current_metadata);
        if !same_file_object(original_identity, current_identity) {
            return Err(WorkspaceError::FileIdentityChanged {
                path: relative.clone(),
                expected_device: original_identity.device,
                expected_inode: original_identity.inode,
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

        // Repeat the root check immediately before the path-based publish.
        // This is the strongest portable guard available in this module; the
        // remaining tiny rename window is deliberately documented rather than
        // overstated as a complete CAS.
        self.revalidate_identity()?;
        temporary.persist(&absolute)?;
        sync_directory(parent)?;
        Ok(ApplyPatchResult {
            relative_path: relative,
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

    fn resolve_expected(&self, relative: &Path) -> WorkspaceResult<PathBuf> {
        let canonical = self.resolve_existing(relative)?;
        let actual = canonical
            .strip_prefix(&self.root)
            .expect("resolve_existing enforces workspace containment")
            .to_owned();
        if actual != relative {
            return Err(WorkspaceError::ResolvedPathChanged {
                expected: relative.to_owned(),
                actual,
            });
        }
        Ok(canonical)
    }

    fn revision_snapshot_at(
        &self,
        absolute: &Path,
        relative: &Path,
    ) -> WorkspaceResult<FileRevisionSnapshot> {
        // `absolute` is resolved by a caller after a live root check.  Keep a
        // second check here because this helper is also used by public
        // revalidation APIs and should remain safe if the call graph changes.
        self.revalidate_identity()?;
        let (mut file, metadata) = open_regular_single_link(absolute, relative)?;
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
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            device: None,
            inode: None,
            byte_length: metadata.len(),
            mode: None,
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
    {
        return Err(WorkspaceError::FileChangedDuringRead(relative.to_owned()));
    }
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

fn open_readonly_nofollow(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{open, Mode, OFlags};
        let descriptor = open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        Ok(descriptor.into())
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
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

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
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
