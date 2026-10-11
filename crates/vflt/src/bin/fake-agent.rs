//! Test fixture: a tiny "agent" the shell runner can launch. It reads the
//! prompt on stdin, writes it to `prompt-received.md` in its cwd, and then
//! drives the real `vflt` binary (path in `VFLT_BIN`, default `vflt`) the way
//! an agent would, on every item of the turn (`VFLT_ITEMS`). `FAKE_AGENT_MODE`
//! selects the behaviour:
//!
//! * `complete` (default): add a note, complete each item
//! * `bounce`: complete --bounce with a note
//! * `block`: block each item
//! * `raise`: raise each item
//! * `wait`: park each item until `FAKE_WAIT_ON` is done
//! * `noop`: exit 0 without transitioning (the loop must raise the items)
//! * `fail`: exit 2 without transitioning
//! * `hang`: sleep 30s (used to test the wall-clock kill)
//! * `slow`: sleep 3s, then behave like `complete` (used to test graceful stops)
//! * `cycle`: print a summary and exit (for cycle turns)
//! * `file`: (cycle turn) file one item for this agent, then exit
//! * `snooze`: (cycle turn) snooze this agent, then exit
//!
//! With `FAKE_AGENT_ENV_OUT` set, the environment it saw is written there.

use std::io::Read;
use std::process::Command;

fn main() {
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let _ = std::fs::write("prompt-received.md", &prompt);
    if let Ok(path) = std::env::var("FAKE_AGENT_ENV_OUT") {
        let mut env: Vec<String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("VFLT_") || k.starts_with("FAKE_") || k == "EXTRA")
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        env.sort();
        env.push(format!(
            "CWD={}",
            std::env::current_dir().unwrap().display()
        ));
        let _ = std::fs::write(path, env.join("\n"));
    }

    let mode = std::env::var("FAKE_AGENT_MODE").unwrap_or_else(|_| "complete".into());
    let vflt = std::env::var("VFLT_BIN").unwrap_or_else(|_| "vflt".into());
    let items: Vec<String> = std::env::var("VFLT_ITEMS")
        .or_else(|_| std::env::var("VFLT_ITEM"))
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    let run = |args: &[&str]| {
        let status = Command::new(&vflt)
            .args(args)
            .status()
            .unwrap_or_else(|e| panic!("fake-agent: failed to run {vflt}: {e}"));
        if !status.success() {
            eprintln!("fake-agent: vflt {} failed with {status}", args.join(" "));
            std::process::exit(3);
        }
    };

    let mode = if mode == "slow" {
        std::thread::sleep(std::time::Duration::from_secs(3));
        "complete".to_string()
    } else {
        mode
    };
    match mode.as_str() {
        "complete" => {
            for item in &items {
                run(&[
                    "item",
                    "note",
                    item,
                    "--body",
                    "fake-agent: looked at the spec and did the work",
                ]);
                run(&["item", "complete", item, "--note", "fake-agent: done"]);
            }
            println!("fake-agent completed {}", items.join(","));
        }
        "bounce" => {
            for item in &items {
                run(&[
                    "item",
                    "complete",
                    item,
                    "--bounce",
                    "--note",
                    "fake-agent: findings",
                ]);
            }
            println!("fake-agent bounced {}", items.join(","));
        }
        "block" => {
            for item in &items {
                run(&[
                    "item",
                    "block",
                    item,
                    "--on",
                    "fake-agent: waiting on vf-other",
                ]);
            }
        }
        "raise" => {
            for item in &items {
                run(&[
                    "item",
                    "raise",
                    item,
                    "--question",
                    "fake-agent: which database?",
                ]);
            }
        }
        "wait" => {
            let on = std::env::var("FAKE_WAIT_ON").expect("FAKE_WAIT_ON");
            for item in &items {
                run(&[
                    "item",
                    "wait",
                    item,
                    "--on",
                    &on,
                    "--note",
                    "fake-agent: shipped, awaiting the probe",
                ]);
            }
        }
        "noop" => println!("fake-agent did nothing"),
        "fail" => {
            eprintln!("fake-agent: simulated failure");
            std::process::exit(2);
        }
        "hang" => std::thread::sleep(std::time::Duration::from_secs(30)),
        "cycle" => println!("fake-agent cycle: board looks fine"),
        "file" => {
            let me = std::env::var("VFLT_AGENT").unwrap_or_default();
            run(&[
                "item",
                "add",
                "--title",
                "self-filed scope work",
                "--assign",
                &me,
                "--spec",
                "do the next thing",
            ]);
            println!("fake-agent cycle: filed the next piece of work");
        }
        "snooze" => {
            run(&["agent", "snooze"]);
            println!("fake-agent cycle: caught up");
        }
        other => {
            eprintln!("fake-agent: unknown mode {other}");
            std::process::exit(4);
        }
    }
}
