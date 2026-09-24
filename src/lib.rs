//! omp-deck: a small dashboard for the live `omp` collab sessions on this machine.
//!
//! Pure logic (`model`, `view`, `bind`'s chooser) is kept apart from the I/O
//! layers (`omp`, `server`) so it can be unit tested without a process or a socket.

pub mod bind;
pub mod model;
pub mod notify;
pub mod omp;
pub mod server;
pub mod view;

/// Current wall-clock time as unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
