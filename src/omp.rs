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
    /// reported for it. Already-gone is success, not an error. Waits (up to
    /// [`STOP_WAIT`]) for the pid to actually disappear before returning, so
    /// callers that stop-then-start know the old process is gone and won't
    /// race a fresh one over the same session.
    async fn stop(&self, pid: u32) -> Result<(), OmpError>;
    /// Reopen a saved session by id: `omp --cwd <cwd> --resume=<session_id>`,
    /// detached, same as `start`. `omp` restores the session's own saved
    /// model on resume, so no model is passed here. Does not touch any
    /// existing process for that session; callers that want to replace a
    /// live one call `stop` first.
    async fn resume(&self, cwd: &Path, session_id: &str) -> Result<(), OmpError>;
}

/// How long `start` watches the launcher for an immediate failure.
const START_WATCH: Duration = Duration::from_secs(2);

/// How long `watch` waits, after a failed exit, for the pty reader to reach
/// EOF and hand over the child's final output.
#[cfg(not(windows))]
const PTY_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// How long `stop` waits for a killed pid to actually disappear.
const STOP_WAIT: Duration = Duration::from_secs(5);

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

    async fn resume(&self, cwd: &Path, session_id: &str) -> Result<(), OmpError> {
        let exe = self.resolve()?;
        let args = vec![
            "--cwd".to_string(),
            cwd.display().to_string(),
            format!("--resume={session_id}"),
        ];
        tokio::task::spawn_blocking(move || spawn_detached(&exe, &args))
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
    watch(cmd, ExitSource::Piped)
}

/// Elsewhere: `omp` is a TUI and exits immediately (SIGHUP-style, code 129)
/// when its stdio is `/dev/null` instead of a tty, the same failure the
/// Windows path avoids by handing it a fresh console. Here that means
/// allocating a pseudo-terminal and giving `omp` the slave side as its
/// stdin/stdout/stderr; `setsid` in the child both makes it a session leader
/// (replacing the `process_group(0)` this used before setsid existed here —
/// the two do not compose: setsid already makes the child a new process
/// group leader, and `process_group(0)` on top of that can fail with EPERM)
/// and, via `TIOCSCTTY`, is what lets that pty become its controlling
/// terminal. The master half stays open in this process, drained by a
/// background thread so the pty's buffer never fills up and blocks `omp`;
/// its last few KB are kept to fill in `OmpError::Exit`'s otherwise-empty
/// stderr when `omp` dies within `START_WATCH`.
///
/// Known limitation: the master fd lives in this process, not in a
/// separate long-lived host the way `cmd /c start` outlives this process on
/// Windows. A self-update restart of omp-deck itself closes every live
/// session's master half and, with it, SIGHUPs that session's `omp`. Fixing
/// that needs a detached pty-holding process (e.g. a re-exec'd helper); out
/// of scope here.
#[cfg(not(windows))]
fn spawn_detached(exe: &Path, args: &[String]) -> Result<(), OmpError> {
    use std::os::unix::process::CommandExt;

    let pty = pty::open().map_err(|e| OmpError::Spawn(e.to_string()))?;
    let [stdin, stdout, stderr] = pty
        .dup_slave_stdio()
        .map_err(|e| OmpError::Spawn(e.to_string()))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(args).stdin(stdin).stdout(stdout).stderr(stderr);
    // SAFETY: `setsid` and `ioctl` are both async-signal-safe and are the
    // only calls made between `fork` and `exec` in the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, pty::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Close our own slave now: while the parent holds one, the master never
    // reaches EOF, so the reader could not signal that it has drained.
    watch(cmd, ExitSource::Pty(pty.into_master()))
}

/// A minimal `openpty(3)` wrapper: allocate a pty, hand the child the slave
/// side, keep the master side here to drain and to detect the pty closing.
#[cfg(not(windows))]
mod pty {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[cfg(target_os = "macos")]
    pub const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
    #[cfg(target_os = "linux")]
    pub const TIOCSCTTY: libc::c_ulong = 0x540E;

    pub struct Pty {
        pub master: OwnedFd,
        slave: OwnedFd,
    }

