//! Self-update support for `omp-deck`, using the [`kaishin`] library — the
//! same self-update integration `renri` and `rvpm` use.
//!
//! Two entry points:
//! - [`run_self_update`] drives the explicit `omp-deck self-update` command.
//! - [`maybe_spawn_auto_update_check`] / [`finalize_auto_update_check`] run a
//!   throttled (24h) background version check that prints an update banner
//!   once the command completes.
//!
//! Unlike `renri`/`rvpm`, `omp-deck` has no config file, so there is only one
//! mode: check and notify (never a silent background install). Set
//! `OMP_DECK_NO_AUTOUPDATE=1` to disable the check entirely.

use kaishin::{Checker, KaishinOptions, LatestRelease, UpdateOptions};
use tokio::task::JoinHandle;

fn options() -> KaishinOptions {
    KaishinOptions::new(
        "yukimemi",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    )
}

/// Whether `OMP_DECK_NO_AUTOUPDATE` disables the background check. `0`,
/// `false` (case-insensitive), and unset/blank all count as "not disabled".
fn auto_update_disabled_by_env() -> bool {
    match std::env::var("OMP_DECK_NO_AUTOUPDATE") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
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
