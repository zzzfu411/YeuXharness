// Baseline excerpt captured from crates/yeuxd/src/server.rs before remediation.
// This file is audit evidence only and is not compiled.

pub async fn serve_unix(self, path: PathBuf) -> Result<(), DaemonError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use tokio::net::{UnixListener, UnixStream};

    if let Some(parent) = path.parent() {
        ensure_socket_parent(parent)?;
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err(DaemonError::InvalidSocketPath(path));
        }
        if UnixStream::connect(&path).await.is_ok() {
            return Err(DaemonError::SocketInUse(path));
        }
        std::fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    // accept loop omitted
}

fn ensure_socket_parent(parent: &Path) -> io::Result<()> {
    let existed = parent.exists();
    std::fs::create_dir_all(parent)?;
    if !existed {
        set_private_directory(parent)?;
    }
    Ok(())
}
