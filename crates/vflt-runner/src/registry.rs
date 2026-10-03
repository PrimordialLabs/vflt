//! Runner lookup by name. Built-ins: `claude`, `shell`. Custom runners come
//! from the user config file `<config_dir>/vflt/config.toml`:
//!
//! ```toml
//! [runners.codex]
//! command = "codex exec --full-auto"
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::{ClaudeRunner, Runner, ShellRunner};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default)]
    pub runners: BTreeMap<String, RunnerConfig>,
    /// Override the program used by the claude runner (name or path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_program: Option<String>,
    #[serde(default)]
    pub claude_extra_args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub config: UserConfig,
}

impl Registry {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("vflt").join("config.toml"))
    }

    /// Load the user config if present; an absent file is an empty config.
    pub fn load() -> Result<Self> {
        let Some(path) = Self::config_path() else {
            return Ok(Registry::default());
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Registry {
                config: toml::from_str(&text)?,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve a runner. `profile_command` is the profile's `runner_command`,
    /// used for the built-in `shell` runner or to override a named runner.
    pub fn get(&self, name: &str, profile_command: Option<&str>) -> Result<Box<dyn Runner>> {
        match name {
            "claude" => Ok(Box::new(ClaudeRunner {
                program: self
                    .config
                    .claude_program
                    .clone()
                    .unwrap_or_else(|| "claude".into()),
                extra_args: self.config.claude_extra_args.clone(),
            })),
            "shell" => match profile_command {
                Some(cmd) => Ok(Box::new(ShellRunner::new("shell", cmd))),
                None => bail!("runner `shell` needs `runner_command` in the profile"),
            },
            other => {
                if let Some(cmd) = profile_command {
                    return Ok(Box::new(ShellRunner::new(other, cmd)));
                }
                match self.config.runners.get(other) {
                    Some(rc) => Ok(Box::new(ShellRunner::new(other, rc.command.clone()))),
                    None => bail!(
                        "unknown runner `{other}`; built-ins are `claude` and `shell`, custom runners go in {}",
                        Self::config_path().map(|p| p.display().to_string()).unwrap_or_else(|| "the vflt config".into())
                    ),
                }
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut v = vec!["claude".to_string(), "shell".to_string()];
        v.extend(self.config.runners.keys().cloned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtins_and_custom() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("config.toml");
        std::fs::write(
            &p,
            "[runners.codex]\ncommand = \"codex exec --full-auto\"\n",
        )
        .unwrap();
        let r = Registry::load_from(&p).unwrap();
        assert_eq!(r.get("claude", None).unwrap().name(), "claude");
        assert_eq!(r.get("codex", None).unwrap().name(), "codex");
        assert_eq!(r.get("shell", Some("echo hi")).unwrap().name(), "shell");
        assert!(r.get("shell", None).is_err());
        assert!(r.get("nope", None).is_err());
        assert_eq!(r.names(), vec!["claude", "shell", "codex"]);
        let empty = Registry::load_from(&tmp.path().join("missing.toml")).unwrap();
        assert!(empty.config.runners.is_empty());
    }
}
