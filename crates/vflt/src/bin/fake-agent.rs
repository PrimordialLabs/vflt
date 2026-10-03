//! Test fixture: a tiny "agent" the shell runner can launch. It reads the
//! prompt on stdin, writes it to `prompt-received.md` in its cwd, and then
//! drives the real `vflt` binary (path in `VFLT_BIN`, default `vflt`) the way
//! an agent would. `FAKE_AGENT_MODE` selects the behaviour:
//!
//! * `complete` (default): add a note, complete the item
//! * `bounce`: complete --bounce with a note
//! * `block`: block the item
//! * `raise`: raise the item
//! * `noop`: exit 0 without transitioning (the loop must mark needs_human)
//! * `fail`: exit 2 without transitioning
//! * `hang`: sleep 30s (used to test the wall-clock kill)
//! * `cycle`: print a summary and exit (for cycle profiles)

use std::io::Read;
use std::process::Command;

fn main() {
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let _ = std::fs::write("prompt-received.md", &prompt);

    let mode = std::env::var("FAKE_AGENT_MODE").unwrap_or_else(|_| "complete".into());
    let vflt = std::env::var("VFLT_BIN").unwrap_or_else(|_| "vflt".into());
    let item = std::env::var("VFLT_ITEM").unwrap_or_default();

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

    match mode.as_str() {
        "complete" => {
            run(&[
                "item",
                "note",
                &item,
                "--body",
                "fake-agent: looked at the spec and did the work",
            ]);
            run(&["item", "complete", &item, "--note", "fake-agent: done"]);
            println!("fake-agent completed {item}");
        }
        "bounce" => {
            run(&[
                "item",
                "complete",
                &item,
                "--bounce",
                "--note",
                "fake-agent: findings",
            ]);
            println!("fake-agent bounced {item}");
        }
        "block" => run(&[
            "item",
            "block",
            &item,
            "--on",
            "fake-agent: waiting on vf-other",
        ]),
        "raise" => run(&[
            "item",
            "raise",
            &item,
            "--question",
            "fake-agent: which database?",
        ]),
        "noop" => println!("fake-agent did nothing"),
        "fail" => {
            eprintln!("fake-agent: simulated failure");
            std::process::exit(2);
        }
        "hang" => std::thread::sleep(std::time::Duration::from_secs(30)),
        "cycle" => println!("fake-agent cycle: board looks fine"),
        other => {
            eprintln!("fake-agent: unknown mode {other}");
            std::process::exit(4);
        }
    }
}
