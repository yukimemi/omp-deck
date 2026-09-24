//! Lenient parsing of `omp collab list --json`.
//!
//! Unknown fields are ignored and optional ones default. There is deliberately
//! no URL field on [`Host`]: link URLs carry secrets and must never be part of
//! anything that gets serialized to the API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Control,
    View,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct Model {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub instance_id: String,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub model: Option<Model>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub participants: u32,
    #[serde(default)]
    pub relay_connected: bool,
    #[serde(default)]
    pub input_required: bool,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub access: Access,
}

#[derive(Debug, Deserialize)]
struct HostList {
    #[serde(default)]
    hosts: Vec<Host>,
}

/// Parse the stdout of `omp collab list --json`.
pub fn parse_hosts(json: &str) -> Result<Vec<Host>, serde_json::Error> {
    serde_json::from_str::<HostList>(json).map(|l| l.hosts)
}

/// Last component of a Windows or POSIX path, ignoring trailing separators.
fn last_component(path: &str) -> &str {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
}

impl Host {
    /// The session title, or the last path component of `cwd` while omp has not named it yet.
    pub fn display_name(&self) -> &str {
        match self.session_name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => name,
            _ => {
                let dir = last_component(&self.cwd);
                if dir.is_empty() {
                    &self.instance_id
                } else {
                    dir
                }
            }
        }
    }

    /// `provider/id`, when omp reported a model.
    pub fn model_label(&self) -> Option<String> {
        let m = self.model.as_ref()?;
        match (m.provider.is_empty(), m.id.is_empty()) {
            (true, true) => None,
            (true, false) => Some(m.id.clone()),
            (false, true) => Some(m.provider.clone()),
            (false, false) => Some(format!("{}/{}", m.provider, m.id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/hosts.json");
    const EMPTY: &str = include_str!("../tests/fixtures/empty.json");

    #[test]
    fn parses_fixture_ignoring_unknown_fields() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].instance_id, "inst-aaa");
        assert_eq!(hosts[0].pid, Some(4242));
        assert!(hosts[0].input_required);
        assert_eq!(hosts[1].access, Access::View);
    }

    #[test]
    fn empty_hosts_is_an_empty_list() {
        assert!(parse_hosts(EMPTY).unwrap().is_empty());
        assert!(parse_hosts("{\"version\":1}").unwrap().is_empty());
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let hosts = parse_hosts(r#"{"hosts":[{"instanceId":"x","access":"weird"}]}"#).unwrap();
        assert_eq!(hosts[0].access, Access::Unknown);
        assert_eq!(hosts[0].participants, 0);
        assert_eq!(hosts[0].started_at, None);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_hosts("not json").is_err());
    }

    #[test]
    fn null_session_name_falls_back_to_windows_cwd_leaf() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        assert_eq!(hosts[0].display_name(), "omp-deck");
    }

    #[test]
    fn display_name_prefers_session_name_and_handles_edges() {
        let mut h = parse_hosts(FIXTURE).unwrap().remove(1);
        assert_eq!(h.display_name(), "<script>alert(1)</script>");
        h.session_name = Some("  ".into());
        assert_eq!(h.display_name(), "<b>evil");
        h.cwd = "/home/me/proj/".into();
        assert_eq!(h.display_name(), "proj");
        h.cwd = String::new();
        assert_eq!(h.display_name(), "inst-bbb");
    }

    #[test]
    fn model_label_joins_provider_and_id() {
        let hosts = parse_hosts(FIXTURE).unwrap();
        assert_eq!(
            hosts[0].model_label().as_deref(),
            Some("anthropic/claude-x")
        );
    }

    #[test]
    fn serialized_host_has_no_url() {
        let json = serde_json::to_string(&parse_hosts(FIXTURE).unwrap()).unwrap();
        assert!(!json.contains("url"));
        assert!(!json.contains("my.omp.sh"));
    }
}
