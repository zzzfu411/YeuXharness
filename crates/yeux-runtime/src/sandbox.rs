//! Platform sandbox capability discovery and fail-closed command wrapping.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const MINIMAL_PATH: &str = "/usr/bin:/bin";

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
}

impl SandboxBackend {
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            let path = PathBuf::from("/usr/bin/sandbox-exec");
            if executable_file(&path) {
                return Self::MacOsSeatbelt { sandbox_exec: path };
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
                return Self::LinuxBubblewrap {
                    bubblewrap: path,
                    landlock: Path::new("/sys/kernel/security/landlock").exists(),
                    // bwrap uses namespaces; a separate seccomp filter is not
                    // installed by this initial adapter.
                    seccomp: false,
                };
            }
            return Self::Unavailable {
                reason: "bubblewrap was not found in a trusted system path".into(),
            };
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
                process_isolation: true,
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
                let workspace = quote_profile_path(workspace_root)?;
                let mut profile = format!(
                    "(version 1)(deny default)(import \"system.sb\")\
                     (allow process-exec)(allow process-fork)\
                     (allow file-read* (subpath \"/usr\") (subpath \"/bin\")\
                     (subpath \"/System\") (subpath \"/Library\")\
                     (subpath \"/opt/homebrew\") (subpath \"/usr/local\")\
                     (subpath \"{workspace}\"))"
                );
                if requirement.allow_workspace_write {
                    profile.push_str(&format!("(allow file-write* (subpath \"{workspace}\"))"));
                }
                if requirement.allow_network {
                    profile.push_str("(allow network-outbound)");
                }
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
                SandboxRequirement::default(),
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
}
