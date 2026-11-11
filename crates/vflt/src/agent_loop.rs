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
///
/// A free slot goes to the agent whose work is most urgent, not to whoever
/// asks first: an agent that has just finished a turn asks again at once,
/// while the others ask every few seconds, so first-come would let the agents
/// already running keep every slot for as long as they have work. Each asker
/// is ranked by the best priority among the items it is ready to run (lower
/// runs first); every full minute it has waited counts as one priority point,
/// so low-priority work is delayed and never starved; ties go to whoever has
/// waited longest.
pub struct TurnSlots {
    cap: usize,
    state: std::sync::Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    used: usize,
    waiting: Vec<Waiter>,
}

struct Waiter {
    who: String,
    priority: i32,
    /// When it first asked and was refused.
    since: Instant,
    /// When it last asked: an agent that stops asking drops out of the line.
    seen: Instant,
}

/// A waiter that has not asked again for this long has gone (paused, gated,
/// stopped, or out of work). Waiting agents ask at least every five seconds.
const WAITER_GONE: Duration = Duration::from_secs(30);
/// How long a wait counts as one priority point.
const WAIT_PER_POINT: Duration = Duration::from_secs(60);
/// The priority a cycle turn (no claimable item) asks with.
pub const CYCLE_TURN_PRIORITY: i32 = 100;

impl Waiter {
    fn rank(&self, now: Instant) -> (i64, Instant) {
        let waited = now.saturating_duration_since(self.since).as_secs() / WAIT_PER_POINT.as_secs();
        (i64::from(self.priority) - waited as i64, self.since)
    }
}

impl TurnSlots {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(TurnSlots {
            cap: cap.max(1),
            state: std::sync::Mutex::new(SlotState::default()),
        })
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// A slot, if one is free and nobody with more urgent work (or the same
    /// urgency and a longer wait) is in line for it. `priority` is the best
    /// priority among the work `who` would run. The slot is given back when
    /// the guard drops. A refused asker keeps its place by asking again.
    pub fn try_take(self: &Arc<Self>, who: &str, priority: i32) -> Option<SlotGuard> {
        self.try_take_at(who, priority, Instant::now())
    }

    fn try_take_at(self: &Arc<Self>, who: &str, priority: i32, now: Instant) -> Option<SlotGuard> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.waiting
            .retain(|w| w.who == who || now.saturating_duration_since(w.seen) < WAITER_GONE);
        match st.waiting.iter_mut().find(|w| w.who == who) {
            Some(w) => {
                w.priority = priority;
                w.seen = now;
            }
            None => st.waiting.push(Waiter {
                who: who.to_string(),
                priority,
                since: now,
                seen: now,
            }),
        }
        let free = self.cap.saturating_sub(st.used);
        if free == 0 {
            return None;
        }
        let mine = st
            .waiting
            .iter()
            .find(|w| w.who == who)
            .map(|w| w.rank(now))?;
        let ahead = st
            .waiting
            .iter()
            .filter(|w| w.who != who && w.rank(now) < mine)
            .count();
        if ahead >= free {
            return None;
        }
        st.waiting.retain(|w| w.who != who);
        st.used += 1;
        Some(SlotGuard(Arc::clone(self)))
    }
}

pub struct SlotGuard(Arc<TurnSlots>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.used = st.used.saturating_sub(1);
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
    /// While the runner fails before doing any work: no turn starts before
    /// the instant, and the wait doubles with each failure.
    fault: std::sync::Mutex<Option<Backoff>>,
}

#[derive(Debug, Clone)]
struct Backoff {
    until: Instant,
    wait: Duration,
    why: String,
}

const FAULT_FIRST_WAIT: Duration = Duration::from_secs(60);
const FAULT_MAX_WAIT: Duration = Duration::from_secs(15 * 60);

impl Backoff {
    fn after(prev: Option<&Backoff>, why: String) -> Backoff {
        let wait = prev
            .map(|b| (b.wait * 2).min(FAULT_MAX_WAIT))
            .unwrap_or(FAULT_FIRST_WAIT);
        Backoff {
            until: Instant::now() + wait,
            wait,
            why,
        }
    }
}

