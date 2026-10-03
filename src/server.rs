//! HTTP layer. Nothing is cached and nothing is logged: link URLs are secrets
//! that only ever appear in a `Location` header, fetched fresh per request.

use crate::model::Host;
use crate::omp::{Access, Omp, OmpError};
use crate::remote;
use crate::repos;
use crate::sessions;
use crate::update::{RealUpdater, SelfUpdater};
use crate::view;
use axum::{
    Json, Router,
    extract::{FromRef, Path, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

type Shared = Arc<dyn Omp>;

/// What "new session" may offer: the scanned checkouts, the configured model
/// candidates, and where to look for a checkout's past sessions. Nothing
/// outside these is ever handed to `omp`.
pub struct Launcher {
    pub repos: repos::Cache,
    /// GitHub repositories not checked out locally, and the clone flow.
    pub remote: remote::Remote,
    pub models: Vec<String>,
    /// Root of `omp`'s own session log (`<dir>/sessions`), `None` if it could
    /// not be resolved -- then no repo ever has past sessions to offer.
    pub sessions_root: Option<PathBuf>,
    /// `$HOME`, used to decode the directory name `omp` derives from a cwd.
    pub home: Option<PathBuf>,
}

#[derive(Clone)]
pub struct AppState {
    pub omp: Shared,
    pub launcher: Arc<Launcher>,
    pub updater: Arc<dyn SelfUpdater>,
    pub restart_tx: watch::Sender<bool>,
    /// Guards against a second `/api/self-update` press while an install is
    /// already running -- an install replaces the on-disk binary and then
    /// restarts the process, so two overlapping ones would race each other.
    pub update_in_progress: Arc<AtomicBool>,
}

impl FromRef<AppState> for Shared {
    fn from_ref(state: &AppState) -> Self {
        state.omp.clone()
    }
}

/// Builds the router with the real `SelfUpdater` and a throwaway restart
/// channel nobody is watching -- for callers (tests, and any future use that
/// doesn't care about self-update) that don't need to observe or trigger it.
/// `omp-deck serve` uses [`router_with_updater`] directly so it can await the
/// restart signal itself.
pub fn router(omp: Arc<dyn Omp>, launcher: Arc<Launcher>) -> Router {
    let (restart_tx, _restart_rx) = watch::channel(false);
    router_with_updater(omp, launcher, Arc::new(RealUpdater::new()), restart_tx)
}

pub fn router_with_updater(
    omp: Arc<dyn Omp>,
    launcher: Arc<Launcher>,
    updater: Arc<dyn SelfUpdater>,
    restart_tx: watch::Sender<bool>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/hosts", get(api_hosts))
        .route("/api/repos", get(api_repos))
        .route("/api/models", get(api_models))
        .route("/api/sessions", post(api_start))
        .route("/api/sessions/{instance_id}", delete(api_stop))
        .route("/api/sessions/{instance_id}/resume", post(api_resume))
        .route("/api/repos/{path}/sessions", get(api_repo_sessions))
        .route(
            "/api/repos/{path}/sessions/resume",
            post(api_repo_sessions_resume),
        )
        .route("/api/update", get(api_update))
        .route("/api/self-update", post(api_self_update))
        .route("/go/{instance_id}/{kind}", get(go))
        .layer(middleware::map_response(harden))
        .with_state(AppState {
            omp,
            launcher,
            updater,
            restart_tx,
            update_in_progress: Arc::new(AtomicBool::new(false)),
        })
}

/// How long the picker waits for the first `gh` listing before showing local
/// checkouts alone.
const FIRST_LISTING_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

const NO_ROOTS_HINT: &str = "No repository roots configured. Add [repos] roots = [\"...\"] to      the omp-deck config file (see the README).";

impl FromRef<AppState> for Arc<Launcher> {
    fn from_ref(state: &AppState) -> Self {
        state.launcher.clone()
    }
}

async fn api_repos(State(l): State<Arc<Launcher>>) -> Response {
    let local = l.repos.get().await;
    // Remote candidates are only useful (cloneable) when a root is configured.
    // `pending`: the first listing is still running, so the page should ask again.
    let (remote, pending) = if l.repos.has_roots() {
        match l.remote.get_within(FIRST_LISTING_WAIT).await {
            Some(names) => (names, false),
            None => (Default::default(), true),
        }
    } else {
        (Default::default(), false)
    };
    let repos = remote::merge(&local, &remote);
    let hint = (!l.repos.has_roots()).then_some(NO_ROOTS_HINT);
    Json(json!({ "repos": repos, "hint": hint, "pending": pending })).into_response()
}

async fn api_models(State(l): State<Arc<Launcher>>) -> Response {
    Json(json!({ "models": l.models })).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    /// A checkout the scan listed. Exclusive with `remote`.
    #[serde(default)]
    path: Option<String>,
    /// `owner/repo` of a remote-only GitHub repository to clone first.
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

async fn api_start(
    State(omp): State<Shared>,
    State(l): State<Arc<Launcher>>,
    Json(req): Json<StartRequest>,
) -> Response {
    // Only ever start omp in a checkout the scan itself just listed (or one
    // cloned from a repository gh just listed), with a model the config names;
    // the values passed on are ours, not the request's.
    let model = match req.model.as_deref() {
        None | Some("") => None,
        Some(m) => match l.models.iter().find(|c| c.as_str() == m) {
            Some(c) => Some(c.as_str()),
            None => return plain(StatusCode::BAD_REQUEST, "not a configured model"),
        },
    };
    let cwd = match (req.path, req.remote) {
        (Some(path), None) => {
            let repos = l.repos.get().await;
            let Some(repo) = repos.iter().find(|r| r.path == path) else {
                return plain(StatusCode::NOT_FOUND, "not a known checkout");
            };
            repo.path.clone()
        }
        (None, Some(name)) => {
            if remote::split_name(&name).is_none() {
                return plain(StatusCode::BAD_REQUEST, "invalid repository name");
            }
            if !l.remote.get().await.contains(&name) {
                return plain(StatusCode::NOT_FOUND, "not a known remote repository");
            }
            if !l.repos.has_roots() {
                return plain(StatusCode::BAD_REQUEST, NO_ROOTS_HINT);
            }
            match l
                .remote
                .ensure_checkout(l.repos.roots(), &name, &l.repos)
                .await
            {
                Ok(path) => path,
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
                }
            }
        }
        _ => {
            return plain(
                StatusCode::BAD_REQUEST,
                "give exactly one of path and remote",
            );
        }
    };
    match omp.start(std::path::Path::new(&cwd), model).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn api_stop(State(omp): State<Shared>, Path(instance_id): Path<String>) -> Response {
    // Only ever kill a pid omp itself just reported for this instance id,
    // never one taken from the request.
    let hosts: Vec<Host> = match omp.list().await {
        Ok(h) => h,
        Err(e) => return plain(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let Some(host) = hosts.iter().find(|h| h.instance_id == instance_id) else {
        return plain(StatusCode::NOT_FOUND, "no such live omp session");
    };
    let Some(pid) = host.pid else {
        return plain(
            StatusCode::BAD_GATEWAY,
            "omp did not report a pid for this session",
        );
    };
    match omp.stop(pid).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "stopped": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Only characters `omp` session ids are known to use, and never a leading
/// `-` (which `--resume=<value>` already neutralizes, but a hand-checked
/// value is one less thing to trust from a subprocess's stdout).
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

async fn api_resume(State(omp): State<Shared>, Path(instance_id): Path<String>) -> Response {
    // Same rule as api_stop and go: only ever act on a pid/cwd/session id
    // omp itself just reported for this instance id, never the request's.
    let hosts: Vec<Host> = match omp.list().await {
        Ok(h) => h,
        Err(e) => return plain(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    let Some(host) = hosts.iter().find(|h| h.instance_id == instance_id) else {
        return plain(StatusCode::NOT_FOUND, "no such live omp session");
    };
    let Some(pid) = host.pid else {
        return plain(
            StatusCode::BAD_GATEWAY,
            "omp did not report a pid for this session",
        );
    };
    if host.cwd.is_empty() {
        return plain(
            StatusCode::BAD_GATEWAY,
            "omp did not report a cwd for this session",
        );
    }
    if !valid_session_id(&host.session_id) {
        return plain(
            StatusCode::BAD_GATEWAY,
            "omp did not report a usable session id",
        );
    }
    if let Err(e) = omp.stop(pid).await {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    match omp
        .resume(std::path::Path::new(&host.cwd), &host.session_id)
        .await
    {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "resumed": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("stopped, but could not restart: {e}") })),
        )
            .into_response(),
    }
}

async fn api_repo_sessions(State(l): State<Arc<Launcher>>, Path(path): Path<String>) -> Response {
    // Same rule as api_start: only ever scan for a checkout the scan itself
    // just listed, never a path taken straight from the request.
    let repos = l.repos.get().await;
    let Some(repo) = repos.iter().find(|r| r.path == path) else {
        return plain(StatusCode::NOT_FOUND, "not a known checkout");
    };
    let Some(root) = &l.sessions_root else {
        return Json(json!({ "sessions": [] })).into_response();
    };
    let entries = sessions::list(root, l.home.as_deref(), std::path::Path::new(&repo.path));
    Json(json!({ "sessions": entries })).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeSessionRequest {
    session_id: String,
}

async fn api_repo_sessions_resume(
    State(omp): State<Shared>,
    State(l): State<Arc<Launcher>>,
    Path(path): Path<String>,
    Json(req): Json<ResumeSessionRequest>,
) -> Response {
    let repos = l.repos.get().await;
    let Some(repo) = repos.iter().find(|r| r.path == path) else {
        return plain(StatusCode::NOT_FOUND, "not a known checkout");
    };
    let Some(root) = &l.sessions_root else {
        return plain(StatusCode::NOT_FOUND, "no resumable session with that id");
    };
    // Re-scan rather than trust anything cached: a session whose lock got
    // held (or that vanished) between listing and resuming must not be resumed.
    let entries = sessions::list(root, l.home.as_deref(), std::path::Path::new(&repo.path));
    if !entries.iter().any(|e| e.session_id == req.session_id) {
        return plain(StatusCode::NOT_FOUND, "no resumable session with that id");
    }
    match omp
        .resume(std::path::Path::new(&repo.path), &req.session_id)
        .await
    {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({ "resumed": true }))).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Checks for a newer release and, if there is one, installs it and hands
/// the dashboard over to a fresh successor process.
///
/// Order is the whole design, and it is enforced across two places: this
/// handler answers 202 and fires the install off in the background (never
/// blocking the response on it), so the reply reaches the caller before
/// anything about this process changes; installing then flips `restart_tx`,
/// which is what `main.rs::serve` is waiting on to stop `axum::serve`'s
/// graceful shutdown -- only once that future returns is the `TcpListener`
/// actually dropped, and only then does `main.rs` spawn the successor and
/// let this process exit. Spawning the successor any earlier would race it
/// against this process for the port.
async fn api_update(State(state): State<AppState>) -> Response {
    if state.updater.disabled() {
        return Json(json!({ "available": false })).into_response();
    }
    match state.updater.known_update().await {
        Ok(Some(latest)) => Json(json!({ "available": true, "to": latest.tag_name })),
        Ok(None) => Json(json!({ "available": false })),
        // Unknown is not available: a failed check hides the button.
        Err(e) => {
            eprintln!("omp-deck: update check failed: {e}");
            Json(json!({ "available": false }))
        }
    }
    .into_response()
}

async fn api_self_update(State(state): State<AppState>) -> Response {
    if state.updater.disabled() {
        return plain(
            StatusCode::FORBIDDEN,
            "self-update disabled (OMP_DECK_NO_AUTOUPDATE)",
        );
    }
    if state.update_in_progress.swap(true, Ordering::SeqCst) {
        return plain(StatusCode::CONFLICT, "self-update already in progress");
    }
    let latest = match state.updater.newer_release().await {
        Ok(latest) => latest,
        Err(e) => {
            state.update_in_progress.store(false, Ordering::SeqCst);
            return plain(StatusCode::BAD_GATEWAY, &e);
        }
    };
    let Some(latest) = latest else {
        // Already up to date: restarting now would only drop every open
        // connection for nothing, so this is the end of the road.
        state.update_in_progress.store(false, Ordering::SeqCst);
        return Json(json!({ "updated": false })).into_response();
    };
    let updater = state.updater.clone();
    let restart_tx = state.restart_tx.clone();
    let in_progress = state.update_in_progress.clone();
    tokio::spawn(async move {
        if let Err(e) = updater.install().await {
            eprintln!("omp-deck: self-update failed: {e}");
            in_progress.store(false, Ordering::SeqCst);
            return;
        }
        let _ = restart_tx.send(true);
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "updated": true, "tag": latest.tag_name })),
    )
        .into_response()
}

async fn harden(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    res
}

async fn index(State(omp): State<Shared>) -> Response {
    match omp.list().await {
        Ok(hosts) => Html(view::render_page(&hosts, crate::now_ms())).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Html(view::render_error(&e.to_string())),
        )
            .into_response(),
    }
}

async fn api_hosts(State(omp): State<Shared>) -> Response {
    match omp.list().await {
        Ok(hosts) => Json(json!({ "version": 1, "hosts": hosts })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn plain(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

async fn go(
    State(omp): State<Shared>,
    Path((instance_id, kind)): Path<(String, String)>,
) -> Response {
    let access = match kind.as_str() {
        "view" => Access::View,
        "control" => Access::Control,
        _ => return plain(StatusCode::NOT_FOUND, "unknown link kind"),
    };
    let hosts: Vec<Host> = match omp.list().await {
        Ok(h) => h,
        Err(e) => return plain(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    // Only ever hand omp a value that omp itself just reported.
    let Some(host) = hosts.iter().find(|h| h.instance_id == instance_id) else {
        return plain(StatusCode::NOT_FOUND, "no such live omp session");
    };
    let url = match omp.link(&host.instance_id, access).await {
        Ok(url) => url,
        Err(e @ (OmpError::Exit { .. } | OmpError::Parse(_))) => {
            // The session may have restarted between list and link.
            return plain(StatusCode::BAD_GATEWAY, &format!("could not get link: {e}"));
        }
        Err(e) => return plain(StatusCode::BAD_GATEWAY, &e.to_string()),
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return plain(StatusCode::BAD_GATEWAY, "omp returned an unexpected link");
    }
    match HeaderValue::from_str(&url) {
        Ok(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
        Err(_) => plain(StatusCode::BAD_GATEWAY, "omp returned an unusable link"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_hosts;
    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use kaishin::LatestRelease;
    use parking_lot::Mutex;
    use tower::ServiceExt;

    const FIXTURE: &str = include_str!("../tests/fixtures/hosts.json");
    const SECRET: &str = "https://my.omp.sh/#secret-room-key";

    struct FakeOmp {
        hosts: Result<Vec<Host>, String>,
        links: Mutex<Vec<(String, Access)>>,
        starts: Mutex<Vec<(std::path::PathBuf, Option<String>)>>,
        start_error: Option<String>,
        stops: Mutex<Vec<u32>>,
        stop_error: Option<String>,
        resumes: Mutex<Vec<(std::path::PathBuf, String)>>,
        resume_error: Option<String>,
        /// Records "stop"/"resume" in call order, so tests can pin that a
        /// resume request stops the old process before starting a new one.
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeOmp {
        fn new(hosts: Result<Vec<Host>, String>) -> Arc<Self> {
            Arc::new(Self {
                hosts,
                links: Mutex::new(Vec::new()),
                starts: Mutex::new(Vec::new()),
                start_error: None,
                stops: Mutex::new(Vec::new()),
                stop_error: None,
                resumes: Mutex::new(Vec::new()),
                resume_error: None,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Omp for FakeOmp {
        async fn list(&self) -> Result<Vec<Host>, OmpError> {
            self.hosts.clone().map_err(|stderr| OmpError::Exit {
                code: Some(1),
                stderr,
            })
        }
        async fn link(&self, instance_id: &str, access: Access) -> Result<String, OmpError> {
            self.links.lock().push((instance_id.to_string(), access));
            Ok(SECRET.to_string())
        }
        async fn start(&self, cwd: &std::path::Path, model: Option<&str>) -> Result<(), OmpError> {
            if let Some(stderr) = &self.start_error {
                return Err(OmpError::Exit {
                    code: Some(3),
                    stderr: stderr.clone(),
                });
            }
            self.starts
                .lock()
                .push((cwd.to_path_buf(), model.map(String::from)));
            Ok(())
        }
        async fn stop(&self, pid: u32) -> Result<(), OmpError> {
            self.calls.lock().push("stop");
            if let Some(stderr) = &self.stop_error {
                return Err(OmpError::Exit {
                    code: Some(4),
                    stderr: stderr.clone(),
                });
            }
            self.stops.lock().push(pid);
            Ok(())
        }
        async fn resume(&self, cwd: &std::path::Path, session_id: &str) -> Result<(), OmpError> {
            self.calls.lock().push("resume");
            if let Some(stderr) = &self.resume_error {
                return Err(OmpError::Exit {
                    code: Some(5),
                    stderr: stderr.clone(),
                });
            }
            self.resumes
                .lock()
                .push((cwd.to_path_buf(), session_id.to_string()));
            Ok(())
        }
    }

    struct FakeUpdater {
        disabled: bool,
        newer: Result<Option<LatestRelease>, String>,
        install_result: Result<(), String>,
        known: Result<Option<LatestRelease>, String>,
        known_calls: Mutex<u32>,
        newer_calls: Mutex<u32>,
        installs: Mutex<u32>,
    }

    impl FakeUpdater {
        fn new(disabled: bool, newer: Result<Option<LatestRelease>, String>) -> Arc<Self> {
            Arc::new(Self {
                disabled,
                known: newer.clone(),
                newer,
                known_calls: Mutex::new(0),
                install_result: Ok(()),
                newer_calls: Mutex::new(0),
                installs: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl SelfUpdater for FakeUpdater {
        fn disabled(&self) -> bool {
            self.disabled
        }
        async fn newer_release(&self) -> Result<Option<LatestRelease>, String> {
            *self.newer_calls.lock() += 1;
            self.newer.clone()
        }
        async fn known_update(&self) -> Result<Option<LatestRelease>, String> {
            *self.known_calls.lock() += 1;
            self.known.clone()
        }
        async fn install(&self) -> Result<(), String> {
            *self.installs.lock() += 1;
            self.install_result.clone()
        }
    }

    fn release(tag: &str) -> LatestRelease {
        LatestRelease {
            tag_name: tag.to_string(),
            html_url: String::new(),
        }
    }

    fn remote_of(gh: Arc<remote::fake::FakeGitHub>) -> remote::Remote {
        let d = std::time::Duration::from_secs(60);
        remote::Remote::new(gh, d, d)
    }

    fn no_remote() -> remote::Remote {
        remote_of(remote::fake::FakeGitHub::new(Err("no gh".into())))
    }

    fn launcher(roots: Vec<std::path::PathBuf>, models: &[&str]) -> Arc<Launcher> {
        Arc::new(Launcher {
            repos: repos::Cache::new(roots, std::time::Duration::from_secs(30)),
            remote: no_remote(),
            models: models.iter().map(|m| m.to_string()).collect(),
            sessions_root: None,
            home: None,
        })
    }

    /// A launcher whose sessions root/home are wired to a tempdir sessions
    /// layout, for the past-sessions endpoints.
    fn launcher_with_sessions(
        roots: Vec<std::path::PathBuf>,
        sessions_root: std::path::PathBuf,
        home: std::path::PathBuf,
    ) -> Arc<Launcher> {
        Arc::new(Launcher {
            repos: repos::Cache::new(roots, std::time::Duration::from_secs(30)),
            remote: no_remote(),
            models: Vec::new(),
            sessions_root: Some(sessions_root),
            home: Some(home),
        })
    }

    async fn get_path(
        omp: &Arc<FakeOmp>,
        path: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let app = router(omp.clone(), launcher(Vec::new(), &[]));
        let res = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = res.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn fixture() -> Arc<FakeOmp> {
        FakeOmp::new(Ok(parse_hosts(FIXTURE).unwrap()))
    }

    #[tokio::test]
    async fn index_and_api_never_contain_link_urls() {
        let omp = fixture();
        for path in ["/", "/api/hosts"] {
            let (status, _, body) = get_path(&omp, path).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!body.contains(SECRET), "{path}");
            assert!(!body.contains("my.omp.sh"), "{path}");
        }
        assert!(omp.links.lock().is_empty());
    }

    #[tokio::test]
    async fn api_lists_hosts_as_json() {
        let (_, headers, body) = get_path(&fixture(), "/api/hosts").await;
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["hosts"].as_array().unwrap().len(), 2);
        assert_eq!(v["hosts"][0]["instanceId"], "inst-aaa");
    }

    #[tokio::test]
    async fn responses_are_not_cacheable_and_send_no_referrer() {
        let omp = fixture();
        for path in ["/", "/api/hosts", "/go/inst-aaa/view", "/go/nope/view"] {
            let (_, headers, _) = get_path(&omp, path).await;
            assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{path}");
            assert_eq!(headers["referrer-policy"], "no-referrer", "{path}");
        }
    }

    #[tokio::test]
    async fn index_escapes_hostile_strings() {
        let (_, _, body) = get_path(&fixture(), "/").await;
        assert!(!body.contains("<script>alert(1)"));
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[tokio::test]
    async fn go_redirects_only_for_listed_instances() {
        let omp = fixture();
        let (status, headers, body) = get_path(&omp, "/go/inst-bbb/view").await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(headers[header::LOCATION], SECRET);
        assert!(!body.contains(SECRET));
        let (status, _, _) = get_path(&omp, "/go/inst-aaa/control").await;
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(
            *omp.links.lock(),
            vec![
                ("inst-bbb".to_string(), Access::View),
                ("inst-aaa".to_string(), Access::Control)
            ]
        );
    }

    #[tokio::test]
    async fn go_rejects_unknown_ids_and_kinds_without_calling_link() {
        let omp = fixture();
        for path in [
            "/go/unknown/view",
            "/go/%2D%2Dhelp/control",
            "/go/inst-aaa/admin",
        ] {
            let (status, _, _) = get_path(&omp, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
        assert!(omp.links.lock().is_empty());
    }

    async fn delete_path(omp: &Arc<FakeOmp>, path: &str) -> (StatusCode, String) {
        let req = Request::delete(path).body(Body::empty()).unwrap();
        call(router(omp.clone(), launcher(Vec::new(), &[])), req).await
    }

    #[tokio::test]
    async fn stop_kills_the_pid_of_a_listed_instance() {
        let omp = fixture();
        let (status, body) = delete_path(&omp, "/api/sessions/inst-aaa").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("\"stopped\":true"));
        assert_eq!(*omp.stops.lock(), vec![4242]);
    }

    #[tokio::test]
    async fn stop_rejects_unknown_instances_without_calling_stop() {
        let omp = fixture();
        let (status, _) = delete_path(&omp, "/api/sessions/unknown").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(omp.stops.lock().is_empty());
    }

    #[tokio::test]
    async fn stop_502s_when_the_host_has_no_pid() {
        let mut hosts = parse_hosts(FIXTURE).unwrap();
        hosts[0].pid = None;
        let omp = FakeOmp::new(Ok(hosts));
        let (status, body) = delete_path(&omp, "/api/sessions/inst-aaa").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("pid"));
        assert!(omp.stops.lock().is_empty());
    }

    #[tokio::test]
    async fn stop_failure_is_502_with_the_error() {
        let omp = Arc::new(FakeOmp {
            hosts: Ok(parse_hosts(FIXTURE).unwrap()),
            links: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            start_error: None,
            stops: Mutex::new(Vec::new()),
            stop_error: Some("access denied".into()),
            resumes: Mutex::new(Vec::new()),
            resume_error: None,
            calls: Mutex::new(Vec::new()),
        });
        let (status, body) = delete_path(&omp, "/api/sessions/inst-aaa").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("access denied"));
    }

    async fn resume_path(omp: &Arc<FakeOmp>, path: &str) -> (StatusCode, String) {
        let req = Request::post(path).body(Body::empty()).unwrap();
        call(router(omp.clone(), launcher(Vec::new(), &[])), req).await
    }

    #[tokio::test]
    async fn resume_stops_then_restarts_the_listed_instance() {
        let omp = fixture();
        let (status, body) = resume_path(&omp, "/api/sessions/inst-aaa/resume").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("\"resumed\":true"));
        assert_eq!(*omp.stops.lock(), vec![4242]);
        assert_eq!(
            *omp.resumes.lock(),
            vec![(
                std::path::PathBuf::from(
                    "C:\\Users\\yukimemi\\src\\github.com\\yukimemi\\omp-deck"
                ),
                "sess-1".to_string()
            )]
        );
        assert_eq!(*omp.calls.lock(), vec!["stop", "resume"]);
    }

    #[tokio::test]
    async fn resume_rejects_unknown_instances_without_calling_stop_or_resume() {
        let omp = fixture();
        let (status, _) = resume_path(&omp, "/api/sessions/unknown/resume").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(omp.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn resume_502s_without_calling_stop_when_the_host_is_missing_pid_cwd_or_session_id() {
        for (mutate, hint) in [
            ((|h: &mut Host| h.pid = None) as fn(&mut Host), "pid"),
            (
                (|h: &mut Host| h.cwd = String::new()) as fn(&mut Host),
                "cwd",
            ),
            (
                (|h: &mut Host| h.session_id = String::new()) as fn(&mut Host),
                "session id",
            ),
            (
                (|h: &mut Host| h.session_id = "-x".to_string()) as fn(&mut Host),
                "session id",
            ),
            (
                (|h: &mut Host| h.session_id = "sess;rm -rf".to_string()) as fn(&mut Host),
                "session id",
            ),
        ] {
            let mut hosts = parse_hosts(FIXTURE).unwrap();
            mutate(&mut hosts[0]);
            let omp = FakeOmp::new(Ok(hosts));
            let (status, body) = resume_path(&omp, "/api/sessions/inst-aaa/resume").await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{hint}");
            assert!(body.contains(hint), "{hint}: {body}");
            assert!(omp.calls.lock().is_empty(), "{hint}");
        }
    }

    #[tokio::test]
    async fn resume_does_not_start_a_new_process_when_stop_fails() {
        let omp = Arc::new(FakeOmp {
            hosts: Ok(parse_hosts(FIXTURE).unwrap()),
            links: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            start_error: None,
            stops: Mutex::new(Vec::new()),
            stop_error: Some("access denied".into()),
            resumes: Mutex::new(Vec::new()),
            resume_error: None,
            calls: Mutex::new(Vec::new()),
        });
        let (status, body) = resume_path(&omp, "/api/sessions/inst-aaa/resume").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("access denied"));
        assert!(omp.resumes.lock().is_empty());
        assert_eq!(*omp.calls.lock(), vec!["stop"]);
    }

    #[tokio::test]
    async fn resume_502s_mentioning_stopped_when_restart_fails() {
        let omp = Arc::new(FakeOmp {
            hosts: Ok(parse_hosts(FIXTURE).unwrap()),
            links: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            start_error: None,
            stops: Mutex::new(Vec::new()),
            stop_error: None,
            resumes: Mutex::new(Vec::new()),
            resume_error: Some("no console".into()),
            calls: Mutex::new(Vec::new()),
        });
        let (status, body) = resume_path(&omp, "/api/sessions/inst-aaa/resume").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("stopped"));
        assert!(body.contains("no console"));
        assert_eq!(*omp.calls.lock(), vec!["stop", "resume"]);
    }

    #[tokio::test]
    async fn resume_rejects_get() {
        let omp = fixture();
        let req = Request::get("/api/sessions/inst-aaa/resume")
            .body(Body::empty())
            .unwrap();
        let (status, _) = call(router(omp.clone(), launcher(Vec::new(), &[])), req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(omp.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn omp_failure_is_502_with_error_and_never_an_empty_list() {
        let omp = FakeOmp::new(Err("kaboom <x>".into()));
        let (status, _, body) = get_path(&omp, "/").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("kaboom &lt;x&gt;"));
        assert!(!body.contains("no live omp sessions"));
        let (status, _, body) = get_path(&omp, "/api/hosts").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("kaboom"));
        let (status, _, _) = get_path(&omp, "/go/inst-aaa/view").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn empty_list_is_ok_with_hint() {
        let omp = FakeOmp::new(Ok(Vec::new()));
        let (status, _, body) = get_path(&omp, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("no live omp sessions"));
        let (status, _, body) = get_path(&omp, "/api/hosts").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"hosts\":[]"));
    }

    /// A tempdir holding one ghq-layout checkout, plus the path the scan reports.
    fn one_checkout() -> (tempfile::TempDir, Arc<Launcher>, String) {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("h/o/r/.git")).unwrap();
        let l = launcher(vec![t.path().to_path_buf()], &["opus", "gpt-5.2"]);
        let path = repos::scan(&[t.path().to_path_buf()])[0].path.clone();
        (t, l, path)
    }

    async fn call(app: Router, req: Request<Body>) -> (StatusCode, String) {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn post_session(
        omp: &Arc<FakeOmp>,
        l: &Arc<Launcher>,
        body: serde_json::Value,
    ) -> (StatusCode, String) {
        let req = Request::post("/api/sessions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        call(router(omp.clone(), l.clone()), req).await
    }

    #[tokio::test]
    async fn start_runs_omp_in_a_scanned_checkout_with_a_candidate_model() {
        let (_t, l, path) = one_checkout();
        let omp = fixture();
        let (status, body) = post_session(&omp, &l, json!({ "path": path, "model": "opus" })).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("\"started\":true"));
        assert_eq!(
            *omp.starts.lock(),
            vec![(std::path::PathBuf::from(&path), Some("opus".to_string()))]
        );
    }

    #[tokio::test]
    async fn start_without_or_with_empty_model_passes_no_model() {
        let (_t, l, path) = one_checkout();
        let omp = fixture();
        for body in [
            json!({ "path": path }),
            json!({ "path": path, "model": "" }),
            json!({ "path": path, "model": null }),
        ] {
            assert_eq!(post_session(&omp, &l, body).await.0, StatusCode::ACCEPTED);
        }
        let starts = omp.starts.lock();
        assert_eq!(starts.len(), 3);
        assert!(starts.iter().all(|(_, m)| m.is_none()));
    }

    #[tokio::test]
    async fn start_rejects_paths_the_scan_did_not_list() {
        let (t, l, path) = one_checkout();
        let omp = fixture();
        let elsewhere = tempfile::tempdir().unwrap();
        for bad in [
            elsewhere.path().display().to_string(),
            t.path().display().to_string(),
            format!("{path}{}..", std::path::MAIN_SEPARATOR),
            format!("{path} "),
            "--help".to_string(),
            String::new(),
        ] {
            let (status, _) = post_session(&omp, &l, json!({ "path": bad })).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{bad:?}");
        }
        assert!(omp.starts.lock().is_empty());
    }

    fn remote_launcher(root: &std::path::Path, gh: Arc<remote::fake::FakeGitHub>) -> Arc<Launcher> {
        Arc::new(Launcher {
            repos: repos::Cache::new(vec![root.to_path_buf()], std::time::Duration::from_secs(30)),
            remote: remote_of(gh),
            models: Vec::new(),
            sessions_root: None,
            home: None,
        })
    }

    #[tokio::test]
    async fn repos_lists_remote_only_entries_once_and_marks_them() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("github.com/o/r/.git")).unwrap();
        let gh = remote::fake::FakeGitHub::new(Ok(vec!["O/R".into(), "o/new".into()]));
        let l = remote_launcher(t.path(), gh);
        let res = call(
            router(FakeOmp::new(Ok(Vec::new())), l),
            Request::get("/api/repos").body(Body::empty()).unwrap(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&res.1).unwrap();
        assert_eq!(v["repos"][0]["name"], "o/new");
        assert_eq!(v["repos"][0]["remote"], true);
        assert_eq!(v["repos"][0]["path"], "");
        assert_eq!(v["repos"][1]["name"], "o/r");
        assert_eq!(v["repos"][1]["remote"], false);
        assert_eq!(v["repos"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn remote_start_clones_then_starts_in_the_ghq_path() {
        let t = tempfile::tempdir().unwrap();
        let gh = remote::fake::FakeGitHub::new(Ok(vec!["acme/tool".into()]));
        let l = remote_launcher(t.path(), gh.clone());
        let omp = FakeOmp::new(Ok(Vec::new()));
        let (status, body) = post_session(&omp, &l, json!({ "remote": "acme/tool" })).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let want = t
            .path()
            .join("github.com/acme/tool")
            .canonicalize()
            .unwrap();
        assert_eq!(gh.clones.lock().unwrap().len(), 1);
        assert_eq!(*omp.starts.lock(), vec![(want, None)]);
    }

    #[tokio::test]
    async fn remote_start_of_an_unknown_repo_is_404_without_cloning() {
        let t = tempfile::tempdir().unwrap();
        let gh = remote::fake::FakeGitHub::new(Ok(vec!["acme/tool".into()]));
        let l = remote_launcher(t.path(), gh.clone());
        let omp = FakeOmp::new(Ok(Vec::new()));
        let (status, _) = post_session(&omp, &l, json!({ "remote": "acme/other" })).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = post_session(&omp, &l, json!({ "remote": "../x/y" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(gh.clones.lock().unwrap().is_empty());
        assert!(omp.starts.lock().is_empty());
    }

    #[tokio::test]
    async fn remote_clone_failure_is_502_and_starts_nothing() {
        let t = tempfile::tempdir().unwrap();
        let gh = Arc::new(remote::fake::FakeGitHub {
            clone_error: Some("permission denied".into()),
            ..Arc::try_unwrap(remote::fake::FakeGitHub::new(Ok(vec!["acme/tool".into()])))
                .ok()
                .unwrap()
        });
        let l = remote_launcher(t.path(), gh);
        let omp = FakeOmp::new(Ok(Vec::new()));
        let (status, body) = post_session(&omp, &l, json!({ "remote": "acme/tool" })).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("permission denied"), "{body}");
        assert!(omp.starts.lock().is_empty());
    }

    #[tokio::test]
    async fn start_needs_exactly_one_of_path_and_remote() {
        let (_t, l, path) = one_checkout();
        let omp = FakeOmp::new(Ok(Vec::new()));
        for body in [json!({}), json!({ "path": path, "remote": "o/r" })] {
            let (status, _) = post_session(&omp, &l, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        assert!(omp.starts.lock().is_empty());
    }

    #[tokio::test]
    async fn start_rejects_models_that_are_not_candidates() {
        let (t, l, path) = one_checkout();
        let omp = fixture();
        for bad in ["OPUS", "opus ", "--yolo", "opus;calc", "gpt"] {
            let (status, _) = post_session(&omp, &l, json!({ "path": path, "model": bad })).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        // No candidates configured: any non-empty model is refused too.
        let bare = launcher(vec![t.path().to_path_buf()], &[]);
        let (status, _) = post_session(&omp, &bare, json!({ "path": path, "model": "opus" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(omp.starts.lock().is_empty());
    }

    #[tokio::test]
    async fn start_failure_is_502_with_the_error() {
        let (_t, l, path) = one_checkout();
        let omp = Arc::new(FakeOmp {
            hosts: Ok(Vec::new()),
            links: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            start_error: Some("no console".into()),
            stops: Mutex::new(Vec::new()),
            stop_error: None,
            resumes: Mutex::new(Vec::new()),
            resume_error: None,
            calls: Mutex::new(Vec::new()),
        });
        let (status, body) = post_session(&omp, &l, json!({ "path": path })).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("no console"));
    }

    #[tokio::test]
    async fn start_rejects_unknown_fields_and_malformed_bodies() {
        let (_t, l, path) = one_checkout();
        let omp = fixture();
        let (status, _) = post_session(&omp, &l, json!({ "path": path, "args": ["--x"] })).await;
        assert!(status.is_client_error());
        let (status, _) = post_session(&omp, &l, json!({ "model": "opus" })).await;
        assert!(status.is_client_error());
        assert!(omp.starts.lock().is_empty());
    }

    #[tokio::test]
    async fn repos_and_models_endpoints() {
        let (_t, l, path) = one_checkout();
        let omp = fixture();
        let get = |app: Router, p: &'static str| async move {
            let (_, body) = call(app, Request::get(p).body(Body::empty()).unwrap()).await;
            serde_json::from_str::<serde_json::Value>(&body).unwrap()
        };
        let v = get(router(omp.clone(), l.clone()), "/api/repos").await;
        assert_eq!(v["repos"][0]["name"], "o/r");
        assert_eq!(v["repos"][0]["path"], path.as_str());
        assert!(v["hint"].is_null());
        let v = get(router(omp.clone(), l.clone()), "/api/models").await;
        assert_eq!(v["models"], json!(["opus", "gpt-5.2"]));
        // Unconfigured: empty picker with a hint, no models.
        let bare = || router(omp.clone(), launcher(Vec::new(), &[]));
        let v = get(bare(), "/api/repos").await;
        assert_eq!(v["repos"], json!([]));
        assert!(v["hint"].as_str().unwrap().contains("roots"));
        assert_eq!(get(bare(), "/api/models").await["models"], json!([]));
    }

    /// Percent-encodes a path the way `encodeURIComponent` does for the one
    /// character that matters here: the path separator, which must survive
    /// as a single route segment.
    fn encode_path(path: &str) -> String {
        path.replace('\\', "%5C").replace('/', "%2F")
    }

    fn write_session_fixture(
        dir: &std::path::Path,
        filename: &str,
        title: &str,
        id: &str,
        cwd: &std::path::Path,
        age_secs: u64,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        let body = format!(
            "{{\"type\":\"title\",\"v\":1,\"title\":{title},\"source\":\"auto\",\"updatedAt\":\"2026-01-01T00:00:00.000Z\"}}\n{{\"type\":\"session\",\"version\":3,\"id\":{id},\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":{cwd},\"title\":\"x\",\"titleSource\":\"auto\"}}\n",
            title = serde_json::to_string(title).unwrap(),
            id = serde_json::to_string(id).unwrap(),
            cwd = serde_json::to_string(&cwd.display().to_string()).unwrap(),
        );
        let path = dir.join(filename);
        std::fs::write(&path, body).unwrap();
        let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// A ghq-layout checkout under a tempdir "home", plus a sessions root
    /// holding two resumable sessions and one still-locked (live) one.
    fn sessions_layout() -> (tempfile::TempDir, Arc<Launcher>, String, std::fs::File) {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("h/o/r/.git")).unwrap();
        let home = t.path().canonicalize().unwrap();
        let path = repos::scan(&[t.path().to_path_buf()])[0].path.clone();
        let sessions_root = t.path().join("sessions");
        let dir = sessions_root.join("-h-o-r");
        let repo_path = std::path::Path::new(&path);
        write_session_fixture(
            &dir,
            "2026-01-01T00-00-00-000Z_older.jsonl",
            "Older session",
            "older",
            repo_path,
            120,
        );
        write_session_fixture(
            &dir,
            "2026-01-02T00-00-00-000Z_newer.jsonl",
            "Newer session",
            "newer",
            repo_path,
            10,
        );
        write_session_fixture(
            &dir,
            "2026-01-03T00-00-00-000Z_locked.jsonl",
            "Locked session",
            "locked",
            repo_path,
            1,
        );
        // Held for as long as the caller keeps the returned handle, like a
        // running omp; a bare leftover file would count as stale.
        let lock_path = dir.join(".2026-01-03T00-00-00-000Z_locked.jsonl.lock.os");
        std::fs::write(&lock_path, "").unwrap();
        let held = std::fs::File::open(&lock_path).unwrap();
        held.try_lock().unwrap();
        // A leftover lock file nobody holds must not hide its session.
        write_session_fixture(
            &dir,
            "2026-01-04T00-00-00-000Z_stale.jsonl",
            "Stale lock session",
            "stale",
            repo_path,
            5,
        );
        std::fs::write(
            dir.join(".2026-01-04T00-00-00-000Z_stale.jsonl.lock.os"),
            "",
        )
        .unwrap();
        let l = launcher_with_sessions(vec![t.path().to_path_buf()], sessions_root, home);
        (t, l, path, held)
    }

    #[tokio::test]
    async fn repo_sessions_lists_resumable_sessions_excluding_locked_ones() {
        let (_t, l, path, _lock) = sessions_layout();
        let omp = fixture();
        let (status, _, body) = {
            let app = router(omp.clone(), l.clone());
            let req = Request::get(format!("/api/repos/{}/sessions", encode_path(&path)))
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            let (parts, body) = res.into_parts();
            let bytes = to_bytes(body, usize::MAX).await.unwrap();
            (
                parts.status,
                parts.headers,
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        };
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let ids: Vec<&str> = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["stale", "newer", "older"]);
        assert_eq!(v["sessions"][1]["title"], "Newer session");
    }

    #[tokio::test]
    async fn repo_sessions_rejects_unknown_checkout_paths() {
        let (_t, l, _path, _lock) = sessions_layout();
        let omp = fixture();
        let (status, _) = call(
            router(omp.clone(), l.clone()),
            Request::get("/api/repos/%2Fnowhere/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn repo_sessions_is_an_empty_list_without_a_configured_sessions_root() {
        let (_t, l, path) = one_checkout();
        let omp = fixture();
        let (status, body) = call(
            router(omp.clone(), l.clone()),
            Request::get(format!("/api/repos/{}/sessions", encode_path(&path)))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["sessions"],
            json!([])
        );
    }

    async fn post_resume_session(
        omp: &Arc<FakeOmp>,
        l: &Arc<Launcher>,
        path: &str,
        session_id: &str,
    ) -> (StatusCode, String) {
        let req = Request::post(format!("/api/repos/{}/sessions/resume", encode_path(path)))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "session_id": session_id }).to_string()))
            .unwrap();
        call(router(omp.clone(), l.clone()), req).await
    }

    #[tokio::test]
    async fn resume_session_starts_the_named_past_session_without_stopping_anything() {
        let (_t, l, path, _lock) = sessions_layout();
        let omp = fixture();
        let (status, body) = post_resume_session(&omp, &l, &path, "newer").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("\"resumed\":true"));
        assert_eq!(
            *omp.resumes.lock(),
            vec![(std::path::PathBuf::from(&path), "newer".to_string())]
        );
        assert!(omp.stops.lock().is_empty());
    }

    #[tokio::test]
    async fn resume_session_accepts_a_session_with_only_a_stale_lock_file() {
        let (_t, l, path, _lock) = sessions_layout();
        let omp = fixture();
        let (status, _) = post_resume_session(&omp, &l, &path, "stale").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(omp.resumes.lock().len(), 1);
    }

    #[tokio::test]
    async fn resume_session_rejects_an_id_the_rescan_does_not_list() {
        let (_t, l, path, _lock) = sessions_layout();
        let omp = fixture();
        for bad in ["locked", "unknown-id", "../escape"] {
            let (status, _) = post_resume_session(&omp, &l, &path, bad).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
        }
        assert!(omp.resumes.lock().is_empty());
    }

    #[tokio::test]
    async fn resume_session_rejects_unknown_checkout_paths_without_calling_resume() {
        let (_t, l, _path, _lock) = sessions_layout();
        let omp = fixture();
        let (status, _) = post_resume_session(&omp, &l, "/nowhere", "newer").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(omp.resumes.lock().is_empty());
    }

    async fn post_self_update(app: Router) -> (StatusCode, String) {
        let res = app
            .oneshot(
                Request::post("/api/self-update")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (parts, body) = res.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn self_update_refuses_when_disabled_without_checking_github() {
        let omp = fixture();
        let updater = FakeUpdater::new(true, Err("must not be called".to_string()));
        let (tx, _rx) = watch::channel(false);
        let app = router_with_updater(omp, launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status, body) = post_self_update(app).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("OMP_DECK_NO_AUTOUPDATE"));
        assert_eq!(*updater.newer_calls.lock(), 0);
        assert_eq!(*updater.installs.lock(), 0);
    }

    #[tokio::test]
    async fn self_update_does_nothing_when_already_up_to_date() {
        let omp = fixture();
        let updater = FakeUpdater::new(false, Ok(None));
        let (tx, mut rx) = watch::channel(false);
        let app = router_with_updater(omp, launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status, body) = post_self_update(app).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"updated\":false"));
        assert_eq!(*updater.newer_calls.lock(), 1);
        assert_eq!(*updater.installs.lock(), 0);
        assert!(!*rx.borrow_and_update());
    }

    #[tokio::test]
    async fn self_update_reports_the_error_when_the_check_fails() {
        let omp = fixture();
        let updater = FakeUpdater::new(false, Err("github is down".to_string()));
        let (tx, _rx) = watch::channel(false);
        let app = router_with_updater(omp, launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status, body) = post_self_update(app).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("github is down"));
        assert_eq!(*updater.installs.lock(), 0);
    }

    #[tokio::test]
    async fn self_update_installs_and_triggers_restart_when_a_newer_release_is_found() {
        let omp = fixture();
        let updater = FakeUpdater::new(false, Ok(Some(release("v9.9.9"))));
        let (tx, mut rx) = watch::channel(false);
        let app = router_with_updater(omp, launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status, body) = post_self_update(app).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(body.contains("v9.9.9"));
        // The install runs in the background; wait for the restart signal
        // it sends on success rather than sleeping a fixed amount.
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
        assert_eq!(*updater.installs.lock(), 1);
    }

    #[tokio::test]
    async fn self_update_second_request_is_rejected_while_the_first_is_in_progress() {
        let omp = fixture();
        let updater = FakeUpdater::new(false, Ok(Some(release("v9.9.9"))));
        let (tx, _rx) = watch::channel(false);
        let app = router_with_updater(omp, launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status1, _) = post_self_update(app.clone()).await;
        assert_eq!(status1, StatusCode::ACCEPTED);
        let (status2, _) = post_self_update(app).await;
        assert_eq!(status2, StatusCode::CONFLICT);
        assert_eq!(*updater.newer_calls.lock(), 1);
    }

    async fn get_update(app: Router) -> (StatusCode, String) {
        let res = app
            .oneshot(Request::get("/api/update").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = res.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn update_json(
        updater: &Arc<FakeUpdater>,
    ) -> (StatusCode, serde_json::Value, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        let app = router_with_updater(fixture(), launcher(Vec::new(), &[]), updater.clone(), tx);
        let (status, body) = get_update(app).await;
        (status, serde_json::from_str(&body).unwrap(), rx)
    }

    #[tokio::test]
    async fn update_reports_a_newer_release() {
        let updater = FakeUpdater::new(false, Ok(Some(release("v9.9.9"))));
        let (status, v, rx) = update_json(&updater).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v, json!({ "available": true, "to": "v9.9.9" }));
        // A GET never installs or restarts.
        assert_eq!(*updater.installs.lock(), 0);
        assert!(!*rx.borrow());
    }

    #[tokio::test]
    async fn update_reports_unavailable_when_up_to_date() {
        let updater = FakeUpdater::new(false, Ok(None));
        let (status, v, _rx) = update_json(&updater).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v, json!({ "available": false }));
        assert_eq!(*updater.known_calls.lock(), 1);
    }

    #[tokio::test]
    async fn update_reports_unavailable_when_disabled_without_checking() {
        let updater = FakeUpdater::new(true, Ok(Some(release("v9.9.9"))));
        let (status, v, _rx) = update_json(&updater).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v, json!({ "available": false }));
        assert_eq!(*updater.known_calls.lock(), 0);
        assert_eq!(*updater.newer_calls.lock(), 0);
    }

    #[tokio::test]
    async fn update_reports_unavailable_when_the_check_fails() {
        let updater = FakeUpdater::new(false, Err("github is down".to_string()));
        let (status, v, _rx) = update_json(&updater).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v, json!({ "available": false }));
        assert_eq!(*updater.installs.lock(), 0);
    }
}
