//! A runner that spawns an arbitrary command template. The template is split
//! shlex-style into argv and executed directly, never via a shell. The prompt
//! arrives on stdin; `VFLT_PROMPT_FILE` and `VFLT_SYSTEM_PROMPT_FILE` point at
//! files holding the prompt and system prompt for runners that prefer paths.

use anyhow::{bail, Context, Result};

use crate::process::{run_capture, tail};
use crate::{Availability, RunOutcome, RunRequest, Runner};

#[derive(Debug, Clone)]
pub struct ShellRunner {
    pub name: String,
    pub command: String,
}

impl ShellRunner {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        ShellRunner {
            name: name.into(),
            command: command.into(),
        }
    }

    /// Split the command template into argv without invoking a shell. POSIX
    /// shell-style on Unix; on Windows, whitespace-separated with double or
    /// single quotes and no backslash escapes, so `C:\path\agent.exe` survives.
    pub fn argv(&self) -> Result<Vec<String>> {
        let split = if cfg!(windows) {
            split_windows(&self.command)
        } else {
            shlex::split(&self.command)
        };
        let argv = split.with_context(|| {
            format!(
                "runner {}: cannot parse command template {:?}",
                self.name, self.command
            )
        })?;
        if argv.is_empty() {
            bail!("runner {}: empty command template", self.name);
        }
        Ok(argv)
    }
}

/// Windows template splitter: whitespace separates arguments, a run opened
/// with `"` or `'` closes only on the same character, and backslashes are
/// ordinary characters. Returns `None` on an unbalanced quote.
fn split_windows(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut had = false;
    for c in s.chars() {
        match (c, quote) {
            (q, Some(open)) if q == open => quote = None,
            ('"' | '\'', None) => {
                quote = Some(c);
                had = true;
            }
            (c, None) if c.is_whitespace() => {
                if had {
                    out.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            (c, _) => {
                cur.push(c);
                had = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if had {
        out.push(cur);
    }
    Some(out)
}

impl Runner for ShellRunner {
    fn name(&self) -> &str {
        &self.name
    }

    fn available(&self) -> Result<Availability> {
        let argv = self.argv()?;
        let program = std::path::Path::new(&argv[0]);
        if program.components().count() > 1 {
            return Ok(if program.is_file() {
                Availability::Available {
                    path: program.to_path_buf(),
                }
            } else {
                Availability::Missing {
                    reason: format!("{} does not exist", program.display()),
                }
            });
        }
        Ok(match which::which(&argv[0]) {
            Ok(path) => Availability::Available { path },
            Err(e) => Availability::Missing {
                reason: format!("{} not found on PATH: {e}", argv[0]),
            },
        })
    }

    fn run(&self, req: RunRequest) -> Result<RunOutcome> {
        std::fs::create_dir_all(&req.cwd)
            .with_context(|| format!("creating {}", req.cwd.display()))?;
        let prompt_path = req.cwd.join(".vflt-prompt.md");
        let sys_path = req.cwd.join(".vflt-system-prompt.md");
        std::fs::write(&prompt_path, &req.prompt)?;
        std::fs::write(&sys_path, &req.system_prompt)?;

        let mut env = req.env.clone();
        env.push((
            "VFLT_PROMPT_FILE".into(),
            prompt_path.to_string_lossy().into_owned(),
        ));
        env.push((
            "VFLT_SYSTEM_PROMPT_FILE".into(),
            sys_path.to_string_lossy().into_owned(),
        ));

        let mut argv = self.argv()?;
        if std::path::Path::new(&argv[0]).components().count() == 1 {
            if let Ok(p) = which::which(&argv[0]) {
                argv[0] = p.to_string_lossy().into_owned();
            }
        }
        let mut log = match &req.log_path {
            Some(p) => {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d)?;
                }
                Some(std::fs::File::create(p)?)
            }
            None => None,
        };
        let captured = run_capture(
            &argv,
            &req.cwd,
            &env,
            &req.prompt,
            req.budget.wall_clock(),
            |line| {
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{line}");
                }
            },
        )?;
        let _ = std::fs::remove_file(&prompt_path);
        let _ = std::fs::remove_file(&sys_path);

        let mut result_text = captured.lines.join("\n");
        if result_text.trim().is_empty() {
            result_text = captured.stderr.trim().to_string();
        }
        Ok(RunOutcome {
            result_text,
            session_id: None,
            cost_usd: None,
            num_turns: None,
            permission_denials: vec![],
            exit_code: captured.exit_code,
            timed_out: captured.timed_out,
            is_error: captured.exit_code != 0 || captured.timed_out,
            transcript_tail: tail(&captured.lines, 60),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_splits_shlex_style() {
        let r = ShellRunner::new("x", r#"my-agent --flag "two words" plain"#);
        assert_eq!(
            r.argv().unwrap(),
            vec!["my-agent", "--flag", "two words", "plain"]
        );
        assert!(ShellRunner::new("x", "").argv().is_err());
    }

    #[test]
    fn windows_splitter_keeps_backslashes_and_both_quote_styles() {
        assert_eq!(
            split_windows(r#"C:\p\agent.exe "my agent.py" --flag 'x y'"#).unwrap(),
            vec![r"C:\p\agent.exe", "my agent.py", "--flag", "x y"]
        );
        assert_eq!(
            split_windows(r#"a "it's" 'say "hi"'"#).unwrap(),
            vec!["a", "it's", r#"say "hi""#]
        );
        assert!(split_windows("unbalanced 'quote").is_none());
        assert_eq!(split_windows(r#""""#).unwrap(), vec![""]);
    }

    #[test]
    fn missing_program_is_reported_not_fatal() {
        let r = ShellRunner::new("x", "definitely-not-a-real-program-vflt");
        assert!(!r.available().unwrap().is_available());
    }
}
