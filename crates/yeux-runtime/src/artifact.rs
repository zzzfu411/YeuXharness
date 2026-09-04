//! Content-addressed durable artifact storage.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use url::Url;

const ARTIFACT_URI_PREFIX: &str = "artifact://blake3/";
/// A canonical artifact URI is 18 prefix bytes plus a 64-byte digest. Reject
/// oversized attacker input before it is copied into an error value.
pub const MAX_ARTIFACT_URI_BYTES: usize = ARTIFACT_URI_PREFIX.len() + 64;

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
    #[error("invalid artifact URI: {0}")]
    InvalidUri(String),
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

    /// Build the only URI form accepted for content-addressed artifacts.
    ///
    /// This is an associated function because URI construction does not need
    /// a store instance. The returned value is canonical and contains no
    /// query, fragment, or user-controlled path components.
    pub fn uri_for_digest(digest: &str) -> ArtifactResult<String> {
        validate_digest(digest)?;
        Ok(format!("{ARTIFACT_URI_PREFIX}{digest}"))
    }

    /// Parse a canonical `artifact://blake3/<64 lowercase hex>` URI and return
    /// a borrowed digest slice. Raw prefix checking is intentional: URL
    /// schemes and hosts are case-insensitive to generic parsers, while the
    /// artifact format is deliberately one stable, cache-key-safe spelling.
    pub fn digest_from_uri(uri: &str) -> ArtifactResult<&str> {
        if uri.len() > MAX_ARTIFACT_URI_BYTES {
            return Err(ArtifactError::InvalidUri(
                "URI exceeds the canonical artifact URI length".into(),
            ));
        }
        if !uri.starts_with(ARTIFACT_URI_PREFIX) {
            return Err(ArtifactError::InvalidUri(
                "URI must start with artifact://blake3/".into(),
            ));
        }

        // Parse the URI as a URL as a second, independent authority check.
        // The raw prefix above prevents alternate casing and userinfo/port
        // spellings from being normalized into the accepted form.
        let parsed = Url::parse(uri)
            .map_err(|_| ArtifactError::InvalidUri("URI is not a valid URL".into()))?;
        if parsed.scheme() != "artifact"
            || parsed.host_str() != Some("blake3")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ArtifactError::InvalidUri(
                "URI authority, query, or fragment is not canonical".into(),
            ));
        }

        let digest = &uri[ARTIFACT_URI_PREFIX.len()..];
        // `path()` must be exactly the raw digest path. This rejects percent
        // encoding, extra separators, dot segments, and trailing slashes even
        // if a URL parser would otherwise normalize them.
        if parsed.path() != format!("/{digest}") {
            return Err(ArtifactError::InvalidUri(
                "URI path is not a single canonical digest segment".into(),
            ));
        }
        validate_digest(digest).map_err(|_| {
            ArtifactError::InvalidUri("URI path must contain 64 lowercase hexadecimal bytes".into())
        })?;
        Ok(digest)
    }

    /// Verify an artifact addressed by a canonical URI and return its byte
    /// length. Parsing happens before filesystem resolution, so traversal and
    /// alternate URI forms cannot reach `path_for_digest`.
    pub fn verify_uri(&self, uri: &str) -> ArtifactResult<u64> {
        let digest = Self::digest_from_uri(uri)?;
        self.verify(digest)
    }
}

/// Module-level convenience wrapper for callers that do not need a store.
pub fn uri_for_digest(digest: &str) -> ArtifactResult<String> {
    ArtifactStore::uri_for_digest(digest)
}

/// Module-level convenience wrapper returning a borrowed canonical digest.
pub fn digest_from_uri(uri: &str) -> ArtifactResult<&str> {
    ArtifactStore::digest_from_uri(uri)
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

    #[test]
    fn canonical_uri_round_trips_and_verifies() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path()).unwrap();
        let artifact = store.put(b"uri payload", "text/plain").unwrap();

        let uri = ArtifactStore::uri_for_digest(&artifact.digest).unwrap();
        assert_eq!(uri, format!("artifact://blake3/{}", artifact.digest));
        assert_eq!(
            ArtifactStore::digest_from_uri(&uri).unwrap(),
            artifact.digest
        );
        assert_eq!(digest_from_uri(&uri).unwrap(), artifact.digest);
        assert_eq!(store.verify_uri(&uri).unwrap(), artifact.size);
    }

    #[test]
    fn uri_generation_rejects_non_canonical_digests() {
        assert!(matches!(
            ArtifactStore::uri_for_digest("../outside"),
            Err(ArtifactError::InvalidDigest(_))
        ));
        assert!(matches!(
            uri_for_digest(&"f".repeat(63)),
            Err(ArtifactError::InvalidDigest(_))
        ));
    }

    #[test]
    fn uri_parser_rejects_query_fragment_and_path_traversal() {
        let digest = blake3::hash(b"payload").to_hex().to_string();
        let valid = format!("artifact://blake3/{digest}");
        let invalid = [
            format!("{valid}?download=1"),
            format!("{valid}#fragment"),
            format!("artifact://blake3/{digest}/extra"),
            "artifact://blake3/../outside".to_owned(),
            "artifact://blake3/%2e%2e/outside".to_owned(),
            format!("artifact://blake3/{digest}/"),
            format!("artifact://blake3/{digest}%2fextra"),
            format!(
                "artifact://blake3/{digest_upper}",
                digest_upper = digest.to_uppercase()
            ),
            format!("ARTIFACT://blake3/{digest}"),
            format!("artifact://BLAKE3/{digest}"),
            format!("artifact://user:secret@blake3/{digest}"),
            format!("artifact://blake3:443/{digest}"),
            format!("artifact://blake3//{digest}"),
        ];
        for uri in invalid {
            assert!(
                matches!(
                    ArtifactStore::digest_from_uri(&uri),
                    Err(ArtifactError::InvalidUri(_))
                ),
                "unexpectedly accepted URI: {uri}"
            );
        }
        assert_eq!(ArtifactStore::digest_from_uri(&valid).unwrap(), digest);
    }

    #[test]
    fn uri_parser_bounds_input_before_diagnostic_allocation() {
        let oversized = format!("artifact://blake3/{}", "a".repeat(256));
        let error = ArtifactStore::digest_from_uri(&oversized).unwrap_err();
        assert!(matches!(error, ArtifactError::InvalidUri(_)));
        assert!(error.to_string().len() < 128);
    }

    #[test]
    fn verify_uri_reports_missing_and_corrupt_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(directory.path()).unwrap();
        let digest = blake3::hash(b"missing").to_hex().to_string();
        let uri = ArtifactStore::uri_for_digest(&digest).unwrap();
        assert!(matches!(
            store.verify_uri(&uri),
            Err(ArtifactError::Missing(value)) if value == digest
        ));

        let artifact = store.put(b"good", "application/octet-stream").unwrap();
        let path = store.path_for_digest(&artifact.digest).unwrap();
        fs::write(path, b"tampered").unwrap();
        let uri = ArtifactStore::uri_for_digest(&artifact.digest).unwrap();
        assert!(matches!(
            store.verify_uri(&uri),
            Err(ArtifactError::Corrupt { expected, .. }) if expected == artifact.digest
        ));
    }
}
