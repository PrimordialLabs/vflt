//! `vflt agent run`: register, read the control plane, pick work, claim,
//! launch the runner, record the outcome, repeat.
//!
//! Between turns the agent re-reads its own record (desired state, hold,
//! overrides) and its profile (so prompt and policy edits apply to the next
//! turn), checks its gate hooks and spend cap, then works the highest-priority
//! claimable items, or runs a cycle turn when none are claimable and its
//! cycle is due. A thread heartbeats throughout every turn, so a long turn is
//! never mistaken for a dead agent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use vflt_core::profile::WorkspaceMode;
use vflt_core::{
    select_batch, ActorId, Agent, AgentRuntime, AgentStatus, Desired, Event, EventFilter, Item,
    ItemFilter, NewAgent, Picking, Profile, RaiseKind, Status, Store, Transition,
};
use vflt_runner::{process::run_capture, Registry, RunOutcome, RunRequest, Runner, ShellRunner};

use crate::ctx::Ctx;
use crate::prompt;

pub struct RunOptions {
    /// Agent name; registered on first run when `profile` is given.
    pub name: String,
    /// Profile to run (default: the agent's first profile).
    pub profile: Option<String>,
    pub once: bool,
    pub interval: Duration,
    pub runner: Option<String>,
    /// Prefix for log lines (fleet mode).
    pub prefix: String,
    /// Set to ask the loop to finish its turn and return.
    pub stop: Arc<AtomicBool>,
    /// Turn slots shared with the rest of the fleet (fleet mode with a cap).
    pub slots: Option<Arc<TurnSlots>>,
}

/// A fleet-wide cap on turns running at once. An agent takes a slot after
/// its gates pass and before it claims anything, and holds it for the turn.
pub struct TurnSlots {
    cap: usize,
    used: std::sync::Mutex<usize>,
}

impl TurnSlots {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(TurnSlots {
            cap: cap.max(1),
            used: std::sync::Mutex::new(0),
        })
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// A slot if one is free; it is given back when the guard drops.
    pub fn try_take(self: &Arc<Self>) -> Option<SlotGuard> {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if *used >= self.cap {
            return None;
        }
        *used += 1;
        Some(SlotGuard(Arc::clone(self)))
    }
}

pub struct SlotGuard(Arc<TurnSlots>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut used = self.0.used.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(1);
    }
}

/// What one pass of the loop did.
enum Pass {
    /// A turn ran (or was attempted).
    Ran,
    /// Nothing to do, or held back by a gate.
    Nothing,
    /// Work is waiting, but every fleet turn slot is taken.
    Waiting,
}

const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const HOOK_LIMIT: Duration = Duration::from_secs(600);

struct Agentry<'a> {
    ctx: &'a Ctx,
    me: ActorId,
    profile_name: String,
    opts: &'a RunOptions,
}

impl Agentry<'_> {
    fn store(&self) -> &dyn Store {
        self.ctx.store()
    }

    fn log(&self, msg: impl AsRef<str>) {
        vflt_core::log_line(&format!("vflt{}: {}", self.opts.prefix, msg.as_ref()));
    }

    /// Update the runtime record, keeping what the closure does not touch.
    fn runtime(&self, f: impl FnOnce(&mut AgentRuntime)) {
        let mut rt = self
            .store()
            .get_agent(&self.me)
            .map(|a| a.runtime)
            .unwrap_or_default();
        let before = rt.clone();
        f(&mut rt);
        if rt != before {
            if let Err(e) = self.store().set_runtime(&self.me, &rt) {
                self.log(format!("could not record runtime: {e}"));
            }
        }
    }

    fn set_status(&self, status: AgentStatus, note: Option<String>) {
        self.runtime(|rt| {
            rt.status = status;
            if note.is_some() {
                rt.note = note;
            }
            if status != AgentStatus::Active {
                rt.current.clear();
            }
        });
    }

    /// Sleep `d`, waking early when asked to stop.
    fn nap(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            if self.opts.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250).min(end - Instant::now()));
        }
    }
}

