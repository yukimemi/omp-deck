//! HTTP layer. Nothing is cached and nothing is logged: link URLs are secrets
//! that only ever appear in a `Location` header, fetched fresh per request.

use crate::model::Host;
use crate::omp::{Access, Omp, OmpError};
use crate::view;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::sync::Arc;

type Shared = Arc<dyn Omp>;

pub fn router(omp: Arc<dyn Omp>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/hosts", get(api_hosts))
        .route("/go/{instance_id}/{kind}", get(go))
        .layer(middleware::map_response(harden))
        .with_state(omp)
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
    use std::sync::Mutex;
    use tower::ServiceExt;

    const FIXTURE: &str = include_str!("../tests/fixtures/hosts.json");
    const SECRET: &str = "https://my.omp.sh/#secret-room-key";

    struct FakeOmp {
        hosts: Result<Vec<Host>, String>,
        links: Mutex<Vec<(String, Access)>>,
    }

    impl FakeOmp {
        fn new(hosts: Result<Vec<Host>, String>) -> Arc<Self> {
            Arc::new(Self {
                hosts,
                links: Mutex::new(Vec::new()),
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
            self.links
                .lock()
                .unwrap()
                .push((instance_id.to_string(), access));
            Ok(SECRET.to_string())
        }
    }

    async fn get_path(
        omp: &Arc<FakeOmp>,
        path: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let app = router(omp.clone());
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
        assert!(omp.links.lock().unwrap().is_empty());
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
            *omp.links.lock().unwrap(),
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
        assert!(omp.links.lock().unwrap().is_empty());
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
}
