//! vflt-runner: the `Runner` trait and the runners that ship with vflt.
//!
//! A runner executes one agent turn as a child process. `claude` (headless
//! Claude Code) is the default; `shell`-style runners spawn any command
//! template, split shlex-style and executed directly (never through a shell).

pub mod claude;
pub mod process;
pub mod registry;
pub mod shell;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use vflt_core::profile::{Budget, Policy};

pub use claude::ClaudeRunner;
pub use registry::{Registry, RunnerConfig};
pub use shell::ShellRunner;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Available { path: PathBuf },
    Missing { reason: String },
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available { .. })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunRequest {
    /// Appended to the runner's own system prompt (profile prompt + guide).
    pub system_prompt: String,
    /// The turn's instruction (item spec, notes, context).
    pub prompt: String,
    pub cwd: PathBuf,
    /// Directories the runner may touch beyond `cwd`.
    pub extra_dirs: Vec<PathBuf>,
    pub policy: Policy,
    pub budget: Budget,
    /// Environment variables for the child (VFLT_ITEM, VFLT_COLLECTIVE, ...).
    pub env: Vec<(String, String)>,
    /// Where to mirror the raw transcript (jsonl for claude, stdout for shell).
    pub log_path: Option<PathBuf>,
    /// Resume a prior session when the runner supports it.
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionDenial {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunOutcome {
    pub result_text: String,
    pub session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub permission_denials: Vec<PermissionDenial>,
    pub exit_code: i32,
    pub timed_out: bool,
    pub is_error: bool,
    /// Last few hundred lines of assistant text, for notes on failure.
    pub transcript_tail: String,
}

pub trait Runner: Send + Sync {
    fn name(&self) -> &str;
    fn available(&self) -> anyhow::Result<Availability>;
    fn run(&self, req: RunRequest) -> anyhow::Result<RunOutcome>;
}
