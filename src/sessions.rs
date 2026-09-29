//! Past (already-ended) `omp` sessions for a repo, read from the on-disk
//! session log `omp` itself maintains.
//!
//! The layout is undocumented and observed empirically, so everything here
//! is defensive: a directory-name guess or a file this module cannot make
//! sense of is silently skipped, never trusted. Read-only, same spirit as
//! [`crate::repos`].

use serde::Serialize;
use std::io::BufRead;
use std::path::{Component, Path, PathBuf};

/// Bytes read from the start of a session file while looking for its title
/// and session record. Real records are a few hundred bytes each; this is a
/// generous ceiling against a corrupt or unexpectedly huge first line.
const HEAD_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    pub session_id: String,
    pub title: String,
    pub updated_at_ms: i64,
}

/// `$PI_CODING_AGENT_DIR/sessions`, or `~/.omp/agent/sessions` if that env
/// var is unset or empty. `None` if neither resolves to anything (no `$HOME`
/// and no override) -- callers then simply have no sessions to offer.
pub fn default_root() -> Option<PathBuf> {
    match std::env::var("PI_CODING_AGENT_DIR") {
        Ok(dir) if !dir.is_empty() => return Some(PathBuf::from(dir).join("sessions")),
        _ => {}
    }
    dirs::home_dir().map(|h| h.join(".omp").join("agent").join("sessions"))
}

/// The directory name `omp` derives from a cwd under `$HOME`: the path
/// relative to `$HOME` with each separator replaced by `-` and a leading `-`
/// (e.g. under `$HOME` = `/Users/yukimemi`, `/Users/yukimemi/src/x` ->
/// `-src-x`, and `$HOME` itself -> `-`). Only ever called once `repo_path`
/// is confirmed to start with `home`; there is no confirmed rule for a cwd
/// outside `$HOME` (see [`list`]).
fn sanitize_cwd(home: &Path, cwd: &Path) -> String {
    let rel = cwd.strip_prefix(home).unwrap_or(cwd);
    let mut out = String::from("-");
    let mut first = true;
    for part in rel.components() {
        let Component::Normal(seg) = part else {
            continue;
        };
        if !first {
            out.push('-');
        }
        out.push_str(&seg.to_string_lossy());
        first = false;
    }
    out
}

/// Read just far enough into a session `.jsonl` file to find its `title`
/// record and its `session` record (id + cwd), in whatever order they
/// appear. `None` if no session record turns up within [`HEAD_LIMIT`].
fn read_head(path: &Path) -> Option<(Option<String>, String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut title = None;
    let mut record = None;
    let mut read = 0usize;
    let mut line = String::new();
    while read < HEAD_LIMIT {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        read += n;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("title") => {
                if let Some(t) = value.get("title").and_then(|t| t.as_str()) {
                    title = Some(t.to_string());
                }
            }
            Some("session") => {
                let id = value.get("id").and_then(|i| i.as_str());
                let cwd = value.get("cwd").and_then(|c| c.as_str());
                if let (Some(id), Some(cwd)) = (id, cwd) {
                    record = Some((id.to_string(), cwd.to_string()));
                }
                break;
            }
            _ => {}
        }
    }
    let (id, cwd) = record?;
    Some((title, id, cwd))
}

/// Whether a session's own recorded `cwd` is the same checkout as
/// `repo_path`, canonicalizing both where possible so a trailing separator
/// or a symlink hop doesn't cause a false miss.
fn cwd_matches(repo_path: &Path, record_cwd: &str) -> bool {
    let candidate = Path::new(record_cwd);
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(repo_path) == canon(candidate)
}

/// Resumable sessions for `repo_path`, newest first: every `.jsonl` file
/// belonging to it that has no live `.<name>.jsonl.lock.os` lock next to it,
/// whose filename-derived id and recorded `cwd` both match. A missing or
/// unreadable directory, or a file this module cannot parse, contributes
/// nothing rather than failing the whole list.
///
/// For a `repo_path` under `$HOME`, this looks only inside the one directory
/// [`sanitize_cwd`] derives -- confirmed against a real `omp`. For a
/// `repo_path` outside `$HOME` (or with no `$HOME` resolved at all), there is
/// no confirmed naming rule to derive that one directory from, so every
/// project directory under `sessions_root` is scanned instead; this is still
/// safe because every candidate file is cross-checked against its own
/// recorded `cwd`, exactly as in the single-directory case, so a session
/// belonging to some other project is never picked up by mistake.
pub fn list(sessions_root: &Path, home: Option<&Path>, repo_path: &Path) -> Vec<SessionEntry> {
    let mut out = Vec::new();
    match home.filter(|h| repo_path.starts_with(h)) {
        Some(home) => collect_dir(
            &sessions_root.join(sanitize_cwd(home, repo_path)),
            repo_path,
            &mut out,
        ),
        None => {
            if let Ok(entries) = std::fs::read_dir(sessions_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        collect_dir(&path, repo_path, &mut out);
                    }
                }
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.updated_at_ms));
    out
}