pub fn run(ctx: &Ctx, opts: RunOptions) -> Result<()> {
    let store = ctx.store();
    let me = ActorId::new(&opts.name);
    let agent = match &opts.profile {
        Some(p) => {
            store.profile(p)?;
            store.register_agent(NewAgent {
                name: opts.name.clone(),
                profiles: vec![p.clone()],
                ..Default::default()
            })?
        }
        None => store.get_agent(&me).with_context(|| {
            format!(
                "agent {} is not registered; run it once with --profile",
                opts.name
            )
        })?,
    };
    let profile_name = opts
        .profile
        .clone()
        .or_else(|| agent.config.profiles.first().cloned())
        .ok_or_else(|| anyhow!("agent {} has no profile", opts.name))?;
    let profile = store.profile(&profile_name)?;
    let runner_name = opts
        .runner
        .clone()
        .unwrap_or_else(|| profile.runner.clone());
    let runner = Registry::load()
        .unwrap_or_default()
        .get(&runner_name, profile.runner_command.as_deref())?;
    match runner.available()? {
        vflt_runner::Availability::Available { .. } => {}
        vflt_runner::Availability::Missing { reason } => {
            bail!("runner `{runner_name}` is not available: {reason}")
        }
    }

    let a = Agentry {
        ctx,
        me: me.clone(),
        profile_name,
        opts: &opts,
    };
    a.log(format!(
        "agent {} running profile {} with runner {} on collective {} ({})",
        me,
        a.profile_name,
        runner.name(),
        store.collective()?.name,
        ctx.root.display()
    ));
    recover(&a)?;
    store.heartbeat(&me)?;
    a.runtime(|rt| {
        rt.status = AgentStatus::Idle;
        rt.pid = Some(std::process::id());
        rt.current.clear();
    });

    let result = work_loop(&a);

    a.runtime(|rt| {
        rt.status = AgentStatus::Offline;
        rt.pid = None;
        rt.current.clear();
    });
    result
}

/// Release items this agent still holds from a turn that never finished (a
/// crash, a kill). They go back to pending for whoever should work them.
fn recover(a: &Agentry) -> Result<()> {
    let items = a.store().list_items(&ItemFilter {
        statuses: vec![Status::Claimed, Status::InProgress],
        ..Default::default()
    })?;
    for it in items {
        if it.claim.as_ref().is_some_and(|c| c.actor == a.me) {
            a.store().add_note(
                it.id(),
                &a.me,
                "Recovered on agent start: the turn that held this item did not finish (the agent exited mid-turn). Back to pending.",
            )?;
            a.store().release(it.id(), &a.me)?;
            a.log(format!("recovered {} from an unfinished turn", it.id()));
        }
    }
    Ok(())
}

fn work_loop(a: &Agentry) -> Result<()> {
    let store = a.store();
    let mut idle_logged = false;
    loop {
        if a.opts.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        store.heartbeat(&a.me)?;
        let agent = store.get_agent(&a.me)?;
        match agent.config.desired {
            Desired::Stopped => {
                a.log(format!("{} stopping: desired state is stopped", a.me));
                return Ok(());
            }
            Desired::Paused => {
                a.set_status(AgentStatus::Paused, Some("paused".into()));
                if a.opts.once {
                    return Ok(());
                }
                a.nap(a.opts.interval);
                continue;
            }
            Desired::Running => {}
        }
        let profile = match store.profile(&a.profile_name) {
            Ok(p) => p,
            Err(e) => {
                a.set_status(
                    AgentStatus::Gated,
                    Some(format!("profile {} does not load: {e}", a.profile_name)),
                );
                if a.opts.once {
                    return Err(e.into());
                }
                a.nap(a.opts.interval);
                continue;
            }
        };
        match iteration(a, &agent, &profile) {
            Ok(Pass::Ran) => {
                idle_logged = false;
            }
            Ok(Pass::Waiting) => {
                // Even with --once: the turn is owed, just not yet.
                a.nap(a.opts.interval.min(Duration::from_secs(5)));
                continue;
            }
            Ok(Pass::Nothing) => {
                if a.opts.once {
                    if !idle_logged {
                        a.log(format!("no claimable items for profile {}", profile.name));
                    }
                    return Ok(());
                }
                idle_logged = true;
                a.nap(a.opts.interval);
            }
            Err(e) => {
                a.log(format!("turn failed: {e:#}"));
                a.set_status(AgentStatus::Idle, Some(format!("last turn failed: {e}")));
                if a.opts.once {
                    return Err(e);
                }
                a.nap(a.opts.interval);
            }
        }
        if a.opts.once {
            return Ok(());
        }
    }
}

