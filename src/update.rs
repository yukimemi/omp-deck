//! Self-update support for `omp-deck`, using the [`kaishin`] library — the
//! same self-update integration `renri` and `rvpm` use.
//!
//! Entry points:
//! - [`run_self_update`] drives the explicit `omp-deck self-update` command.
//! - [`maybe_spawn_auto_update_check`] / [`finalize_auto_update_check`] run a
//!   throttled (24h) background version check that prints an update banner
//!   once the command completes.
//! - [`SelfUpdater`] (backed by [`RealUpdater`]) is the same check-then-install
//!   flow, injectable so the web dashboard's `/api/self-update` handler in
//!   `server.rs` can trigger it and be tested against a fake.
//!
//! Unlike `renri`/`rvpm`, `omp-deck` has no config file, so there is only one
//! CLI mode: check and notify (never a silent background install). Set
//! `OMP_DECK_NO_AUTOUPDATE=1` to disable both the CLI check and the web
//! trigger.

use async_trait::async_trait;
use kaishin::{Checker, KaishinOptions, LatestRelease, UpdateOptions, check_latest_release};
use tokio::task::JoinHandle;

fn options() -> KaishinOptions {
    KaishinOptions::new(
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// Pure form of the `OMP_DECK_NO_AUTOUPDATE` check: `0`, `false`
/// (case-insensitive), and unset/blank all count as "not disabled".
fn disabled_by(value: Option<&str>) -> bool {
    match value {
        Some(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        None => false,
    }
}

/// Whether `OMP_DECK_NO_AUTOUPDATE` disables the background check (and the
/// web `/api/self-update` trigger, which refuses outright rather than
/// silently contacting GitHub).
fn auto_update_disabled_by_env() -> bool {
    disabled_by(std::env::var("OMP_DECK_NO_AUTOUPDATE").ok().as_deref())
}

/// A background update check in flight, or a cached result found within the
/// throttle window. Resolved by [`finalize_auto_update_check`].
pub enum AutoUpdateHandle {
    /// Still inside the throttle window; the last check already found a
    /// newer release, so there is nothing to fetch — just report it.
    Cached {
        checker: Checker,
        latest: LatestRelease,
    },
    /// Due for a check: a fetch is running on this runtime, to be joined
    /// (with a short timeout) once the command finishes.
    Pending {
        checker: Checker,
        handle: JoinHandle<anyhow::Result<Option<LatestRelease>>>,
        /// Fallback if the fetch times out or fails.
        cached: Option<LatestRelease>,
    },
}

/// Starts a throttled (24h) background check for a newer release, unless
/// `OMP_DECK_NO_AUTOUPDATE` disables it. Never blocks: a due check is
/// spawned on the current tokio runtime, to be joined later by
/// [`finalize_auto_update_check`].
pub fn maybe_spawn_auto_update_check() -> Option<AutoUpdateHandle> {
    if auto_update_disabled_by_env() {
        return None;
    }
    let checker = Checker::new(env!("CARGO_PKG_NAME"), options());
    if !checker.should_check() {
        return checker
            .cached_update()
            .map(|latest| AutoUpdateHandle::Cached { checker, latest });
    }
    let cached = checker.cached_update();
    let checker_clone = checker.clone();
    let handle = tokio::spawn(async move { checker_clone.check_and_save().await });
    Some(AutoUpdateHandle::Pending {
        checker,
        handle,
        cached,
    })
}

/// Joins a pending background check (1 second timeout — a slow fetch is
/// skipped, not waited on) and prints an update banner to stderr if a newer
/// release is available.
pub async fn finalize_auto_update_check(handle: AutoUpdateHandle) {
    match handle {
        AutoUpdateHandle::Cached { checker, latest } => {
            eprintln!("\n{}", checker.format_banner(&latest));
        }
        AutoUpdateHandle::Pending {
            checker,
            handle,
            cached,
        } => {
            let res = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
            match res {
                Ok(Ok(Ok(Some(latest)))) => eprintln!("\n{}", checker.format_banner(&latest)),
                Ok(Ok(Ok(None))) => {}
                // Timed out, the fetch failed, or the task panicked: fall
                // back to the last known result rather than blocking.
                _ => {
                    if let Some(latest) = cached {
                        eprintln!("\n{}", checker.format_banner(&latest));
                    }
                }
            }
        }
    }
}

/// `omp-deck self-update`.
pub async fn run_self_update(yes: bool, check_only: bool) -> Result<(), String> {
    let upd_opts = UpdateOptions::new().yes(yes).check_only(check_only);
    kaishin::run_self_update(&options(), upd_opts)
        .await
        .map_err(|e| e.to_string())
}

/// The self-update flow the web dashboard's `/api/self-update` handler
/// drives, injectable so that handler can be tested against a fake instead
/// of making a real GitHub call or replacing the real binary.
#[async_trait]
pub trait SelfUpdater: Send + Sync {
    /// Whether `OMP_DECK_NO_AUTOUPDATE` refuses updates outright. Checked
    /// first, and before any network call: a disabled instance must not
    /// contact GitHub just because a button was pressed.
    fn disabled(&self) -> bool;
    /// A real (uncached) check for a newer release. `Ok(None)` means already
    /// up to date -- nothing to install, nothing to restart.
    async fn newer_release(&self) -> Result<Option<LatestRelease>, String>;
    /// Installs the newer release in place and leaves it for the caller to
    /// hand the listening socket over to a successor process. Non-interactive
    /// (`yes = true`): nobody is at a terminal to answer a prompt.
    async fn install(&self) -> Result<(), String>;
}

/// The real backend, used by `omp-deck serve`.
#[derive(Default)]
pub struct RealUpdater;

impl RealUpdater {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SelfUpdater for RealUpdater {
    fn disabled(&self) -> bool {
        auto_update_disabled_by_env()
    }

    async fn newer_release(&self) -> Result<Option<LatestRelease>, String> {
        let opts = options();
        let latest = check_latest_release(&opts)
            .await
            .map_err(|e| e.to_string())?;
        let available = kaishin::is_update_available(&opts.current_version, &latest.tag_name)
            .map_err(|e| e.to_string())?;
        Ok(available.then_some(latest))
    }

    async fn install(&self) -> Result<(), String> {
        run_self_update(true, false).await
    }
}

/// Args for the successor process's own `serve` invocation.
///
/// Always binds to `bind` -- the address this process is actually listening
/// on -- rather than replaying whatever `--bind` (if any) the operator gave
/// this process: without an explicit `--bind`, `bind.rs::choose_bind` picks
/// the Tailscale IPv4 address on an OS-chosen port, and a naive `execve`-style
/// replay would have the successor call `choose_bind` fresh and land on a
/// *different* random port, breaking the URL the phone was just using.
/// `--omp`/`--config` are carried over verbatim (both are plain paths); the
/// Discord webhook is deliberately left out here -- see `spawn_successor`,
/// which passes it through the environment instead so it never shows up in
/// a process listing.
pub fn successor_args(
    bind: std::net::SocketAddr,
    omp: Option<&std::path::Path>,
    config: Option<&std::path::Path>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(omp) = omp {
        args.push("--omp".to_string());
        args.push(omp.display().to_string());
    }
    if let Some(config) = config {
        args.push("--config".to_string());
        args.push(config.display().to_string());
    }
    args.push("serve".to_string());
    args.push("--bind".to_string());
    args.push(bind.to_string());
    args
}

/// Hands the dashboard over to a freshly spawned copy of this binary, bound
/// to the same address this process was listening on.
///
/// Must only be called after the caller has already dropped its
/// `TcpListener` (see the doc comment on `/api/self-update` in `server.rs`
/// for the full handover order and why it matters): the freshly-installed
/// binary has no bind-retry loop of its own, so spawning it while the port
/// is still held races it against "address already in use", with nowhere to
/// report why if it loses.
///
/// The child is left to outlive this process (never `wait`ed, never killed
/// on drop): once this call returns, the caller is expected to exit shortly
/// after. If the child itself fails to start (a panic on its own startup
/// path, say), nothing reports that beyond its own stderr -- there is no one
/// left mid-handover to tell.
pub fn spawn_successor(
    exe: &std::path::Path,
    bind: std::net::SocketAddr,
    omp: Option<&std::path::Path>,
    config: Option<&std::path::Path>,
    discord_webhook: Option<&str>,
) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(successor_args(bind, omp, config));
    cmd.stdin(std::process::Stdio::null());
    if let Some(webhook) = discord_webhook {
        cmd.env("OMP_DECK_DISCORD_WEBHOOK", webhook);
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn disabled_by_table() {
        let cases = [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("false"), false),
            (Some("FALSE"), false),
            (Some("  false  "), false),
            (Some("1"), true),
            (Some("true"), true),
            (Some("yes"), true),
        ];
        for (input, expected) in cases {
            assert_eq!(disabled_by(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn successor_args_uses_the_actual_bound_address() {
        let bind: std::net::SocketAddr = "100.64.0.7:54321".parse().unwrap();
        assert_eq!(
            successor_args(bind, None, None),
            vec!["serve", "--bind", "100.64.0.7:54321"]
        );
    }

    #[test]
    fn successor_args_ignores_port_0_and_carries_over_omp_and_config() {
        // Even if this process itself was started with no --bind (port 0),
        // `bind` here is always the real bound port -- never 0.
        let bind: std::net::SocketAddr = "100.64.0.7:41234".parse().unwrap();
        let args = successor_args(
            bind,
            Some(Path::new("/usr/local/bin/omp")),
            Some(Path::new("/etc/omp-deck.toml")),
        );
        assert_eq!(
            args,
            vec![
                "--omp",
                "/usr/local/bin/omp",
                "--config",
                "/etc/omp-deck.toml",
                "serve",
                "--bind",
                "100.64.0.7:41234",
            ]
        );
    }

    #[test]
    fn successor_args_never_contains_the_webhook() {
        let args = successor_args("127.0.0.1:8080".parse().unwrap(), None, None);
        assert!(args.iter().all(|a| !a.contains("discord")));
    }
}
