//! The only module that touches the `omp` CLI.
//!
//! The process is spawned directly with an argument vector (never through a
//! shell), with a short timeout, and is killed if the future is dropped.

use crate::model::{Host, parse_hosts};
use async_trait::async_trait;
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;
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

    #[tokio::test]
    async fn missing_explicit_binary_is_not_found() {
        let omp = RealOmp::new(Some(PathBuf::from("definitely-not-a-real-omp-binary")));
        assert!(matches!(omp.list().await, Err(OmpError::NotFound(_))));
    }
}