/// One pass of the loop: pick, gate, take a turn slot, claim, run.
fn iteration(a: &Agentry, agent: &Agent, profile: &Profile) -> Result<Pass> {
    let store = a.store();
    let coll = store.collective()?;
    let items = store.list_items(&ItemFilter::default())?;
    let batch: Vec<Item> = select_batch(
        &items,
        profile,
        &a.me,
        coll.claim_mode,
        Picking {
            hold: agent.config.hold,
        },
        profile.batch as usize,
    )
    .into_iter()
    .cloned()
    .collect();
    let cycle = if batch.is_empty() {
        cycle_due(agent, profile)
    } else {
        None
    };
    if batch.is_empty() && cycle.is_none() {
        if agent.config.wake_requested.is_some() {
            // A wake that cannot run (held, or no cycle to run) is spent.
            let mut cfg = agent.config.clone();
            cfg.wake_requested = None;
            store.update_agent(&cfg, &a.me, &format!("{} had nothing to wake for", a.me))?;
        }
        a.set_status(AgentStatus::Idle, Some("no work".into()));
        return Ok(Pass::Nothing);
    }

    if let Some(reason) = check_gates(a, profile)? {
        a.set_status(AgentStatus::Gated, Some(reason));
        return Ok(Pass::Nothing);
    }
    if let Some(reason) = spend_cap(a, agent, profile)? {
        a.set_status(AgentStatus::Gated, Some(reason));
        return Ok(Pass::Nothing);
    }
    let _slot = match &a.opts.slots {
        None => None,
        Some(slots) => match slots.try_take() {
            Some(guard) => Some(guard),
            None => {
                let what = if batch.is_empty() {
                    "a cycle".to_string()
                } else {
                    format!("{} ready", batch.len())
                };
                a.set_status(
                    AgentStatus::Waiting,
                    Some(format!("{what}; all {} turn slots in use", slots.cap())),
                );
                return Ok(Pass::Waiting);
            }
        },
    };

    if !batch.is_empty() {
        let mut claimed = Vec::new();
        for it in &batch {
            match store.transition(it.id(), &a.me, Transition::Start) {
                Ok(_) => claimed.push(store.get_item(it.id())?),
                Err(vflt_core::Error::AlreadyClaimed { .. })
                | Err(vflt_core::Error::InvalidTransition { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if claimed.is_empty() {
            return Ok(Pass::Ran); // lost every race; look again at once
        }
        // Work routed here by someone else wakes a snoozed cycle.
        if agent.runtime.snoozed.is_some()
            && claimed
                .iter()
                .any(|i| i.owner != a.me && i.owner.as_str() != profile.name)
        {
            a.runtime(|rt| rt.snoozed = None);
        }
        item_turn(a, agent, profile, claimed)?;
    } else if let Some(why) = cycle {
        cycle_turn(a, agent, profile, &items, why)?;
    }
    Ok(Pass::Ran)
}

/// Why a cycle turn is due now, if it is.
fn cycle_due(agent: &Agent, profile: &Profile) -> Option<&'static str> {
    if agent.config.hold {
        return None;
    }
    let cycle = agent
        .config
        .cycle
        .clone()
        .unwrap_or_else(|| profile.cycle.clone());
    let interval = vflt_core::duration::parse_opt(&cycle).ok().flatten();
    if agent.config.wake_requested.is_some() {
        return Some("wake requested");
    }
    let interval = interval?;
    if agent.runtime.snoozed.is_some() {
        return None;
    }
    match agent.runtime.last_cycle {
        None => Some("first cycle"),
        Some(t) => {
            let elapsed = (Utc::now() - t).to_std().unwrap_or_default();
            (elapsed >= interval).then_some("cycle interval elapsed")
        }
    }
}

fn base_env(a: &Agentry, profile: &Profile) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "VFLT_COLLECTIVE".into(),
            a.ctx.root.to_string_lossy().into_owned(),
        ),
        ("VFLT_AGENT".into(), a.me.to_string()),
        ("VFLT_PROFILE".into(), profile.name.clone()),
    ];
    // Hand the child the exact binary that launched it, so an agent's
    // `vflt item ...` calls hit the same build even when `vflt` is not on PATH.
    if let Ok(exe) = std::env::current_exe() {
        env.push(("VFLT_BIN".into(), exe.to_string_lossy().into_owned()));
    }
    for (k, v) in &profile.env {
        env.push((k.clone(), v.clone()));
    }
    env
}

