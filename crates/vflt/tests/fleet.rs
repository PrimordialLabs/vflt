//! End-to-end tests for the fleet features: the sqlite store, sequential ids
//! and the mirror, drafts, waits and bounces to origin, respond, flags, the
//! inbox, the agent control plane, hooks, batches, hybrid cycles, shared
//! workspaces, recovery, the fleet runner and cost accounting.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const VFLT: &str = env!("CARGO_BIN_EXE_vflt");
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");

struct Coll {
    tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Coll {
    fn init(extra: &[&str]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("board");
        let mut args = vec!["collective", "init", "--name", "t"];
        args.extend_from_slice(extra);
        let out = Command::new(VFLT).args(&args).arg(&root).output().unwrap();
        assert!(
            out.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Coll { tmp, root }
    }

    /// A delegated, single-lane sqlite board with sequential ids and a mirror.
    fn lanes() -> Self {
        Coll::init(&[
            "--store",
            "sqlite",
            "--ids",
            "seq",
            "--mirror",
            "mirror",
            "--claim-mode",
            "delegated",
            "--pipeline",
            "work,done",
        ])
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(VFLT);
        c.arg("--collective").arg(&self.root);
        c.env_remove("VFLT_AGENT");
        c.env_remove("VFLT_COLLECTIVE");
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

    fn add(&self, actor: &str, title: &str, assign: &str) -> String {
        let v = self.json(&[
            "--as",
            actor,
            "item",
            "add",
            "--title",
            title,
            "--assign",
            assign,
            "--spec",
            "do the thing",
        ]);
        v["id"].as_str().unwrap().to_string()
    }

    fn profile(&self, name: &str, body: &str) {
        let exe = Path::new(FAKE_AGENT).to_string_lossy().replace('\\', "/");
        let toml = format!(
            "name = \"{name}\"\nrunner = \"shell\"\nrunner_command = \"\\\"{exe}\\\"\"\n{body}\n"
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

    fn run_agent(
        &self,
        name: &str,
        profile: &str,
        mode: &str,
        extra_env: &[(&str, &str)],
    ) -> Output {
        let mut c = self.cmd();
        c.args([
            "agent",
            "run",
            "--profile",
            profile,
            "--once",
            "--name",
            name,
        ])
        .env("VFLT_BIN", VFLT)
        .env("FAKE_AGENT_MODE", mode);
        for (k, v) in extra_env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn show(&self, id: &str) -> Value {
        self.json(&["item", "show", id])
    }
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn sqlite_board_ids_filters_and_mirror() {
    let c = Coll::lanes();
    let show = c.json(&["collective", "show"]);
    assert_eq!(show["collective"]["store"]["kind"], "sqlite");
    assert!(c.root.join("board.db").is_file());

    let a = c.add("lead", "first", "worker");
    let b = c.json(&[
        "--as",
        "lead",
        "item",
        "add",
        "--title",
        "Second thing",
        "--slug",
        "wrk_second_001",
        "--summary",
        "short",
        "--assign",
        "worker",
        "--tag",
        "Area:Core",
        "--tag",
        "source:panel",
        "--priority",
        "10",
        "--parent",
        &a,
    ])["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(a, "vf-1");
    assert_eq!(b, "vf-2");
    let item = c.show("2");
    assert_eq!(item["slug"], "wrk_second_001");
    assert_eq!(item["summary"], "short");
    assert_eq!(item["parent"], "vf-1");
    assert_eq!(
        item["tags"],
        serde_json::json!(["area:core", "source:panel"])
    );
    assert_eq!(
        item["origin"]["assignee"], "lead",
        "delegated items bounce to their filer"
    );

    // filters: status lists, exclusions, tags, sort and paging
    let list = c.json(&["item", "list", "--tag", "area:core"]);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let list = c.json(&["item", "list", "--not-tag", "area:core"]);
    assert_eq!(list[0]["id"], "vf-1");
    assert!(list[0].get("spec").is_none(), "lists do not carry specs");
    let list = c.json(&[
        "item", "list", "--sort", "created", "--desc", "--limit", "1",
    ]);
    assert_eq!(list[0]["id"], "vf-2");
    let list = c.json(&[
        "item", "list", "--sort", "created", "--limit", "1", "--offset", "1",
    ]);
    assert_eq!(list[0]["id"], "vf-2");
    c.ok(&["item", "cancel", "1", "--note", "superseded by vf-2"]);
    assert_eq!(c.json(&["item", "list"]).as_array().unwrap().len(), 1);
    assert_eq!(
        c.json(&["item", "list", "--all"]).as_array().unwrap().len(),
        2
    );
    assert_eq!(
        c.json(&["item", "list", "--status", "pending,cancelled"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        c.json(&["item", "list", "--all", "--not-status", "cancelled"])
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // set, tag, show with events (from/to status on transitions)
    c.ok(&[
        "item",
        "set",
        "2",
        "--priority",
        "5",
        "--title",
        "Second, retitled",
        "--summary",
        "",
    ]);
    c.ok(&[
        "item",
        "tag",
        "2",
        "--add",
        "cap:x",
        "--remove",
        "source:panel",
    ]);
    let item = c.json(&["item", "show", "2", "--events"]);
    assert_eq!(item["priority"], 5);
    assert!(item.get("summary").is_none());
    assert_eq!(item["tags"], serde_json::json!(["area:core", "cap:x"]));
    let cancel = c.json(&["item", "show", "1", "--events"]);
    let ev = cancel["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "item.cancelled")
        .unwrap();
    assert_eq!(ev["detail"]["from_status"], "pending");
    assert_eq!(ev["detail"]["to_status"], "cancelled");
    assert!(cancel["notes"][0]["body"]
        .as_str()
        .unwrap()
        .contains("superseded"));
    let noted = cancel["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "item.noted")
        .unwrap();
    assert_eq!(noted["detail"]["seq"], 1);
    assert_eq!(
        c.json(&["--as", "w", "item", "start", "2"])["status"],
        "in_progress"
    );
    c.ok(&["--as", "w", "item", "complete", "2", "--note", "shipped"]);
    let done = c.json(&["item", "show", "2", "--events"]);
    let ev = done["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "item.completed")
        .unwrap();
    assert_eq!(ev["detail"]["note_seq"], 1);

    // the mirror tracks every change and can rebuild a board on its own
    let mirror = c.root.join("mirror").join("vf-2.md");
    let text = std::fs::read_to_string(&mirror).unwrap();
    assert!(text.contains("Second, retitled") && text.contains("cap:x"));
    let other = Coll::lanes();
    other.ok(&[
        "collective",
        "import",
        "--mirror",
        c.root.join("mirror").to_str().unwrap(),
    ]);
    assert_eq!(other.show("2")["title"], "Second, retitled");
    assert_eq!(other.show("2")["status"], "done");
    assert!(other.show("1")["status"] == "cancelled");
    assert_eq!(
        other.add("lead", "next", "worker"),
        "vf-3",
        "sequence continues after imported ids"
    );

    // a bundle round-trips into a file store
    let bundle = c.tmp.path().join("bundle.jsonl");
    c.ok(&["collective", "export", "--to", bundle.to_str().unwrap()]);
    let files = Coll::init(&["--no-git"]);
    let counts = files.json(&["collective", "import", "--from", bundle.to_str().unwrap()]);
    assert_eq!(counts["item"], 2);
    assert!(counts["event"].as_u64().unwrap() >= 5);
    assert_eq!(files.show("vf-2")["title"], "Second, retitled");
}

#[test]
fn drafts_waits_bounces_and_respond() {
    let c = Coll::lanes();
    // drafts: hidden, unclaimable, editable, then promoted
    let d = c.json(&[
        "--as",
        "lead",
        "item",
        "add",
        "--title",
        "draft spec",
        "--assign",
        "worker",
        "--draft",
        "--spec",
        "v1",
    ])["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(c.json(&["item", "list"]).as_array().unwrap().len(), 0);
    c.fails(&["--as", "worker", "item", "claim", &d]);
    c.ok(&["item", "set", &d, "--spec", "v2"]);
    assert_eq!(c.show(&d)["spec"], "v2\n");
    c.ok(&["item", "promote", &d]);
    c.fails(&["item", "set", &d, "--spec", "v3"]);
    c.ok(&["item", "demote", &d]);
    c.ok(&["item", "promote", &d]);

    // the shipped-then-probe shape: park the parent on the probe
    c.ok(&["--as", "worker", "item", "start", &d]);
    let probe = c.add("worker", "probe it", "tester");
    let parked = c.json(&[
        "--as",
        "worker",
        "item",
        "wait",
        &d,
        "--on",
        &probe,
        "--note",
        "shipped; waiting on the probe",
    ]);
    assert_eq!(parked["status"], "pending");
    assert_eq!(
        parked["assignee"], "worker",
        "parks with its resting assignee"
    );
    assert_eq!(parked["deps"], serde_json::json!([probe.clone()]));
    c.fails(&["--as", "tester", "item", "wait", &probe, "--on", &d]);

    // the tester bounces the probe: it goes back to the worker that filed it
    c.ok(&["--as", "tester", "item", "start", &probe]);
    let b = c.json(&[
        "--as",
        "tester",
        "item",
        "complete",
        &probe,
        "--bounce",
        "--note",
        "step B fails",
    ]);
    assert_eq!(b["assignee"], "worker");
    assert_eq!(b["returned"], true);
    assert_eq!(b["bounces"], 1);
    // the worker fixes and hands it back; the flag clears on the move
    let h = c.json(&[
        "--as",
        "worker",
        "item",
        "handoff",
        &probe,
        "--to",
        "tester",
        "--note",
        "fixed in abc123",
    ]);
    assert!(h.get("returned").is_none());
    c.ok(&["--as", "tester", "item", "start", &probe]);
    c.ok(&["--as", "tester", "item", "complete", &probe, "--note", "GO"]);
    assert_eq!(c.show(&probe)["status"], "done");

    // respond: a raised item reopens for its assignee; one resting with the
    // responder goes back to its owner
    let q = c.add("lead", "needs a call", "worker");
    c.ok(&["--as", "worker", "item", "start", &q]);
    c.ok(&[
        "--as",
        "worker",
        "item",
        "raise",
        &q,
        "--question",
        "A or B?",
    ]);
    let r = c.json(&["--as", "ana", "item", "respond", &q, "--note", "B"]);
    assert_eq!(r["requeued"], true);
    assert_eq!(r["was_status"], "needs_human");
    assert_eq!(r["status"], "pending");
    assert_eq!(r["assignee"], "worker");
    let held = c.add("worker", "for the human", "ana");
    let r = c.json(&[
        "--as",
        "ana",
        "item",
        "respond",
        &held,
        "--note",
        "done on my side",
    ]);
    assert_eq!(r["assignee"], "worker");
    let r = c.json(&[
        "--as",
        "worker",
        "item",
        "respond",
        &held,
        "--note",
        "fyi",
        "--note-only",
    ]);
    assert_eq!(r["requeued"], false);
}

#[test]
fn flags_and_inbox() {
    let c = Coll::lanes();
    c.ok(&["agent", "add", "--name", "worker", "--profile", "coder"]);
    c.ok(&["agent", "add", "--name", "ana", "--human"]);
    let f = c.json(&[
        "--as",
        "worker",
        "flag",
        "--key",
        "disk_low",
        "--title",
        "Disk below 5GB",
        "--body",
        "free space is 3GB",
        "--assign",
        "ana",
    ]);
    assert_eq!(f["created"], true);
    assert_eq!(f["item"]["status"], "needs_human");
    assert_eq!(f["item"]["raise_kind"], "action");
    let again = c.json(&[
        "--as",
        "tester2",
        "flag",
        "--key",
        "disk_low",
        "--title",
        "Disk below 5GB",
        "--body",
        "still 3GB",
    ]);
    assert_eq!(again["created"], false);
    assert_eq!(again["item"]["id"], f["item"]["id"]);
    let q = c.add("lead", "decide", "worker");
    c.ok(&["--as", "worker", "item", "start", &q]);
    c.ok(&[
        "--as",
        "worker",
        "item",
        "raise",
        &q,
        "--question",
        "which?",
    ]);
    c.add("lead", "orphan", "nobody-answers-this");
    c.add("lead", "ready work", "worker");
    c.ok(&["agent", "pause", "worker"]);

    let inbox = c.json(&["inbox"]);
    assert_eq!(inbox["decisions"], 1);
    assert_eq!(inbox["actions"], 1);
    assert_eq!(inbox["items"].as_array().unwrap().len(), 2);
    assert!(
        inbox["items"][0]["spec"].is_string(),
        "inbox items carry their spec"
    );
    let kinds: Vec<&str> = inbox["attention"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"stranded"), "{kinds:?}");
    assert!(kinds.contains(&"paused_backlog"), "{kinds:?}");
    assert_eq!(
        c.json(&["inbox", "--kind", "action"])["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // closing the flag re-arms the key
    let id = f["item"]["id"].as_str().unwrap();
    c.ok(&["item", "cancel", id]);
    assert_eq!(
        c.json(&["flag", "--key", "disk_low", "--title", "again"])["created"],
        true
    );
}

#[test]
fn control_plane_pause_hold_wake_snooze_and_overrides() {
    let c = Coll::lanes();
    c.profile("lane", "stages = [\"work\"]\ncycle = \"1h\"\nmodel = \"profile-model\"\n[budget]\nwall_clock = \"2m\"\n");
    c.ok(&["agent", "add", "--name", "w", "--profile", "lane"]);
    let routed = c.add("lead", "routed", "w");
    let own = c.add("w", "self-filed", "w");

    // paused: the agent takes nothing
    c.ok(&["agent", "pause", "w"]);
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert_eq!(c.show(&routed)["status"], "pending");
    let list = c.json(&["agent", "list"]);
    assert_eq!(list[0]["desired"], "paused");
    assert_eq!(list[0]["queue"]["ready"], 2);
    c.ok(&["agent", "resume", "w"]);

    // hold: routed work only, self-filed work waits
    c.ok(&["agent", "hold", "w", "on"]);
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert_eq!(c.show(&routed)["status"], "done");
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert_eq!(
        c.show(&own)["status"],
        "pending",
        "held agents leave self-filed work"
    );
    c.ok(&["agent", "hold", "w", "off"]);

    // model override reaches the runner
    let env_out = c.tmp.path().join("env.txt");
    c.ok(&["agent", "set", "w", "--model", "override-model"]);
    ok(&c.run_agent(
        "w",
        "lane",
        "complete",
        &[("FAKE_AGENT_ENV_OUT", env_out.to_str().unwrap())],
    ));
    let env = std::fs::read_to_string(&env_out).unwrap();
    assert!(env.contains("VFLT_MODEL=override-model"), "{env}");
    assert!(env.contains(&format!("VFLT_ITEM={own}")), "{env}");
    c.ok(&["agent", "set", "w", "--model", "none"]);
    let left = c.add("lead", "left in progress", "w");
    ok(&c.run_agent(
        "w",
        "lane",
        "noop",
        &[("FAKE_AGENT_ENV_OUT", env_out.to_str().unwrap())],
    ));
    assert_eq!(c.show(&left)["status"], "needs_human");
    assert!(std::fs::read_to_string(&env_out)
        .unwrap()
        .contains("VFLT_MODEL=profile-model"));

    // hybrid: nothing routed, so the first cycle runs; it files work for itself
    ok(&c.run_agent("w", "lane", "file", &[]));
    let mine = c.json(&["item", "list", "--assignee", "w"]);
    assert!(mine
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["title"] == "self-filed scope work"));
    let cycles = c.json(&["events", "--kind", "agent.cycle"]);
    assert_eq!(cycles.as_array().unwrap().len(), 1);
    // the interval has not elapsed: no second cycle (the filed item is worked instead)
    ok(&c.run_agent("w", "lane", "complete", &[]));
    ok(&c.run_agent("w", "lane", "cycle", &[]));
    assert_eq!(
        c.json(&["events", "--kind", "agent.cycle"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // a wake forces one; a snooze from inside the turn stops the next
    c.ok(&["agent", "wake", "w"]);
    ok(&c.run_agent("w", "lane", "snooze", &[]));
    assert_eq!(
        c.json(&["events", "--kind", "agent.cycle"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let a = &c.json(&["agent", "list"])[0];
    assert!(a["snoozed"].is_string());
    assert!(a.get("wake_requested").is_none(), "the wake is spent");
    // work routed by someone else clears the snooze
    c.add("lead", "more", "w");
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert!(c.json(&["agent", "list"])[0].get("snoozed").is_none());

    // stop: the agent exits without working
    c.add("lead", "after stop", "w");
    c.ok(&["agent", "stop", "--all"]);
    let out = c.run_agent("w", "lane", "complete", &[]);
    ok(&out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("stopping"));
}

#[test]
fn hooks_gate_before_after_and_spend_cap() {
    let c = Coll::lanes();
    let flag = c.tmp.path().join("gate-open");
    let gate = format!(
        "{} {}",
        if cfg!(windows) {
            "cmd /C if exist"
        } else {
            "test -e"
        },
        flag.display()
    );
    c.profile(
        "lane",
        &format!("stages = [\"work\"]\n[hooks]\ngate = [{gate:?}]\n"),
    );
    let id = c.add("lead", "gated", "w");
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert_eq!(
        c.show(&id)["status"],
        "pending",
        "a failing gate claims nothing"
    );
    let a = &c.json(&["agent", "list"])[0];
    assert!(a["note"].as_str().unwrap().contains("gate"), "{a}");
    std::fs::write(&flag, "").unwrap();
    ok(&c.run_agent("w", "lane", "complete", &[]));
    assert_eq!(c.show(&id)["status"], "done");

    if cfg!(unix) {
        c.profile(
            "lane2",
            "stages = [\"work\"]\n[hooks]\nbefore = [\"false\"]\n",
        );
        let id = c.add("lead", "before fails", "w2");
        ok(&c.run_agent("w2", "lane2", "complete", &[]));
        let item = c.show(&id);
        assert_eq!(
            item["status"], "pending",
            "a failing before hook releases the item"
        );
        assert!(item["notes"][0]["body"]
            .as_str()
            .unwrap()
            .contains("Turn not started"));

        c.profile(
            "lane3",
            "stages = [\"work\"]\n[hooks]\nafter = [\"sh -c 'echo build broke; exit 1'\"]\n",
        );
        let id = c.add("lead", "after fails", "w3");
        ok(&c.run_agent("w3", "lane3", "complete", &[]));
        let item = c.show(&id);
        assert_eq!(item["status"], "done");
        assert!(item["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["body"].as_str().unwrap().contains("build broke")));
    }

    // a daily cap of zero holds the agent before it claims
    c.profile("capped", "stages = [\"work\"]\n[budget]\ndaily_usd = 0.0\n");
    let id = c.add("lead", "capped", "w4");
    ok(&c.run_agent("w4", "capped", "complete", &[]));
    assert_eq!(c.show(&id)["status"], "pending");
    assert!(c
        .json(&["agent", "list"])
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["note"].as_str().unwrap_or("").contains("spend cap")));
}

#[test]
fn batches_shared_workspaces_and_recovery() {
    let c = Coll::lanes();
    let shared = c.tmp.path().join("checkout");
    std::fs::create_dir_all(&shared).unwrap();
    c.profile(
        "batcher",
        &format!("stages = [\"work\"]\nbatch = 3\n[workspace]\nmode = \"shared\"\ndir = {:?}\n[env]\nEXTRA = \"from-profile\"\n", shared.to_string_lossy()),
    );
    let ids: Vec<String> = (0..4)
        .map(|n| c.add("lead", &format!("batch {n}"), "b"))
        .collect();
    let env_out = c.tmp.path().join("env.txt");
    ok(&c.run_agent(
        "b",
        "batcher",
        "complete",
        &[("FAKE_AGENT_ENV_OUT", env_out.to_str().unwrap())],
    ));
    let done: Vec<&String> = ids
        .iter()
        .filter(|i| c.show(i)["status"] == "done")
        .collect();
    assert_eq!(done.len(), 3, "one turn takes the batch");
    let runs = c.json(&["events", "--kind", "agent.run"]);
    assert_eq!(runs.as_array().unwrap().len(), 3);
    assert_eq!(runs[0]["detail"]["batch"], 3);
    let env = std::fs::read_to_string(&env_out).unwrap();
    assert!(env.contains("EXTRA=from-profile"));
    let cwd = std::fs::canonicalize(&shared).unwrap();
    assert!(env.contains(&format!("CWD={}", cwd.display())), "{env}");
    // the shared checkout only gets what the agent writes; prompt files stay in scratch
    let names: Vec<String> = std::fs::read_dir(&shared)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["prompt-received.md".to_string()]);

    // an item left in progress by a dead turn is released when the agent starts
    let stuck = ids
        .iter()
        .find(|i| c.show(i)["status"] == "pending")
        .unwrap()
        .clone();
    c.ok(&["--as", "b", "item", "start", &stuck]);
    ok(&c.run_agent("b", "batcher", "noop", &[]));
    let item = c.show(&stuck);
    assert!(item["notes"][0]["body"]
        .as_str()
        .unwrap()
        .contains("Recovered"));
    assert_eq!(
        item["status"], "needs_human",
        "recovered, then worked by a noop turn"
    );
}

#[test]
fn fleet_runs_every_local_agent_and_costs_add_up() {
    let c = Coll::lanes();
    c.profile("lane", "stages = [\"work\"]\n");
    for n in ["a1", "a2"] {
        c.ok(&["agent", "add", "--name", n, "--profile", "lane"]);
    }
    c.ok(&["agent", "add", "--name", "person", "--human"]);
    let x = c.add("lead", "for a1", "a1");
    let y = c.add("lead", "for a2", "a2");
    let out = c
        .cmd()
        .args(["fleet", "run", "--once"])
        .env("VFLT_BIN", VFLT)
        .env("FAKE_AGENT_MODE", "complete")
        .output()
        .unwrap();
    ok(&out);
    assert_eq!(c.show(&x)["status"], "done");
    assert_eq!(c.show(&y)["status"], "done");
    let cost = c.json(&["cost", "--by", "agent"]);
    let keys: Vec<&str> = cost
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["a1", "a2"]);
    assert_eq!(
        c.json(&["agent", "list"])
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "human")
            .count(),
        1
    );
}

#[test]
fn pointer_file_finds_a_collective_in_a_subdirectory() {
    let c = Coll::lanes();
    let repo = c.tmp.path();
    std::fs::write(repo.join(".vflt"), "path = \"board\"\n").unwrap();
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    let out = Command::new(VFLT)
        .args(["--json", "collective", "show"])
        .current_dir(repo.join("src/deep"))
        .env_remove("VFLT_COLLECTIVE")
        .output()
        .unwrap();
    ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["collective"]["name"], "t");
}

#[cfg(unix)]
#[test]
fn sigterm_lets_the_running_turn_finish() {
    let c = Coll::lanes();
    c.profile("lane", "stages = [\"work\"]\n");
    c.ok(&["agent", "add", "--name", "slowpoke", "--profile", "lane"]);
    let id = c.add("lead", "takes a while", "slowpoke");
    let child = c
        .cmd()
        .args(["fleet", "run", "--interval", "1s"])
        .env("VFLT_BIN", VFLT)
        .env("FAKE_AGENT_MODE", "slow")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // wait until the turn has started, then ask the fleet to stop
    let start = std::time::Instant::now();
    while c.show(&id)["status"] != "in_progress" {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "turn never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("finishing the current turn"));
    assert_eq!(c.show(&id)["status"], "done", "the turn completed its item");
    let a = &c.json(&["agent", "list"])[0];
    assert_eq!(
        a["desired"], "running",
        "a graceful stop leaves the control plane alone"
    );
    assert_eq!(a["status"], "offline");
}
