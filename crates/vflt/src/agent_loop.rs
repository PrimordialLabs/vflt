//! `vflt agent run`: register, pick work, claim, launch the runner, record the
//! outcome, repeat.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use vflt_core::{
    select_next, ActorId, AgentStatus, Error as CoreError, Event, Item, ItemFilter, NewAgent,
    Profile, Status, Transition,
};
use vflt_runner::{Registry, RunOutcome, RunRequest, Runner};

use crate::ctx::Ctx;
use crate::prompt;

pub struct RunOptions {
    pub profile: String,
    pub once: bool,
    pub interval: Duration,
    pub name: Option<String>,
    pub runner: Option<String>,
}

pub fn run(ctx: &Ctx, opts: RunOptions) -> Result<()> {
    let store = ctx.store();
    let coll = store.collective()?;
    let profile = store.profile(&opts.profile)?;
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("{}@{}", profile.name, crate::identity::hostname()));
    let me = ActorId::new(&name);

    let registry = Registry::load().unwrap_or_default();
    let runner_name = opts
        .runner
        .clone()
        .unwrap_or_else(|| profile.runner.clone());
    let runner = registry.get(&runner_name, profile.runner_command.as_deref())?;
    match runner.available()? {
        vflt_runner::Availability::Available { .. } => {}
        vflt_runner::Availability::Missing { reason } => {
            bail!("runner `{runner_name}` is not available: {reason}")
        }
    }

    let agent = store.register_agent(NewAgent {
        name: name.clone(),
        profiles: vec![profile.name.clone()],
        pid: Some(std::process::id()),
        ..Default::default()
    })?;
    eprintln!(
        "vflt: agent {} running profile {} with runner {} on collective {} ({})",
        agent.id,
        profile.name,
        runner.name(),
        coll.name,
        ctx.root.display()
    );

    let result = work_loop(ctx, &profile, &me, runner.as_ref(), &opts);

    if let Ok(mut a) = store.get_agent(&me) {
        a.status = AgentStatus::Offline;
        a.pid = None;
        let _ = store.update_agent(&a);
    }
    result
}

fn work_loop(
    ctx: &Ctx,
    profile: &Profile,
    me: &ActorId,
    runner: &dyn Runner,
    opts: &RunOptions,
) -> Result<()> {
    let store = ctx.store();
    let coll = store.collective()?;
    loop {
        store.heartbeat(me)?;

        if profile.is_cycle_only() {
            let cycle = profile.cycle().unwrap_or(Duration::from_secs(900));
            cycle_turn(ctx, profile, me, runner)?;
            if opts.once {
                return Ok(());
            }
            std::thread::sleep(cycle);
            continue;
        }

        let items = store.list_items(&ItemFilter::default())?;
        let Some(item) = select_next(&items, profile, me, coll.claim_mode).cloned() else {
            if opts.once {
                eprintln!("vflt: no claimable items for profile {}", profile.name);
                return Ok(());
            }
            std::thread::sleep(opts.interval);
            continue;
        };

        match store.claim(item.id(), me) {
            Ok(_) => {}
            Err(CoreError::AlreadyClaimed { .. }) | Err(CoreError::InvalidTransition { .. }) => {
                // someone else got there first; look again
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        store.transition(item.id(), me, Transition::Start)?;
        eprintln!(
            "vflt: {} working {} \"{}\" at {}",
            me,
            item.id(),
            item.title,
            item.stage
        );

        let outcome = item_turn(ctx, profile, me, runner, &item);
        match outcome {
            Ok(o) => eprintln!(
                "vflt: {} finished {} (exit {}, turns {}, cost {})",
                me,
                item.id(),
                o.exit_code,
                o.num_turns
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                o.cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "n/a".into())
            ),
            Err(e) => {
                eprintln!("vflt: runner failed on {}: {e:#}", item.id());
                let _ = store.add_note(item.id(), me, &format!("runner failed to launch: {e:#}"));
                let _ = store.transition(
                    item.id(),
                    me,
                    Transition::Raise {
                        question: format!("runner `{}` failed to launch: {e}", runner.name()),
                    },
                );
            }
        }
        // make sure nothing of ours is left claimed
        if let Ok(it) = store.get_item(item.id()) {
            if it.claim.as_ref().is_some_and(|c| &c.actor == me) {
                let _ = store.release(item.id(), me);
            }
        }
        if opts.once {
            return Ok(());
        }
    }
}

fn workspace_for(ctx: &Ctx, item: &Item) -> Result<PathBuf> {
    let ws = ctx.store.work_dir().join(item.id().as_str());
    std::fs::create_dir_all(&ws).with_context(|| format!("creating workspace {}", ws.display()))?;
    Ok(ws)
}

fn run_log_path(ws: &std::path::Path) -> PathBuf {
    ws.join("runs")
        .join(format!("{}.jsonl", Utc::now().format("%Y%m%dT%H%M%SZ")))
}

fn base_env(ctx: &Ctx, me: &ActorId, profile: &Profile) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "VFLT_COLLECTIVE".into(),
            ctx.root.to_string_lossy().into_owned(),
        ),
        ("VFLT_AGENT".into(), me.to_string()),
        ("VFLT_PROFILE".into(), profile.name.clone()),
    ];
    // Hand the child the exact binary that launched it, so an agent's
    // `vflt item ...` calls hit the same build even when `vflt` is not on PATH.
    if let Ok(exe) = std::env::current_exe() {
        env.push(("VFLT_BIN".into(), exe.to_string_lossy().into_owned()));
    }
    env
}

