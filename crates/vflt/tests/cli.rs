//! CLI smoke tests and the end-to-end agent loop with the fake-agent fixture.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const VFLT: &str = env!("CARGO_BIN_EXE_vflt");
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");

struct Coll {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Coll {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".vflt");
        let out = Command::new(VFLT)
            .args(["collective", "init", "--no-git", "--name", "t"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Coll { _tmp: tmp, root }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(VFLT);
        c.arg("--collective").arg(&self.root);
        c.env_remove("VFLT_AGENT");
        c.env("USER", "tester");
        c.env("USERNAME", "tester");
        c
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "vflt {} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.ok(&a)).expect("valid json")
    }

    fn fails(&self, args: &[&str]) -> Output {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            !out.status.success(),
            "vflt {} unexpectedly succeeded",
            args.join(" ")
        );
        out
    }

    fn add(&self, title: &str, stage: &str) -> String {
        let v = self.json(&[
            "item",
            "add",
            "--title",
            title,
            "--stage",
            stage,
            "--spec",
            "do the thing",
        ]);
        v["id"].as_str().unwrap().to_string()
    }

    /// Install a profile that runs the fake agent through the shell runner.
    fn fake_profile(&self, name: &str, stages: &str, cycle: &str, wall_clock: &str) {
        let exe = Path::new(FAKE_AGENT).to_string_lossy().replace('\\', "/");
        let toml = format!(
            "name = \"{name}\"\nstages = {stages}\nrunner = \"shell\"\nrunner_command = \"\\\"{exe}\\\"\"\ncycle = \"{cycle}\"\n[budget]\nmax_turns = 5\nwall_clock = \"{wall_clock}\"\n"
        );
        std::fs::write(
            self.root.join("profiles").join(format!("{name}.toml")),
            toml,
        )
        .unwrap();
        std::fs::write(
            self.root.join("profiles").join(format!("{name}.md")),
            "You are a fake agent.\n",
        )
        .unwrap();
    }

    fn run_agent(&self, profile: &str, mode: &str) -> Output {
        self.cmd()
            .args([
                "agent",
                "run",
                "--profile",
                profile,
                "--once",
                "--name",
                "fake-1",
            ])
            .env("VFLT_BIN", VFLT)
            .env("FAKE_AGENT_MODE", mode)
            .output()
            .unwrap()
    }
}

