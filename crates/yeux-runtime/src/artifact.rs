//! Content-addressed durable artifact storage.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid BLAKE3 artifact digest: {0}")]
    InvalidDigest(String),
    #[error("artifact is missing: {0}")]
    Missing(String),
    #[error("artifact {expected} is corrupt; actual digest is {actual}")]
    Corrupt { expected: String, actual: String },
    #[error("failed to publish artifact: {0}")]
    Persist(#[from] tempfile::PersistError),
}

pub type ArtifactResult<T> = std::result::Result<T, ArtifactError>;

impl ArtifactStore {
    pub fn open(root: impl AsRef<Path>) -> ArtifactResult<Self> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Atomically publish bytes under their BLAKE3 digest. Existing content is reused.
    pub fn put(&self, bytes: &[u8], media_type: impl Into<String>) -> ArtifactResult<Artifact> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        let destination = self.path_for_digest(&digest)?;
        if destination.exists() {
            self.verify(&digest)?;
            return Ok(Artifact {
                digest,
                size: bytes.len() as u64,
                media_type: media_type.into(),
            });
        }

        let parent = destination.parent().expect("artifact has shard parent");
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => sync_directory(parent)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A concurrent writer won; verify that it published the same digest.
                self.verify(&digest)?;
            }
            Err(error) => return Err(ArtifactError::Persist(error)),
        }
        Ok(Artifact {
            digest,
            size: bytes.len() as u64,
            media_type: media_type.into(),
        })
    }

    pub fn read(&self, digest: &str) -> ArtifactResult<Vec<u8>> {
        let path = self.path_for_digest(digest)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let actual = blake3::hash(&bytes).to_hex().to_string();
                if actual != digest {
                    Err(ArtifactError::Corrupt {
                        expected: digest.to_owned(),
                        actual,
                    })
                } else {
                    Ok(bytes)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(ArtifactError::Missing(digest.to_owned()))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn verify(&self, digest: &str) -> ArtifactResult<u64> {
        let path = self.path_for_digest(digest)?;
        let mut file = File::open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactError::Missing(digest.to_owned())
            } else {
                ArtifactError::Io(error)
            }
        })?;
        let mut hasher = blake3::Hasher::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            hasher.update(&buffer[..read]);
        }
        let actual = hasher.finalize().to_hex().to_string();
        if actual != digest {
            return Err(ArtifactError::Corrupt {
                expected: digest.to_owned(),
                actual,
            });
        }
        Ok(size)
    }

    pub fn path_for_digest(&self, digest: &str) -> ArtifactResult<PathBuf> {
        validate_digest(digest)?;
        Ok(self.root.join("blake3").join(&digest[..2]).join(digest))
    }
}

fn validate_digest(digest: &str) -> ArtifactResult<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ArtifactError::InvalidDigest(digest.to_owned()));
    }
    Ok(())
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

    #[test]
    fn content_is_addressed_verified_and_deduplicated() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path()).unwrap();
        let first = store.put(b"large output", "text/plain").unwrap();
        let second = store.put(b"large output", "text/plain").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.read(&first.digest).unwrap(), b"large output");
        assert_eq!(store.verify(&first.digest).unwrap(), 12);
    }

    #[test]
    fn digest_cannot_be_used_for_path_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path()).unwrap();
        assert!(matches!(
            store.read("../../outside"),
            Err(ArtifactError::InvalidDigest(_))
        ));
        assert!(matches!(
            store.read(&"A".repeat(64)),
            Err(ArtifactError::InvalidDigest(_))
        ));
    }

    #[test]
    fn corruption_is_detected() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path()).unwrap();
        let artifact = store.put(b"good", "application/octet-stream").unwrap();
        fs::write(store.path_for_digest(&artifact.digest).unwrap(), b"bad").unwrap();
        assert!(matches!(
            store.read(&artifact.digest),
            Err(ArtifactError::Corrupt { .. })
        ));
    }
}
