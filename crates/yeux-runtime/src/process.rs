//! Serialized, non-shell process execution with a minimal environment.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
    task::AbortHandle,
};

#[cfg(unix)]
use rustix::process::{kill_process_group, Pid, Signal};

use crate::{
    sandbox::{SandboxBackend, SandboxError, SandboxRequirement},
    workspace::{Workspace, WorkspaceError},
};

#[derive(Debug, Clone)]
pub struct ProcessRequest {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
    /// Explicit variables only. The parent process environment is never inherited.
    pub environment: BTreeMap<String, String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    pub output_limit_bytes: usize,
    pub sandbox: SandboxRequirement,
}

impl ProcessRequest {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            arguments: Vec::new(),
            cwd: PathBuf::new(),
            environment: BTreeMap::new(),
            stdin: None,
            timeout: Duration::from_secs(30),
            output_limit_bytes: 1024 * 1024,
            sandbox: SandboxRequirement::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub duration: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("workspace error: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("sandbox error: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("executable must be absolute: {0}")]
    ExecutableNotAbsolute(PathBuf),
    #[error("executable is not a regular file: {0}")]
    InvalidExecutable(PathBuf),
    #[error("environment variable is reserved for the credential broker: {0}")]
    SensitiveEnvironmentVariable(String),
    #[error("invalid environment variable name: {0}")]
    InvalidEnvironmentVariableName(String),
    #[error("process output task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("process output pipes did not close after the process group was terminated")]
    OutputDrainTimeout,
}

pub type ProcessResult<T> = std::result::Result<T, ProcessError>;

/// Process calls share one mutex: v1 never infers that arbitrary processes commute.
#[derive(Debug)]
pub struct ProcessExecutor {
    backend: SandboxBackend,
    serial: Mutex<()>,
}

impl ProcessExecutor {
    pub fn detect() -> Self {
        Self::new(SandboxBackend::detect())
    }

    pub fn new(backend: SandboxBackend) -> Self {
        Self {
            backend,
            serial: Mutex::new(()),
        }
    }

    pub fn backend(&self) -> &SandboxBackend {
        &self.backend
    }

    pub async fn execute(
        &self,
        workspace: &Workspace,
        request: ProcessRequest,
    ) -> ProcessResult<ProcessOutput> {
        let _serial = self.serial.lock().await;
        validate_environment(&request.environment)?;
        let executable = validate_executable(&request.executable)?;
        let cwd = workspace.resolve_directory(&request.cwd)?;
        let target_environment = target_environment(&request.environment);
        let wrapped = self.backend.wrap(
            &executable,
            &request.arguments,
            &target_environment,
            workspace.root(),
            &cwd,
            request.sandbox,
        )?;

        let mut command = Command::new(&wrapped.executable);
        command
            .args(&wrapped.arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(&wrapped.environment)
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let started = Instant::now();
        let mut child = command.spawn()?;
        let process_id = child.id();
        let mut process_group = ProcessGroupGuard::new(process_id);
        if let Some(input) = request.stdin {
            let mut stdin = child.stdin.take().expect("stdin requested as piped");
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
                let _ = stdin.shutdown().await;
            });
        }
        let stdout = child.stdout.take().expect("stdout configured as piped");
        let stderr = child.stderr.take().expect("stderr configured as piped");
        let stdout_task = tokio::spawn(read_capped(stdout, request.output_limit_bytes));
        let stderr_task = tokio::spawn(read_capped(stderr, request.output_limit_bytes));
        let mut output_tasks = OutputTaskGuard::new(&stdout_task, &stderr_task);