    /// Allocate a pty. `openpty` is a libc extension (not POSIX) but is
    /// present on both Linux (glibc/musl) and macOS.
    pub fn open() -> std::io::Result<Pty> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // SAFETY: `openpty` fully initializes both out-params on success;
        // null is accepted for the name/termios/winsize out-params we don't
        // need.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `openpty` just returned these as freshly opened, uniquely
        // owned fds.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        set_cloexec(&master)?;
        set_cloexec(&slave)?;
        Ok(Pty { master, slave })
    }

    impl Pty {
        /// Consume the pty, closing the parent's slave fd and keeping the
        /// master.
        pub fn into_master(self) -> OwnedFd {
            self.master
        }

        /// Three independent duplicates of the slave fd, one per stdio
        /// stream, so each can be handed to `Command` and closed
        /// independently of the others and of `self.slave`.
        pub fn dup_slave_stdio(&self) -> std::io::Result<[std::process::Stdio; 3]> {
            Ok([
                dup_stdio(&self.slave)?,
                dup_stdio(&self.slave)?,
                dup_stdio(&self.slave)?,
            ])
        }
    }

    fn dup_stdio(fd: &OwnedFd) -> std::io::Result<std::process::Stdio> {
        // Close-on-exec so the copy cannot leak into unrelated children
        // spawned concurrently (which would keep the master from hitting
        // EOF); `Command` dup2s it onto 0/1/2 in the child, clearing the flag.
        // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` on a valid, open fd returns
        // either -1 or a new, uniquely owned fd.
        let d = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if d < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(d) }.into())
    }

    fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: `fd` is a valid, open fd for the duration of this call.
        let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn has_cloexec(fd: &OwnedFd) -> bool {
            // SAFETY: `fd` is a valid, open fd for the duration of this call.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD failed");
            flags & libc::FD_CLOEXEC != 0
        }

        /// A freshly opened pty must not leak either end into unrelated
        /// children exec'd later; only the `dup_slave_stdio` copies (which
        /// `Command` dup2s onto 0/1/2) are meant to reach `omp`.
        #[test]
        fn open_marks_master_and_slave_cloexec() {
            let pty = open().expect("openpty");
            assert!(has_cloexec(&pty.master), "master lacks FD_CLOEXEC");
            assert!(has_cloexec(&pty.slave), "slave lacks FD_CLOEXEC");
        }
    }
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
        return wait_pid_gone(pid);
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
        return wait_pid_gone(pid);
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

/// Whether `pid` still names a live process.
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

/// Whether `pid` still names a live process, via the no-op `kill -0`.
#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Poll for a just-killed pid to actually disappear, up to [`STOP_WAIT`].
/// A pid still alive after that is reported as a timeout rather than success,
/// so a caller that chains a fresh `start`/`resume` onto `stop` never races
/// the old process over the same session.
fn wait_pid_gone(pid: u32) -> Result<(), OmpError> {
    let deadline = std::time::Instant::now() + STOP_WAIT;
    while pid_alive(pid) {
        if std::time::Instant::now() >= deadline {
            return Err(OmpError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Spawn and report a failure that happens within `START_WATCH`.
#[cfg_attr(not(windows), allow(dead_code))]
enum ExitSource {
    /// Windows: read `child.stderr` once, on failure, the way `Command`
    /// already buffers it.
    Piped,
    /// Elsewhere: continuously drain the pty master in a background thread
    /// so it never blocks `omp`, and use its tail as the failure's stderr.
    #[cfg(not(windows))]
    Pty(std::os::fd::OwnedFd),
}

fn watch(mut cmd: std::process::Command, exit_source: ExitSource) -> Result<(), OmpError> {
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OmpError::NotFound(e.to_string())
        } else {
            OmpError::Spawn(e.to_string())
        }
    })?;
    // Drop the `Command` so the stdio fds it holds are closed in this
    // process; otherwise a pty master never sees EOF.
    drop(cmd);
    // On Windows, `ExitSource::Piped` is the only variant, so this just
    // consumes the parameter (it would otherwise go unused on that
    // platform, since the `match` right below it is unix-only).
    #[cfg(windows)]
    let ExitSource::Piped = exit_source;
    #[cfg(not(windows))]
    let captured = match exit_source {
        ExitSource::Piped => None,
        ExitSource::Pty(master) => Some(spawn_pty_reader(master)),
    };
    let deadline = std::time::Instant::now() + START_WATCH;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                #[cfg(not(windows))]
                let stderr = match &captured {
                    Some((captured, done)) => {
                        // The child has exited, but the reader thread may
                        // not have drained the master yet; wait for its EOF
                        // so the tail is complete.
                        let _ = done.recv_timeout(PTY_DRAIN_GRACE);
                        let buf = captured.lock().unwrap_or_else(|e| e.into_inner());
                        String::from_utf8_lossy(&buf)
                            .trim()
                            .chars()
                            .take(500)
                            .collect()
                    }
                    None => read_child_stderr(&mut child),
                };
                #[cfg(windows)]
                let stderr = read_child_stderr(&mut child);
                return Err(OmpError::Exit {
                    code: status.code(),
                    stderr,
                });
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            // Still running: `omp` itself (non-Windows) is up. Nothing here
            // ever calls `wait` on it again, and on Unix an un-waited child
            // that later exits sits as a zombie — still visible to `kill -0`
            // — until something reaps it. `stop` polls exactly that signal
            // to tell a killed process has actually gone, so a background
            // thread blocked on `wait` is what makes that polling ever see
            // "gone" once the process exits on its own.
            Ok(None) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(e) => return Err(OmpError::Spawn(e.to_string())),
        }
    }
}

/// Windows-only fallback: read `child`'s piped stderr once it has exited.
fn read_child_stderr(child: &mut std::process::Child) -> String {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr.trim().chars().take(500).collect()
}

