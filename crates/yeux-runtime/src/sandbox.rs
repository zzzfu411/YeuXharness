//! Platform sandbox capability discovery and fail-closed command wrapping.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

const MINIMAL_PATH: &str = "/usr/bin:/bin";
/// A launcher probe must never be allowed to hang the daemon.  The probe is
/// run only for trusted, platform-provided paths returned by [`detect`], but
/// the same bound also protects callers that construct a backend explicitly.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxCapabilities {
    pub filesystem_isolation: bool,
    pub process_isolation: bool,
    pub network_isolation: bool,
    pub landlock: bool,
    pub seccomp: bool,
}

impl SandboxCapabilities {
    pub const fn unavailable() -> Self {
        Self {
            filesystem_isolation: false,
            process_isolation: false,
            network_isolation: false,
            landlock: false,
            seccomp: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxRequirement {
    pub filesystem_isolation: bool,
    pub process_isolation: bool,
    pub network_isolation: bool,
    pub allow_workspace_write: bool,
    pub allow_network: bool,
}

impl Default for SandboxRequirement {
    fn default() -> Self {
        Self {
            filesystem_isolation: true,
            process_isolation: true,
            network_isolation: true,
            allow_workspace_write: false,
            allow_network: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    MacOsSeatbelt {
        sandbox_exec: PathBuf,
    },
    LinuxBubblewrap {
        bubblewrap: PathBuf,
        landlock: bool,
        seccomp: bool,
    },
    Unavailable {
        reason: String,
    },
    #[cfg(test)]
    TestPassthrough,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedCommand {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    /// Environment for the outermost executable. Production backends keep
    /// this fixed so target-controlled variables cannot affect the sandbox
    /// launcher before isolation is active.
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("required OS sandbox is unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox lacks required capability: {0}")]
    MissingCapability(&'static str),
    #[error("sandbox path is not valid UTF-8: {0}")]
    InvalidPath(PathBuf),
    #[error("sandbox launcher handshake failed: {0}")]
    HandshakeFailed(String),
}

impl SandboxBackend {
    pub fn name(&self) -> &'static str {
        match self {
            Self::MacOsSeatbelt { .. } => "seatbelt",
            Self::LinuxBubblewrap { .. } => "bubblewrap",
            Self::Unavailable { .. } => "unavailable",
            #[cfg(test)]
            Self::TestPassthrough => "test",
        }
    }

    pub fn detect() -> Self {
        static DETECTED: OnceLock<SandboxBackend> = OnceLock::new();
        DETECTED.get_or_init(Self::detect_uncached).clone()
    }

    fn detect_uncached() -> Self {
        #[cfg(target_os = "macos")]
        {
            let path = PathBuf::from("/usr/bin/sandbox-exec");
            if executable_file(&path) {
                let backend = Self::MacOsSeatbelt { sandbox_exec: path };
                return match backend.verify_capabilities() {
                    Ok(()) => backend,
                    Err(error) => Self::Unavailable {
                        reason: error.to_string(),
                    },
                };
            }
            Self::Unavailable {
                reason: "macOS sandbox-exec was not found".into(),
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(path) = ["/usr/bin/bwrap", "/bin/bwrap"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| executable_file(path))
            {
                let backend = Self::LinuxBubblewrap {
                    bubblewrap: path,
                    landlock: Path::new("/sys/kernel/security/landlock").exists(),
                    // bwrap uses namespaces; a separate seccomp filter is not
                    // installed by this initial adapter.
                    seccomp: false,
                };
                return match backend.verify_capabilities() {
                    Ok(()) => backend,
                    Err(error) => Self::Unavailable {
                        reason: error.to_string(),
                    },
                };
            }
            Self::Unavailable {
                reason: "bubblewrap was not found in a trusted system path".into(),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Self::Unavailable {
            reason: "YeuX v1 supports sandboxed execution only on macOS and Linux".into(),
        }
    }

    pub fn capabilities(&self) -> SandboxCapabilities {
        match self {
            Self::MacOsSeatbelt { .. } => SandboxCapabilities {
                filesystem_isolation: true,
                // Seatbelt confines filesystem/network access but does not
                // provide a kernel-enforced process tree/job boundary.  A
                // child can call setsid/setpgid and outlive the launcher, so
                // strict process execution remains unavailable on macOS until
                // a launchd/job supervisor is integrated.
                process_isolation: false,
                network_isolation: true,
                landlock: false,
                seccomp: false,
            },
            Self::LinuxBubblewrap {
                landlock, seccomp, ..
            } => SandboxCapabilities {
                filesystem_isolation: true,
                process_isolation: true,
                network_isolation: true,
                landlock: *landlock,
                seccomp: *seccomp,
            },
            Self::Unavailable { .. } => SandboxCapabilities::unavailable(),
            #[cfg(test)]
            Self::TestPassthrough => SandboxCapabilities {
                filesystem_isolation: true,
                process_isolation: true,
                network_isolation: true,
                landlock: false,
                seccomp: false,
            },
        }
    }

    pub fn ensure(&self, requirement: SandboxRequirement) -> Result<(), SandboxError> {
        if let Self::Unavailable { reason } = self {
            return Err(SandboxError::Unavailable(reason.clone()));
        }
        let capability = self.capabilities();
        if requirement.filesystem_isolation && !capability.filesystem_isolation {
            return Err(SandboxError::MissingCapability("filesystem isolation"));
        }
        if requirement.process_isolation && !capability.process_isolation {
            return Err(SandboxError::MissingCapability("process isolation"));
        }
        if requirement.network_isolation && !capability.network_isolation {
            return Err(SandboxError::MissingCapability("network isolation"));
        }
        Ok(())
    }

    /// Perform a bounded launcher handshake immediately before spawning a
    /// target.  [`detect`] additionally runs an isolation probe and turns a
    /// failed probe into [`SandboxBackend::Unavailable`].  Keeping this
    /// handshake separate lets tests and embedders validate a custom launcher
    /// without silently claiming that it provides a stronger OS boundary than
    /// has actually been verified.
    pub fn handshake(&self) -> Result<(), SandboxError> {
        match self {
            Self::MacOsSeatbelt { sandbox_exec } => {
                let profile = macos_profile(None, false, false)?;
                let target = trusted_true();
                let mut command = Command::new(sandbox_exec);
                command.arg("-p").arg(profile).arg(target);
                run_handshake(&mut command, "sandbox-exec")
            }
            Self::LinuxBubblewrap { bubblewrap, .. } => {
                let mut command = Command::new(bubblewrap);
                append_linux_probe_arguments(&mut command);
                run_handshake(&mut command, "bubblewrap")
            }
            Self::Unavailable { reason } => Err(SandboxError::Unavailable(reason.clone())),
            #[cfg(test)]
            Self::TestPassthrough => Ok(()),
        }
    }

    /// Verify that the platform launcher can establish the *specific* basic
    /// isolation contract used by YeuX.  This is intentionally conservative:
    /// a non-zero result disables the backend instead of falling back to an
    /// unsandboxed process.  The probe uses a private temporary directory and
    /// never touches the caller's workspace.
    fn verify_capabilities(&self) -> Result<(), SandboxError> {
        self.handshake()?;
        match self {
            Self::MacOsSeatbelt { sandbox_exec } => verify_macos_isolation(sandbox_exec),
            Self::LinuxBubblewrap { bubblewrap, .. } => verify_linux_isolation(bubblewrap),
            Self::Unavailable { reason } => Err(SandboxError::Unavailable(reason.clone())),
            #[cfg(test)]
            Self::TestPassthrough => Ok(()),
        }
    }

    pub fn wrap(
        &self,
        executable: &Path,
        arguments: &[String],
        target_environment: &BTreeMap<String, String>,
        workspace_root: &Path,
        cwd: &Path,
        requirement: SandboxRequirement,
    ) -> Result<SandboxedCommand, SandboxError> {
        self.ensure(requirement)?;
        match self {
            Self::MacOsSeatbelt { sandbox_exec } => {
                let profile = macos_profile(
                    Some(workspace_root),
                    requirement.allow_workspace_write,
                    requirement.allow_network,
                )?;
                // sandbox-exec receives only the fixed launcher environment.
                // env applies target-controlled variables after Seatbelt has
                // installed the profile, immediately before it execs target.
                let mut wrapped = vec!["-p".into(), profile, "/usr/bin/env".into(), "-i".into()];
                wrapped.extend(
                    target_environment
                        .iter()
                        .map(|(name, value)| format!("{name}={value}")),
                );
                wrapped.push(executable_string(executable)?);
                wrapped.extend_from_slice(arguments);
                Ok(SandboxedCommand {
                    executable: sandbox_exec.clone(),
                    arguments: wrapped,
                    environment: launcher_environment(),
                })
            }
            Self::LinuxBubblewrap { bubblewrap, .. } => {
                let workspace = executable_string(workspace_root)?;
                let cwd = executable_string(cwd)?;
                let mut wrapped = vec![
                    "--die-with-parent".into(),
                    "--new-session".into(),
                    "--unshare-all".into(),
                    // Keep this explicit even though current bubblewrap
                    // expands --unshare-all to include PID.  The process-tree
                    // guarantee depends on a PID namespace plus bwrap's
                    // trusted init/--die-with-parent monitor.
                    "--unshare-pid".into(),
                    "--clearenv".into(),
                    "--proc".into(),
                    "/proc".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--tmpfs".into(),
                    "/tmp".into(),
                    "--ro-bind".into(),
                    "/usr".into(),
                    "/usr".into(),
                ];
                for system_path in ["/bin", "/lib", "/lib64", "/etc"] {
                    wrapped.extend([
                        "--ro-bind-try".into(),
                        system_path.into(),
                        system_path.into(),
                    ]);
                }
                append_parent_directories(&mut wrapped, workspace_root);
                wrapped.push(if requirement.allow_workspace_write {
                    "--bind".into()
                } else {
                    "--ro-bind".into()
                });
                wrapped.push(workspace.clone());
                wrapped.push(workspace);
                if requirement.allow_network {
                    wrapped.push("--share-net".into());
                }
                for (name, value) in target_environment {
                    wrapped.extend(["--setenv".into(), name.clone(), value.clone()]);
                }
                wrapped.extend([
                    "--chdir".into(),
                    cwd,
                    "--".into(),
                    executable_string(executable)?,
                ]);
                wrapped.extend_from_slice(arguments);
                Ok(SandboxedCommand {
                    executable: bubblewrap.clone(),
                    arguments: wrapped,
                    environment: launcher_environment(),
                })
            }
            Self::Unavailable { reason } => Err(SandboxError::Unavailable(reason.clone())),
            #[cfg(test)]
            Self::TestPassthrough => Ok(SandboxedCommand {
                executable: executable.to_owned(),
                arguments: arguments.to_vec(),
                // There is intentionally no launcher in this test backend;
                // the outermost executable is the target itself.
                environment: target_environment.clone(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) const fn test_passthrough() -> Self {
        Self::TestPassthrough
    }
}

fn launcher_environment() -> BTreeMap<String, String> {
    BTreeMap::from([("PATH".into(), MINIMAL_PATH.into())])
}

fn trusted_true() -> PathBuf {
    ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| executable_file(path))
        .unwrap_or_else(|| PathBuf::from("/usr/bin/true"))
}

fn trusted_shell() -> PathBuf {
    ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| executable_file(path))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Build the small, deny-by-default Seatbelt profile shared by the runtime
/// wrapper and the capability probe.  `workspace_root == None` deliberately
/// grants no caller filesystem path; the probe then checks that an unrelated
/// temporary path remains inaccessible.
fn macos_profile(
    workspace_root: Option<&Path>,
    allow_workspace_write: bool,
    allow_network: bool,
) -> Result<String, SandboxError> {
    let mut profile = String::from(
        "(version 1)(deny default)(import \"system.sb\")\
         (allow process-exec)(allow process-fork)\
         (allow file-read* (subpath \"/usr\") (subpath \"/bin\")\
         (subpath \"/System\") (subpath \"/Library\")\
         (subpath \"/opt/homebrew\") (subpath \"/usr/local\"))",
    );
    if let Some(workspace_root) = workspace_root {
        let workspace = quote_profile_path(workspace_root)?;
        profile.push_str(&format!("(allow file-read* (subpath \"{workspace}\"))"));
        if allow_workspace_write {
            profile.push_str(&format!("(allow file-write* (subpath \"{workspace}\"))"));
        }
    }
    if allow_network {
        profile.push_str("(allow network-outbound)");
    }
    Ok(profile)
}

/// Run a launcher with no inherited environment and no output pipes.  Using
/// `try_wait` rather than `wait_with_output` means a broken or hostile custom
/// launcher cannot hold the daemon forever by keeping a pipe open.
fn run_handshake(command: &mut Command, label: &str) -> Result<(), SandboxError> {
    let mut child = command
        .env_clear()
        .env("PATH", MINIMAL_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| SandboxError::HandshakeFailed(format!("{label} spawn: {error}")))?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(SandboxError::HandshakeFailed(format!(
                    "{label} exited with {status}"
                )))
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::HandshakeFailed(format!(
                    "{label} did not become ready within {} ms",
                    HANDSHAKE_TIMEOUT.as_millis()
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::HandshakeFailed(format!(
                    "{label} status: {error}"
                )));
            }
        }
    }
}

fn verify_macos_isolation(sandbox_exec: &Path) -> Result<(), SandboxError> {
    let directory = tempfile::tempdir()
        .map_err(|error| SandboxError::HandshakeFailed(format!("probe tempdir: {error}")))?;
    let secret = directory.path().join("secret");
    fs::write(&secret, b"probe")
        .map_err(|error| SandboxError::HandshakeFailed(format!("probe fixture: {error}")))?;
    let profile = macos_profile(None, false, false)?;
    let shell = trusted_shell();
    let secret_string = executable_string(&secret)?;
    let mut command = Command::new(sandbox_exec);
    command
        .arg("-p")
        .arg(profile)
        .arg(shell)
        .args([
            "-c",
            // Return success only when the file is unreadable and cannot be
            // created.  This checks the actual deny profile, not just a zero
            // exit status from sandbox-exec itself.
            "if test -r \"$1\"; then exit 41; fi; if touch \"$1\" 2>/dev/null; then exit 42; fi; exit 0",
            "yeux-sandbox-probe",
            &secret_string,
        ]);
    run_handshake(&mut command, "sandbox-exec isolation")
}

fn append_linux_base_arguments(command: &mut Command) {
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--unshare-pid",
        "--clearenv",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
        "/usr",
        "/usr",
    ]);
    for system_path in ["/bin", "/lib", "/lib64", "/etc"] {
        command.args(["--ro-bind-try", system_path, system_path]);
    }
}

fn append_linux_probe_arguments(command: &mut Command) {
    append_linux_base_arguments(command);
    command.args(["--", "/usr/bin/true"]);
}

fn verify_linux_isolation(bubblewrap: &Path) -> Result<(), SandboxError> {
    let directory = tempfile::tempdir()
        .map_err(|error| SandboxError::HandshakeFailed(format!("probe tempdir: {error}")))?;
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace)
        .map_err(|error| SandboxError::HandshakeFailed(format!("probe workspace: {error}")))?;
    let secret = directory.path().join("outside-secret");
    fs::write(&secret, b"probe")
        .map_err(|error| SandboxError::HandshakeFailed(format!("probe fixture: {error}")))?;
    let workspace = executable_string(&workspace)?;
    let secret = executable_string(&secret)?;
    let shell = trusted_shell();
    let mut command = Command::new(bubblewrap);
    append_linux_base_arguments(&mut command);
    // Add only the private workspace.  The outside fixture is intentionally
    // not bound into the namespace and must remain invisible.  The command
    // succeeds only if both that file is absent and the read-only bind rejects
    // writes.
    command
        .arg("--ro-bind")
        .arg(&workspace)
        .arg("/workspace")
        .arg("--chdir")
        .arg("/workspace")
        .arg("--")
        .arg(shell)
        .args([
            "-c",
            "if test -e \"$1\"; then exit 43; fi; if touch /workspace/write 2>/dev/null; then exit 44; fi; exit 0",
            "yeux-sandbox-probe",
        ])
        .arg(&secret);
    run_handshake(&mut command, "bubblewrap isolation")
}

fn executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn executable_string(path: &Path) -> Result<String, SandboxError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SandboxError::InvalidPath(path.to_owned()))
}

fn quote_profile_path(path: &Path) -> Result<String, SandboxError> {
    Ok(executable_string(path)?
        .replace('\\', "\\\\")
        .replace('"', "\\\""))
}

fn append_parent_directories(arguments: &mut Vec<String>, path: &Path) {
    let mut parents: Vec<_> = path
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .filter(|parent| parent.parent().is_some())
        .collect();
    parents.reverse();
    for parent in parents {
        if ["/usr", "/bin", "/lib", "/lib64", "/etc"]
            .iter()
            .any(|system| parent == Path::new(system) || parent.starts_with(system))
        {
            continue;
        }
        if let Some(parent) = parent.to_str() {
            arguments.extend(["--dir".into(), parent.into()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_backend_fails_closed() {
        let backend = SandboxBackend::Unavailable {
            reason: "missing".into(),
        };
        assert!(matches!(
            backend.ensure(SandboxRequirement::default()),
            Err(SandboxError::Unavailable(_))
        ));
    }

    #[test]
    fn linux_wrapper_defaults_to_no_network_and_read_only_workspace() {
        let backend = SandboxBackend::LinuxBubblewrap {
            bubblewrap: "/usr/bin/bwrap".into(),
            landlock: false,
            seccomp: false,
        };
        let command = backend
            .wrap(
                Path::new("/usr/bin/true"),
                &[],
                &BTreeMap::from([
                    ("LD_PRELOAD".into(), "/workspace/inject.so".into()),
                    ("PATH".into(), "/workspace/bin".into()),
                ]),
                Path::new("/workspace"),
                Path::new("/workspace"),
                SandboxRequirement::default(),
            )
            .unwrap();
        assert!(command.arguments.contains(&"--unshare-all".into()));
        assert!(!command.arguments.contains(&"--share-net".into()));
        assert!(command.arguments.contains(&"--ro-bind".into()));
        assert!(command.arguments.contains(&"--tmpfs".into()));
        assert!(command.arguments.contains(&"--clearenv".into()));
        assert!(command
            .arguments
            .windows(3)
            .any(|arguments| { arguments == ["--setenv", "LD_PRELOAD", "/workspace/inject.so"] }));
        assert!(command.arguments.contains(&"--unshare-pid".into()));
        assert_eq!(command.environment, launcher_environment());
        assert!(!command.environment.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn macos_wrapper_applies_target_environment_after_seatbelt() {
        let backend = SandboxBackend::MacOsSeatbelt {
            sandbox_exec: "/usr/bin/sandbox-exec".into(),
        };
        let target_environment = BTreeMap::from([
            (
                "DYLD_INSERT_LIBRARIES".into(),
                "/workspace/inject.dylib".into(),
            ),
            ("PATH".into(), "/workspace/bin".into()),
        ]);
        let command = backend
            .wrap(
                Path::new("/usr/bin/true"),
                &[],
                &target_environment,
                Path::new("/workspace"),
                Path::new("/workspace"),
                SandboxRequirement {
                    process_isolation: false,
                    ..SandboxRequirement::default()
                },
            )
            .unwrap();

        assert_eq!(command.environment, launcher_environment());
        assert!(!command.environment.contains_key("DYLD_INSERT_LIBRARIES"));
        assert_eq!(command.arguments[2], "/usr/bin/env");
        assert_eq!(command.arguments[3], "-i");
        assert!(command
            .arguments
            .contains(&"DYLD_INSERT_LIBRARIES=/workspace/inject.dylib".into()));
        assert!(command.arguments.contains(&"PATH=/workspace/bin".into()));
    }

    #[test]
    fn macos_seatbelt_does_not_claim_strict_process_tree_containment() {
        let backend = SandboxBackend::MacOsSeatbelt {
            sandbox_exec: "/usr/bin/sandbox-exec".into(),
        };
        assert!(!backend.capabilities().process_isolation);
        assert!(matches!(
            backend.ensure(SandboxRequirement::default()),
            Err(SandboxError::MissingCapability("process isolation"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn handshake_fails_closed_for_a_launcher_that_exits_nonzero() {
        let directory = tempfile::tempdir().unwrap();
        let launcher = directory.path().join("broken-sandbox");
        std::fs::write(&launcher, b"#!/bin/sh\nexit 73\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&launcher).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&launcher, permissions).unwrap();
        let backend = SandboxBackend::MacOsSeatbelt {
            sandbox_exec: launcher,
        };
        assert!(matches!(
            backend.handshake(),
            Err(SandboxError::HandshakeFailed(_))
        ));
    }

    #[test]
    fn test_backend_handshake_is_explicitly_available_only_for_tests() {
        assert!(SandboxBackend::test_passthrough().handshake().is_ok());
    }
}
