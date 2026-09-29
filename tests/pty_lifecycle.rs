//! An `omp` started through the pty host must outlive the process that asked
//! for it, and keep its controlling terminal.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn script(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("fake-omp.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", path.display());
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn wait_gone(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} still alive");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn omp_survives_the_launcher_and_keeps_its_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let omp = script(
        dir.path(),
        &format!(
            "[ -t 0 ] || exit 42\nsleep 3\nif [ -t 0 ]; then t=tty; else t=notty; fi\n\
             echo \"$$ $t\" > {m}.tmp && mv {m}.tmp {m}\nsleep 30",
            m = marker.display()
        ),
    );
    // Started the way the server starts it: in its own session. The status
    // reader then goes away (as it does when the server exits) while `omp`
    // is still starting up.
    let mut host = Command::new(env!("CARGO_BIN_EXE_omp-deck"));
    host.arg("pty-host")
        .arg("--")
        .arg(&omp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe.
    unsafe {
        host.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut host = host.spawn().unwrap();
    let mut out = String::new();
    host.stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.contains(r#""ok":true"#), "{out}");

    let seen = wait_for(&marker);
    let mut parts = seen.split_whitespace();
    let pid = parts.next().unwrap().to_string();
    assert_eq!(parts.next(), Some("tty"), "omp lost its terminal");

    // Explicit stop: the process goes away.
    Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    wait_gone(&pid);
    // The host exits once `omp` has been reaped.
    assert!(host.wait().is_ok());
}

#[test]
fn early_exit_is_reported_with_code_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let omp = script(dir.path(), "echo boom\nexit 3");
    let out = Command::new(env!("CARGO_BIN_EXE_omp-deck"))
        .args(["pty-host", "--"])
        .arg(&omp)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(
        line.contains(r#""code":3"#) && line.contains("boom"),
        "{line}"
    );
}

#[test]
fn heavy_output_does_not_stall_omp() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("done");
    let omp = script(
        dir.path(),
        &format!(
            "sleep 3\nyes | head -c 2000000\necho ok > {m}.tmp && mv {m}.tmp {m}\nsleep 1",
            m = marker.display()
        ),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_omp-deck"))
        .args(["pty-host", "--"])
        .arg(&omp)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""ok":true"#));
    wait_for(&marker);
}