/// Continuously drain a pty master in the background so it never fills up
/// and blocks the child attached to its slave side; keep the last few KB so
/// a caller can use them as an `OmpError::Exit`'s stderr. Runs until the
/// master read errors or returns EOF, which on Unix happens once every
/// slave fd (held only by the child here) has closed.
#[cfg(not(windows))]
fn spawn_pty_reader(
    master: std::os::fd::OwnedFd,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::sync::mpsc::Receiver<()>,
) {
    use std::io::Read;
    const TAIL: usize = 8192;
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let buf2 = std::sync::Arc::clone(&buf);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(master);
        let mut chunk = [0u8; 4096];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let mut b = buf2.lock().unwrap_or_else(|e| e.into_inner());
                    b.extend_from_slice(&chunk[..n]);
                    let len = b.len();
                    if len > TAIL {
                        b.drain(0..len - TAIL);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // Most commonly EIO once the last slave fd closes.
                Err(_) => break,
            }
        }
        let _ = tx.send(());
    });
    (buf, rx)
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
        let child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/c", "ping -n 31 127.0.0.1 >NUL"])
                .spawn()
        } else {
            std::process::Command::new("sleep").arg("30").spawn()
        }
        .unwrap();
        let pid = child.id();
        // `stop` now waits for the pid to actually disappear, which on Unix
        // needs *something* to reap it once it exits, same as a spawned
        // `omp` needs `watch`'s own reaper thread. Here that's this thread,
        // standing in for whatever normally owns the process (this test's
        // direct child is unusual; a real target is typically reaped by its
        // own unrelated parent, not by omp-deck).
        let reaper = std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        assert!(omp.stop(pid).await.is_ok());
        reaper.join().unwrap();
        // Stopping an already-gone pid is still Ok: idempotent.
        assert!(omp.stop(pid).await.is_ok());
    }

    // No Windows equivalent of an unkillable-by-terminate process is set up
    // here; taskkill's own `/F` already forces termination, so the timeout
    // branch there is unreached in practice, unlike SIGTERM on Unix.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn stop_502s_as_a_timeout_when_the_process_ignores_sigterm() {
        let omp = RealOmp::default();
        let mut child = std::process::Command::new("sh")
            // Prints once the trap is actually installed, so the test never
            // races the shell's own startup: without this, `stop` can send
            // SIGTERM before `trap` has run, in which case it kills the
            // shell normally and the test observes `Ok` instead of the
            // timeout it means to exercise.
            .args(["-c", "trap '' TERM; echo ready; sleep 30"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        {
            use std::io::{BufRead, BufReader};
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "ready");
        }
        assert!(matches!(omp.stop(pid).await, Err(OmpError::Timeout)));
        // Clean up: the process ignored SIGTERM, so SIGKILL it directly.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output();
        let _ = child.wait();
    }

    /// Writes `body` as an executable `sh` script to a fresh temp file and
    /// returns its path, kept alive for as long as the returned `TempPath`
    /// is (spawn_detached only needs the path, not an open handle).
    #[cfg(not(windows))]
    fn shell_script(body: &str) -> tempfile::TempPath {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        f.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .unwrap();
        f.into_temp_path()
    }

    // Regression test for the bug this pty support fixes: `omp` is a TUI
    // that used to be spawned with all of stdin/stdout/stderr set to
    // `/dev/null`, so it saw no tty and exited immediately (code 129,
    // reported as a 502 with empty stderr). `spawn_detached` must instead
    // give it a real tty on stdin and stdout.
    #[cfg(not(windows))]
    #[test]
    fn spawn_detached_gives_the_child_a_real_tty() {
        let script = shell_script("[ -t 0 ] && [ -t 1 ] && exec sleep 5\nexit 42");
        // Still running after `START_WATCH` (it's inside `sleep 5`) counts
        // as success; dying with 42 would mean it saw no tty.
        assert!(spawn_detached(&script, &[]).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn spawn_detached_reports_exit_code_and_output_on_early_failure() {
        let script = shell_script("echo boom\nexit 3");
        match spawn_detached(&script, &[]) {
            Err(OmpError::Exit { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert!(stderr.contains("boom"), "{stderr:?}");
            }
            other => panic!("expected Exit{{code: 3, ..}}, got {other:?}"),
        }
    }

    // Before the pty's master was drained in a background thread, a chatty
    // child could fill the pty's buffer and block forever on write, which
    // would make this hang instead of observing the child's own exit.
    #[cfg(not(windows))]
    #[test]
    fn spawn_detached_drains_large_output_without_hanging() {
        let script = shell_script("yes | head -c 200000\nexit 7");
        let start = std::time::Instant::now();
        let result = spawn_detached(&script, &[]);
        assert!(
            start.elapsed() < START_WATCH,
            "spawn_detached took {:?}, likely blocked on a full pty buffer",
            start.elapsed()
        );
        assert!(
            matches!(result, Err(OmpError::Exit { code: Some(7), .. })),
            "{result:?}"
        );
    }
}
