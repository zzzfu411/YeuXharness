// Baseline excerpts captured from crates/yeux-runtime/src/process.rs before
// remediation. This file is audit evidence only and is not compiled.

pub async fn execute(
    &self,
    workspace: &Workspace,
    request: ProcessRequest,
) -> ProcessResult<ProcessOutput> {
    let _serial = self.serial.lock().await;
    validate_environment(&request.environment)?;
    let executable = validate_executable(&request.executable)?;
    let cwd = workspace.resolve_directory(&request.cwd)?;
    let wrapped = self.backend.wrap(
        &executable,
        &request.arguments,
        workspace.root(),
        &cwd,
        request.sandbox,
    )?;

    let mut command = Command::new(&wrapped.executable);
    command
        .args(&wrapped.arguments)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(&request.environment)
        .stdin(if request.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // spawn/wait/output handling omitted
}

fn validate_environment(environment: &BTreeMap<String, String>) -> ProcessResult<()> {
    for name in environment.keys() {
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
