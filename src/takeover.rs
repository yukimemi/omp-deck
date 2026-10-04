//! `omp-deck resume`: take over a headless instance from the host terminal.
//!
//! The pure parts (`select`, `parse_choice`, `render_choices`) are unit
//! tested; `run` does the I/O: re-list, validate, stop, then run `omp` in
//! the foreground with inherited stdio so it re-publishes via
//! `collab.autoStart`.

use crate::model::Host;
use crate::omp::{Omp, RealOmp, foreground_command, passes_cmd_safely, resume_args, resume_target};
use crate::{now_ms, view};
use std::io::{BufRead, IsTerminal, Write};

#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    NotFound,
    Ambiguous(Vec<usize>),
}

/// Pick one host by instance id or session id: an exact match wins,
/// otherwise a unique prefix. Returns an index into `hosts`.
pub fn select(hosts: &[Host], query: &str) -> Result<usize, SelectError> {
    let exact: Vec<usize> = (0..hosts.len())
        .filter(|&i| hosts[i].instance_id == query || hosts[i].session_id == query)
        .collect();
    let found = if exact.is_empty() {
        (0..hosts.len())
            .filter(|&i| {
                !query.is_empty()
                    && (hosts[i].instance_id.starts_with(query)
                        || hosts[i].session_id.starts_with(query))
            })
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [] => Err(SelectError::NotFound),
        [one] => Ok(*one),
        _ => Err(SelectError::Ambiguous(found)),
    }
}

/// Interpret a typed 1-based number. `Ok(None)` means "cancel" (empty
/// input); `Err` is out-of-range or not a number.
pub fn parse_choice(input: &str, len: usize) -> Result<Option<usize>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    match input.parse::<usize>() {
        Ok(n) if (1..=len).contains(&n) => Ok(Some(n - 1)),
        _ => Err(format!("enter a number from 1 to {len}")),
    }
}

/// The numbered list: the usual table with a `#` column and the full
/// session id under each row.
pub fn render_choices(hosts: &[Host], now: i64) -> String {
    let table = view::render_table(hosts, now);
    if hosts.is_empty() {
        return table;
    }
    let mut out = String::new();
    for (i, line) in table.lines().enumerate() {
        if i == 0 {
            out.push_str(&format!("{:>3}  {line}\n", "#"));
        } else {
            let h = &hosts[i - 1];
            out.push_str(&format!("{i:>3}  {line}\n"));
            out.push_str(&format!("       id: {}\n", h.session_id));
        }
    }
    out
}

/// Entry point; the returned code is the process exit code.
pub async fn run(omp: &RealOmp, query: Option<&str>) -> Result<i32, String> {
    let hosts = omp.list().await.map_err(|e| e.to_string())?;
    if !std::io::stdout().is_terminal() {
        print!("{}", render_choices(&hosts, now_ms()));
        return Err("stdout is not a terminal; not picking a session".into());
    }
    if hosts.is_empty() {
        return Err("no live omp sessions".into());
    }
    let idx = match query {
        Some(q) => select(&hosts, q).map_err(|e| match e {
            SelectError::NotFound => format!("no live session matches {q:?}"),
            SelectError::Ambiguous(ix) => {
                let picked: Vec<Host> = ix.iter().map(|&i| hosts[i].clone()).collect();
                format!(
                    "{q:?} matches several sessions:\n{}",
                    render_choices(&picked, now_ms())
                )
            }
        })?,
        None => {
            if !std::io::stdin().is_terminal() {
                print!("{}", render_choices(&hosts, now_ms()));
                return Err("stdin is not a terminal; pass an id to choose".into());
            }
            match prompt(&hosts)? {
                Some(i) => i,
                None => return Ok(0),
            }
        }
    };
    let chosen = &hosts[idx];

    // Re-list: act only on what omp reports right now for this instance.
    let fresh = omp.list().await.map_err(|e| e.to_string())?;
    let host = fresh
        .iter()
        .find(|h| h.instance_id == chosen.instance_id && h.session_id == chosen.session_id)
        .ok_or("the session is no longer live")?;
    let target = resume_target(host)?;
    let args = resume_args(&target.cwd, &target.session_id);
    // Everything that can fail without side effects happens before the kill.
    let exe = omp.resolve().map_err(|e| e.to_string())?;
    if !passes_cmd_safely(&exe, &args) {
        return Err("path or session contains a character cmd.exe cannot pass safely".into());
    }
    omp.stop(target.pid).await.map_err(|e| e.to_string())?;

    let manual = format!("omp --cwd {} --resume={}", target.cwd, target.session_id);
    let mut cmd = foreground_command(&exe, &args);
    // The TUI takes Ctrl-C itself; do not let a stray SIGINT end us first.
    #[cfg(unix)]
    let _ = ignore_sigint();
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("stopped, but could not start omp: {e}; run `{manual}` yourself"))?;
    Ok(status.code().unwrap_or(1))
}

fn prompt(hosts: &[Host]) -> Result<Option<usize>, String> {
    let stdin = std::io::stdin();
    loop {
        print!("{}", render_choices(hosts, now_ms()));
        print!(
            "resume which session? [1-{}, empty to cancel] ",
            hosts.len()
        );
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Ok(None);
        }
        match parse_choice(&line, hosts.len()) {
            Ok(choice) => return Ok(choice),
            Err(msg) => eprintln!("{msg}"),
        }
    }
}

#[cfg(unix)]
fn ignore_sigint() -> std::io::Result<()> {
    // Block-free and dependency-free: tokio's signal support is already
    // linked, but a handler needs a runtime task; the default disposition is
    // replaced for the lifetime of this short-lived process instead.
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = signal(SignalKind::interrupt())?;
    tokio::spawn(async move { while sig.recv().await.is_some() {} });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(inst: &str, sess: &str) -> Host {
        serde_json::from_str(&format!(
            r#"{{"instanceId":"{inst}","sessionId":"{sess}","cwd":"/w"}}"#
        ))
        .unwrap()
    }

    #[test]
    fn select_prefers_exact_then_unique_prefix() {
        let hosts = [h("inst-1", "abc"), h("inst-2", "abcdef")];
        assert_eq!(select(&hosts, "abc"), Ok(0)); // exact beats the prefix of abcdef
        assert_eq!(select(&hosts, "abcd"), Ok(1));
        assert_eq!(select(&hosts, "inst-2"), Ok(1));
    }

    #[test]
    fn select_reports_ambiguous_and_missing() {
        let hosts = [h("inst-1", "abc1"), h("inst-2", "abc2")];
        assert_eq!(
            select(&hosts, "ab"),
            Err(SelectError::Ambiguous(vec![0, 1]))
        );
        assert_eq!(select(&hosts, "zzz"), Err(SelectError::NotFound));
        assert_eq!(select(&hosts, ""), Err(SelectError::NotFound));
        assert_eq!(select(&[], "a"), Err(SelectError::NotFound));
    }

    #[test]
    fn parse_choice_cases() {
        assert_eq!(parse_choice("2\n", 3), Ok(Some(1)));
        assert_eq!(parse_choice("  \n", 3), Ok(None));
        assert!(parse_choice("0", 3).is_err());
        assert!(parse_choice("4", 3).is_err());
        assert!(parse_choice("x", 3).is_err());
        assert!(parse_choice("-1", 3).is_err());
    }

    #[test]
    fn render_choices_numbers_rows_and_shows_full_session_id() {
        let out = render_choices(&[h("inst-1", "abcdefghijkl")], 0);
        assert!(out.contains("  1  "), "{out}");
        assert!(out.contains("id: abcdefghijkl"), "{out}");
    }
}
