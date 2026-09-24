//! Choosing the address to listen on. Never defaults to 0.0.0.0.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::process::Command;

/// Where the server ends up, and why it did not get the preferred address.
#[derive(Debug, PartialEq, Eq)]
pub struct Bind {
    pub addr: SocketAddr,
    pub warning: Option<String>,
}

/// First usable IPv4 address in `tailscale ip -4` output.
fn first_tailscale_ip(output: &str) -> Option<Ipv4Addr> {
    output
        .lines()
        .filter_map(|l| l.trim().parse::<Ipv4Addr>().ok())
        .find(|ip| !ip.is_unspecified() && !ip.is_loopback())
}

fn parse_explicit(s: &str) -> Result<SocketAddr, String> {
    let s = s.trim();
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // Address without a port: let the OS pick one.
    s.parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, 0))
        .map_err(|_| format!("invalid --bind {s:?}, expected ADDR:PORT"))
}

/// Pure choice of bind address. `tailscale` is the stdout of `tailscale ip -4`,
/// or `None` when it could not be run.
pub fn choose_bind(explicit: Option<&str>, tailscale: Option<&str>) -> Result<Bind, String> {
    if let Some(explicit) = explicit {
        return Ok(Bind {
            addr: parse_explicit(explicit)?,
            warning: None,
        });
    }
    Ok(match tailscale.and_then(first_tailscale_ip) {
        Some(ip) => Bind {
            addr: SocketAddr::new(IpAddr::V4(ip), 0),
            warning: None,
        },
        None => Bind {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            warning: Some(
                "could not detect a Tailscale IPv4 address (`tailscale ip -4`); \
                 falling back to 127.0.0.1"
                    .to_string(),
            ),
        },
    })
}

/// Run `tailscale ip -4`; `None` on any failure.
pub async fn tailscale_ip_output() -> Option<String> {
    let exe = which::which("tailscale").ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(["ip", "-4"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_first_tailscale_ipv4_with_os_chosen_port() {
        let b = choose_bind(None, Some("100.64.0.7\n100.64.0.8\n")).unwrap();
        assert_eq!(b.addr, "100.64.0.7:0".parse().unwrap());
        assert!(b.warning.is_none());
    }

    #[test]
    fn skips_ipv6_and_junk_lines() {
        let b = choose_bind(None, Some("fd7a:115c::1\nnope\n100.100.1.2\n")).unwrap();
        assert_eq!(b.addr.ip().to_string(), "100.100.1.2");
    }

    #[test]
    fn default_is_never_unspecified() {
        let inputs = [
            None,
            Some(""),
            Some("garbage"),
            Some("0.0.0.0\n"),
            Some("::\n"),
            Some("127.0.0.1\n"),
            Some("100.1.2.3\n"),
        ];
        for input in inputs {
            let b = choose_bind(None, input).unwrap();
            assert!(!b.addr.ip().is_unspecified(), "input {input:?}");
            assert_eq!(b.addr.port(), 0, "input {input:?}");
        }
    }

    #[test]
    fn falls_back_to_loopback_with_warning() {
        let b = choose_bind(None, None).unwrap();
        assert_eq!(b.addr, "127.0.0.1:0".parse().unwrap());
        assert!(b.warning.unwrap().contains("127.0.0.1"));
    }

    #[test]
    fn explicit_port_is_used_as_given() {
        let b = choose_bind(Some("100.64.0.7:9999"), Some("100.1.1.1")).unwrap();
        assert_eq!(b.addr, "100.64.0.7:9999".parse().unwrap());
        let b = choose_bind(Some("127.0.0.1"), None).unwrap();
        assert_eq!(b.addr, "127.0.0.1:0".parse().unwrap());
        assert!(choose_bind(Some("bogus"), None).is_err());
    }
}
