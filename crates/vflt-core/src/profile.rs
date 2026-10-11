//! Profiles: a role an agent plays. `profiles/<name>.toml` plus
//! `profiles/<name>.md` (the system prompt).
//!
//! A prompt may pull in shared text with a line of the form
//! `#include <path>`, the path relative to the collective root. Includes
//! nest, and are expanded every time the profile loads, so editing a shared
//! fragment changes every agent's next turn.

use std::collections::BTreeMap;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_wall_clock")]
    pub wall_clock: String,
    /// Spend cap per UTC day, in USD, for each agent running this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
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
            daily_usd: None,
        }
    }
}

impl Budget {
    pub fn wall_clock(&self) -> Option<std::time::Duration> {
        crate::duration::parse_opt(&self.wall_clock).ok().flatten()
    }
}

/// Where a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// A private directory per item under the collective's work dir.
    #[default]
    Item,
    /// `dir` itself, shared by every item and agent (an existing checkout).
    Shared,
    /// A git worktree of `dir` per item, on branch `vflt/<item>`.
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub mode: WorkspaceMode,
    /// For `shared` and `worktree`: the directory, relative to the collective
    /// root or absolute.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    /// More directories the runner may touch, resolved the same way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_dirs: Vec<String>,
}

/// Commands run around the agent's work. Each is a command template split
/// shlex-style and spawned directly, with the `VFLT_*` environment set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Hooks {
    /// Before claiming anything. A non-zero exit holds the agent (status
    /// `gated`, the first output line as its note) until a later check passes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate: Vec<String>,
    /// In the workspace, after the claim and before the runner. A non-zero
    /// exit releases the turn's items back to pending and holds the agent
    /// like a failed gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// In the workspace, after the runner exits. A non-zero exit adds the
    /// output to every item of the turn as a note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.gate.is_empty() && self.before.is_empty() && self.after.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Model for the runner (claude: `--model`). Empty: the runner's default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Empty: wake on work only. "15m": also run a cycle turn every 15
    /// minutes when no item is claimable (every 15 minutes regardless for a
    /// profile with no stages).
    #[serde(default)]
    pub cycle: String,
    /// Claim up to this many items into one turn.
    #[serde(default = "default_batch")]
    pub batch: u32,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub hooks: Hooks,
    /// Extra environment for runners and hooks.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// System prompt, from `<name>.md` with includes expanded. Not part of the
    /// toml.
    #[serde(skip)]
    pub prompt: String,
}

fn default_runner() -> String {
    "claude".to_string()
}

fn default_batch() -> u32 {
    1
}

const MAX_INCLUDE_DEPTH: usize = 16;

/// Expand `#include <path>` lines, paths relative to `base`.
pub fn expand_includes(text: &str, base: &Path) -> Result<String> {
    fn go(text: &str, base: &Path, depth: usize, out: &mut String) -> Result<()> {
        if depth > MAX_INCLUDE_DEPTH {
            return Err(Error::invalid(
                "profile",
                "#include nesting too deep (a cycle?)",
            ));
        }
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if let Some(rest) = trimmed.strip_prefix("#include ") {
                let path = base.join(rest.trim());
                let inner = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
                go(&inner.replace("\r\n", "\n"), base, depth + 1, out)?;
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            } else {
                out.push_str(line);
            }
        }
        Ok(())
    }
    let mut out = String::with_capacity(text.len());
    go(text, base, 0, &mut out)?;
    Ok(out)
}

impl Profile {
    pub fn parse(toml_text: &str, prompt: &str) -> Result<Self> {
        let mut p: Profile = toml::from_str(toml_text)?;
        p.prompt = prompt.to_string();
        p.validate()?;
        Ok(p)
    }

    /// Load `<dir>/<name>.toml` and `<dir>/<name>.md`, expanding includes
    /// relative to `base` (the collective root).
    pub fn load(dir: &Path, name: &str, base: &Path) -> Result<Self> {
        let toml_path = dir.join(format!("{name}.toml"));
        let md_path = dir.join(format!("{name}.md"));
        if !toml_path.exists() {
            return Err(Error::ProfileNotFound(name.to_string()));
        }
        let toml_text =
            std::fs::read_to_string(&toml_path).map_err(|e| Error::io(&toml_path, e))?;
        let prompt = match std::fs::read_to_string(&md_path) {
            Ok(s) => expand_includes(&s, base)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Error::io(&md_path, e)),
        };
        let p = Profile::parse(&toml_text, &prompt)?;
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
        Ok(p)
    }

    pub fn load_all(dir: &Path, base: &Path) -> Result<Vec<Self>> {
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
            out.push(Profile::load(dir, &n, base)?);
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
        if self.batch == 0 {
            return Err(Error::invalid(
                "profile",
                format!("{}: batch must be at least 1", self.name),
            ));
        }
        if self.workspace.mode != WorkspaceMode::Item && self.workspace.dir.trim().is_empty() {
            return Err(Error::invalid(
                "profile",
                format!(
                    "{}: workspace mode {:?} needs a workspace.dir",
                    self.name, self.workspace.mode
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
        assert_eq!(p.batch, 1);
        assert_eq!(p.workspace.mode, WorkspaceMode::Item);
        assert!(p.hooks.is_empty());
    }

    #[test]
    fn stageless_profile_needs_a_cycle() {
        assert!(Profile::parse("name = \"x\"\n", "").is_err());
        let p = Profile::parse("name = \"x\"\ncycle = \"15m\"\n", "").unwrap();
        assert!(p.is_cycle_only());
        let hybrid =
            Profile::parse("name = \"x\"\nstages = [\"code\"]\ncycle = \"2h\"\n", "").unwrap();
        assert!(!hybrid.is_cycle_only());
        assert!(hybrid.cycle().is_some());
    }

    #[test]
    fn shared_workspace_needs_a_dir() {
        let bad = "name = \"x\"\nstages = [\"code\"]\n[workspace]\nmode = \"shared\"\n";
        assert!(Profile::parse(bad, "").is_err());
        let ok = "name = \"x\"\nstages = [\"code\"]\nbatch = 3\nmodel = \"m\"\n[workspace]\nmode = \"shared\"\ndir = \"..\"\nadd_dirs = [\"../other\"]\n[hooks]\ngate = [\"check-disk\"]\n[env]\nA = \"1\"\n";
        let p = Profile::parse(ok, "").unwrap();
        assert_eq!(p.workspace.dir, "..");
        assert_eq!(p.batch, 3);
        assert_eq!(p.model, "m");
        assert_eq!(p.hooks.gate, vec!["check-disk".to_string()]);
        assert_eq!(p.env.get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn includes_expand_recursively_and_fail_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("frag")).unwrap();
        std::fs::write(tmp.path().join("frag/a.md"), "A1\n#include frag/b.md\nA2").unwrap();
        std::fs::write(tmp.path().join("frag/b.md"), "B\n").unwrap();
        let out = expand_includes("top\n#include frag/a.md\nend\n", tmp.path()).unwrap();
        assert_eq!(out, "top\nA1\nB\nA2\nend\n");
        assert!(expand_includes("#include nope.md\n", tmp.path()).is_err());
        std::fs::write(tmp.path().join("frag/loop.md"), "#include frag/loop.md\n").unwrap();
        assert!(expand_includes("#include frag/loop.md\n", tmp.path()).is_err());
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
