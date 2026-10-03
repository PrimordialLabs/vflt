//! Headless Claude Code runner: `claude -p` with the profile policy.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use vflt_core::profile::Policy;

use crate::process::{run_capture, tail};
use crate::{Availability, PermissionDenial, RunOutcome, RunRequest, Runner};

#[derive(Debug, Clone)]
pub struct ClaudeRunner {
    /// Program name or path; `claude` by default.
    pub program: String,
    /// Extra args appended verbatim (e.g. `--model`).
    pub extra_args: Vec<String>,
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        ClaudeRunner {
            program: "claude".to_string(),
            extra_args: vec![],
        }
    }
}

impl ClaudeRunner {
    /// The `autoMode` settings block Claude Code reads for its permission
    /// classifier, built from the policy's classifier rules. Empty when the
    /// policy has none, so the user's own settings stay untouched.
    pub fn settings_json(policy: &Policy) -> Value {
        let mut settings = json!({});
        let c = &policy.classifier;
        if !c.is_empty() {
            let mut auto = serde_json::Map::new();
            if !c.environment.is_empty() {
                auto.insert("environment".into(), json!(c.environment));
            }
            if !c.allow.is_empty() {
                auto.insert("allow".into(), json!(c.allow));
            }
            if !c.soft_deny.is_empty() {
                auto.insert("soft_deny".into(), json!(c.soft_deny));
            }
            if !c.hard_deny.is_empty() {
                auto.insert("hard_deny".into(), json!(c.hard_deny));
            }
            settings["autoMode"] = Value::Object(auto);
        }
        if !policy.disallowed_tools.is_empty() {
            settings["permissions"] = json!({ "deny": policy.disallowed_tools });
        }
        settings
    }

    /// Build the argv (without the program) for a request. The prompt itself
    /// goes on stdin; the system prompt goes through `--append-system-prompt-file`.
    pub fn argv(&self, req: &RunRequest, system_prompt_file: &std::path::Path) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            req.policy.permission_mode.as_str().into(),
            "--permission-prompts".into(),
            "none".into(),
            "--append-system-prompt-file".into(),
            system_prompt_file.to_string_lossy().into_owned(),
            "--max-turns".into(),
            req.budget.max_turns.to_string(),
        ];
        if !req.policy.allowed_tools.is_empty() {
            a.push("--allowedTools".into());
            a.push(req.policy.allowed_tools.join(","));
        }
        if !req.policy.disallowed_tools.is_empty() {
            a.push("--disallowedTools".into());
            a.push(req.policy.disallowed_tools.join(","));
        }
        for d in &req.extra_dirs {
            a.push("--add-dir".into());
            a.push(d.to_string_lossy().into_owned());
        }
        let settings = Self::settings_json(&req.policy);
        if settings.as_object().is_some_and(|o| !o.is_empty()) {
            a.push("--settings".into());
            a.push(settings.to_string());
        }
        if let Some(s) = &req.session {
            a.push("--resume".into());
            a.push(s.clone());
        }
        a.extend(self.extra_args.iter().cloned());
        a
    }
}

impl Runner for ClaudeRunner {
    fn name(&self) -> &str {
        "claude"
    }

    fn available(&self) -> Result<Availability> {
        Ok(match which::which(&self.program) {
            Ok(path) => Availability::Available { path },
            Err(e) => Availability::Missing {
                reason: format!("{} not found on PATH: {e}", self.program),
            },
        })
    }

    fn run(&self, req: RunRequest) -> Result<RunOutcome> {
        std::fs::create_dir_all(&req.cwd)
            .with_context(|| format!("creating {}", req.cwd.display()))?;
        let sys_path: PathBuf = req.cwd.join(".vflt-system-prompt.md");
        std::fs::write(&sys_path, &req.system_prompt).context("writing system prompt file")?;

        // Resolve through `which` so PATHEXT shims (claude.cmd on Windows) spawn correctly.
        let program = which::which(&self.program)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.program.clone());
        let mut argv = vec![program];
        argv.extend(self.argv(&req, &sys_path));

        let mut log = match &req.log_path {
            Some(p) => {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d)?;
                }
                Some(std::fs::File::create(p)?)
            }
            None => None,
        };

        let mut outcome = RunOutcome::default();
        let mut assistant_lines: Vec<String> = Vec::new();
        let captured = run_capture(
            &argv,
            &req.cwd,
            &req.env,
            &req.prompt,
            req.budget.wall_clock(),
            |line| {
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{line}");
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    absorb_event(&v, &mut outcome, &mut assistant_lines);
                }
            },
        )?;
        let _ = std::fs::remove_file(&sys_path);

        outcome.exit_code = captured.exit_code;
        outcome.timed_out = captured.timed_out;
        if outcome.result_text.is_empty() && !captured.stderr.trim().is_empty() {
            outcome.result_text = captured.stderr.trim().to_string();
        }
        if captured.exit_code != 0 || captured.timed_out {
            outcome.is_error = true;
        }
        outcome.transcript_tail = tail(&assistant_lines, 60);
        Ok(outcome)
    }
}

