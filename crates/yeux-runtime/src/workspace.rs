//! Workspace path confinement and revision-checked atomic edits.

use std::{
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
    #[error("path does not exist: {0}")]
    NotFound(PathBuf),
    #[error("path is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("file has multiple hard links and is not safe to access: {0}")]
    MultipleHardLinks(PathBuf),
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
        let identity = workspace_identity(&root)?;
        Ok(Self { root, identity })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable for the canonical root and its filesystem identity during this run.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn resolve_directory(&self, relative: impl AsRef<Path>) -> WorkspaceResult<PathBuf> {
        let relative = validate_relative_allow_empty(relative.as_ref())?;
        let directory = if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.resolve_existing(&relative)?
        };
        if !directory.is_dir() {
            return Err(WorkspaceError::NotDirectory(relative));
        }
        Ok(directory)
    }

    pub fn read(&self, relative: impl AsRef<Path>) -> WorkspaceResult<RevisionedFile> {
        let relative = validate_relative(relative.as_ref())?;
        let absolute = self.resolve_existing(&relative)?;
        let (mut file, _) = open_regular_single_link(&absolute, &relative)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(RevisionedFile {
            relative_path: relative,
            revision: digest(&bytes),
            bytes,
        })
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
        let absolute = self.resolve_existing(&relative)?;
        let (mut original, metadata) = open_regular_single_link(&absolute, &relative)?;
        let first_revision = digest_open_file(&mut original)?;
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

        // Re-resolve to detect a symlink swap and re-hash immediately before rename.
        let current_absolute = self.resolve_existing(&relative)?;
        if current_absolute != absolute {
            return Err(WorkspaceError::OutsideWorkspace {
                root: self.root.clone(),
                path: current_absolute,
            }
            .into());
        }
        let (mut current, _) = open_regular_single_link(&current_absolute, &relative)?;
        let current_revision = digest_open_file(&mut current)?;
        if current_revision != base_revision {
            return Err(ApplyPatchError::StaleRevision {
                path: relative,
                expected: base_revision.to_owned(),
                actual: current_revision,
            });
        }

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
        let candidate = self.root.join(relative);
        let canonical = fs::canonicalize(&candidate).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NotFound(relative.to_owned())
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        self.ensure_contained(&canonical)?;
        Ok(canonical)
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
    if path.is_absolute()
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

fn workspace_identity(root: &Path) -> WorkspaceResult<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(root.as_os_str().as_encoded_bytes());
    let metadata = fs::metadata(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hasher.update(&metadata.dev().to_le_bytes());
        hasher.update(&metadata.ino().to_le_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(&metadata.len().to_le_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
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
