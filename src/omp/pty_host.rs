//! The detached per-session helper behind [`super::spawn_detached`]: owns one
//! `omp`'s pty master so the session survives the server that started it.

use super::{host_status_for, start_on_pty};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

/// `argv` is the omp executable followed by its arguments. Reports the
/// start-up outcome as one JSON line on stdout, then stays alive until `omp`
/// exits, so the pty master (and with it the controlling terminal) stays open.
pub fn run(argv: Vec<String>) -> ExitCode {
    let Some((exe, args)) = argv.split_first() else {
        return ExitCode::FAILURE;
    };
    let started = start_on_pty(Path::new(exe), args);
    let status = host_status_for(&started);
    // A failed write (the server went away or stopped listening) must not
    // affect `omp`'s lifetime.
    if let Ok(line) = serde_json::to_string(&status) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    silence_stdout();
    match started {
        Ok(Some(mut child)) => {
            // Always reap `omp`; the reader thread dies with this process, so
            // descendants that keep the slave open cannot hold us up.
            let _ = child.wait();
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Point fd 1 at `/dev/null` so the server's status pipe sees EOF.
fn silence_stdout() {
    use std::os::fd::AsRawFd;
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: both fds are valid and open for the duration of the call.
        unsafe { libc::dup2(null.as_raw_fd(), 1) };
    }
}