/// A runner that reports an error and a cost of exactly nothing never
/// completed a model call, so it did no work: the fault is the runner's
/// (credentials, quota, a broken install), not the item's. Runners that do
/// not report cost are never judged this way.
fn runner_fault(o: &RunOutcome) -> bool {
    o.is_error && !o.timed_out && o.exit_code != 0 && o.cost_usd == Some(0.0)
}

/// How a turn that left its item unfinished was cut short when the cause is
/// the turn's circumstances, not the item: nothing for a human to decide, so
/// the item goes back to its agent and the next turn resumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutShort {
    /// Every denial was the permission check giving no verdict (an outage of
    /// the check), never a judgment about an action.
    NoVerdict,
    /// The wall-clock budget ran out with nothing denied.
    WallClock,
}

/// Every note releasing an item after a cut-short turn starts with this.
const CUT_SHORT: &str = "Turn cut short";

impl CutShort {
    fn why(self) -> &'static str {
        match self {
            CutShort::NoVerdict => "the permission check was unavailable",
            CutShort::WallClock => "wall-clock budget exceeded",
        }
    }
}

fn cut_short(o: &RunOutcome) -> Option<CutShort> {
    let d = &o.permission_denials;
    if !d.is_empty() && d.iter().all(|d| d.no_verdict) {
        Some(CutShort::NoVerdict)
    } else if o.timed_out && d.is_empty() {
        Some(CutShort::WallClock)
    } else {
        None
    }
}

/// The directory private to one turn, exported under the profile's
/// `workspace.turn_tmp` names and removed when the turn ends, however it
/// ends. A tool killed mid-turn (a wall-clock limit, a timed-out shell call)
/// never removes its temporary files; this does it for it.
struct TurnTmp(Option<PathBuf>);

impl TurnTmp {
    /// One directory per agent under `base`: an agent runs one turn at a
    /// time, so anything already there was left by a turn that never reached
    /// its end (a killed fleet, a crash) and is removed first.
    fn new(base: &Path, collective: &str, agent: &str, names: &[String]) -> Result<TurnTmp> {
        if names.is_empty() {
            return Ok(TurnTmp(None));
        }
        let slug = |s: &str| s.replace(['/', '\\', ' '], "_");
        let dir = base
            .join(format!("vflt-{}", slug(collective)))
            .join(slug(agent));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(TurnTmp(Some(dir)))
    }

    /// Point each of `names` at the directory.
    fn export(&self, names: &[String], env: &mut Vec<(String, String)>) {
        if let Some(dir) = &self.0 {
            for n in names {
                env.push((n.clone(), dir.to_string_lossy().into_owned()));
            }
        }
    }
}

impl Drop for TurnTmp {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The turn's private directory, or none (logged) when it cannot be made:
/// the turn then runs with the variables unset rather than not at all.
fn turn_tmp(
    a: &Agentry,
    collective: &str,
    profile: &Profile,
    env: &mut Vec<(String, String)>,
) -> TurnTmp {
    let names = &profile.workspace.turn_tmp;
    match TurnTmp::new(&std::env::temp_dir(), collective, a.me.as_str(), names) {
        Ok(t) => {
            t.export(names, env);
            t
        }
        Err(e) => {
            a.log(format!(
                "turn temp dir not prepared ({e:#}); running without it"
            ));
            TurnTmp(None)
        }
    }
}

impl Agentry<'_> {
    fn store(&self) -> &dyn Store {
        self.ctx.store()
    }

    fn log(&self, msg: impl AsRef<str>) {
        vflt_core::log_line(&format!("vflt{}: {}", self.opts.prefix, msg.as_ref()));
    }

    /// Record a runner fault; returns how long the agent now waits.
    fn runner_failed(&self, why: &str) -> Duration {
        let mut f = self.fault.lock().unwrap_or_else(|e| e.into_inner());
        let next = Backoff::after(f.as_ref(), why.to_string());
        let wait = next.wait;
        *f = Some(next);
        wait
    }