#[test]
fn init_show_and_profiles() {
    let c = Coll::new();
    let show = c.json(&["collective", "show"]);
    assert_eq!(show["collective"]["name"], "t");
    assert_eq!(show["collective"]["store"]["kind"], "file");
    assert_eq!(show["collective"]["store"]["git"], false);
    let profiles = c.json(&["profiles"]);
    let names: Vec<&str> = profiles
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"coder") && names.contains(&"supervisor"));
    // second init at the same path fails
    let out = Command::new(VFLT)
        .args(["collective", "init", "--no-git"])
        .arg(&c.root)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn item_lifecycle_through_cli() {
    let c = Coll::new();
    let id = c.add("Write RDS module", "code");
    assert!(id.starts_with("vf-"));

    let list = c.json(&["item", "list"]);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["slug"], "write-rds-module");

    // prefix resolution
    let short = &id[..10];
    let show = c.json(&["item", "show", short]);
    assert_eq!(show["id"], id.as_str());
    assert_eq!(show["spec"], "do the thing\n");

    // claim as me, start, note, complete -> review
    let claim = c.json(&["item", "claim", &id, "--me"]);
    assert_eq!(claim["actor"], "tester");
    c.ok(&["item", "start", &id]);
    c.ok(&["item", "note", &id, "--body", "progress"]);
    let done = c.json(&["item", "complete", &id, "--note", "shipped"]);
    assert_eq!(done["stage"], "review");
    assert_eq!(done["status"], "pending");
    assert!(done["assignee"].is_null());
    let show = c.json(&["item", "show", &id]);
    assert_eq!(show["notes"].as_array().unwrap().len(), 2);

    // reviewer bounces
    c.ok(&["--as", "rev", "item", "start", &id]);
    let b = c.json(&[
        "--as", "rev", "item", "complete", &id, "--bounce", "--note", "nit",
    ]);
    assert_eq!(b["stage"], "code");

    // someone else cannot complete a claimed item
    c.ok(&["--as", "coder-1", "item", "start", &id]);
    let out = c.fails(&["--as", "coder-2", "item", "complete", &id]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("claimed by coder-1"));

    // force release, then block / reopen / raise / handoff / link / assign
    c.ok(&["item", "release", &id, "--force"]);
    c.ok(&["--as", "coder-1", "item", "start", &id]);
    let bl = c.json(&["--as", "coder-1", "item", "block", &id, "--on", "vf-dep"]);
    assert_eq!(bl["status"], "blocked");
    assert_eq!(c.json(&["item", "reopen", &id])["status"], "pending");
    c.ok(&["--as", "coder-1", "item", "start", &id]);
    assert_eq!(
        c.json(&[
            "--as",
            "coder-1",
            "item",
            "raise",
            &id,
            "--question",
            "which db?"
        ])["status"],
        "needs_human"
    );
    let h = c.json(&["item", "handoff", &id, "--to", "planner", "--stage", "plan"]);
    assert_eq!(h["assignee"], "planner");
    assert_eq!(h["stage"], "plan");
    let l = c.json(&["item", "link", &id, "--system", "jira", "--key", "PAY-1"]);
    assert_eq!(l["external"]["key"], "PAY-1");
    assert!(c.json(&["item", "assign", &id, "--none"])["assignee"].is_null());
    assert_eq!(c.json(&["item", "assign", &id, "vito"])["assignee"], "vito");

    // list filters
    assert_eq!(
        c.json(&["item", "list", "--assignee", "vito"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        c.json(&["item", "list", "--stage", "deploy"])
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // cancel hides from default list, --all shows it
    assert_eq!(c.json(&["item", "cancel", &id])["status"], "cancelled");
    assert_eq!(c.json(&["item", "list"]).as_array().unwrap().len(), 0);
    assert_eq!(
        c.json(&["item", "list", "--all"]).as_array().unwrap().len(),
        1
    );

    // events and board
    let evs = c.json(&["events", "--item", &id]);
    let kinds: Vec<&str> = evs
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "item.created");
    assert!(
        kinds.contains(&"item.bounced")
            && kinds.contains(&"claim.broken")
            && kinds.contains(&"item.cancelled")
    );
    let since = c.json(&["events", "--since", "1h", "--kind", "item.linked"]);
    assert_eq!(since.as_array().unwrap().len(), 1);
    let board = c.json(&["board"]);
    assert_eq!(board["total"], 1);
    assert_eq!(board["open"], 0);
    let text = c.ok(&["board"]);
    assert!(text.contains("collective t"));
}

#[test]
fn deps_gate_selection_and_human_text_output_works() {
    let c = Coll::new();
    let a = c.add("first", "code");
    let b = c.json(&[
        "item", "add", "--title", "second", "--stage", "code", "--dep", &a,
    ]);
    assert_eq!(b["deps"][0], a.as_str());
    let text = c.ok(&["item", "list"]);
    assert!(text.contains("first") && text.contains("second"));
    let show = c.ok(&["item", "show", &a]);
    assert!(show.contains("--- spec ---"));
    c.fails(&["item", "add", "--title", "bad", "--dep", "vf-missing"]);
}

#[test]
fn agents_register_list_lend_and_remove() {
    let c = Coll::new();
    let a = c.json(&["agent", "add", "--name", "coder-1", "--profile", "coder"]);
    assert_eq!(a["kind"], "local");
    c.fails(&["agent", "add", "--name", "x", "--profile", "nope"]);
    let r = c.json(&[
        "agent",
        "add",
        "--name",
        "far",
        "--profile",
        "coder",
        "--remote",
        "https://example",
    ]);
    assert_eq!(r["kind"], "remote");
    assert_eq!(c.json(&["agent", "list"]).as_array().unwrap().len(), 2);

    // lend to another file collective on this machine
    let other = Coll::new();
    let lent = c.json(&[
        "agent",
        "lend",
        "coder-1",
        "--to",
        other.root.to_str().unwrap(),
    ]);
    assert_eq!(lent["id"], "coder-1");
    assert!(lent["lent_from"].as_str().unwrap().contains(".vflt"));
    let there = other.json(&["agent", "list"]);
    assert!(there
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == "coder-1"));
    let here = c.json(&["agent", "list"]);
    let me = here
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "coder-1")
        .unwrap();
    assert!(me["lent_to"].as_str().is_some());
    // urls are refused with a clear message
    let out = c.fails(&[
        "agent",
        "lend",
        "coder-1",
        "--to",
        "https://fleet.example/collective",
    ]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented"));

    c.ok(&["agent", "remove", "far"]);
    assert_eq!(c.json(&["agent", "list"]).as_array().unwrap().len(), 1);
}

