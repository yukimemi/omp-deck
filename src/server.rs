//! HTTP layer. Nothing is cached and nothing is logged: link URLs are secrets
//! that only ever appear in a `Location` header, fetched fresh per request.

use crate::model::Host;
use crate::omp::{Access, Omp, OmpError};
use crate::repos;
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
use std::sync::Arc;

type Shared = Arc<dyn Omp>;

/// What "new session" may offer: the scanned checkouts and the configured
/// model candidates. Nothing outside these is ever handed to `omp`.
pub struct Launcher {
    pub repos: repos::Cache,
    pub models: Vec<String>,
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
            if let Some(stderr) = &self.stop_error {
                return Err(OmpError::Exit {
                    code: Some(4),
                    stderr: stderr.clone(),
                });
            }
            self.stops.lock().push(pid);
            Ok(())
        }
    }

    fn launcher(roots: Vec<std::path::PathBuf>, models: &[&str]) -> Arc<Launcher> {
        Arc::new(Launcher {
            repos: repos::Cache::new(roots, std::time::Duration::from_secs(30)),
            models: models.iter().map(|m| m.to_string()).collect(),
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
        });
        let (status, body) = delete_path(&omp, "/api/sessions/inst-aaa").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("access denied"));
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
}
