//! Local repository discovery for the "new session" picker, modelled on
//! `magi repos`: walk each root for a ghq-layout checkout
//! (`<root>/<host>/<owner>/<repo>` holding a `.git`).
//!
//! Read-only. A missing or unreadable root/host/owner contributes nothing
//! instead of failing the scan, and checkouts reachable through several roots
//! (symlinks, nested roots) are listed once, by canonical path.

use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repo {
    /// `<owner>/<repo>`.
    pub name: String,
    /// Canonical path of the checkout, as a string: the exact text shown in
    /// the picker and matched against on submit, so both sides use this one
    /// representation (Windows verbatim prefix stripped).
    pub path: String,
}

pub fn scan(roots: &[PathBuf]) -> Vec<Repo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        for host in subdirs(root) {
            for owner in subdirs(&host) {
                for dir in subdirs(&owner) {
                    if !dir.join(".git").exists() {
                        continue;
                    }
                    let path = dir.canonicalize().unwrap_or_else(|_| dir.clone());
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    out.push(Repo {
                        name: format!("{}/{}", file_name(&owner), file_name(&dir)),
                        path: display_path(&path),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Canonical path as text, without the `\\?\` verbatim prefix Windows adds
/// (omp shows and compares plain `C:\...` paths).
fn display_path(path: &Path) -> String {
    let s = path.display().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => s,
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.is_dir())
        .collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Short-TTL cache of [`scan`], so polling the picker does not walk the disk
/// each time. A checkout deleted within the TTL stays listed; starting omp
/// there simply fails.
pub struct Cache {
    roots: Vec<PathBuf>,
    ttl: Duration,
    slot: Mutex<Option<(Instant, Arc<Vec<Repo>>)>>,
}

impl Cache {
    pub fn new(roots: Vec<PathBuf>, ttl: Duration) -> Self {
        Self {
            roots,
            ttl,
            slot: Mutex::new(None),
        }
    }

    pub fn has_roots(&self) -> bool {
        !self.roots.is_empty()
    }

    pub async fn get(&self) -> Arc<Vec<Repo>> {
        if let Some((at, repos)) = &*self.slot.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < self.ttl
        {
            return repos.clone();
        }
        let roots = self.roots.clone();
        let repos = Arc::new(
            tokio::task::spawn_blocking(move || scan(&roots))
                .await
                .unwrap_or_default(),
        );
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), repos.clone()));
        repos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn finds_ghq_layout_checkouts_sorted() {
        let t = tempfile::tempdir().unwrap();
        checkout(t.path(), "github.com/zed/b");
        let a = checkout(t.path(), "github.com/acme/a");
        std::fs::create_dir_all(t.path().join("github.com/acme/not-a-repo")).unwrap();
        std::fs::create_dir_all(t.path().join("github.com/too-shallow/.git")).unwrap();
        let repos = scan(&[t.path().to_path_buf()]);
        let names: Vec<_> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["acme/a", "zed/b"]);
        // The listed string is exactly what a lookup compares against.
        assert_eq!(repos[0].path, display_path(&a.canonicalize().unwrap()));
    }

    #[test]
    fn verbatim_prefix_is_stripped_only_for_drive_paths() {
        assert_eq!(display_path(Path::new(r"\\?\C:\src\r")), r"C:\src\r");
        assert_eq!(display_path(Path::new("/src/r")), "/src/r");
        #[cfg(windows)]
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\srv\share\r")),
            r"\\?\UNC\srv\share\r"
        );
    }

    #[test]
    fn stale_roots_are_skipped_not_fatal() {
        let t = tempfile::tempdir().unwrap();
        checkout(t.path(), "h/o/r");
        let repos = scan(&[t.path().join("gone"), t.path().to_path_buf()]);
        assert_eq!(repos.len(), 1);
        assert!(scan(&[]).is_empty());
    }

    #[test]
    fn duplicate_roots_list_once() {
        let t = tempfile::tempdir().unwrap();
        checkout(t.path(), "h/o/r");
        let repos = scan(&[
            t.path().to_path_buf(),
            t.path().to_path_buf(),
            t.path().join("."),
        ]);
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn symlinked_root_dedups() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        checkout(&real, "h/o/r");
        let link = t.path().join("link");
        #[cfg(unix)]
        let ok = std::os::unix::fs::symlink(&real, &link).is_ok();
        #[cfg(windows)]
        let ok = std::os::windows::fs::symlink_dir(&real, &link).is_ok();
        if !ok {
            return; // no symlink privilege on this machine
        }
        assert_eq!(scan(&[real, link]).len(), 1);
    }

    #[tokio::test]
    async fn cache_serves_within_ttl_then_rescans() {
        let t = tempfile::tempdir().unwrap();
        checkout(t.path(), "h/o/one");
        let cache = Cache::new(vec![t.path().to_path_buf()], Duration::from_millis(300));
        assert_eq!(cache.get().await.len(), 1);
        checkout(t.path(), "h/o/two");
        assert_eq!(cache.get().await.len(), 1);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(cache.get().await.len(), 2);
    }
}