/// Appends every resumable, cross-checked session in one project directory
/// to `out`. A missing/unreadable directory contributes nothing.
fn collect_dir(dir: &Path, repo_path: &Path, out: &mut Vec<SessionEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Hidden files are either the lock markers themselves or not ours.
        if name.starts_with('.') {
            continue;
        }
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some((_, filename_id)) = stem.split_once('_') else {
            continue;
        };
        if dir.join(format!(".{name}.lock.os")).exists() {
            continue; // A live process still owns this session.
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let updated_at_ms = modified
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let Some((title, record_id, record_cwd)) = read_head(&path) else {
            continue;
        };
        if record_id != filename_id || !cwd_matches(repo_path, &record_cwd) {
            continue;
        }
        out.push(SessionEntry {
            session_id: record_id,
            title: title.unwrap_or_else(|| filename_id.to_string()),
            updated_at_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn write_session(
        dir: &Path,
        filename: &str,
        title: Option<&str>,
        id: &str,
        cwd: &Path,
        age_secs: u64,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let mut body = String::new();
        if let Some(title) = title {
            body.push_str(&format!(
                r#"{{"type":"title","v":1,"title":"{title}","source":"auto","updatedAt":"2026-01-01T00:00:00.000Z"}}"#
            ));
            body.push('\n');
        }
        body.push_str(&format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":{},"title":"x","titleSource":"auto"}}"#,
            serde_json::to_string(&cwd.display().to_string()).unwrap()
        ));
        body.push('\n');
        let path = dir.join(filename);
        std::fs::write(&path, body).unwrap();
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(mtime).unwrap();
    }

    #[test]
    fn sanitizes_a_home_relative_cwd() {
        let home = Path::new("/Users/yukimemi");
        assert_eq!(
            sanitize_cwd(home, Path::new("/Users/yukimemi/src/x/y")),
            "-src-x-y"
        );
        assert_eq!(sanitize_cwd(home, home), "-");
    }

    #[test]
    fn finds_a_repo_outside_home_by_scanning_every_project_directory() {
        // The real directory name `omp` would use for a cwd outside `$HOME`
        // is not confirmed (see `list`'s doc comment), so this deliberately
        // uses a directory name that does *not* follow the home-relative
        // scheme at all, to prove the match comes from the recorded `cwd`
        // cross-check rather than from guessing that name.
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let repo = t.path().join("elsewhere").join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let root = t.path().join("sessions");
        let dir = root.join("whatever-omp-actually-calls-it");
        write_session(
            &dir,
            "2026-01-01T00-00-00-000Z_outside.jsonl",
            Some("Outside home"),
            "outside",
            &repo,
            1,
        );
        let entries = list(&root, Some(&home), &repo);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "outside");
    }

    #[test]
    fn scanning_every_project_directory_still_excludes_locked_and_mismatched() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let repo = t.path().join("elsewhere").join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let root = t.path().join("sessions");
        let dir = root.join("some-guess");
        write_session(
            &dir,
            "2026-01-01T00-00-00-000Z_locked.jsonl",
            Some("Locked"),
            "locked",
            &repo,
            1,
        );
        std::fs::write(
            dir.join(".2026-01-01T00-00-00-000Z_locked.jsonl.lock.os"),
            "",
        )
        .unwrap();
        let other = t.path().join("elsewhere").join("other");
        std::fs::create_dir_all(&other).unwrap();
        write_session(
            &root.join("another-guess"),
            "2026-01-02T00-00-00-000Z_other.jsonl",
            Some("Other repo"),
            "other",
            &other,
            1,
        );
        // No $HOME at all: every project directory must still be scanned.
        assert!(list(&root, None, &repo).is_empty());
    }

    #[test]
    fn lists_newest_first_and_skips_locked_and_mismatched() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let repo = home.join("src").join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let root = t.path().join("sessions");
        let dir = root.join("-src-proj");

        write_session(
            &dir,
            "2026-01-01T00-00-00-000Z_older.jsonl",
            Some("Older one"),
            "older",
            &repo,
            120,
        );
        write_session(
            &dir,
            "2026-01-02T00-00-00-000Z_newer.jsonl",
            Some("Newer one"),
            "newer",
            &repo,
            10,
        );
        // Still open: a live lock file sits next to it.
        write_session(
            &dir,
            "2026-01-03T00-00-00-000Z_locked.jsonl",
            Some("Locked one"),
            "locked",
            &repo,
            1,
        );
        std::fs::write(
            dir.join(".2026-01-03T00-00-00-000Z_locked.jsonl.lock.os"),
            "",
        )
        .unwrap();
        // Belongs to a different checkout under the same sanitized dir name.
        let other = home.join("other");
        std::fs::create_dir_all(&other).unwrap();
        write_session(
            &dir,
            "2026-01-04T00-00-00-000Z_other.jsonl",
            Some("Other repo"),
            "other",
            &other,
            5,
        );

        let entries = list(&root, Some(&home), &repo);
        assert_eq!(
            entries
                .iter()
                .map(|e| e.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        assert_eq!(entries[0].title, "Newer one");
    }

    #[test]
    fn missing_title_record_falls_back_to_the_filename_id() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let repo = home.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let root = t.path().join("sessions");
        let dir = root.join("-proj");
        write_session(
            &dir,
            "2026-01-01T00-00-00-000Z_abc.jsonl",
            None,
            "abc",
            &repo,
            1,
        );
        let entries = list(&root, Some(&home), &repo);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "abc");
    }

    #[test]
    fn missing_sessions_directory_is_an_empty_list_not_an_error() {
        let t = tempfile::tempdir().unwrap();
        let entries = list(&t.path().join("nope"), None, Path::new("/wherever"));
        assert!(entries.is_empty());
    }

    #[test]
    fn ignores_non_jsonl_and_hidden_files() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let repo = home.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        let root = t.path().join("sessions");
        let dir = root.join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        // Companion file with no extension, as seen alongside real .jsonl files.
        std::fs::write(dir.join("2026-01-01T00-00-00-000Z_abc"), "").unwrap();
        write_session(
            &dir,
            "2026-01-01T00-00-00-000Z_abc.jsonl",
            Some("Real one"),
            "abc",
            &repo,
            1,
        );
        let entries = list(&root, Some(&home), &repo);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "abc");
    }
}
