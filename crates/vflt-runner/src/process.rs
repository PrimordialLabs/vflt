//! Spawning a child with the prompt on stdin, a wall-clock budget enforced by
//! `Child::kill`, and line-by-line capture of stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Pids of children currently running, so an abort can stop them.
fn live() -> &'static Mutex<Vec<u32>> {
    static LIVE: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Kill every child this process is running (and, on Unix, its process
/// group). For a second interrupt: the first one lets turns finish.
pub fn abort_children() {
    let pids: Vec<u32> = live().lock().map(|v| v.clone()).unwrap_or_default();
    for pid in pids {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

use anyhow::{Context, Result};

pub struct Captured {
    pub lines: Vec<String>,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Run `argv[0] argv[1..]` in `cwd`, feeding `stdin_text`, calling `on_line`
/// for each stdout line (CRLF tolerant). Kills the child when `wall_clock`
/// elapses.
pub fn run_capture(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin_text: &str,
    wall_clock: Option<Duration>,
    mut on_line: impl FnMut(&str),
) -> Result<Captured> {
    let (program, args) = argv.split_first().context("empty argv")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Its own process group: an interrupt aimed at the orchestrator (a
    // terminal Ctrl-C) lets the turn finish instead of killing it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;
    let pid = child.id();
    if let Ok(mut v) = live().lock() {
        v.push(pid);
    }

    let mut stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let prompt = stdin_text.to_string();
    let writer = std::thread::spawn(move || {
        // The child may exit before reading everything; that is not an error.
        let _ = stdin.write_all(prompt.as_bytes());
        drop(stdin);
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut s = String::new();
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            s.push_str(&line);
            s.push('\n');
        }
        s
    });

    let child = Arc::new(Mutex::new(child));
    let timed_out = Arc::new(Mutex::new(false));
    let watchdog = wall_clock.map(|limit| {
        let child = Arc::clone(&child);
        let timed_out = Arc::clone(&timed_out);
        let start = Instant::now();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            let mut c = child.lock().unwrap();
            match c.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() >= limit => {
                    *timed_out.lock().unwrap() = true;
                    let _ = c.kill();
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
        })
    });

    let mut lines = Vec::new();
    for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
        let line = line.trim_end_matches('\r').to_string();
        on_line(&line);
        lines.push(line);
    }

    let status = child.lock().unwrap().wait().context("waiting for child");
    if let Ok(mut v) = live().lock() {
        v.retain(|p| *p != pid);
    }
    let status = status?;
    let _ = writer.join();
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let stderr = stderr_reader.join().unwrap_or_default();
    let timed_out = *timed_out.lock().unwrap();
    Ok(Captured {
        lines,
        stderr,
        exit_code: status.code().unwrap_or(if timed_out { 124 } else { -1 }),
        timed_out,
    })
}

/// Keep the last `n` lines of a transcript.
pub fn tail(lines: &[String], n: usize) -> String {
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
