//! Optional config file: which directories to scan for checkouts and which
//! models the "new session" picker offers.
//!
//! The file is a Tera template (via `teravars`, so `system.*` and `[vars]` are
//! available), rendered *before* TOML is parsed: write paths that contain
//! quotes as single-quoted TOML strings.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub repos: Repos,
    pub models: Models,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Repos {
    /// ghq-layout roots (`root/host/owner/repo`). Empty: nothing is scanned.
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    /// Candidates passed verbatim to `omp --model`. Empty: omp's default.
    pub list: Vec<String>,
}

impl Config {
    /// Load one file through teravars.
    pub fn load(path: &Path) -> Result<Self> {
        let mut engine = teravars::Engine::default();
        let ctx = teravars::system_context();
        let merged = teravars::load_merged(&[path.to_path_buf()], &mut engine, &ctx)
            .with_context(|| format!("rendering config via teravars: {}", path.display()))?;
        let mut table = merged.config;
        // `[vars]` is teravars' own input, already resolved into the context.
        table.remove("vars");
        toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("deserializing config: {}", path.display()))
    }

    /// `explicit` (`--config` / `OMP_DECK_CONFIG`) must exist; the default
    /// `<config_dir>/omp-deck/config.toml` may be absent, which means defaults.
    pub fn load_or_default(explicit: Option<&Path>) -> Result<Self> {
        if let Some(path) = explicit {
            return Self::load(path);
        }
        match default_path() {
            Some(path) if path.is_file() => Self::load(&path),
            _ => Ok(Self::default()),
        }
    }
}

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("omp-deck").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn loads_roots_and_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "[repos]\nroots = ['/a/b', '/c']\n[models]\nlist = [\"opus\", \"gpt-5.2\"]\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(
            cfg.repos.roots,
            [PathBuf::from("/a/b"), PathBuf::from("/c")]
        );
        assert_eq!(cfg.models.list, ["opus", "gpt-5.2"]);
    }

    #[test]
    fn expands_templates_and_vars() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir,
            "# {{ this comment is stripped before rendering }}\n\
             [vars]\nname = \"deck\"\n\
             [repos]\nroots = ['/x/{{ vars.name }}']\n\
             [models]\nlist = [\"{{ system.os }}\"]\n",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.repos.roots, [PathBuf::from("/x/deck")]);
        assert_eq!(cfg.models.list.len(), 1);
        assert!(!cfg.models.list[0].contains("{{"));
    }

    #[test]
    fn single_quoted_paths_may_hold_quotes_and_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "[repos]\nroots = ['C:\\Users\\me\\it\"s']\n");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.repos.roots, [PathBuf::from("C:\\Users\\me\\it\"s")]);
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load_or_default(Some(&dir.path().join("nope.toml"))).is_err());
    }

    #[test]
    fn empty_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&write(&dir, "")).unwrap(), Config::default());
        assert!(Config::load(&write(&dir, "[repoz]\nx = 1\n")).is_err());
    }
}
