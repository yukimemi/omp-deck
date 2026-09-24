//! Pure presentation: relative time, HTML escaping, page and table rendering.

use crate::model::Host;
use std::fmt::Write as _;

const PAGE: &str = include_str!("page.html");
const BODY_MARKER: &str = "<!--BODY-->";

/// Escape text for use in HTML element content and double-quoted attributes.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Percent-encode everything but unreserved characters, for use as a path segment.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// "5m ago" style age of `started_ms` as seen at `now_ms`.
pub fn relative_time(started_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(started_ms) / 1000;
    match secs {
        i64::MIN..=9 => "just now".to_string(),
        10..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn started_label(host: &Host, now_ms: i64) -> String {
    host.started_at
        .map_or_else(|| "unknown".to_string(), |t| relative_time(t, now_ms))
}

/// Status of a host, most attention-worthy first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InputRequired,
    Busy,
    Idle,
}

impl Status {
    pub fn of(host: &Host) -> Self {
        if host.input_required {
            Self::InputRequired
        } else if host.busy {
            Self::Busy
        } else {
            Self::Idle
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::InputRequired => "input required",
            Self::Busy => "busy",
            Self::Idle => "idle",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::InputRequired => "input",
            Self::Busy => "busy",
            Self::Idle => "idle",
        }
    }
}

fn render_card(host: &Host, now_ms: i64) -> String {
    let status = Status::of(host);
    let id = encode_segment(&host.instance_id);
    let mut meta = Vec::new();
    if let Some(model) = host.model_label() {
        meta.push(escape_html(&model));
    }
    meta.push(format!(
        "{} participant{}",
        host.participants,
        if host.participants == 1 { "" } else { "s" }
    ));
    meta.push(format!("started {}", started_label(host, now_ms)));
    let link = |kind: &str| {
        format!(
            "<a class=\"{kind}\" href=\"/go/{id}/{kind}\" target=\"_blank\" \
             rel=\"noopener noreferrer\">{kind}</a>"
        )
    };
    format!(
        "<article class=\"card {class}\">\n\
         <header><h2>{name}</h2><span class=\"badge {class}\">{label}</span></header>\n\
         <p class=\"cwd\">{cwd}</p>\n\
         <p class=\"meta\">{meta}</p>\n\
         <p class=\"links\">{view}{control}<button type=\"button\" class=\"stop\" \
         data-id=\"{id}\">close</button></p>\n\
         </article>\n",
        class = status.class(),
        name = escape_html(host.display_name()),
        label = status.label(),
        cwd = escape_html(&host.cwd),
        meta = meta.join(" &middot; "),
        view = link("view"),
        control = link("control"),
    )
}

fn wrap(body: &str) -> String {
    PAGE.replace(BODY_MARKER, body)
}

/// The dashboard page for a successfully fetched host list.
pub fn render_page(hosts: &[Host], now_ms: i64) -> String {
    if hosts.is_empty() {
        return wrap(
            "<div class=\"note\"><p><strong>no live omp sessions</strong></p>\n\
             <p>Start an interactive <code>omp</code>. To have sessions show up here without \
             typing /collab, run <code>omp config set collab.autoStart control</code>.</p></div>\n",
        );
    }
    let mut body = String::from("<main class=\"grid\">\n");
    for host in hosts {
        body.push_str(&render_card(host, now_ms));
    }
    body.push_str("</main>\n");
    wrap(&body)
}

/// The dashboard page when the host list could not be fetched.
pub fn render_error(message: &str) -> String {
    wrap(&format!(
        "<div class=\"error\"><strong>could not list omp sessions</strong>\n<pre>{}</pre></div>\n",
        escape_html(message)
    ))
}

/// Plain-text table for `omp-deck list`.
pub fn render_table(hosts: &[Host], now_ms: i64) -> String {
    if hosts.is_empty() {
        return "no live omp sessions\n".to_string();
    }
    let header = ["NAME", "STATUS", "PARTICIPANTS", "STARTED", "MODEL", "CWD"];
    let mut rows: Vec<[String; 6]> = vec![header.map(String::from)];
    for h in hosts {
        rows.push([
            h.display_name().to_string(),
            Status::of(h).label().to_string(),
            h.participants.to_string(),
            started_label(h, now_ms),
            h.model_label().unwrap_or_default(),
            h.cwd.clone(),
        ]);
    }
    let mut widths = [0usize; 6];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_hosts;

    const FIXTURE: &str = include_str!("../tests/fixtures/hosts.json");
    const NOW: i64 = 1_700_000_000_000 + 5 * 60_000;

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(1000, 1000), "just now");
        assert_eq!(relative_time(2000, 1000), "just now");
        assert_eq!(relative_time(0, 30_000), "30s ago");
        assert_eq!(relative_time(0, 5 * 60_000), "5m ago");
        assert_eq!(relative_time(0, 3 * 3_600_000), "3h ago");
        assert_eq!(relative_time(0, 2 * 86_400_000), "2d ago");
    }

    #[test]
    fn escapes_special_characters() {
        assert_eq!(
            escape_html(r#"<a href="x">&'"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(encode_segment("a-b_c.d"), "a-b_c.d");
        assert_eq!(encode_segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn page_escapes_attacker_influenceable_strings() {
        let html = render_page(&parse_hosts(FIXTURE).unwrap(), NOW);
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<b>evil"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("C:\\work\\&lt;b&gt;evil"));
    }

    #[test]
    fn page_has_a_card_per_host_with_safe_links() {
        let html = render_page(&parse_hosts(FIXTURE).unwrap(), NOW);
        assert_eq!(html.matches("<article").count(), 2);
        assert!(html.contains("href=\"/go/inst-aaa/view\""));
        assert!(html.contains("href=\"/go/inst-aaa/control\""));
        assert_eq!(html.matches("rel=\"noopener noreferrer\"").count(), 4);
        assert!(html.contains("input required"));
        assert!(html.contains("started 5m ago"));
        assert!(html.contains("anthropic/claude-x"));
        assert!(!html.contains("my.omp.sh"));
    }

    #[test]
    fn empty_state_is_not_an_error() {
        let html = render_page(&[], NOW);
        assert!(html.contains("no live omp sessions"));
        assert!(html.contains("omp config set collab.autoStart control"));
        assert!(!html.contains("class=\"error\""));
    }

    #[test]
    fn error_page_escapes_message() {
        let html = render_error("spawn <omp> failed");
        assert!(html.contains("class=\"error\""));
        assert!(html.contains("spawn &lt;omp&gt; failed"));
        assert!(!html.contains("no live omp sessions"));
    }

    #[test]
    fn status_priority() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        assert_eq!(Status::of(&hosts[0]), Status::InputRequired);
        assert_eq!(Status::of(&hosts[1]), Status::Busy);
        let mut h = hosts[1].clone();
        h.busy = false;
        assert_eq!(Status::of(&h), Status::Idle);
    }

    #[test]
    fn table_lists_hosts() {
        let table = render_table(&parse_hosts(FIXTURE).unwrap(), NOW);
        assert_eq!(table.lines().count(), 3);
        assert!(table.lines().next().unwrap().starts_with("NAME"));
        assert!(table.contains("omp-deck"));
        assert_eq!(render_table(&[], NOW), "no live omp sessions\n");
    }
}
