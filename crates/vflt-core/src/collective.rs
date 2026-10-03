//! `collective.toml`: the configuration of a board.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::Stage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    File,
    Sqlite,
    Turso,
    Dynamodb,
    Mysql,
    Http,
}

impl StoreKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StoreKind::File => "file",
            StoreKind::Sqlite => "sqlite",
            StoreKind::Turso => "turso",
            StoreKind::Dynamodb => "dynamodb",
            StoreKind::Mysql => "mysql",
            StoreKind::Http => "http",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreConfig {
    pub kind: StoreKind,
    /// For `file`: the root directory, relative to `collective.toml`.
    #[serde(default = "default_root")]
    pub root: String,
    /// For `file`: commit every state mutation to git.
    #[serde(default = "default_true")]
    pub git: bool,
    /// For database and http stores: a connection string or base url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn default_root() -> String {
    ".".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            kind: StoreKind::File,
            root: default_root(),
            git: true,
            url: None,
        }
    }
}

/// How agents pick up work.
///
/// * `pull` (default): unassigned pool items plus items delegated to the agent.
/// * `delegated`: only items assigned to the agent.
/// * `both`: alias of `pull`, kept so configs read naturally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimMode {
    #[default]
    Pull,
    Delegated,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collective {
    pub name: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub claim_mode: ClaimMode,
    /// Claims whose holder has not heartbeated within this window are stale.
    #[serde(default = "default_claim_ttl")]
    pub claim_ttl: String,
    /// Ordered stages. `complete` advances along this list.
    #[serde(default = "Stage::default_pipeline")]
    pub pipeline: Vec<Stage>,
    /// Where `complete --bounce` sends an item.
    #[serde(default = "default_bounce_to")]
    pub bounce_to: Stage,
    #[serde(default)]
    pub store: StoreConfig,
}

fn default_claim_ttl() -> String {
    "30m".to_string()
}

fn default_bounce_to() -> Stage {
    Stage::new(Stage::CODE)
}

impl Collective {
    pub fn new(name: impl Into<String>) -> Self {
        Collective {
            name: name.into(),
            created: Utc::now(),
            claim_mode: ClaimMode::Pull,
            claim_ttl: default_claim_ttl(),
            pipeline: Stage::default_pipeline(),
            bounce_to: default_bounce_to(),
            store: StoreConfig::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let c: Collective = toml::from_str(&text)?;
        c.validate()?;
        Ok(c)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).map_err(|e| Error::io(path, e))
    }

    pub fn validate(&self) -> Result<()> {
        if self.pipeline.is_empty() {
            return Err(Error::invalid("collective", "pipeline must not be empty"));
        }
        crate::duration::parse(&self.claim_ttl)?;
        Ok(())
    }

    pub fn claim_ttl(&self) -> std::time::Duration {
        crate::duration::parse(&self.claim_ttl).unwrap_or(std::time::Duration::from_secs(1800))
    }

    /// The stage after `current` in the pipeline. `None` when `current` is the
    /// last stage or not in the pipeline at all.
    pub fn next_stage(&self, current: &Stage) -> Option<&Stage> {
        let idx = self.pipeline.iter().position(|s| s == current)?;
        self.pipeline.get(idx + 1)
    }

    pub fn terminal_stage(&self) -> &Stage {
        self.pipeline.last().expect("validated non-empty pipeline")
    }

    pub fn is_terminal_stage(&self, s: &Stage) -> bool {
        s == self.terminal_stage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pipeline_advances_and_ends() {
        let c = Collective::new("t");
        assert_eq!(
            c.next_stage(&Stage::new("code")).unwrap().as_str(),
            "review"
        );
        assert_eq!(
            c.next_stage(&Stage::new("deploy")).unwrap().as_str(),
            "done"
        );
        assert!(c.next_stage(&Stage::new("done")).is_none());
        assert!(c.next_stage(&Stage::new("custom")).is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let c = Collective::new("payments");
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Collective = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
        assert!(text.contains("kind = \"file\""));
    }

    #[test]
    fn non_file_store_kinds_parse() {
        let text = r#"
name = "x"
created = "2026-10-02T21:10:00Z"
[store]
kind = "dynamodb"
url = "arn:aws:dynamodb:..."
"#;
        let c: Collective = toml::from_str(text).unwrap();
        assert_eq!(c.store.kind, StoreKind::Dynamodb);
    }
}
