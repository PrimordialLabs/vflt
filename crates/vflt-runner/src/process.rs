//! Spawning a child with the prompt on stdin, a wall-clock budget enforced by
//! `Child::kill`, and line-by-line capture of stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

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

    let status = child.lock().unwrap().wait().context("waiting for child")?;
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