fn resolve_dir(root: &Path, dir: &str) -> PathBuf {
    let p = Path::new(dir.trim());
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// The directory hooks and cycle turns run in when no item is involved.
fn base_dir(a: &Agentry, profile: &Profile) -> PathBuf {
    match profile.workspace.mode {
        WorkspaceMode::Item => a.ctx.root.clone(),
        _ => resolve_dir(&a.ctx.root, &profile.workspace.dir),
    }
}

/// The profile's policy for one turn, with its MCP config files resolved
/// against the collective root (the turn may run anywhere).
fn turn_policy(a: &Agentry, profile: &Profile) -> vflt_core::profile::Policy {
    let mut policy = profile.policy.clone();
    if let Some(files) = policy.mcp_config.as_mut() {
        for f in files.iter_mut() {
            *f = resolve_dir(&a.ctx.root, f).to_string_lossy().into_owned();
        }
    }
    policy
}

fn extra_dirs(a: &Agentry, profile: &Profile) -> Vec<PathBuf> {
    let mut v = vec![a.ctx.root.clone()];
    for d in &profile.workspace.add_dirs {
        v.push(resolve_dir(&a.ctx.root, d));
    }
    v
}

fn workspace_for(a: &Agentry, profile: &Profile, item: &Item) -> Result<PathBuf> {
    let store = a.store();
    match profile.workspace.mode {
        WorkspaceMode::Item => {
            let ws = store.work_dir().join(item.id().as_str());
            std::fs::create_dir_all(&ws)
                .with_context(|| format!("creating workspace {}", ws.display()))?;
            Ok(ws)
        }
        WorkspaceMode::Shared => {
            let ws = resolve_dir(&a.ctx.root, &profile.workspace.dir);
            if !ws.is_dir() {
                bail!("shared workspace {} does not exist", ws.display());
            }
            Ok(ws)
        }
        WorkspaceMode::Worktree => {
            let repo = resolve_dir(&a.ctx.root, &profile.workspace.dir);
            let ws = store.work_dir().join(item.id().as_str()).join("tree");
            if !ws.join(".git").exists() {
                if let Some(parent) = ws.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let branch = format!("vflt/{}", item.id());
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["worktree", "add", "-B", &branch])
                    .arg(&ws)
                    .output()
                    .context("running git worktree add")?;
                if !out.status.success() {
                    bail!(
                        "git worktree add for {} failed: {}",
                        item.id(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
            Ok(ws)
        }
    }
}

fn scratch_for(a: &Agentry, anchor: &str) -> PathBuf {
    a.store().work_dir().join(anchor).join("runs").join(format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        a.me.as_str().replace(['/', '\\', ' '], "_")
    ))
}

/// Run one hook command. Returns the exit code and combined output.
fn run_hook(cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<(i32, String)> {
    let argv = ShellRunner::new("hook", cmd).argv()?;
    let mut argv = argv;
    if Path::new(&argv[0]).components().count() == 1 {
        if let Ok(p) = which_program(&argv[0]) {
            argv[0] = p;
        }
    } else if Path::new(&argv[0]).is_relative() {
        argv[0] = cwd.join(&argv[0]).to_string_lossy().into_owned();
    }
    let captured = run_capture(&argv, cwd, env, "", Some(HOOK_LIMIT), |_| {})?;
    let mut out = captured.lines.join("\n");
    if !captured.stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(captured.stderr.trim_end());
    }
    Ok((
        if captured.timed_out {
            124
        } else {
            captured.exit_code
        },
        out,
    ))
}

fn which_program(name: &str) -> Result<String> {
    // Resolve through PATH (honouring PATHEXT on Windows) the same way runners do.
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for cand in [
            dir.join(name),
            dir.join(format!("{name}.exe")),
            dir.join(format!("{name}.cmd")),
        ] {
            if cand.is_file() {
                return Ok(cand.to_string_lossy().into_owned());
            }
        }
    }
    Err(anyhow!("{name} not found on PATH"))
}

fn first_line(s: &str) -> String {
    crate::util::truncate(s.trim(), 200)
}

/// The first gate that fails, as a reason. Gates run in the agent's base
/// directory.
fn check_gates(a: &Agentry, profile: &Profile) -> Result<Option<String>> {
    if profile.hooks.gate.is_empty() {
        return Ok(None);
    }
    let cwd = base_dir(a, profile);
    let env = base_env(a, profile);
    for g in &profile.hooks.gate {
        let (code, out) = run_hook(g, &cwd, &env)?;
        if code != 0 {
            let line = first_line(&out);
            return Ok(Some(if line.is_empty() {
                format!("gate `{g}` failed (exit {code})")
            } else {
                format!("gate: {line}")
            }));
        }
    }
    Ok(None)
}

fn spend_cap(a: &Agentry, agent: &Agent, profile: &Profile) -> Result<Option<String>> {
    let Some(cap) = agent.config.daily_usd.or(profile.budget.daily_usd) else {
        return Ok(None);
    };
    let midnight = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc();
    let mut spent = 0.0;
    for kind in ["agent.run", "agent.cycle"] {
        for e in a.store().events(&EventFilter {
            since: Some(midnight),
            kind: Some(kind.into()),
            actor: Some(a.me.clone()),
            ..Default::default()
        })? {
            spent += e
                .detail
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
        }
    }
    Ok((spent >= cap).then(|| format!("daily spend cap ${cap:.2} reached (${spent:.2} today)")))
}

/// Run `runner` while a thread keeps the agent's heartbeat fresh.
fn run_with_heartbeat(a: &Agentry, runner: &dyn Runner, req: RunRequest) -> Result<RunOutcome> {
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut last = Instant::now();
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                if last.elapsed() >= HEARTBEAT_EVERY {
                    let _ = a.store().heartbeat(&a.me);
                    last = Instant::now();
                }
            }
        });
        let out = runner.run(req);
        done.store(true, Ordering::Relaxed);
        out
    })
}

