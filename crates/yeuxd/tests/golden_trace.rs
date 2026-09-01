use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::Value;

const TRACE: &str = include_str!("../../../spec/traces/thread-lifecycle-v2.jsonl");
const IO_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum TraceStep {
    Command {
        label: String,
        message: Value,
        #[serde(default)]
        capture: BTreeMap<String, String>,
        #[serde(default)]
        expect: BTreeMap<String, Value>,
        #[serde(default)]
        lengths: BTreeMap<String, usize>,
        #[serde(default)]
        events: Vec<BTreeMap<String, Value>>,
    },
    Restart,
}

struct TestServer {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<Result<String, std::io::Error>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(state_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_yeuxd"))
            .args(["--stdio", "--no-execute-turns", "--state-dir"])
            .arg(state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().ok_or("yeuxd stdin is unavailable")?;
        let stdout = child.stdout.take().ok_or("yeuxd stdout is unavailable")?;
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input: Some(input),
            output,
            reader: Some(reader),
        })
    }

    fn send(&mut self, message: &Value) -> Result<Value, Box<dyn std::error::Error>> {
        let input = self.input.as_mut().ok_or("yeuxd stdin is closed")?;
        serde_json::to_writer(&mut *input, message)?;
        input.write_all(b"\n")?;
        input.flush()?;
        self.read_message()
    }

    fn read_message(&self) -> Result<Value, Box<dyn std::error::Error>> {
        let line = self
            .output
            .recv_timeout(IO_TIMEOUT)
            .map_err(|error| format!("timed out waiting for yeuxd output: {error}"))??;
        Ok(serde_json::from_str(&line)?)
    }

    fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        drop(self.input.take());
        let deadline = Instant::now() + IO_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                return Err("yeuxd did not exit after stdin closed".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            return Err(format!("yeuxd exited unsuccessfully: {status}").into());
        }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| "stdout reader thread panicked")?;
        }
        let trailing = self.output.try_iter().collect::<Result<Vec<_>, _>>()?;
        if !trailing.is_empty() {
            return Err(format!("unexpected trailing server messages: {trailing:?}").into());
        }
        Ok(())
    }
}

#[test]
fn golden_thread_lifecycle_replays_and_deduplicates_after_restart(
) -> Result<(), Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let steps = TRACE
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<Vec<TraceStep>, _>>()?;
    let mut server = Some(TestServer::start(state.path())?);
    let mut variables = BTreeMap::from([(
        "workspace_path".to_owned(),
        Value::String(workspace.path().to_string_lossy().into_owned()),
    )]);

    for step in steps {
        match step {
            TraceStep::Command {
                label,
                message,
                capture,
                expect,
                lengths,
                events,
            } => {
                let message = substitute(message, &variables)?;
                let response = server.as_mut().expect("server is running").send(&message)?;
                if let Some(error) = response.get("error") {
                    return Err(format!("{label} returned an RPC error: {error}").into());
                }
                for (name, pointer) in capture {
                    let value = response
                        .pointer(&pointer)
                        .ok_or_else(|| format!("{label}: capture pointer is absent: {pointer}"))?;
                    variables.insert(name, value.clone());
                }
                assert_pointers(&label, &response, &expect, &variables)?;
                assert_lengths(&label, &response, &lengths)?;
                for (index, expected) in events.iter().enumerate() {
                    let event = server.as_ref().expect("server is running").read_message()?;
                    assert_pointers(
                        &format!("{label} event {index}"),
                        &event,
                        expected,
                        &variables,
                    )?;
                }
            }
            TraceStep::Restart => {
                server.take().expect("server is running").stop()?;
                server = Some(TestServer::start(state.path())?);
            }
        }
    }

    server.take().expect("server is running").stop()?;
    Ok(())
}

fn assert_pointers(
    label: &str,
    actual: &Value,
    expected: &BTreeMap<String, Value>,
    variables: &BTreeMap<String, Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (pointer, expected) in expected {
        let expected = substitute(expected.clone(), variables)?;
        let found = actual
            .pointer(pointer)
            .ok_or_else(|| format!("{label}: expected pointer is absent: {pointer}"))?;
        if found != &expected {
            return Err(format!(
                "{label}: mismatch at {pointer}: expected {expected}, found {found}"
            )
            .into());
        }
    }
    Ok(())
}

fn assert_lengths(
    label: &str,
    actual: &Value,
    lengths: &BTreeMap<String, usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (pointer, expected) in lengths {
        let found = actual
            .pointer(pointer)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{label}: expected array pointer is absent: {pointer}"))?;
        if found.len() != *expected {
            return Err(format!(
                "{label}: length mismatch at {pointer}: expected {expected}, found {}",
                found.len()
            )
            .into());
        }
    }
    Ok(())
}

fn substitute(
    value: Value,
    variables: &BTreeMap<String, Value>,
) -> Result<Value, Box<dyn std::error::Error>> {
    match value {
        Value::String(text) if text.starts_with("${") && text.ends_with('}') => {
            let name = &text[2..text.len() - 1];
            variables
                .get(name)
                .cloned()
                .ok_or_else(|| format!("unknown golden-trace variable: {name}").into())
        }
        Value::Array(values) => values
            .into_iter()
            .map(|value| substitute(value, variables))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, substitute(value, variables)?)))
            .collect::<Result<serde_json::Map<_, _>, Box<dyn std::error::Error>>>()
            .map(Value::Object),
        other => Ok(other),
    }
}