#[test]
fn non_file_store_is_refused_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("collective.toml"),
        "name = \"db\"\ncreated = \"2026-10-02T21:10:00Z\"\n[store]\nkind = \"dynamodb\"\nurl = \"x\"\n",
    )
    .unwrap();
    let out = Command::new(VFLT)
        .arg("--collective")
        .arg(tmp.path())
        .arg("board")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dynamodb"));
}

#[test]
fn missing_collective_is_a_clear_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(VFLT)
        .arg("board")
        .current_dir(tmp.path())
        .env_remove("VFLT_COLLECTIVE")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no collective found"));
}

#[test]
fn agent_loop_completes_an_item_end_to_end() {
    let c = Coll::new();
    c.fake_profile("fake", "[\"code\"]", "", "2m");
    let id = c.add("end to end", "code");
    let out = c.run_agent("fake", "complete");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let item = c.json(&["item", "show", &id]);
    assert_eq!(item["stage"], "review");
    assert_eq!(item["status"], "pending");
    assert!(item["claim"].is_null());
    let notes = item["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["actor"], "fake-1");
    assert!(notes[0]["body"].as_str().unwrap().contains("did the work"));

    // the prompt carried the spec and the guide named the item
    let received =
        std::fs::read_to_string(c.root.join("work").join(&id).join("prompt-received.md")).unwrap();
    assert!(received.contains("do the thing"));
    assert!(received.contains(&id));
    let sys = c.root.join("work").join(&id).join(".vflt-system-prompt.md");
    assert!(
        !sys.exists(),
        "system prompt temp file should be cleaned up"
    );

    // agent record and run event
    let agents = c.json(&["agent", "list"]);
    let a = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "fake-1")
        .unwrap();
    assert_eq!(a["profiles"][0], "fake");
    assert_eq!(a["status"], "offline");
    let evs = c.json(&["events", "--item", &id, "--kind", "agent.run"]);
    assert_eq!(evs.as_array().unwrap().len(), 1);
    assert_eq!(evs[0]["detail"]["exit_code"], 0);

    // nothing left to do for this profile
    let out = c.run_agent("fake", "complete");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no claimable items"));
}

#[test]
fn agent_loop_marks_needs_human_when_runner_does_not_finish() {
    let c = Coll::new();
    c.fake_profile("fake", "[\"code\"]", "", "2m");
    let noop = c.add("noop item", "code");
    let out = c.run_agent("fake", "noop");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let item = c.json(&["item", "show", &noop]);
    assert_eq!(item["status"], "needs_human");
    assert!(item["waiting_on"]
        .as_str()
        .unwrap()
        .contains("exited with code 0"));
    assert!(item["notes"][0]["body"]
        .as_str()
        .unwrap()
        .contains("without completing"));

    let fail = c.add("failing item", "code");
    let out = c.run_agent("fake", "fail");
    assert!(out.status.success());
    let item = c.json(&["item", "show", &fail]);
    assert_eq!(item["status"], "needs_human");
    assert!(item["waiting_on"].as_str().unwrap().contains("code 2"));
}