        let (status, timed_out) = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(Ok(status)) => {
                // A successful direct child may leave background descendants
                // holding pipes or mutating the workspace. An invocation owns
                // the entire process group, so it is always terminated here.
                process_group.terminate();
                (status, false)
            }
            Ok(Err(error)) => {
                process_group.terminate();
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(error.into());
            }
            Err(_) => {
                process_group.terminate();
                // Also target the direct child: it may have exited between the
                // timeout and group signal, so InvalidInput is harmless here.
                let _ = child.start_kill();
                (child.wait().await?, true)
            }
        };
        let mut stdout_task = stdout_task;
        let mut stderr_task = stderr_task;
        let drain = async {
            let (stdout, stderr) = tokio::join!(&mut stdout_task, &mut stderr_task);
            Ok::<_, ProcessError>((stdout??, stderr??))
        };
        let ((stdout, stdout_truncated), (stderr, stderr_truncated)) =
            tokio::time::timeout(Duration::from_secs(1), drain)
                .await
                .map_err(|_| ProcessError::OutputDrainTimeout)??;
        output_tasks.disarm();
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out,
            duration: started.elapsed(),
        })
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct ProcessGroupGuard {
    process_group: Option<Pid>,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(process_id: Option<u32>) -> Self {
        let process_group = process_id
            .and_then(|process_id| i32::try_from(process_id).ok())
            .and_then(Pid::from_raw);
        Self { process_group }
    }

    fn terminate(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            let _ = kill_process_group(process_group, Signal::KILL);
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct ProcessGroupGuard;

#[cfg(not(unix))]
impl ProcessGroupGuard {
    fn new(_process_id: Option<u32>) -> Self {
        Self
    }

    fn terminate(&mut self) {}
}

#[derive(Debug)]
struct OutputTaskGuard {
    handles: [AbortHandle; 2],
    armed: bool,
}

impl OutputTaskGuard {
    fn new(
        stdout: &tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
        stderr: &tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
    ) -> Self {
        Self {
            handles: [stdout.abort_handle(), stderr.abort_handle()],
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OutputTaskGuard {
    fn drop(&mut self) {
        if self.armed {
            for handle in &self.handles {
                handle.abort();
            }
        }
    }
}

fn validate_executable(path: &Path) -> ProcessResult<PathBuf> {
    if !path.is_absolute() {
        return Err(ProcessError::ExecutableNotAbsolute(path.to_owned()));
    }
    let executable = std::fs::canonicalize(path)?;
    if !std::fs::metadata(&executable)?.is_file() {
        return Err(ProcessError::InvalidExecutable(executable));
    }
    Ok(executable)
}

fn validate_environment(environment: &BTreeMap<String, String>) -> ProcessResult<()> {
    for name in environment.keys() {
        if !valid_environment_name(name) {
            return Err(ProcessError::InvalidEnvironmentVariableName(name.clone()));
        }
        let uppercase = name.to_ascii_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
            .iter()
            .any(|marker| uppercase.contains(marker))
        {
            return Err(ProcessError::SensitiveEnvironmentVariable(name.clone()));
        }
    }
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn target_environment(environment: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut target = BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]);
    target.extend(environment.clone());
    target
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let retained = remaining.min(read);
        output.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn executable(name: &str) -> PathBuf {
        [format!("/usr/bin/{name}"), format!("/bin/{name}")]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
            .unwrap()
    }

    #[tokio::test]
    async fn executes_with_minimal_environment_and_bounded_output() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let mut request = ProcessRequest::new(executable("env"));
        request.environment.insert("YEUX_TEST".into(), "yes".into());
        request.output_limit_bytes = 64;
        let output = executor.execute(&workspace, request).await.unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("YEUX_TEST=yes"));
        assert!(!stdout.contains("HOME="));
    }

    #[tokio::test]
    async fn times_out_and_kills_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let mut request = ProcessRequest::new(executable("sleep"));
        request.arguments = vec!["5".into()];
        request.timeout = Duration::from_millis(20);
        let output = executor.execute(&workspace, request).await.unwrap();
        assert!(output.timed_out);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_descendant_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let marker = directory.path().join("orphan-marker");
        let mut request = ProcessRequest::new(executable("sh"));
        request.arguments = vec![
            "-c".into(),
            "(sleep 0.2; echo orphan > \"$1\") & wait".into(),
            "yeux-test".into(),
            marker.to_string_lossy().into_owned(),
        ];
        request.timeout = Duration::from_millis(20);
        let output = executor.execute(&workspace, request).await.unwrap();
        assert!(output.timed_out);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_exit_kills_background_descendants_and_closes_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let marker = directory.path().join("background-marker");
        let mut request = ProcessRequest::new(executable("sh"));
        request.arguments = vec![
            "-c".into(),
            "(sleep 0.2; echo orphan > \"$1\") & exit 0".into(),
            "yeux-test".into(),
            marker.to_string_lossy().into_owned(),
        ];

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            executor.execute(&workspace, request),
        )
        .await
        .expect("process output pipes must close")
        .unwrap();
        assert!(!output.timed_out);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_execute_kills_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let marker = directory.path().join("cancelled-marker");
        let mut request = ProcessRequest::new(executable("sh"));
        request.arguments = vec![
            "-c".into(),
            "(sleep 0.2; echo orphan > \"$1\") & wait".into(),
            "yeux-test".into(),
            marker.to_string_lossy().into_owned(),
        ];

        let task = tokio::spawn(async move { executor.execute(&workspace, request).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn rejects_working_directory_escape_before_spawning() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let mut request = ProcessRequest::new(executable("true"));
        request.cwd = PathBuf::from("..");

        assert!(matches!(
            executor.execute(&workspace, request).await,
            Err(ProcessError::Workspace(
                WorkspaceError::InvalidRelativePath(_)
            ))
        ));
    }

    #[tokio::test]
    async fn unavailable_sandbox_prevents_process_start() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let marker = directory.path().join("sandbox-bypass-marker");
        let executor = ProcessExecutor::new(SandboxBackend::Unavailable {
            reason: "test sandbox unavailable".into(),
        });
        let mut request = ProcessRequest::new(executable("sh"));
        request.arguments = vec![
            "-c".into(),
            "echo bypass > \"$1\"".into(),
            "yeux-test".into(),
            marker.to_string_lossy().into_owned(),
        ];

        assert!(matches!(
            executor.execute(&workspace, request).await,
            Err(ProcessError::Sandbox(SandboxError::Unavailable(_)))
        ));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn rejects_credentials_in_normal_process_environment() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let mut request = ProcessRequest::new(executable("true"));
        request
            .environment
            .insert("API_TOKEN".into(), "nope".into());
        assert!(matches!(
            executor.execute(&workspace, request).await,
            Err(ProcessError::SensitiveEnvironmentVariable(_))
        ));
    }

    #[tokio::test]
    async fn rejects_non_portable_environment_variable_names() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());

        for name in ["", "1INVALID", "HAS=EQUALS", "HAS-DASH", "NON_ASCII_变量"] {
            let mut request = ProcessRequest::new(executable("true"));
            request.environment.insert(name.into(), "value".into());
            assert!(matches!(
                executor.execute(&workspace, request).await,
                Err(ProcessError::InvalidEnvironmentVariableName(invalid)) if invalid == name
            ));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn target_environment_is_not_inherited_by_the_sandbox_launcher() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let launcher = directory.path().join("fake-sandbox-exec");
        std::fs::write(
            &launcher,
            b"#!/bin/sh\n[ \"${LD_PRELOAD+x}\" != x ] || exit 91\n[ \"${DYLD_INSERT_LIBRARIES+x}\" != x ] || exit 92\n[ \"$PATH\" = /usr/bin:/bin ] || exit 93\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&launcher).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&launcher, permissions).unwrap();

        let executor = ProcessExecutor::new(SandboxBackend::MacOsSeatbelt {
            sandbox_exec: launcher,
        });
        let mut request = ProcessRequest::new(executable("true"));
        request
            .environment
            .insert("LD_PRELOAD".into(), String::new());
        request
            .environment
            .insert("DYLD_INSERT_LIBRARIES".into(), String::new());

        let output = executor.execute(&workspace, request).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
    }
}
