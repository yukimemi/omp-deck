//! HTTP layer. Nothing is cached and nothing is logged: link URLs are secrets
//! that only ever appear in a `Location` header, fetched fresh per request.

use crate::model::Host;
use crate::omp::{Access, Omp, OmpError};
use crate::repos;
use crate::sessions;
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

type Shared = Arc<dyn Omp>;

/// What "new session" may offer: the scanned checkouts, the configured model
/// candidates, and where to look for a checkout's past sessions. Nothing
/// outside these is ever handed to `omp`.
pub struct Launcher {
    pub repos: repos::Cache,
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
}

impl FromRef<AppState> for Shared {
    fn from_ref(state: &AppState) -> Self {
        state.omp.clone()
    }
}

pub fn router(omp: Arc<dyn Omp>, launcher: Arc<Launcher>) -> Router {
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
        .route("/go/{instance_id}/{kind}", get(go))
        .layer(middleware::map_response(harden))
        .with_state(AppState { omp, launcher })
}

const NO_ROOTS_HINT: &str = "No repository roots configured. Add [repos] roots = [\"...\"] to      the omp-deck config file (see the README).";

impl FromRef<AppState> for Arc<Launcher> {
    fn from_ref(state: &AppState) -> Self {
        state.launcher.clone()
    }
}

async fn api_repos(State(l): State<Arc<Launcher>>) -> Response {
    let repos = l.repos.get().await;
    let hint = (!l.repos.has_roots()).then_some(NO_ROOTS_HINT);
    Json(json!({ "repos": *repos, "hint": hint })).into_response()
}

async fn api_models(State(l): State<Arc<Launcher>>) -> Response {
    Json(json!({ "models": l.models })).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    path: String,
    #[serde(default)]
    model: Option<String>,
}

async fn api_start(
    State(omp): State<Shared>,
    State(l): State<Arc<Launcher>>,
    Json(req): Json<StartRequest>,
) -> Response {
    // Only ever start omp in a checkout the scan itself just listed, with a
    // model the config names; the values passed on are ours, not the request's.
    let repos = l.repos.get().await;
    let Some(repo) = repos.iter().find(|r| r.path == req.path) else {
        return plain(StatusCode::NOT_FOUND, "not a known checkout");
    };
    let model = match req.model.as_deref() {
        None | Some("") => None,
        Some(m) => match l.models.iter().find(|c| c.as_str() == m) {
            Some(c) => Some(c.as_str()),
            None => return plain(StatusCode::BAD_REQUEST, "not a configured model"),
        },
    };
    match omp.start(std::path::Path::new(&repo.path), model).await {
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
    // Re-scan rather than trust anything cached: a session that picked up a
    // lock file (or vanished) between listing and resuming must not be resumed.
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

    fn launcher(roots: Vec<std::path::PathBuf>, models: &[&str]) -> Arc<Launcher> {
        Arc::new(Launcher {
            repos: repos::Cache::new(roots, std::time::Duration::from_secs(30)),
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
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// A ghq-layout checkout under a tempdir "home", plus a sessions root
    /// holding two resumable sessions and one still-locked (live) one.
    fn sessions_layout() -> (tempfile::TempDir, Arc<Launcher>, String) {
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
        std::fs::write(
            dir.join(".2026-01-03T00-00-00-000Z_locked.jsonl.lock.os"),
            "",
        )
        .unwrap();
        let l = launcher_with_sessions(vec![t.path().to_path_buf()], sessions_root, home);
        (t, l, path)
    }

    #[tokio::test]
    async fn repo_sessions_lists_resumable_sessions_excluding_locked_ones() {
        let (_t, l, path) = sessions_layout();
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
        assert_eq!(ids, vec!["newer", "older"]);
        assert_eq!(v["sessions"][0]["title"], "Newer session");
    }

    #[tokio::test]
    async fn repo_sessions_rejects_unknown_checkout_paths() {
        let (_t, l, _path) = sessions_layout();
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
        let (_t, l, path) = sessions_layout();
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
    async fn resume_session_rejects_an_id_the_rescan_does_not_list() {
        let (_t, l, path) = sessions_layout();
        let omp = fixture();
        for bad in ["locked", "unknown-id", "../escape"] {
            let (status, _) = post_resume_session(&omp, &l, &path, bad).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
        }
        assert!(omp.resumes.lock().is_empty());
    }

    #[tokio::test]
    async fn resume_session_rejects_unknown_checkout_paths_without_calling_resume() {
        let (_t, l, _path) = sessions_layout();
        let omp = fixture();
        let (status, _) = post_resume_session(&omp, &l, "/nowhere", "newer").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(omp.resumes.lock().is_empty());
    }
}