#[test]
fn agent_loop_honours_bounce_block_and_raise() {
    let c = Coll::new();
    c.fake_profile("fakerev", "[\"review\"]", "", "2m");
    let id = c.add("to review", "review");
    assert!(c.run_agent("fakerev", "bounce").status.success());
    let item = c.json(&["item", "show", &id]);
    assert_eq!(item["stage"], "code");
    assert_eq!(item["status"], "pending");

    c.fake_profile("fake", "[\"code\"]", "", "2m");
    assert!(c.run_agent("fake", "block").status.success());
    assert_eq!(c.json(&["item", "show", &id])["status"], "blocked");
    c.ok(&["item", "reopen", &id]);
    assert!(c.run_agent("fake", "raise").status.success());
    let item = c.json(&["item", "show", &id]);
    assert_eq!(item["status"], "needs_human");
    assert!(item["waiting_on"]
        .as_str()
        .unwrap()
        .contains("which database"));
}

#[test]
fn wall_clock_budget_kills_a_hung_runner() {
    let c = Coll::new();
    c.fake_profile("fake", "[\"code\"]", "", "1s");
    let id = c.add("hangs", "code");
    let start = std::time::Instant::now();
    let out = c.run_agent("fake", "hang");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "runner was not killed in time"
    );
    let item = c.json(&["item", "show", &id]);
    assert_eq!(item["status"], "needs_human");
    assert!(item["waiting_on"].as_str().unwrap().contains("wall-clock"));
}

#[test]
fn cycle_profile_runs_once_without_items() {
    let c = Coll::new();
    c.fake_profile("fakesup", "[]", "1m", "2m");
    let out = c.run_agent("fakesup", "cycle");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let evs = c.json(&["events", "--kind", "agent.cycle"]);
    assert_eq!(evs.as_array().unwrap().len(), 1);
    assert!(evs[0]["detail"]["summary"]
        .as_str()
        .unwrap()
        .contains("board looks fine"));
}

#[test]
fn unavailable_runner_is_reported() {
    let c = Coll::new();
    let toml = "name = \"ghost\"\nstages = [\"code\"]\nrunner = \"shell\"\nrunner_command = \"definitely-not-installed-vflt-runner\"\n";
    std::fs::write(c.root.join("profiles").join("ghost.toml"), toml).unwrap();
    std::fs::write(c.root.join("profiles").join("ghost.md"), "x").unwrap();
    c.add("x", "code");
    let out = c.fails(&["agent", "run", "--profile", "ghost", "--once"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not available"));
}

#[test]
fn git_tracked_collective_commits_every_mutation_and_stays_clean() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(".vflt");
    let out = Command::new(VFLT)
        .args(["collective", "init", "--name", "g"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c = Coll { _tmp: tmp, root };
    c.fake_profile("fake", "[\"code\"]", "", "2m");
    // the fixture profile is ours to track, like any hand-edited profile
    for args in [
        vec!["add", "-A", "profiles"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "fake profile",
        ],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(&c.root)
            .status()
            .unwrap()
            .success());
    }

    let id = c.add("tracked item", "code");
    c.ok(&["item", "claim", &id, "--me"]);
    c.ok(&["item", "start", &id]);
    c.ok(&["item", "note", &id, "--body", "n1"]);
    c.ok(&["item", "complete", &id, "--note", "done"]);
    c.ok(&["--as", "rev", "item", "start", &id]);
    c.ok(&[
        "--as", "rev", "item", "complete", &id, "--bounce", "--note", "nit",
    ]);
    c.ok(&["item", "start", &id]);
    c.ok(&["item", "release", &id]);
    c.ok(&["item", "start", &id]);
    c.ok(&["item", "block", &id, "--on", "x"]);
    c.ok(&["item", "reopen", &id]);
    c.ok(&["agent", "add", "--name", "a1", "--profile", "coder"]);
    c.ok(&["agent", "remove", "a1"]);
    let run = c.run_agent("fake", "complete");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&run.stderr).contains("git commit skipped"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&c.root)
        .output()
        .unwrap();
    let dirty = String::from_utf8_lossy(&status.stdout);
    assert!(
        dirty.trim().is_empty(),
        "working tree should be clean:\n{dirty}"
    );
    let log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(&c.root)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    for needle in [
        "init collective",
        "created vf-",
        "claimed vf-",
        "completed vf-",
        "released vf-",
        "blocked vf-",
        "reopened vf-",
        "a1 registered",
        "a1 deregistered",
    ] {
        assert!(
            log.contains(needle),
            "missing commit for {needle:?}:\n{log}"
        );
    }
}
