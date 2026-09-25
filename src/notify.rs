//! Background poller that posts a Discord notification the first time a live
//! `omp` collab session gets a title (`sessionName`). Only runs when a
//! webhook URL is configured (`--discord-webhook` / `OMP_DECK_DISCORD_WEBHOOK`).
//!
//! A session has no title for a short while after it starts (`display_name`
//! falls back to the cwd leaf until `omp` names it), so this waits for the
//! real title rather than notifying on first sight of the instance.

use crate::model::Host;
use crate::omp::{Access, Omp};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// How often the background poller re-checks `omp collab list`. Matches the
/// dashboard page's own auto-reload interval (see `page.html`).
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Upper bound on one webhook POST, so an unresponsive endpoint cannot stall
/// the poll loop forever.
const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Hosts that just got a title and have not been notified about yet. Pure:
/// no I/O, so it is unit-testable without a process or a socket.
fn newly_titled<'a>(hosts: &'a [Host], notified: &HashSet<String>) -> Vec<&'a Host> {
    hosts
        .iter()
        .filter(|h| {
            !notified.contains(&h.instance_id)
                && h.session_name
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|n| !n.is_empty())
        })
        .collect()
}

/// Discord's hard cap on a message's `content` field.
const DISCORD_CONTENT_LIMIT: usize = 2000;

/// The Discord webhook payload for a newly titled session. Mentions in the
/// session's title (`@everyone`, `@here`, role/user pings) are suppressed:
/// the title comes from `omp`, effectively an untrusted string, and a ping
/// storm is not an acceptable side effect of naming a session. `content` is
/// truncated to Discord's 2000-character limit, past which the webhook POST
/// would otherwise fail with 400.
fn payload(host: &Host, url: &str) -> serde_json::Value {
    let mut content = format!(
        "omp collab session started: **{}**\n{}",
        host.display_name(),
        url
    );
    if content.len() > DISCORD_CONTENT_LIMIT {
        let mut cut = DISCORD_CONTENT_LIMIT;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
    }
    serde_json::json!({
        "content": content,
        "allowed_mentions": { "parse": [] },
    })
}

/// POSTs to the webhook; a non-2xx response (429, 5xx, 404 ...) is an error.
async fn post(
    client: &reqwest::Client,
    webhook: &str,
    body: &serde_json::Value,
) -> Result<(), reqwest::Error> {
    client
        .post(webhook)
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// One list-and-notify pass: lists live sessions, and for each newly titled
/// one not already in `notified`, fetches its control link and posts it.
/// Failures (a failed `list`/`link` call, a failed webhook POST) are logged;
/// they never abort the pass, and a session is only added to `notified` after
/// its POST succeeded, so a failed one is retried on the next pass.
async fn poll_once(
    omp: &dyn Omp,
    client: &reqwest::Client,
    webhook: &str,
    notified: &mut HashSet<String>,
) {
    let hosts = match omp.list().await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("omp-deck: discord notify: could not list sessions: {e}");
            return;
        }
    };
    for host in newly_titled(&hosts, notified) {
        let instance_id = host.instance_id.clone();
        let url = match omp.link(&instance_id, Access::Control).await {
            Ok(url) => url,
            Err(e) => {
                eprintln!("omp-deck: discord notify: could not get link for {instance_id}: {e}");
                continue;
            }
        };
        match post(client, webhook, &payload(host, &url)).await {
            Ok(()) => {
                notified.insert(instance_id);
            }
            // The error's Display embeds the webhook URL (which carries the
            // token); strip it before logging.
            Err(e) => eprintln!(
                "omp-deck: discord notify failed for {instance_id}: {}",
                e.without_url()
            ),
        }
    }
}

/// Records every session that is already titled as notified, without posting,
/// so a restart does not re-announce sessions that were running before it.
/// Returns false if the list call failed (nothing recorded).
async fn baseline(omp: &dyn Omp, notified: &mut HashSet<String>) -> bool {
    match omp.list().await {
        Ok(hosts) => {
            let ids: Vec<String> = newly_titled(&hosts, notified)
                .into_iter()
                .map(|h| h.instance_id.clone())
                .collect();
            notified.extend(ids);
            true
        }
        Err(e) => {
            eprintln!("omp-deck: discord notify: could not list sessions: {e}");
            false
        }
    }
}

