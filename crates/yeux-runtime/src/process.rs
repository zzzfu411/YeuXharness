//! Serialized, non-shell process execution with a minimal environment.

#![allow(clippy::result_large_err)]

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
use rustix::process::{
    getpgid, getpgrp, kill_process_group, test_kill_process_group, waitid, Pid, Signal, WaitId,
    WaitIdOptions,
};

use crate::{
    sandbox::{SandboxBackend, SandboxError, SandboxRequirement},
    workspace::{Workspace, WorkspaceError},
};

/// Hard upper bound for retained process output.  Callers may choose a lower
/// value, but no API path may turn an untrusted process into an unbounded
/// memory sink.
pub const MAX_PROCESS_OUTPUT_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_millis(250);

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
    #[error("process could not be spawned: {0}")]
    Spawn(std::io::Error),
    #[error("executable must be absolute: {0}")]
    ExecutableNotAbsolute(PathBuf),
    #[error("executable is not a regular file: {0}")]
    InvalidExecutable(PathBuf),
    #[error("could not validate executable {path}: {source}")]
    ExecutableValidation {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("environment variable is reserved for the credential broker: {0}")]
    SensitiveEnvironmentVariable(String),
    #[error("invalid environment variable name: {0}")]
    InvalidEnvironmentVariableName(String),
    #[error("sandbox handshake task failed before process spawn: {0}")]
    HandshakeJoin(tokio::task::JoinError),
    #[error("process I/O task failed after process spawn: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("process output pipes did not close after the process group was terminated")]
    OutputDrainTimeout,
    #[error(
        "process output limit exceeds the hard ceiling of {MAX_PROCESS_OUTPUT_LIMIT_BYTES} bytes"
    )]
    OutputLimitExceeded,
    #[error("process group isolation could not be established")]
    ProcessGroupUnavailable,
    #[error("process descendants could not be proven terminated")]
    DescendantsMaySurvive,
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
        if request.output_limit_bytes > MAX_PROCESS_OUTPUT_LIMIT_BYTES {
            return Err(ProcessError::OutputLimitExceeded);
        }
        validate_environment(&request.environment)?;
        let executable = validate_executable(&request.executable)?;
        let cwd = workspace.resolve_directory(&request.cwd)?;
        // Check the requested capability before executing any launcher code.
        // In particular, macOS Seatbelt is intentionally not advertised as a
        // strict process-tree boundary; such requests must fail before even a
        // custom launcher path is touched.
        self.backend.ensure(request.sandbox)?;
        // A backend returned by `detect` has already passed its isolation
        // probe.  Repeat the short launcher handshake immediately before
        // spawn so a replaced/broken launcher fails closed instead of silently
        // falling back to an unsandboxed target.
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.handshake())
            .await
            .map_err(ProcessError::HandshakeJoin)??;
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
        let mut child = command.spawn().map_err(ProcessError::Spawn)?;
        let process_id = child.id();
        let mut process_group = ProcessGroupGuard::new(process_id);
        if request.sandbox.process_isolation && !process_group.established() {
            // We have already spawned the target, so synchronously kill the
            // direct child/group before returning the setup failure.  The
            // caller must treat this as an untrusted/unknown side effect.
            process_group.terminate();
            let _ = child.start_kill();
            if child.wait().await.is_ok() {
                process_group.mark_leader_reaped();
            } else {
                // `wait` did not prove whether the numeric PID remains owned.
                // The original group has already been signalled; never retry
                // that numeric identity from `Drop`.
                process_group.disarm();
            }
            return Err(ProcessError::ProcessGroupUnavailable);
        }
        let stdin_task = request.stdin.map(|input| {
            let mut stdin = child.stdin.take().expect("stdin requested as piped");
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
                let _ = stdin.shutdown().await;
            })
        });
        let mut stdin_guard = stdin_task
            .as_ref()
            .map(|task| TaskAbortGuard::new(task.abort_handle()));
        let stdout = child.stdout.take().expect("stdout configured as piped");
        let stderr = child.stderr.take().expect("stderr configured as piped");
        let stdout_task = tokio::spawn(read_capped(stdout, request.output_limit_bytes));
        let stderr_task = tokio::spawn(read_capped(stderr, request.output_limit_bytes));
        let mut output_tasks = OutputTaskGuard::new(&stdout_task, &stderr_task);

        #[cfg(unix)]
        let (status, timed_out) = {
            let leader = process_group
                .leader_pid()
                .ok_or(ProcessError::ProcessGroupUnavailable)?;
            match tokio::time::timeout(request.timeout, wait_until_exited_unreaped(leader)).await {
                Ok(Ok(())) => {
                    // `waitid(..., WNOWAIT)` observes the exit while retaining
                    // the zombie slot. The PID/PGID therefore cannot be reused
                    // while we terminate any remaining descendants.
                    process_group.terminate();
                    let status = match child.wait().await {
                        Ok(status) => {
                            process_group.mark_leader_reaped();
                            status
                        }
                        Err(error) => {
                            // The group was signalled while the leader was
                            // anchored, but a failed wait cannot prove whether
                            // reaping occurred. Avoid a later numeric signal.
                            process_group.disarm();
                            return Err(error.into());
                        }
                    };
                    (status, false)
                }
                Ok(Err(error)) => {
                    process_group.terminate();
                    let _ = child.start_kill();
                    if child.wait().await.is_ok() {
                        process_group.mark_leader_reaped();
                    } else {
                        // The signal was issued while the leader identity was
                        // still anchored, but a failed wait leaves its later
                        // numeric identity unknowable. Never retry by number.
                        process_group.disarm();
                    }
                    return Err(error.into());
                }
                Err(_) => {
                    process_group.terminate();
                    let _ = child.start_kill();
                    let status = match child.wait().await {
                        Ok(status) => {
                            process_group.mark_leader_reaped();
                            status
                        }
                        Err(error) => {
                            process_group.disarm();
                            return Err(error.into());
                        }
                    };
                    (status, true)
                }
            }
        };
        #[cfg(not(unix))]
        let (status, timed_out) = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(status) => (status?, false),
            Err(_) => {
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
            tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, drain)
                .await
                .map_err(|_| ProcessError::OutputDrainTimeout)??;
        if let Some(task) = &stdin_task {
            task.abort();
        }
        if let Some(guard) = &mut stdin_guard {
            guard.disarm();
        }
        // `child.wait` has reaped the direct process.  A strict process
        // request is successful only when the owned process group also
        // disappeared.  If it remains alive, report an explicit failure so
        // the invocation can enter Unknown/reconciliation instead of being
        // reported as a clean completion.
        if request.sandbox.process_isolation
            && !process_group
                .wait_for_termination_async(PROCESS_TERMINATION_GRACE)
                .await
        {
            return Err(ProcessError::DescendantsMaySurvive);
        }
        if request.sandbox.process_isolation {
            process_group.disarm();
        }
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
    leader_pid: Option<Pid>,
    process_group: Option<Pid>,
    safe_group: bool,
    leader_reaped: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn new(process_id: Option<u32>) -> Self {
        let process_id = process_id
            .and_then(|process_id| i32::try_from(process_id).ok())
            .and_then(Pid::from_raw);
        let observed_group = process_id.and_then(|pid| getpgid(Some(pid)).ok());
        Self::from_observation(process_id, observed_group)
    }

    fn from_observation(process_id: Option<Pid>, observed_group: Option<Pid>) -> Self {
        // `Command::process_group(0)` creates the only process group this
        // executor owns: the group whose numeric ID equals the original child
        // PID. Never adopt a different PGID reported after spawn; a target
        // that moved before observation could otherwise trick cleanup into
        // signalling an unrelated group in the daemon's session.
        let process_group = match (process_id, observed_group) {
            (Some(process_id), Some(group)) if group == process_id => Some(process_id),
            _ => None,
        };
        // A missing, moved, or daemon-owned group is deliberately unverified
        // for strict process requests and is never signalled by number.
        let safe_group = process_group.is_some_and(|group| group != getpgrp() && !group.is_init());
        Self {
            leader_pid: process_id,
            process_group,
            safe_group,
            leader_reaped: false,
        }
    }

    fn established(&self) -> bool {
        self.safe_group
    }

    fn leader_pid(&self) -> Option<Pid> {
        self.leader_pid
    }

    /// Send SIGKILL to the process group while the leader PID is still pinned
    /// by a live process or unreaped zombie. The caller separately owns the
    /// direct Tokio child handle and uses that identity-safe handle on timeout.
    fn terminate(&mut self) {
        if self.leader_reaped {
            return;
        }
        if let Some(process_group) = self.process_group {
            if self.safe_group {
                let _ = kill_process_group(process_group, Signal::KILL);
            }
        }
    }

    fn wait_for_termination(&self, grace: Duration) -> bool {
        if !self.safe_group {
            return false;
        }
        let deadline = Instant::now() + grace;
        loop {
            let group_alive = self
                .process_group
                .is_some_and(|group| test_kill_process_group(group).is_ok());
            if !group_alive {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    async fn wait_for_termination_async(&self, grace: Duration) -> bool {
        if !self.safe_group {
            return false;
        }
        let deadline = Instant::now() + grace;
        loop {
            let group_alive = self
                .process_group
                .is_some_and(|group| test_kill_process_group(group).is_ok());
            if !group_alive {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn mark_leader_reaped(&mut self) {
        self.leader_pid = None;
        self.leader_reaped = true;
    }

    fn disarm(&mut self) {
        self.leader_pid = None;
        self.process_group = None;
        self.safe_group = false;
        self.leader_reaped = true;
    }
}

/// Observe a child exit without reaping it. Keeping the zombie waitable pins
/// its numeric PID and process-group identifier until the caller has signalled
/// descendants, closing the PID-reuse window introduced by a plain
/// `Child::wait` followed by `killpg`.
#[cfg(unix)]
async fn wait_until_exited_unreaped(pid: Pid) -> std::io::Result<()> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match waitid(WaitId::Pid(pid), options) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => tokio::time::sleep(Duration::from_millis(2)).await,
            Err(error) if error == rustix::io::Errno::INTR => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
        // A cancelled Tokio future cannot return an error to its caller.  Do
        // a short synchronous retry window in Drop so cancellation still
        // cleans ordinary descendants before the guard disappears.  If the
        // group does not drain, the next durable invocation outcome must be
        // treated as Unknown by its authority layer.
        let _ = self.wait_for_termination(PROCESS_TERMINATION_GRACE);
    }
}

#[cfg(not(unix))]
#[derive(Debug)]
struct ProcessGroupGuard {
    established: bool,
}

#[cfg(not(unix))]
impl ProcessGroupGuard {
    fn new(_process_id: Option<u32>) -> Self {
        Self { established: false }
    }

    fn established(&self) -> bool {
        self.established
    }

    fn terminate(&mut self) {}

    fn mark_leader_reaped(&mut self) {
        self.established = false;
    }

    fn wait_for_termination(&self, _grace: Duration) -> bool {
        false
    }

    async fn wait_for_termination_async(&self, _grace: Duration) -> bool {
        false
    }

    fn disarm(&mut self) {
        self.established = false;
    }
}

#[cfg(not(unix))]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Debug)]
struct TaskAbortGuard {
    handle: AbortHandle,
    armed: bool,
}

impl TaskAbortGuard {
    fn new(handle: AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TaskAbortGuard {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
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
    let executable =
        std::fs::canonicalize(path).map_err(|source| ProcessError::ExecutableValidation {
            path: path.to_owned(),
            source,
        })?;
    if !std::fs::metadata(&executable)
        .map_err(|source| ProcessError::ExecutableValidation {
            path: executable.clone(),
            source,
        })?
        .is_file()
    {
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
        if retained < read {
            // Close the pipe as soon as the cap is crossed.  Continuing to
            // drain an untrusted producer would leave CPU/IO unbounded even
            // though retained memory is bounded; the child will receive
            // SIGPIPE or be terminated by its timeout/group guard.
            truncated = true;
            break;
        }
        if output.len() == limit {
            // Read one byte to distinguish an exactly-at-limit result from a
            // truncated stream.  The enclosing drain timeout bounds a writer
            // that keeps the descriptor open indefinitely.
            let mut extra = [0_u8; 1];
            let read = reader.read(&mut extra).await?;
            truncated = read != 0;
            break;
        }
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
    async fn output_reader_stops_at_the_cap_and_marks_truncation() {
        let (mut writer, reader) = tokio::io::duplex(8192);
        let input = vec![b'x'; 4096];
        let writer_task = tokio::spawn(async move {
            writer.write_all(&input).await.unwrap();
            // Closing the writer is not required: read_capped must stop after
            // observing the first byte beyond the cap.
        });
        let (output, truncated) = read_capped(reader, 32).await.unwrap();
        writer_task.await.unwrap();
        assert_eq!(output.len(), 32);
        assert!(truncated);
    }

    #[tokio::test]
    async fn rejects_output_limit_above_hard_ceiling_before_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let executor = ProcessExecutor::new(SandboxBackend::test_passthrough());
        let mut request = ProcessRequest::new(executable("true"));
        request.output_limit_bytes = MAX_PROCESS_OUTPUT_LIMIT_BYTES + 1;
        assert!(matches!(
            executor.execute(&workspace, request).await,
            Err(ProcessError::OutputLimitExceeded)
        ));
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

    #[cfg(unix)]
    #[test]
    fn process_group_setup_isolated_from_daemon_group() {
        use std::os::unix::process::CommandExt;

        let mut command = std::process::Command::new(executable("sleep"));
        command.arg("1").process_group(0);
        let mut child = command.spawn().unwrap();
        let mut guard = ProcessGroupGuard::new(Some(child.id()));
        assert!(guard.established());
        guard.terminate();
        let _ = child.wait();
        assert!(guard.wait_for_termination(PROCESS_TERMINATION_GRACE));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_process_group_is_never_signalled_as_the_daemon_group() {
        let mut guard = ProcessGroupGuard {
            leader_pid: None,
            process_group: Some(getpgrp()),
            safe_group: false,
            leader_reaped: false,
        };
        assert!(!guard.established());
        guard.terminate();
        assert!(!guard.wait_for_termination(Duration::from_millis(1)));
    }

    #[cfg(unix)]
    #[test]
    fn observed_foreign_process_group_is_never_adopted() {
        let leader = Pid::from_raw(1_000_000).unwrap();
        let foreign_group = Pid::from_raw(1_000_001).unwrap();
        let mut guard = ProcessGroupGuard::from_observation(Some(leader), Some(foreign_group));

        assert_eq!(guard.leader_pid(), Some(leader));
        assert!(guard.process_group.is_none());
        assert!(!guard.established());
        guard.terminate();
        guard.disarm();
    }

    #[cfg(unix)]
    #[test]
    fn reaped_guard_never_signals_a_reused_numeric_group() {
        use std::os::unix::process::CommandExt;

        let mut child = std::process::Command::new(executable("sleep"))
            .arg("1")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut guard = ProcessGroupGuard::new(Some(child.id()));
        assert!(guard.established());
        // Simulate the leader PID having been reaped and its numeric value
        // becoming eligible for reuse. A later cleanup path must not signal
        // this group by number, even if it still appears alive.
        guard.mark_leader_reaped();
        guard.terminate();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
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
        request.sandbox.process_isolation = false;
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