    fn runner_worked(&self) {
        *self.fault.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The pending backoff, while it lasts.
    fn backing_off(&self) -> Option<Backoff> {
        let f = self.fault.lock().unwrap_or_else(|e| e.into_inner());
        f.as_ref().filter(|b| Instant::now() < b.until).cloned()
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
        fault: std::sync::Mutex::new(None),
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

    if let Some(b) = a.backing_off() {
        a.set_status(
            AgentStatus::Gated,
            Some(format!(
                "runner failing ({}); next try in {}s",
                b.why,
                b.until.saturating_duration_since(Instant::now()).as_secs()
            )),
        );
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
        Some(slots) => match slots.try_take(
            a.me.as_str(),
            batch
                .iter()
                .map(|i| i.priority)
                .min()
                .unwrap_or(CYCLE_TURN_PRIORITY),
        ) {
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
    let _turn_tmp = turn_tmp(a, &coll.name, profile, &mut env);

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

    if runner_fault(&outcome) {
        let why = first_line(&outcome.result_text);
        let wait = a.runner_failed(&why);
        a.log(format!(
            "runner fault on {} ({why}): released; next try in {}s",
            ids.join(", "),
            wait.as_secs()
        ));
        for it in &items {
            store.add_note(
                it.id(),
                &a.me,
                &format!(
                    "Turn did no work: the runner failed before its first model call ({why}). \
                     Released back to pending; the agent tries again after a backoff."
                ),
            )?;
            store.release(it.id(), &a.me)?;
        }
        for h in &profile.hooks.after {
            let _ = run_hook(h, &ws, &env);
        }
        a.set_status(AgentStatus::Gated, Some(format!("runner failing: {why}")));
        return Ok(());
    }
    a.runner_worked();

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
    if cut_short(&outcome) == Some(CutShort::NoVerdict) {
        // The check is down for every agent: wait it out rather than spend
        // turns that cannot act.
        let why = CutShort::NoVerdict.why();
        let wait = a.runner_failed(why);
        a.log(format!(
            "{} cut short on {} ({why}): released; next try in {}s",
            a.me,
            ids.join(", "),
            wait.as_secs()
        ));
        a.set_status(AgentStatus::Gated, Some(format!("runner failing: {why}")));
        return Ok(());
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
        let mut body = String::from(if o.permission_denials.iter().all(|d| d.no_verdict) {
            "The runner's permission check gave no verdict (it was unavailable) on these actions:\n\n"
        } else {
            "The runner's permission classifier denied these actions:\n\n"
        });
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

    // The agent exited without finishing this item: leave evidence, then
    // release it for the next turn when the turn was cut short through no
    // fault of the item's, else raise.
    let cut = cut_short(o);
    let resumed_before = store
        .notes(item.id())?
        .iter()
        .any(|n| n.actor == a.me && n.body.starts_with(CUT_SHORT));
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
    // An unavailable check repeats nothing about the item, so it resumes
    // every time (the agent backs off meanwhile); a turn that ran out of
    // time resumes once, and a second one asks a human.
    if let Some(cut) = cut.filter(|c| *c == CutShort::NoVerdict || !resumed_before) {
        store.add_note(
            item.id(),
            &a.me,
            &format!(
                "{CUT_SHORT} ({}): released back to pending; the next turn resumes it from \
                 these notes and the work already done.",
                cut.why()
            ),
        )?;
        store.release(item.id(), &a.me)?;
        return Ok(());
    }
    let question = if !o.permission_denials.is_empty() {
        format!(
            "runner was denied {} action(s) by the permission policy and did not finish; see notes",
            o.permission_denials.len()
        )
    } else if o.timed_out {
        "runner exceeded its wall-clock budget again without finishing; see notes".to_string()
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
    let _turn_tmp = turn_tmp(a, &coll.name, profile, &mut env);
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
    if runner_fault(&o) {
        let why = first_line(&o.result_text);
        let wait = a.runner_failed(&why);
        a.log(format!(
            "runner fault on a cycle turn ({why}); next try in {}s",
            wait.as_secs()
        ));
        a.set_status(AgentStatus::Gated, Some(format!("runner failing: {why}")));
        return Ok(());
    }
    a.runner_worked();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_slot_goes_to_the_longest_waiter_not_to_the_agent_that_just_finished() {
        let slots = TurnSlots::new(1);
        let t0 = Instant::now();
        let held = slots.try_take_at("busy", 50, t0).expect("first asker runs");
        assert!(slots
            .try_take_at("patient", 50, t0 + Duration::from_secs(1))
            .is_none());
        drop(held);
        // The agent that just finished asks again at once, same priority.
        assert!(slots
            .try_take_at("busy", 50, t0 + Duration::from_secs(2))
            .is_none());
        assert!(slots
            .try_take_at("patient", 50, t0 + Duration::from_secs(3))
            .is_some());
    }

    #[test]
    fn a_free_slot_goes_to_the_most_urgent_work_first() {
        let slots = TurnSlots::new(1);
        let t0 = Instant::now();
        let held = slots.try_take_at("busy", 50, t0).unwrap();
        assert!(slots
            .try_take_at("routine", 50, t0 + Duration::from_secs(1))
            .is_none());
        assert!(slots
            .try_take_at("urgent", 5, t0 + Duration::from_secs(2))
            .is_none());
        drop(held);
        assert!(slots
            .try_take_at("routine", 50, t0 + Duration::from_secs(3))
            .is_none());
        let urgent = slots.try_take_at("urgent", 5, t0 + Duration::from_secs(4));
        assert!(urgent.is_some());
        drop(urgent);
        assert!(slots
            .try_take_at("routine", 50, t0 + Duration::from_secs(5))
            .is_some());
    }

    #[test]
    fn waiting_raises_urgency_so_low_priority_work_is_not_starved() {
        let slots = TurnSlots::new(1);
        let t0 = Instant::now();
        let mut held = slots.try_take_at("busy", 40, t0);
        assert!(held.is_some());
        assert!(slots.try_take_at("low", 50, t0).is_none());
        // "busy" always has priority-40 work and asks again the moment it
        // finishes; "low" asks every five seconds. Ten points behind, "low"
        // must get the slot once it has waited ten minutes, and not before.
        let mut got_at = None;
        for s in (5..=15 * 60).step_by(5) {
            let now = t0 + Duration::from_secs(s);
            drop(held.take()); // the turn ends, and "busy" asks first
            held = slots.try_take_at("busy", 40, now);
            if let Some(g) = slots.try_take_at("low", 50, now) {
                assert!(held.is_none(), "one slot, two holders");
                drop(g);
                got_at = Some(s);
                break;
            }
            assert!(held.is_some(), "a free slot nobody took");
        }
        let got_at = got_at.expect("low-priority work ran");
        assert!((10 * 60..=11 * 60).contains(&got_at), "ran after {got_at}s");
    }

    #[test]
    fn an_agent_that_stops_asking_leaves_the_line() {
        let slots = TurnSlots::new(1);
        let t0 = Instant::now();
        let held = slots.try_take_at("busy", 50, t0).unwrap();
        assert!(slots.try_take_at("gone", 1, t0).is_none());
        drop(held);
        // "gone" never asks again; within the grace period it still holds the line.
        assert!(slots
            .try_take_at("busy", 50, t0 + Duration::from_secs(5))
            .is_none());
        assert!(slots
            .try_take_at("busy", 50, t0 + Duration::from_secs(31))
            .is_some());
    }

    #[test]
    fn two_free_slots_serve_the_two_best_askers() {
        let slots = TurnSlots::new(2);
        let t0 = Instant::now();
        let a = slots.try_take_at("a", 50, t0).unwrap();
        let b = slots.try_take_at("b", 50, t0).unwrap();
        assert!(slots.try_take_at("c", 10, t0).is_none());
        assert!(slots.try_take_at("d", 20, t0).is_none());
        assert!(slots.try_take_at("e", 30, t0).is_none());
        drop(a);
        drop(b);
        assert!(slots
            .try_take_at("e", 30, t0 + Duration::from_secs(1))
            .is_none());
        assert!(slots
            .try_take_at("d", 20, t0 + Duration::from_secs(1))
            .is_some());
        assert!(slots
            .try_take_at("c", 10, t0 + Duration::from_secs(1))
            .is_some());
    }

    #[test]
    fn a_turn_cut_short_by_its_circumstances_is_told_apart() {
        let denial = |no_verdict| vflt_runner::PermissionDenial {
            tool: "Bash".into(),
            reason: None,
            input: serde_json::Value::Null,
            no_verdict,
        };
        let mut o = outcome(0, Some(1.0), false, false);
        o.permission_denials = vec![denial(true), denial(true)];
        assert_eq!(cut_short(&o), Some(CutShort::NoVerdict));
        o.permission_denials.push(denial(false));
        assert_eq!(cut_short(&o), None, "a real judgment is a human's to read");

        let mut t = outcome(124, Some(3.0), true, true);
        assert_eq!(cut_short(&t), Some(CutShort::WallClock));
        t.permission_denials = vec![denial(false)];
        assert_eq!(cut_short(&t), None);
        assert_eq!(cut_short(&outcome(0, Some(1.0), false, false)), None);
    }

    #[test]
    fn a_turns_temp_dir_is_exported_and_removed_when_the_turn_ends() {
        let base = tempfile::tempdir().unwrap();
        let names = vec!["TMPDIR".to_string(), "GOTMPDIR".to_string()];
        let mut env = Vec::new();
        let dir = {
            let t = TurnTmp::new(base.path(), "coll", "agent-a", &names).unwrap();
            t.export(&names, &mut env);
            let dir = t.0.clone().unwrap();
            std::fs::write(dir.join("left-by-a-killed-tool"), b"x").unwrap();
            dir
        };
        assert_eq!(env.len(), 2);
        assert!(env.iter().all(|(_, v)| Path::new(v) == dir));
        assert!(!dir.exists(), "dropped with the turn");
    }

    #[test]
    fn a_turns_temp_dir_starts_empty_and_is_skipped_without_names() {
        let base = tempfile::tempdir().unwrap();
        let names = vec!["TMPDIR".to_string()];
        let orphan = base.path().join("vflt-coll").join("agent-a");
        std::fs::create_dir_all(orphan.join("from-a-crashed-turn")).unwrap();
        let sibling = base.path().join("vflt-coll").join("agent-ab");
        std::fs::create_dir_all(&sibling).unwrap();
        let t = TurnTmp::new(base.path(), "coll", "agent-a", &names).unwrap();
        assert!(t.0.as_ref().unwrap().read_dir().unwrap().next().is_none());
        assert!(sibling.exists(), "another agent's dir is not touched");
        drop(t);

        let none = TurnTmp::new(base.path(), "coll", "agent-a", &[]).unwrap();
        assert!(none.0.is_none());
        let mut env = Vec::new();
        none.export(&[], &mut env);
        assert!(env.is_empty());
    }

    fn outcome(exit: i32, cost: Option<f64>, is_error: bool, timed_out: bool) -> RunOutcome {
        RunOutcome {
            exit_code: exit,
            cost_usd: cost,
            is_error,
            timed_out,
            ..Default::default()
        }
    }

    #[test]
    fn a_runner_fault_is_an_error_that_cost_nothing() {
        assert!(runner_fault(&outcome(1, Some(0.0), true, false)));
        // it did some work: the item's problem, not the runner's
        assert!(!runner_fault(&outcome(1, Some(0.02), true, false)));
        // runners that report no cost are never judged this way
        assert!(!runner_fault(&outcome(1, None, true, false)));
        // a wall-clock kill is the item's budget, not a runner fault
        assert!(!runner_fault(&outcome(124, Some(0.0), true, true)));
        assert!(!runner_fault(&outcome(0, Some(0.0), false, false)));
    }

    #[test]
    fn backoff_doubles_up_to_its_cap() {
        let first = Backoff::after(None, "x".into());
        assert_eq!(first.wait, FAULT_FIRST_WAIT);
        let mut b = first;
        for _ in 0..10 {
            b = Backoff::after(Some(&b), "x".into());
        }
        assert_eq!(b.wait, FAULT_MAX_WAIT);
    }
}