/// Runs until the process exits: takes a baseline of already-titled sessions,
/// then sleeps `POLL_INTERVAL` and runs [`poll_once`], forever.
pub async fn run(omp: Arc<dyn Omp>, webhook: String) {
    let client = reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()
        .expect("failed to build discord http client");
    let mut notified: HashSet<String> = HashSet::new();
    while !baseline(omp.as_ref(), &mut notified).await {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        poll_once(omp.as_ref(), &client, &webhook, &mut notified).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_hosts;
    use parking_lot::Mutex;

    const FIXTURE: &str = include_str!("../tests/fixtures/hosts.json");

    #[test]
    fn only_titled_untold_hosts_are_returned() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        let notified = HashSet::new();
        let due = newly_titled(&hosts, &notified);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].instance_id, "inst-bbb");
    }

    #[test]
    fn already_notified_hosts_are_skipped() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        let mut notified = HashSet::new();
        notified.insert("inst-bbb".to_string());
        assert!(newly_titled(&hosts, &notified).is_empty());
    }

    #[test]
    fn blank_title_does_not_count_as_titled() {
        let mut hosts = parse_hosts(FIXTURE).unwrap();
        hosts[1].session_name = Some("   ".to_string());
        assert!(newly_titled(&hosts, &HashSet::new()).is_empty());
    }

    #[test]
    fn payload_carries_the_display_name_and_url() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        let p = payload(&hosts[1], "https://my.omp.sh/#secret");
        let content = p["content"].as_str().unwrap();
        assert!(content.contains("https://my.omp.sh/#secret"));
        assert!(content.contains(hosts[1].display_name()));
    }

    #[test]
    fn payload_suppresses_all_mentions() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        let p = payload(&hosts[1], "https://my.omp.sh/#secret");
        assert_eq!(p["allowed_mentions"]["parse"], serde_json::json!([]));
    }

    #[test]
    fn payload_content_never_exceeds_the_discord_limit() {
        let mut hosts = parse_hosts(FIXTURE).unwrap();
        hosts[1].session_name = Some("x".repeat(DISCORD_CONTENT_LIMIT * 2));
        let p = payload(&hosts[1], "https://my.omp.sh/#secret");
        let content = p["content"].as_str().unwrap();
        assert!(content.len() <= DISCORD_CONTENT_LIMIT);
    }

    #[tokio::test]
    async fn baseline_marks_titled_hosts_without_posting() {
        let omp = FakeOmp {
            hosts: parse_hosts(FIXTURE).unwrap(),
            link_url: String::new(),
        };
        let mut notified = HashSet::new();
        assert!(baseline(&omp, &mut notified).await);
        assert_eq!(notified, HashSet::from(["inst-bbb".to_string()]));
    }

    #[tokio::test]
    async fn failed_post_is_not_marked_notified_and_is_retried() {
        let app = axum::Router::new().route(
            "/webhook",
            axum::routing::post(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let omp = FakeOmp {
            hosts: parse_hosts(FIXTURE).unwrap(),
            link_url: "https://my.omp.sh/#secret".to_string(),
        };
        let mut notified = HashSet::new();
        poll_once(
            &omp,
            &reqwest::Client::new(),
            &format!("http://{addr}/webhook"),
            &mut notified,
        )
        .await;
        assert!(notified.is_empty());
    }

    struct FakeOmp {
        hosts: Vec<Host>,
        link_url: String,
    }

    #[async_trait::async_trait]
    impl Omp for FakeOmp {
        async fn list(&self) -> Result<Vec<Host>, crate::omp::OmpError> {
            Ok(self.hosts.clone())
        }
        async fn link(
            &self,
            _instance_id: &str,
            access: Access,
        ) -> Result<String, crate::omp::OmpError> {
            assert_eq!(access, Access::Control);
            Ok(self.link_url.clone())
        }
        async fn start(
            &self,
            _cwd: &std::path::Path,
            _model: Option<&str>,
        ) -> Result<(), crate::omp::OmpError> {
            unreachable!("notify never starts sessions")
        }
        async fn stop(&self, _pid: u32) -> Result<(), crate::omp::OmpError> {
            unreachable!("notify never stops sessions")
        }
    }

    /// End-to-end: a real HTTP POST lands on a real local server, with the
    /// control link in the body, exactly once per newly titled session.
    #[tokio::test]
    async fn poll_once_posts_to_a_real_server_exactly_once_per_session() {
        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let received_in_handler = received.clone();
        let app = axum::Router::new().route(
            "/webhook",
            axum::routing::post(move |body: String| {
                let received = received_in_handler.clone();
                async move {
                    received.lock().push(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let hosts = parse_hosts(FIXTURE).unwrap();
        let omp = FakeOmp {
            hosts,
            link_url: "https://my.omp.sh/#secret".to_string(),
        };
        let client = reqwest::Client::new();
        let webhook = format!("http://{addr}/webhook");
        let mut notified = HashSet::new();

        poll_once(&omp, &client, &webhook, &mut notified).await;
        assert_eq!(notified, HashSet::from(["inst-bbb".to_string()]));
        let posts = received.lock().clone();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].contains("https://my.omp.sh/#secret"));

        // Second pass over the same hosts must not re-notify.
        poll_once(&omp, &client, &webhook, &mut notified).await;
        assert_eq!(received.lock().len(), 1);
    }
}