fn runner_for(a: &Agentry, profile: &Profile) -> Result<Box<dyn Runner>> {
    let name = a
        .opts
        .runner
        .clone()
        .unwrap_or_else(|| profile.runner.clone());
    Registry::load()
        .unwrap_or_default()
        .get(&name, profile.runner_command.as_deref())
}

fn effective_budget(agent: &Agent, profile: &Profile) -> vflt_core::Budget {
    let mut b = profile.budget.clone();
    if let Some(w) = agent
        .config
        .wall_clock
        .clone()
        .filter(|w| !w.trim().is_empty())
    {
        b.wall_clock = w;
    }
    b
}

fn effective_model(agent: &Agent, profile: &Profile) -> Option<String> {
    agent
        .config
        .model
        .clone()
        .or_else(|| Some(profile.model.clone()))
        .filter(|m| !m.trim().is_empty())
}

fn item_turn(a: &Agentry, agent: &Agent, profile: &Profile, items: Vec<Item>) -> Result<()> {
    let store = a.store();
    let coll = store.collective()?;
    let ids: Vec<String> = items.iter().map(|i| i.id().to_string()).collect();
    a.log(format!(
        "{} working {} at {}",
        a.me,
        items
            .iter()
            .map(|i| format!("{} \"{}\"", i.id(), crate::util::truncate(&i.title, 60)))
            .collect::<Vec<_>>()
            .join(", "),
        items[0].stage
    ));
    a.runtime(|rt| {
        rt.status = AgentStatus::Active;
        rt.current = items.iter().map(|i| i.id().clone()).collect();
        rt.note = Some(format!("working {}", ids.join(", ")));
    });

    let ws = match workspace_for(a, profile, &items[0]) {
        Ok(ws) => ws,
        Err(e) => {
            for it in &items {
                raise(
                    a,
                    it.id(),
                    &format!("workspace could not be prepared: {e:#}"),
                    RaiseKind::Action,
                )?;
            }
            return Ok(());
        }
    };
    let mut env = base_env(a, profile);
    env.push(("VFLT_ITEM".into(), ids[0].clone()));
    env.push(("VFLT_ITEMS".into(), ids.join(",")));
    env.push(("VFLT_WORKSPACE".into(), ws.to_string_lossy().into_owned()));

    for h in &profile.hooks.before {
        let (code, out) = run_hook(h, &ws, &env)?;
        if code != 0 {
            let reason = format!(
                "before hook `{h}` failed (exit {code}): {}",
                first_line(&out)
            );
            a.log(&reason);
            for it in &items {
                store.add_note(
                    it.id(),
                    &a.me,
                    &format!("Turn not started: {reason}\n\n{}", out.trim()),
                )?;
                store.release(it.id(), &a.me)?;
            }
            a.set_status(AgentStatus::Gated, Some(reason));
            return Ok(());
        }
    }

    let mut with_notes = Vec::new();
    for it in &items {
        let full = store.get_item(it.id())?;
        let notes = store.notes(it.id())?;
        with_notes.push((full, notes));
    }
    let full_items: Vec<Item> = with_notes.iter().map(|(i, _)| i.clone()).collect();
    let root = a.ctx.root.to_string_lossy().into_owned();
    let scratch = scratch_for(a, &ids[0]);
    let runner = runner_for(a, profile)?;
    let req = RunRequest {
        system_prompt: prompt::system_prompt(profile, &coll, &root, a.me.as_str(), &full_items),
        prompt: prompt::items_prompt(store, &coll, &with_notes, &ws.to_string_lossy()),
        cwd: ws.clone(),
        scratch_dir: scratch.clone(),
        extra_dirs: extra_dirs(a, profile),
        model: effective_model(agent, profile),
        policy: turn_policy(a, profile),
        budget: effective_budget(agent, profile),
        env: env.clone(),
        log_path: Some(scratch.join("transcript.jsonl")),
        session: None,
    };
    let outcome = run_with_heartbeat(a, runner.as_ref(), req);
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            a.log(format!("runner failed on {}: {e:#}", ids.join(", ")));
            for it in &items {
                store.add_note(it.id(), &a.me, &format!("runner failed to launch: {e:#}"))?;
                raise(
                    a,
                    it.id(),
                    &format!("runner `{}` failed to launch: {e}", runner.name()),
                    RaiseKind::Action,
                )?;
            }
            a.set_status(AgentStatus::Idle, Some(format!("runner failed: {e}")));
            return Ok(());
        }
    };
    a.log(format!(
        "{} finished {} (exit {}, turns {}, cost {})",
        a.me,
        ids.join(", "),
        outcome.exit_code,
        outcome
            .num_turns
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into()),
        outcome
            .cost_usd
            .map(|c| format!("${c:.4}"))
            .unwrap_or_else(|| "n/a".into())
    ));

    let mut after_failure: Option<String> = None;
    for h in &profile.hooks.after {
        let (code, out) = run_hook(h, &ws, &env)?;
        if code != 0 {
            after_failure = Some(format!(
                "After hook `{h}` failed (exit {code}):\n\n{}",
                out.trim()
            ));
        }
    }

    let n = items.len().max(1) as f64;
    for it in &items {
        record_outcome(
            a,
            runner.as_ref(),
            it,
            &outcome,
            n,
            after_failure.as_deref(),
            &scratch,
        )?;
    }
    a.set_status(
        AgentStatus::Idle,
        Some(format!(
            "finished {}: {}",
            ids.join(", "),
            first_line(&outcome.result_text)
        )),
    );
    Ok(())
}