/// Fold one stream-json event into the outcome.
fn absorb_event(v: &Value, out: &mut RunOutcome, assistant_lines: &mut Vec<String>) {
    match v.get("type").and_then(Value::as_str) {
        Some("result") => {
            if let Some(t) = v.get("result").and_then(Value::as_str) {
                out.result_text = t.to_string();
            }
            out.session_id = v
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            out.cost_usd = v.get("total_cost_usd").and_then(Value::as_f64);
            out.num_turns = v.get("num_turns").and_then(Value::as_u64).map(|n| n as u32);
            out.is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            if let Some(arr) = v.get("permission_denials").and_then(Value::as_array) {
                for d in arr {
                    out.permission_denials.push(PermissionDenial {
                        tool: d
                            .get("tool_name")
                            .or(d.get("tool"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        reason: d.get("reason").and_then(Value::as_str).map(str::to_string),
                        input: d.get("tool_input").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
        Some("permission_denied") => {
            out.permission_denials.push(PermissionDenial {
                tool: v
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                reason: v.get("reason").and_then(Value::as_str).map(str::to_string),
                input: v.get("input").cloned().unwrap_or(Value::Null),
            });
        }
        Some("assistant") => {
            if let Some(content) = v.pointer("/message/content").and_then(Value::as_array) {
                for block in content {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                assistant_lines.extend(t.lines().map(str::to_string));
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let input = block
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_default();
                            let short: String = input.chars().take(160).collect();
                            assistant_lines.push(format!("[tool_use {name}] {short}"));
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vflt_core::profile::{Budget, Classifier, PermissionMode};

    fn req() -> RunRequest {
        RunRequest {
            system_prompt: "sys".into(),
            prompt: "do it".into(),
            cwd: PathBuf::from("ws"),
            extra_dirs: vec![PathBuf::from("dest")],
            policy: Policy {
                permission_mode: PermissionMode::Auto,
                allowed_tools: vec!["Read".into(), "Bash(vflt *)".into()],
                disallowed_tools: vec!["Bash(rm -rf *)".into()],
                classifier: Classifier {
                    environment: vec!["env".into()],
                    allow: vec!["a".into()],
                    soft_deny: vec!["s".into()],
                    hard_deny: vec!["h".into()],
                },
            },
            budget: Budget {
                max_turns: 7,
                wall_clock: "5m".into(),
            },
            env: vec![],
            log_path: None,
            session: None,
        }
    }

    #[test]
    fn argv_carries_policy_and_budget() {
        let r = ClaudeRunner::default();
        let a = r.argv(&req(), std::path::Path::new("sys.md"));
        let s = a.join(" ");
        assert!(a.starts_with(&[
            "-p".to_string(),
            "--output-format".into(),
            "stream-json".into()
        ]));
        assert!(s.contains("--permission-mode auto"));
        assert!(s.contains("--permission-prompts none"));
        assert!(s.contains("--append-system-prompt-file sys.md"));
        assert!(s.contains("--max-turns 7"));
        assert!(s.contains("--allowedTools Read,Bash(vflt *)"));
        assert!(s.contains("--disallowedTools Bash(rm -rf *)"));
        assert!(s.contains("--add-dir dest"));
        let idx = a.iter().position(|x| x == "--settings").unwrap();
        let settings: Value = serde_json::from_str(&a[idx + 1]).unwrap();
        assert_eq!(settings["autoMode"]["environment"][0], "env");
        assert_eq!(settings["autoMode"]["allow"][0], "a");
        assert_eq!(settings["autoMode"]["soft_deny"][0], "s");
        assert_eq!(settings["autoMode"]["hard_deny"][0], "h");
        assert_eq!(settings["permissions"]["deny"][0], "Bash(rm -rf *)");
    }

    #[test]
    fn empty_policy_passes_no_settings() {
        let r = ClaudeRunner::default();
        let mut q = req();
        q.policy = Policy::default();
        let a = r.argv(&q, std::path::Path::new("sys.md"));
        assert!(!a.contains(&"--settings".to_string()));
        assert!(!a.contains(&"--allowedTools".to_string()));
    }

    #[test]
    fn result_event_is_absorbed() {
        let mut out = RunOutcome::default();
        let mut lines = vec![];
        absorb_event(
            &json!({"type":"assistant","message":{"content":[{"type":"text","text":"hi\nthere"},{"type":"tool_use","name":"Bash","input":{"command":"vflt board"}}]}}),
            &mut out,
            &mut lines,
        );
        absorb_event(
            &json!({"type":"result","result":"done","session_id":"s1","total_cost_usd":0.12,"num_turns":3,"is_error":false,
                     "permission_denials":[{"tool_name":"Bash","tool_input":{"command":"rm -rf /"}}]}),
            &mut out,
            &mut lines,
        );
        assert_eq!(out.result_text, "done");
        assert_eq!(out.session_id.as_deref(), Some("s1"));
        assert_eq!(out.cost_usd, Some(0.12));
        assert_eq!(out.num_turns, Some(3));
        assert_eq!(out.permission_denials.len(), 1);
        assert_eq!(out.permission_denials[0].tool, "Bash");
        assert_eq!(lines.len(), 3);
        assert!(lines[2].starts_with("[tool_use Bash]"));
    }
}
