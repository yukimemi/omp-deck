//! GitHub repositories that are not checked out locally, offered by the
//! "new session" picker next to the ghq-layout checkouts of [`crate::repos`].
//!
//! Everything shelled out to `gh` sits behind [`GitHub`], so the cache, the
//! merge and the clone flow are testable without the network. A missing,
//! unauthenticated or offline `gh` degrades quietly to "no remote candidates".

use crate::repos::{self, Repo};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const HOST: &str = "github.com";
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

#[async_trait]
pub trait GitHub: Send + Sync {
    /// `owner/repo` of every repository the authenticated user can reach.
    async fn list(&self) -> Result<Vec<String>, String>;
    /// Clones `name` (`owner/repo`) into the (not yet existing) `dest`.
    async fn clone_repo(&self, name: &str, dest: &Path) -> Result<(), String>;
}

/// The `gh` CLI. Arguments are always passed as an array, never through a shell.
pub struct RealGitHub;

async fn run_gh(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Err("gh timed out".to_string()),
        Ok(Err(e)) => return Err(format!("cannot run gh: {e}")),
        Ok(Ok(out)) => out,
    };
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(tail(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// The last few hundred characters of `s`, trimmed: enough to show why `gh` failed.
fn tail(s: &str) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= 400 {
        s.to_string()
    } else {
        s.chars().skip(n - 400).collect()
    }
}

#[async_trait]
impl GitHub for RealGitHub {
    async fn list(&self) -> Result<Vec<String>, String> {
        let out = run_gh(
            &[
                "api",
                "--hostname",
                HOST,
                "--paginate",
                "user/repos?affiliation=owner,collaborator,organization_member&per_page=100",
                "--jq",
                ".[].full_name",
            ],
            LIST_TIMEOUT,
        )
        .await?;
        Ok(parse_list(&out))
    }

    async fn clone_repo(&self, name: &str, dest: &Path) -> Result<(), String> {
        let dest = dest.to_string_lossy();
        // Pinned to the listing host: a bare `owner/repo` would follow GH_HOST.
        let target = format!("{HOST}/{name}");
        run_gh(&["repo", "clone", &target, &dest], CLONE_TIMEOUT)
            .await
            .map(|_| ())
    }
}

/// One `owner/repo` per line; anything that fails validation is dropped.
pub fn parse_list(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| split_name(l).is_some())
        .map(String::from)
        .collect()
}

/// Splits a valid `owner/repo`; `None` if either half is not acceptable.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let (owner, repo) = name.split_once('/')?;
    (valid_owner(owner) && valid_repo(repo)).then_some((owner, repo))
}

/// GitHub logins: ASCII alphanumerics and inner hyphens, at most 39 long.
pub fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Repository names: ASCII alphanumerics, `.`, `_`, `-`; nothing that can
/// traverse a path, look like an option or collide with a Windows device name.
pub fn valid_repo(s: &str) -> bool {
    if s.is_empty()
        || s.len() > 100
        || s.starts_with('-')
        || s.ends_with('.')
        || s.to_ascii_lowercase().ends_with(".git")
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return false;
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|d| d.len() == 1 && d.as_bytes()[0].is_ascii_digit())
        });
    !reserved
}

/// A picker entry: a local checkout, or a remote-only repository (empty `path`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub remote: bool,
}