fn raise(a: &Agentry, id: &vflt_core::ItemId, question: &str, kind: RaiseKind) -> Result<()> {
    a.store()
        .transition(
            id,
            &a.me,
            Transition::Raise {
                question: question.to_string(),
                kind,
            },
        )
        .map_err(|e| anyhow!("raising {id}: {e}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_outcome(
    a: &Agentry,
    runner: &dyn Runner,
    item: &Item,
    o: &RunOutcome,
    batch: f64,
    after_failure: Option<&str>,
    scratch: &Path,
) -> Result<()> {
    let store = a.store();
    store.append_event(
        Event::new(
            &a.me,
            "agent.run",
            Some(item.id()),
            format!(
                "{} ran {} on {} (exit {})",
                a.me,
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
            "cost_usd": o.cost_usd.map(|c| c / batch),
            "num_turns": o.num_turns,
            "denials": o.permission_denials.len(),
            "batch": batch as u32,
            "transcript": scratch.join("transcript.jsonl"),
        })),
    )?;

    if let Some(text) = after_failure {
        store.add_note(item.id(), &a.me, text)?;
    }
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
        store.add_note(item.id(), &a.me, &body)?;
    }

    let after = store.get_item(item.id())?;
    let still_mine = matches!(after.status, Status::InProgress | Status::Claimed)
        && after.claim.as_ref().is_some_and(|c| c.actor == a.me);
    if !still_mine {
        return Ok(());
    }

    // The agent exited without finishing this item: leave evidence and raise.
    let mut body = format!(
        "Runner `{}` exited (code {}{}{}) without completing, bouncing, parking, handing off, blocking or raising this item.\n\n",
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
    store.add_note(item.id(), &a.me, &body)?;
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
    raise(a, item.id(), &question, RaiseKind::Decision)
}

fn cycle_turn(
    a: &Agentry,
    agent: &Agent,
    profile: &Profile,
    items: &[Item],
    why: &str,
) -> Result<()> {
    let store = a.store();
    let coll = store.collective()?;
    a.log(format!("{} cycle turn ({}; {why})", a.me, profile.name));
    a.runtime(|rt| {
        rt.status = AgentStatus::Active;
        rt.current.clear();
        rt.note = Some(format!("cycle turn: {why}"));
    });
    // Spend the wake and stamp the cycle before running, so a crash mid-turn
    // does not loop on the same cycle.
    if agent.config.wake_requested.is_some() {
        let mut cfg = agent.config.clone();
        cfg.wake_requested = None;
        store.update_agent(
            &cfg,
            &a.me,
            &format!("{} took its requested cycle turn", a.me),
        )?;
    }
    a.runtime(|rt| rt.last_cycle = Some(Utc::now()));

    let ws = match profile.workspace.mode {
        WorkspaceMode::Item => {
            let ws = store.work_dir().join("_cycles").join(&profile.name);
            std::fs::create_dir_all(&ws)?;
            ws
        }
        _ => base_dir(a, profile),
    };
    let scratch = scratch_for(
        a,
        &format!("_cycles/{}", a.me.as_str().replace(['/', '\\', ' '], "_")),
    );
    let root = a.ctx.root.to_string_lossy().into_owned();
    let runner = runner_for(a, profile)?;
    let mut env = base_env(a, profile);
    env.push(("VFLT_WORKSPACE".into(), ws.to_string_lossy().into_owned()));
    let req = RunRequest {
        system_prompt: prompt::system_prompt(profile, &coll, &root, a.me.as_str(), &[]),
        prompt: prompt::cycle_prompt(items, &coll, agent, profile),
        cwd: ws,
        scratch_dir: scratch.clone(),
        extra_dirs: extra_dirs(a, profile),
        model: effective_model(agent, profile),
        policy: turn_policy(a, profile),
        budget: effective_budget(agent, profile),
        env,
        log_path: Some(scratch.join("transcript.jsonl")),
        session: None,
    };
    let o = run_with_heartbeat(a, runner.as_ref(), req)?;
    store.append_event(
        Event::new(
            &a.me,
            "agent.cycle",
            None,
            format!(
                "{} ran a {} cycle (exit {})",
                a.me, profile.name, o.exit_code
            ),
        )
        .with_detail(serde_json::json!({
            "runner": runner.name(),
            "exit_code": o.exit_code,
            "timed_out": o.timed_out,
            "session_id": o.session_id,
            "cost_usd": o.cost_usd,
            "num_turns": o.num_turns,
            "reason": why,
            "summary": crate::util::truncate(&o.result_text, 400),
            "transcript": scratch.join("transcript.jsonl"),
        })),
    )?;
    a.set_status(
        AgentStatus::Idle,
        Some(format!("cycle: {}", first_line(&o.result_text))),
    );
    Ok(())
}
