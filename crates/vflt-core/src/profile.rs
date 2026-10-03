//! Profiles: a role an agent plays. `profiles/<name>.toml` plus
//! `profiles/<name>.md` (the system prompt).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::Stage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PermissionMode {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    #[serde(rename = "manual")]
    Manual,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Manual => "manual",
        }
    }
}

/// Advisory rules for the runner's permission classifier. The claude runner
/// maps these onto Claude Code's `autoMode` settings block.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Classifier {
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub soft_deny: Vec<String>,
    #[serde(default)]
    pub hard_deny: Vec<String>,
}

impl Classifier {
    pub fn is_empty(&self) -> bool {
        self.environment.is_empty()
            && self.allow.is_empty()
            && self.soft_deny.is_empty()
            && self.hard_deny.is_empty()
    }

    /// Merge `other` on top of self (lists are concatenated, deduplicated).
    pub fn merged(&self, other: &Classifier) -> Classifier {
        fn join(a: &[String], b: &[String]) -> Vec<String> {
            let mut out: Vec<String> = a.to_vec();
            for s in b {
                if !out.contains(s) {
                    out.push(s.clone());
                }
            }
            out
        }
        Classifier {
            environment: join(&self.environment, &other.environment),
            allow: join(&self.allow, &other.allow),
            soft_deny: join(&self.soft_deny, &other.soft_deny),
            hard_deny: join(&self.hard_deny, &other.hard_deny),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub classifier: Classifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_wall_clock")]
    pub wall_clock: String,
}

fn default_max_turns() -> u32 {
    60
}

fn default_wall_clock() -> String {
    "45m".to_string()
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_turns: default_max_turns(),
            wall_clock: default_wall_clock(),
        }
    }
}

impl Budget {
    pub fn wall_clock(&self) -> Option<std::time::Duration> {
        crate::duration::parse_opt(&self.wall_clock).ok().flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Stages this profile may claim. Empty for cycle-only profiles.
    #[serde(default)]
    pub stages: Vec<Stage>,
    #[serde(default = "default_runner")]
    pub runner: String,
    /// Command template for `shell`-style runners. Ignored by `claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_command: Option<String>,
    /// Empty: wake on work. "15m": run every 15 minutes regardless of items.
    #[serde(default)]
    pub cycle: String,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub budget: Budget,
    /// System prompt, from `<name>.md`. Not part of the toml.
    #[serde(skip)]
    pub prompt: String,
}

fn default_runner() -> String {
    "claude".to_string()
}

impl Profile {
    pub fn parse(toml_text: &str, prompt: &str) -> Result<Self> {
        let mut p: Profile = toml::from_str(toml_text)?;
        p.prompt = prompt.to_string();
        p.validate()?;
        Ok(p)
    }

    /// Load `<dir>/<name>.toml` and `<dir>/<name>.md`.
    pub fn load(dir: &Path, name: &str) -> Result<Self> {
        let toml_path = dir.join(format!("{name}.toml"));
        let md_path = dir.join(format!("{name}.md"));
        if !toml_path.exists() {
            return Err(Error::ProfileNotFound(name.to_string()));
        }
        let toml_text =
            std::fs::read_to_string(&toml_path).map_err(|e| Error::io(&toml_path, e))?;
        let prompt = match std::fs::read_to_string(&md_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Error::io(&md_path, e)),
        };
        let mut p = Profile::parse(&toml_text, &prompt)?;
        if p.name != name {
            return Err(Error::invalid(
                "profile",
                format!(
                    "{} declares name {:?} but file is {name}.toml",
                    toml_path.display(),
                    p.name
                ),
            ));
        }
        p.prompt = prompt;
        Ok(p)
    }

    pub fn load_all(dir: &Path) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::io(dir, e)),
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "toml") {
                    p.file_stem().map(|s| s.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        for n in names {
            out.push(Profile::load(dir, &n)?);
        }
        Ok(out)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::invalid("profile", "name is empty"));
        }
        crate::duration::parse_opt(&self.cycle)?;
        crate::duration::parse_opt(&self.budget.wall_clock)?;
        if self.stages.is_empty() && self.cycle().is_none() {
            return Err(Error::invalid(
                "profile",
                format!(
                    "{} has no stages and no cycle; it would never run",
                    self.name
                ),
            ));
        }
        Ok(())
    }

    pub fn cycle(&self) -> Option<std::time::Duration> {
        crate::duration::parse_opt(&self.cycle).ok().flatten()
    }

    pub fn handles(&self, stage: &Stage) -> bool {
        self.stages.iter().any(|s| s == stage)
    }

    pub fn is_cycle_only(&self) -> bool {
        self.stages.is_empty() && self.cycle().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_profile_parses_with_defaults() {
        let p = Profile::parse(
            r#"
name = "coder"
stages = ["code"]
"#,
            "You write code.",
        )
        .unwrap();
        assert_eq!(p.runner, "claude");
        assert_eq!(p.budget.max_turns, 60);
        assert_eq!(p.policy.permission_mode, PermissionMode::Auto);
        assert!(p.cycle().is_none());
        assert!(p.handles(&Stage::new("code")));
    }

    #[test]
    fn stageless_profile_needs_a_cycle() {
        assert!(Profile::parse("name = \"x\"\n", "").is_err());
        let p = Profile::parse("name = \"x\"\ncycle = \"15m\"\n", "").unwrap();
        assert!(p.is_cycle_only());
    }

    #[test]
    fn classifier_merges_without_duplicates() {
        let a = Classifier {
            allow: vec!["a".into()],
            ..Default::default()
        };
        let b = Classifier {
            allow: vec!["a".into(), "b".into()],
            hard_deny: vec!["h".into()],
            ..Default::default()
        };
        let m = a.merged(&b);
        assert_eq!(m.allow, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(m.hard_deny, vec!["h".to_string()]);
    }
}