fn item_turn(
    ctx: &Ctx,
    profile: &Profile,
    me: &ActorId,
    runner: &dyn Runner,
    item: &Item,
) -> Result<RunOutcome> {
    let store = ctx.store();
    let coll = store.collective()?;
    let ws = workspace_for(ctx, item)?;
    let notes = store.notes(item.id())?;
    let root = ctx.root.to_string_lossy().into_owned();

    let mut env = base_env(ctx, me, profile);
    env.push(("VFLT_ITEM".into(), item.id().to_string()));

    let req = RunRequest {
        system_prompt: prompt::system_prompt(profile, &coll, &root, me.as_str(), Some(item)),
        prompt: prompt::item_prompt(store, &coll, item, &notes, &ws.to_string_lossy()),
        cwd: ws.clone(),
        extra_dirs: vec![ctx.root.clone()],
        policy: profile.policy.clone(),
        budget: profile.budget.clone(),
        env,
        log_path: Some(run_log_path(&ws)),
        session: None,
    };
    let outcome = runner.run(req)?;
    record_outcome(ctx, me, runner, item, &outcome)?;
    Ok(outcome)
}

fn record_outcome(
    ctx: &Ctx,
    me: &ActorId,
    runner: &dyn Runner,
    item: &Item,
    o: &RunOutcome,
) -> Result<()> {
    let store = ctx.store();
    store.append_event(
        Event::new(
            me,
            "agent.run",
            Some(item.id()),
            format!(
                "{me} ran {} on {} (exit {})",
                runner.name(),
                item.id(),
                o.exit_code
            ),
        )
        .with_detail(serde_json::json!({
            "runner": runner.name(),
            "exit_code": o.exit_code,
            "timed_out": o.timed_out,
            "is_error": o.is_error,
            "session_id": o.session_id,
            "cost_usd": o.cost_usd,
            "num_turns": o.num_turns,
            "denials": o.permission_denials.len(),
        })),
    )?;

    let after = store.get_item(item.id())?;
    let transitioned = after.status != Status::InProgress;

    if !o.permission_denials.is_empty() {
        let mut body = String::from("The runner's permission classifier denied these actions:\n\n");
        for d in &o.permission_denials {
            body.push_str(&format!(
                "- {}{}: {}\n",
                d.tool,
                d.reason
                    .as_ref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default(),
                crate::util::truncate(&d.input.to_string(), 200)
            ));
        }
        store.add_note(item.id(), me, &body)?;
    }

    if transitioned {
        return Ok(());
    }

    // The agent exited without a final transition: leave evidence and raise.
    let mut body =
        format!(
        "Runner `{}` exited (code {}{}{}) without completing, blocking or raising the item.\n\n",
        runner.name(),
        o.exit_code,
        if o.timed_out { ", wall-clock budget exceeded" } else { "" },
        if o.is_error { ", error" } else { "" }
    );
    if !o.result_text.trim().is_empty() {
        body.push_str("## Result\n\n");
        body.push_str(o.result_text.trim());
        body.push_str("\n\n");
    }
    if !o.transcript_tail.trim().is_empty() {
        body.push_str("## Transcript tail\n\n");
        body.push_str(o.transcript_tail.trim());
        body.push('\n');
    }
    store.add_note(item.id(), me, &body)?;
    let question = if !o.permission_denials.is_empty() {
        format!(
            "runner was denied {} action(s) by the permission policy and did not finish; see notes",
            o.permission_denials.len()
        )
    } else if o.timed_out {
        "runner exceeded its wall-clock budget without finishing; see notes".to_string()
    } else {
        format!(
            "runner exited with code {} without finishing; see notes",
            o.exit_code
        )
    };
    store
        .transition(item.id(), me, Transition::Raise { question })
        .map_err(|e| anyhow!("raising {} after incomplete run: {e}", item.id()))?;
    Ok(())
}

fn cycle_turn(ctx: &Ctx, profile: &Profile, me: &ActorId, runner: &dyn Runner) -> Result<()> {
    let store = ctx.store();
    let coll = store.collective()?;
    let ws = ctx.store.work_dir().join("_cycles").join(&profile.name);
    std::fs::create_dir_all(&ws)?;
    let items = store.list_items(&ItemFilter::default())?;
    let root = ctx.root.to_string_lossy().into_owned();
    let req = RunRequest {
        system_prompt: prompt::system_prompt(profile, &coll, &root, me.as_str(), None),
        prompt: prompt::cycle_prompt(&items, &coll),
        cwd: ws.clone(),
        extra_dirs: vec![ctx.root.clone()],
        policy: profile.policy.clone(),
        budget: profile.budget.clone(),
        env: base_env(ctx, me, profile),
        log_path: Some(run_log_path(&ws)),
        session: None,
    };
    eprintln!("vflt: {} cycle turn ({})", me, profile.name);
    let o = runner.run(req)?;
    store.append_event(
        Event::new(
            me,
            "agent.cycle",
            None,
            format!("{me} ran a {} cycle (exit {})", profile.name, o.exit_code),
        )
        .with_detail(serde_json::json!({
            "runner": runner.name(),
            "exit_code": o.exit_code,
            "timed_out": o.timed_out,
            "session_id": o.session_id,
            "cost_usd": o.cost_usd,
            "num_turns": o.num_turns,
            "summary": crate::util::truncate(&o.result_text, 400),
        })),
    )?;
    Ok(())
}
