//! The only module that touches the `omp` CLI and the processes it starts.
//!
//! The process is spawned directly with an argument vector (never through a
//! shell), with a short timeout, and is killed if the future is dropped.

use crate::model::{Host, parse_hosts};
use async_trait::async_trait;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    View,
    Control,
}

#[derive(Debug)]
pub enum OmpError {
    NotFound(String),
    Spawn(String),
    Timeout,
    Exit { code: Option<i32>, stderr: String },
    Parse(String),
}

impl fmt::Display for OmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "omp not found: {msg}"),
            Self::Spawn(msg) => write!(f, "failed to run omp: {msg}"),
            Self::Timeout => write!(f, "omp timed out after {}s", TIMEOUT.as_secs()),
            Self::Exit { code, stderr } => {
                let code = code.map_or_else(|| "signal".to_string(), |c| c.to_string());
                write!(f, "omp exited with {code}: {stderr}")
            }
            Self::Parse(msg) => write!(f, "could not parse omp output: {msg}"),
        }
    }
}

impl std::error::Error for OmpError {}

/// Access to the `omp` CLI, injectable so handlers can be tested with a fake.
#[async_trait]
pub trait Omp: Send + Sync {
    /// `omp collab list --json`.
    async fn list(&self) -> Result<Vec<Host>, OmpError>;
    /// `omp collab link <instanceId> --json [--view]`; returns the secret URL.
    async fn link(&self, instance_id: &str, access: Access) -> Result<String, OmpError>;
    /// Start `omp --cwd <cwd> [--model <model>]` detached and return without
    /// waiting for the session to end. Fails if it dies right away.
    async fn start(&self, cwd: &Path, model: Option<&str>) -> Result<(), OmpError>;
    /// End a session by killing its process. `omp collab` has no remote
    /// stop command, so this kills the OS process at the pid `list` last
    /// reported for it. Already-gone is success, not an error.
    async fn stop(&self, pid: u32) -> Result<(), OmpError>;
}

/// How long `start` watches the launcher for an immediate failure.
const START_WATCH: Duration = Duration::from_secs(2);

/// Characters that cannot be passed safely through `cmd.exe`'s command line.
#[cfg(any(windows, test))]
fn unsafe_for_cmd(arg: &str) -> bool {
    arg.is_empty()
        || arg.ends_with('\\')
        || arg
            .chars()
            .any(|c| matches!(c, '"' | '%' | '!') || c.is_control())
}

#[derive(Debug, Deserialize)]
struct LinkOutput {
    url: String,
}

pub fn parse_link(json: &str) -> Result<String, serde_json::Error> {
    serde_json::from_str::<LinkOutput>(json).map(|l| l.url)
}

/// The real thing: runs the `omp` executable.
#[derive(Debug, Clone, Default)]
pub struct RealOmp {
    explicit: Option<PathBuf>,
}

impl RealOmp {
    pub fn new(explicit: Option<PathBuf>) -> Self {
        Self { explicit }
    }

    /// Explicit path first, then PATH lookup preferring `omp.exe` over `omp.cmd`.
    fn resolve(&self) -> Result<PathBuf, OmpError> {
        if let Some(path) = &self.explicit {
            return Ok(path.clone());
        }
        which::which("omp.exe")
            .or_else(|_| which::which("omp"))
            .map_err(|e| OmpError::NotFound(e.to_string()))
    }