/// Local checkouts first-class, remote names only when no `github.com`
/// checkout of the same `owner/repo` (ASCII case-insensitive) exists locally.
/// Sorted by name.
pub fn merge(local: &[Repo], remote: &[String]) -> Vec<Entry> {
    let mut have: std::collections::HashSet<String> = local
        .iter()
        .filter(|r| r.host == HOST)
        .map(|r| r.name.to_ascii_lowercase())
        .collect();
    let mut out: Vec<Entry> = local
        .iter()
        .map(|r| Entry {
            name: r.name.clone(),
            path: r.path.clone(),
            remote: false,
        })
        .collect();
    for name in remote {
        if have.insert(name.to_ascii_lowercase()) {
            out.push(Entry {
                name: name.clone(),
                path: String::new(),
                remote: true,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    out
}

struct Slot {
    at: Instant,
    ttl: Duration,
    names: Arc<Vec<String>>,
}

struct Inner {
    gh: Arc<dyn GitHub>,
    ttl_ok: Duration,
    ttl_fail: Duration,
    slot: Mutex<Option<Slot>>,
    /// Single-flight: only one `gh` listing runs at a time.
    refresh: tokio::sync::Mutex<()>,
    clone_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    seq: AtomicU64,
}

/// Cached remote listing plus the clone flow. Cheap to clone.
#[derive(Clone)]
pub struct Remote {
    inner: Arc<Inner>,
}

impl Inner {
    fn fresh(&self) -> Option<Arc<Vec<String>>> {
        let g = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        g.as_ref()
            .filter(|s| s.at.elapsed() < s.ttl)
            .map(|s| s.names.clone())
    }

    async fn refresh(&self) -> Arc<Vec<String>> {
        let _one = self.refresh.lock().await;
        if let Some(names) = self.fresh() {
            return names;
        }
        let result = self.gh.list().await;
        let mut g = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        let (names, ttl) = match result {
            Ok(mut v) => {
                v.retain(|n| split_name(n).is_some());
                v.sort();
                v.dedup();
                (Arc::new(v), self.ttl_ok)
            }
            // Keep serving the last good list, but retry only after the short TTL.
            Err(_) => (
                g.as_ref().map(|s| s.names.clone()).unwrap_or_default(),
                self.ttl_fail,
            ),
        };
        *g = Some(Slot {
            at: Instant::now(),
            ttl,
            names: names.clone(),
        });
        names
    }
}

impl Remote {
    pub fn new(gh: Arc<dyn GitHub>, ttl_ok: Duration, ttl_fail: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                gh,
                ttl_ok,
                ttl_fail,
                slot: Mutex::new(None),
                refresh: tokio::sync::Mutex::new(()),
                clone_locks: Mutex::new(HashMap::new()),
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Remote names. The very first call waits for `gh`; afterwards a stale
    /// list is returned at once and refreshed in the background.
    pub async fn get(&self) -> Arc<Vec<String>> {
        if let Some(names) = self.inner.fresh() {
            return names;
        }
        let stale = self
            .inner
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|s| s.names.clone());
        match stale {
            Some(names) => {
                let inner = self.inner.clone();
                tokio::spawn(async move {
                    inner.refresh().await;
                });
                names
            }
            None => self.inner.refresh().await,
        }
    }

    /// Like [`Remote::get`], but a first listing slower than `wait` yields an
    /// empty list instead of stalling the caller; the listing keeps running in
    /// the background and a later call picks it up.
    pub async fn get_within(&self, wait: Duration) -> Arc<Vec<String>> {
        let this = self.clone();
        let task = tokio::spawn(async move { this.get().await });
        match tokio::time::timeout(wait, task).await {
            Ok(Ok(names)) => names,
            _ => Arc::default(),
        }
    }

    /// Makes `name` (`owner/repo`, already validated) available under the ghq
    /// layout and returns its canonical display path. Reuses an existing
    /// checkout in any root; otherwise clones into the first root.
    pub async fn ensure_checkout(
        &self,
        roots: &[PathBuf],
        name: &str,
        cache: &repos::Cache,
    ) -> Result<String, String> {
        let (owner, repo) = split_name(name).ok_or("invalid repository name")?;
        let first = roots.first().ok_or("no repository roots configured")?;
        let lock = {
            let mut m = self
                .inner
                .clone_locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            m.entry(name.to_ascii_lowercase()).or_default().clone()
        };
        let _one = lock.lock().await;
        let existing = |roots: &[PathBuf]| {
            roots
                .iter()
                .map(|r| r.join(HOST).join(owner).join(repo))
                .find(|d| d.join(".git").exists())
        };
        let dir = match existing(roots) {
            Some(d) => d,
            None => {
                let parent = first.join(HOST).join(owner);
                let target = parent.join(repo);
                std::fs::create_dir_all(&parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                let tmp = parent.join(format!(
                    ".{repo}.omp-deck-clone-{}-{}",
                    std::process::id(),
                    self.inner.seq.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&tmp);
                if let Err(e) = self.inner.gh.clone_repo(name, &tmp).await {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("clone failed: {e}"));
                }
                if target.exists() {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("{} already exists", target.display()));
                }
                if let Err(e) = std::fs::rename(&tmp, &target) {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Err(format!("cannot move the clone into place: {e}"));
                }
                cache.invalidate();
                target
            }
        };
        let canon = dir.canonicalize().map_err(|e| e.to_string())?;
        Ok(repos::display_path(&canon))
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Scripted `gh`: `list` result and clone outcome, with call counters.
    pub struct FakeGitHub {
        pub listing: Mutex<Result<Vec<String>, String>>,
        pub clone_error: Option<String>,
        pub lists: AtomicUsize,
        pub clones: Mutex<Vec<(String, PathBuf)>>,
    }

    impl FakeGitHub {
        pub fn new(listing: Result<Vec<String>, String>) -> Arc<Self> {
            Arc::new(Self {
                listing: Mutex::new(listing),
                clone_error: None,
                lists: AtomicUsize::new(0),
                clones: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl GitHub for FakeGitHub {
        async fn list(&self) -> Result<Vec<String>, String> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.listing.lock().unwrap().clone()
        }

        async fn clone_repo(&self, name: &str, dest: &Path) -> Result<(), String> {
            self.clones
                .lock()
                .unwrap()
                .push((name.to_string(), dest.to_path_buf()));
            if let Some(e) = &self.clone_error {
                return Err(e.clone());
            }
            std::fs::create_dir_all(dest.join(".git")).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeGitHub;
    use super::*;
    use std::sync::atomic::Ordering;

    fn local(host: &str, name: &str) -> Repo {
        Repo {
            name: name.to_string(),
            path: format!("/src/{host}/{name}"),
            host: host.to_string(),
        }
    }

    fn names(v: &[Entry]) -> Vec<(&str, bool)> {
        v.iter().map(|e| (e.name.as_str(), e.remote)).collect()
    }

    #[test]
    fn merge_dedupes_case_insensitively_and_prefers_local() {
        let l = [local("github.com", "Acme/Tool")];
        let r = ["acme/tool".to_string(), "acme/other".to_string()];
        let m = merge(&l, &r);
        assert_eq!(names(&m), [("Acme/Tool", false), ("acme/other", true)]);
        assert_eq!(m[0].path, "/src/github.com/Acme/Tool");
        assert_eq!(m[1].path, "");
    }

    #[test]
    fn merge_keeps_remote_when_only_another_host_has_it() {
        let l = [local("gitlab.com", "acme/tool")];
        let m = merge(&l, &["acme/tool".to_string()]);
        assert_eq!(names(&m), [("acme/tool", true), ("acme/tool", false)]);
    }

    #[test]
    fn merge_sorts_by_name_and_drops_duplicate_remote_lines() {
        let l = [local("github.com", "zed/b")];
        let r = ["acme/a".into(), "acme/a".into(), "mid/c".into()];
        let m = merge(&l, &r);
        assert_eq!(
            names(&m),
            [("acme/a", true), ("mid/c", true), ("zed/b", false)]
        );
    }

    #[test]
    fn name_validation() {
        for ok in [
            "acme/tool",
            "a-b/c.d_e",
            "A1/Repo-2",
            "o/.github",
            "o/x.git.y",
        ] {
            assert!(split_name(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "/",
            "a",
            "a/",
            "/b",
            "a/b/c",
            "../b",
            "a/..",
            "a/.",
            "-a/b",
            "a/-b",
            "a-/b",
            "a b/c",
            "a/b c",
            "a\\b/c",
            "a/b\\c",
            "a/b.git",
            "a/B.GIT",
            "a/b.",
            "a/CON",
            "a/nul.txt",
            "a/com1",
            "a/lpt9",
            "a/b\0",
            "ä/b",
            "a/ü",
            "a/--upload-pack=x",
            "a//b",
            "a/b:c",
        ] {
            assert!(split_name(bad).is_none(), "{bad:?}");
        }
        assert!(!valid_owner(&"a".repeat(40)));
        assert!(!valid_repo(&"a".repeat(101)));
        assert!(valid_repo("com10"));
    }

    #[test]
    fn parse_list_drops_invalid_lines() {
        let out = "acme/a\n  b/c  \n../x\n\n-o/p\nnot a name\nz/y.git\n";
        assert_eq!(parse_list(out), ["acme/a", "b/c"]);
    }

    #[tokio::test]
    async fn failure_is_cached_briefly_and_not_hammered() {
        let gh = FakeGitHub::new(Err("not logged in".into()));
        let r = Remote::new(gh.clone(), Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..3 {
            assert!(r.get().await.is_empty());
        }
        assert_eq!(gh.lists.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_first_loads_share_one_listing() {
        let gh = FakeGitHub::new(Ok(vec!["a/b".into(), "../bad".into()]));
        let r = Remote::new(gh.clone(), Duration::from_secs(60), Duration::from_secs(60));
        let (x, y, z) = tokio::join!(r.get(), r.get(), r.get());
        assert_eq!(*x, ["a/b"]);
        assert_eq!(x, y);
        assert_eq!(y, z);
        assert_eq!(gh.lists.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn get_within_does_not_wait_for_a_slow_first_listing() {
        let gh = FakeGitHub::new(Ok(vec!["a/b".into()]));
        let r = remote_with(gh.clone());
        assert!(r.get_within(Duration::from_millis(1)).await.is_empty());
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(*r.get_within(Duration::from_millis(1)).await, ["a/b"]);
        assert_eq!(gh.lists.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_list_is_served_while_refreshing_and_survives_failure() {
        let gh = FakeGitHub::new(Ok(vec!["a/b".into()]));
        let r = Remote::new(
            gh.clone(),
            Duration::from_millis(30),
            Duration::from_secs(60),
        );
        assert_eq!(*r.get().await, ["a/b"]);
        *gh.listing.lock().unwrap() = Err("offline".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*r.get().await, ["a/b"]); // stale, refresh spawned
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(*r.get().await, ["a/b"]); // failed refresh keeps the last good list
        assert_eq!(gh.lists.load(Ordering::SeqCst), 2);
    }

    fn remote_with(gh: Arc<FakeGitHub>) -> Remote {
        Remote::new(gh, Duration::from_secs(60), Duration::from_secs(60))
    }

    #[tokio::test]
    async fn ensure_checkout_clones_into_the_first_root_in_ghq_layout() {
        let t = tempfile::tempdir().unwrap();
        let roots = vec![t.path().to_path_buf()];
        let cache = repos::Cache::new(roots.clone(), Duration::from_secs(60));
        let gh = FakeGitHub::new(Ok(vec![]));
        let path = remote_with(gh.clone())
            .ensure_checkout(&roots, "acme/tool", &cache)
            .await
            .unwrap();
        let want = t
            .path()
            .join("github.com/acme/tool")
            .canonicalize()
            .unwrap();
        assert_eq!(path, repos::display_path(&want));
        assert!(want.join(".git").exists());
        assert_eq!(gh.clones.lock().unwrap().len(), 1);
        // No temp directory is left behind.
        let left: Vec<_> = std::fs::read_dir(t.path().join("github.com/acme"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(left.len(), 1);
    }

    #[tokio::test]
    async fn ensure_checkout_reuses_an_existing_checkout() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(b.join("github.com/acme/tool/.git")).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        let roots = vec![a, b.clone()];
        let cache = repos::Cache::new(roots.clone(), Duration::from_secs(60));
        let gh = FakeGitHub::new(Ok(vec![]));
        let path = remote_with(gh.clone())
            .ensure_checkout(&roots, "acme/tool", &cache)
            .await
            .unwrap();
        let want = b.join("github.com/acme/tool").canonicalize().unwrap();
        assert_eq!(path, repos::display_path(&want));
        assert!(gh.clones.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ensure_checkout_failure_cleans_up_and_reports() {
        let t = tempfile::tempdir().unwrap();
        let roots = vec![t.path().to_path_buf()];
        let cache = repos::Cache::new(roots.clone(), Duration::from_secs(60));
        let gh = Arc::new(FakeGitHub {
            clone_error: Some("boom".into()),
            ..Arc::try_unwrap(FakeGitHub::new(Ok(vec![]))).ok().unwrap()
        });
        let err = remote_with(gh)
            .ensure_checkout(&roots, "acme/tool", &cache)
            .await
            .unwrap_err();
        assert!(err.contains("boom"), "{err}");
        assert!(!t.path().join("github.com/acme/tool").exists());
    }

    #[tokio::test]
    async fn ensure_checkout_rejects_bad_names_and_missing_roots() {
        let cache = repos::Cache::new(vec![], Duration::from_secs(60));
        let r = remote_with(FakeGitHub::new(Ok(vec![])));
        let t = tempfile::tempdir().unwrap();
        assert!(
            r.ensure_checkout(&[t.path().to_path_buf()], "../x/y", &cache)
                .await
                .is_err()
        );
        assert!(r.ensure_checkout(&[], "a/b", &cache).await.is_err());
    }
}
