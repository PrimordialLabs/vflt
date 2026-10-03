//! Thin git shell-out used by the file store when `store.git = true`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

const RETRIES: u32 = 6;
const RETRY_DELAY_MS: u64 = 120;

fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(["-c", "user.name=vflt", "-c", "user.email=vflt@localhost"])
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| Error::Git(format!("failed to run git: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(Error::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// The repository that contains `dir`, if any.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

/// Make sure `dir` is inside a git repository, initialising one at `dir` when
/// it is not. Returns the repository toplevel.
pub fn ensure_repo(dir: &Path) -> Result<PathBuf> {
    if let Some(top) = toplevel(dir) {
        return Ok(top);
    }
    git(dir, &["init", "-q"])?;
    toplevel(dir).ok_or_else(|| Error::Git("git init succeeded but no toplevel found".into()))
}

/// Stage `paths` and commit with `message`. Retries on index.lock contention
/// so concurrent agents do not fail each other. Returns `Ok(false)` when
/// there was nothing to commit.
pub fn commit(repo: &Path, paths: &[PathBuf], message: &str) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let mut add_args: Vec<String> = vec!["add".into(), "-A".into(), "--".into()];
    for p in paths {
        add_args.push(p.to_string_lossy().to_string());
    }
    let add_refs: Vec<&str> = add_args.iter().map(String::as_str).collect();
    let mut last = None;
    for attempt in 0..RETRIES {
        match git(repo, &add_refs).and_then(|_| {
            // commit only the staged paths; -q keeps agents' logs clean
            git(
                repo,
                &["commit", "-q", "-m", message, "--only", "--"]
                    .iter()
                    .copied()
                    .chain(paths.iter().map(|p| p.to_str().unwrap_or("")))
                    .collect::<Vec<_>>(),
            )
        }) {
            Ok(_) => return Ok(true),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("nothing to commit") || msg.contains("no changes added") {
                    return Ok(false);
                }
                last = Some(e);
                if attempt + 1 < RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(
                        RETRY_DELAY_MS * (attempt as u64 + 1),
                    ));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Git("commit failed".into())))
}

pub fn is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