    async fn run(&self, args: &[&str]) -> Result<String, OmpError> {
        let exe = self.resolve()?;
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let output = tokio::time::timeout(TIMEOUT, cmd.output())
            .await
            .map_err(|_| OmpError::Timeout)?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    OmpError::NotFound(e.to_string())
                } else {
                    OmpError::Spawn(e.to_string())
                }
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(OmpError::Exit {
                code: output.status.code(),
                stderr: stderr.trim().chars().take(500).collect(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[async_trait]
impl Omp for RealOmp {
    async fn list(&self) -> Result<Vec<Host>, OmpError> {
        let out = self.run(&["collab", "list", "--json"]).await?;
        parse_hosts(&out).map_err(|e| OmpError::Parse(e.to_string()))
    }

    async fn link(&self, instance_id: &str, access: Access) -> Result<String, OmpError> {
        let mut args = vec!["collab", "link", instance_id, "--json"];
        if access == Access::View {
            args.push("--view");
        }
        let out = self.run(&args).await?;
        // Do not echo `out` on failure: it holds the secret.
        parse_link(&out).map_err(|_| OmpError::Parse("unexpected link output".into()))
    }

    async fn start(&self, cwd: &Path, model: Option<&str>) -> Result<(), OmpError> {
        let exe = self.resolve()?;
        let mut args = vec!["--cwd".to_string(), cwd.display().to_string()];
        if let Some(model) = model {
            args.push("--model".into());
            args.push(model.to_string());
        }
        tokio::task::spawn_blocking(move || spawn_detached(&exe, &args))
            .await
            .map_err(|e| OmpError::Spawn(e.to_string()))?
    }

    async fn stop(&self, pid: u32) -> Result<(), OmpError> {
        tokio::task::spawn_blocking(move || kill_pid(pid))
            .await
            .map_err(|e| OmpError::Spawn(e.to_string()))?
    }
}

/// Windows: `omp` is a TUI that exits (as if hung up) when it has no console
/// to read, and Rust's `Command` always hands the child explicit std handles,
/// so `CREATE_NEW_CONSOLE` alone does not give it one. `cmd /c start` creates
/// the process with a fresh console and no inherited handles, and the omp it
/// starts outlives both `cmd` and this server. Every argument is wrapped in
/// quotes by hand, and the few characters that cannot survive `cmd` are refused.
#[cfg(windows)]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<(), OmpError> {
    use std::os::windows::process::CommandExt;
    let exe = exe.display().to_string();
    if std::iter::once(&exe).chain(args).any(|a| unsafe_for_cmd(a)) {
        return Err(OmpError::Spawn(
            "path or model contains a character cmd.exe cannot pass safely".into(),
        ));
    }
    let mut cmd = std::process::Command::new("cmd.exe");
    cmd.args(["/c", "start"])
        .raw_arg("\"\"")
        .arg("/min")
        .raw_arg(format!("\"{exe}\""));
    for a in args {
        cmd.raw_arg(format!("\"{a}\""));
    }
    cmd.creation_flags(0x0800_0000) // CREATE_NO_WINDOW: for cmd itself, not omp
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    watch(cmd)
}

/// Elsewhere: best effort, own process group, no terminal. Untested against a
/// real omp; if it needs a tty there, it dies within `START_WATCH` and 502s.
#[cfg(not(windows))]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<(), OmpError> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    watch(cmd)
}

/// Windows: `taskkill /T` also takes down `omp`'s own child processes (e.g.
/// a running tool). Exit code 128 means the pid was already gone, which
/// counts as success: the goal is "not running", not "we did the killing".
#[cfg(windows)]
fn kill_pid(pid: u32) -> Result<(), OmpError> {
    use std::os::windows::process::CommandExt;
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    let output = cmd.output().map_err(|e| OmpError::Spawn(e.to_string()))?;
    if output.status.success() || output.status.code() == Some(128) {
        return Ok(());
    }
    Err(OmpError::Exit {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(500)
            .collect(),
    })
}

/// Elsewhere: `SIGTERM` via the `kill` utility; "No such process" counts as
/// success (see above).
#[cfg(not(windows))]
fn kill_pid(pid: u32) -> Result<(), OmpError> {
    let output = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .map_err(|e| OmpError::Spawn(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such process") {
        return Ok(());
    }
    Err(OmpError::Exit {
        code: output.status.code(),
        stderr: stderr.trim().chars().take(500).collect(),
    })
}

/// Spawn and report a failure that happens within `START_WATCH`.
fn watch(mut cmd: std::process::Command) -> Result<(), OmpError> {
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OmpError::NotFound(e.to_string())
        } else {
            OmpError::Spawn(e.to_string())
        }
    })?;
    let deadline = std::time::Instant::now() + START_WATCH;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(OmpError::Exit {
                    code: status.code(),
                    stderr: stderr.trim().chars().take(500).collect(),
                });
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Still running: `omp` itself (non-Windows) is up.
            Ok(None) => return Ok(()),
            Err(e) => return Err(OmpError::Spawn(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_link_output_ignoring_extras() {
        let json = r#"{"version":1,"instanceId":"a","generation":2,"access":"view","url":"https://my.omp.sh/#k"}"#;
        assert_eq!(parse_link(json).unwrap(), "https://my.omp.sh/#k");
        assert!(parse_link("{}").is_err());
    }

    #[test]
    fn error_display_is_informative() {
        let e = OmpError::Exit {
            code: Some(1),
            stderr: "boom".into(),
        };
        assert_eq!(e.to_string(), "omp exited with 1: boom");
    }

    #[test]
    fn cmd_unsafe_args_are_detected() {
        for bad in ["", "a\"b", "50%", "hi!", "dir\\", "a\nb"] {
            assert!(unsafe_for_cmd(bad), "{bad:?}");
        }
        for ok in [r"C:\src\my repo", "gpt-5.2", "a&b"] {
            assert!(!unsafe_for_cmd(ok), "{ok:?}");
        }
    }

    #[tokio::test]
    async fn missing_explicit_binary_is_not_found() {
        let omp = RealOmp::new(Some(PathBuf::from("definitely-not-a-real-omp-binary")));
        assert!(matches!(omp.list().await, Err(OmpError::NotFound(_))));
    }

    #[tokio::test]
    async fn stop_kills_a_running_process_and_is_idempotent() {
        let omp = RealOmp::default();
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/c", "ping -n 31 127.0.0.1 >NUL"])
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("30").spawn()
        }
        .unwrap();
        let pid = child.id();
        assert!(omp.stop(pid).await.is_ok());
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(matches!(child.try_wait(), Ok(Some(_))), "still running");
        // Stopping an already-gone pid is still Ok: idempotent.
        assert!(omp.stop(pid).await.is_ok());
    }
}
