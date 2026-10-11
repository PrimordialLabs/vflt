use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde::Serialize;
use vflt_core::{
    discover, ActorId, Agent, AgentKind, AgentStatus, Collective, Desired, Item, ItemFilter,
    NewAgent, Profile, Status, StoreKind,
};

use crate::agent_loop::{self, RunOptions};
use crate::cli::AgentCmd;
use crate::ctx::Ctx;
use crate::util;

pub fn run(ctx: &Ctx, cmd: AgentCmd) -> Result<()> {
    let store = ctx.store();
    match cmd {
        AgentCmd::Add {
            name,
            profiles,
            remote,
            human,
        } => {
            if !human && profiles.is_empty() {
                bail!("an agent needs at least one --profile (or --human for a person)");
            }
            for p in &profiles {
                store.profile(p)?;
            }
            let a = store.register_agent(NewAgent {
                name,
                profiles,
                kind: Some(if human {
                    AgentKind::Human
                } else if remote.is_some() {
                    AgentKind::Remote
                } else {
                    AgentKind::Local
                }),
                remote,
                ..Default::default()
            })?;
            ctx.emit(&a, || {
                format!(
                    "registered {} {} with profiles {}\n",
                    kind_str(a.config.kind),
                    a.id(),
                    a.config.profiles.join(",")
                )
            })
        }
        AgentCmd::Run {
            profile,
            once,
            interval,
            name,
            runner,
        } => {
            let interval = humantime::parse_duration(&interval)
                .with_context(|| format!("--interval {interval:?}"))?;
            let name = match (&name, &profile) {
                (Some(n), _) => n.clone(),
                (None, Some(p)) => format!("{p}@{}", crate::identity::hostname()),
                (None, None) => bail!("give --profile, or --name of a registered agent"),
            };
            agent_loop::run(
                ctx,
                RunOptions {
                    name,
                    profile,
                    once,
                    interval,
                    runner,
                    prefix: String::new(),
                    stop: Default::default(),
                },
            )
        }
        AgentCmd::Lend { agent, to } => lend(ctx, &agent, &to),
        AgentCmd::List => list(ctx),
        AgentCmd::Remove { name } => {
            let id = ActorId::new(&name);
            store.deregister_agent(&id)?;
            ctx.emit(&serde_json::json!({"removed": name}), || {
                format!("removed agent {name}\n")
            })
        }
        AgentCmd::Pause { names, all } => set_desired(ctx, names, all, Desired::Paused),
        AgentCmd::Resume { names, all } => set_desired(ctx, names, all, Desired::Running),
        AgentCmd::Stop { names, all } => set_desired(ctx, names, all, Desired::Stopped),
        AgentCmd::Wake { name } => {
            let mut a = store.get_agent(&ActorId::new(&name))?;
            a.config.wake_requested = Some(Utc::now());
            store.update_agent(
                &a.config,
                &ctx.actor,
                &format!("{} asked {name} for a cycle turn", ctx.actor),
            )?;
            ctx.emit(
                &serde_json::json!({"ok": true, "name": name, "wake_requested": true}),
                || format!("{name} will run a cycle turn at its next opportunity\n"),
            )
        }
        AgentCmd::Hold { name, state } => {
            let on = match state.as_str() {
                "on" => true,
                "off" => false,
                other => bail!("hold takes on or off (got {other})"),
            };
            let mut a = store.get_agent(&ActorId::new(&name))?;
            a.config.hold = on;
            store.update_agent(
                &a.config,
                &ctx.actor,
                &format!("{} set hold {} on {name}", ctx.actor, state),
            )?;
            ctx.emit(
                &serde_json::json!({"ok": true, "name": name, "hold": on}),
                || {
                    if on {
                        format!("{name} now works only what others route to it\n")
                    } else {
                        format!("{name} hold released\n")
                    }
                },
            )
        }
        AgentCmd::Snooze { name } => {
            let id = name.map(ActorId::new).unwrap_or_else(|| ctx.actor.clone());
            let mut a = store.get_agent(&id)?;
            a.runtime.snoozed = Some(Utc::now());
            store.set_runtime(&id, &a.runtime)?;
            store.append_event(vflt_core::Event::new(
                &ctx.actor,
                "agent.snoozed",
                None,
                format!("{id} snoozed its cycle until work is routed to it"),
            ))?;
            ctx.emit(
                &serde_json::json!({"ok": true, "name": id, "snoozed": true}),
                || format!("{id} snoozed: no cycle turns until work is routed to it\n"),
            )
        }
        AgentCmd::Set {
            name,
            model,
            cycle,
            wall_clock,
            daily_usd,
        } => {
            let mut a = store.get_agent(&ActorId::new(&name))?;
            let mut changes = Vec::new();
            fn clearable(v: String) -> Option<String> {
                let v = v.trim().to_string();
                if v.is_empty() || v == "none" || v == "default" {
                    None
                } else {
                    Some(v)
                }
            }
            if let Some(m) = model {
                a.config.model = clearable(m);
                changes.push(format!(
                    "model={}",
                    a.config.model.as_deref().unwrap_or("(profile)")
                ));
            }
            if let Some(c) = cycle {
                let v = clearable(c);
                if let Some(v) = &v {
                    if v != "off" {
                        humantime::parse_duration(v).with_context(|| format!("--cycle {v:?}"))?;
                    }
                }
                // "off" disables cycles for this agent; stored as an empty string.
                a.config.cycle = v.map(|v| if v == "off" { String::new() } else { v });
                changes.push(format!(
                    "cycle={}",
                    a.config
                        .cycle
                        .as_deref()
                        .map(|c| if c.is_empty() { "off" } else { c })
                        .unwrap_or("(profile)")
                ));
            }
            if let Some(w) = wall_clock {
                let v = clearable(w);
                if let Some(v) = &v {
                    humantime::parse_duration(v).with_context(|| format!("--wall-clock {v:?}"))?;
                }
                a.config.wall_clock = v;
                changes.push(format!(
                    "wall_clock={}",
                    a.config.wall_clock.as_deref().unwrap_or("(profile)")
                ));
            }
            if let Some(d) = daily_usd {
                a.config.daily_usd = match clearable(d) {
                    Some(v) => Some(
                        v.parse::<f64>()
                            .map_err(|e| anyhow!("--daily-usd {v:?}: {e}"))?,
                    ),
                    None => None,
                };
                changes.push(format!(
                    "daily_usd={}",
                    a.config
                        .daily_usd
                        .map(|d| format!("{d}"))
                        .unwrap_or_else(|| "(profile)".into())
                ));
            }
            if changes.is_empty() {
                bail!("nothing to set: give --model, --cycle, --wall-clock or --daily-usd");
            }
            store.update_agent(
                &a.config,
                &ctx.actor,
                &format!("{} set {name}: {}", ctx.actor, changes.join(", ")),
            )?;
            ctx.emit(&a.config, || format!("{name}: {}\n", changes.join(", ")))
        }
    }
}

fn kind_str(k: AgentKind) -> &'static str {
    match k {
        AgentKind::Local => "agent",
        AgentKind::Remote => "remote agent",
        AgentKind::Human => "human",
    }
}

fn set_desired(ctx: &Ctx, names: Vec<String>, all: bool, desired: Desired) -> Result<()> {
    let store = ctx.store();
    let targets: Vec<Agent> = if all {
        store
            .list_agents()?
            .into_iter()
            .filter(|a| a.config.kind == AgentKind::Local)
            .collect()
    } else {
        if names.is_empty() {
            bail!("name at least one agent, or --all");
        }
        names
            .iter()
            .map(|n| {
                store
                    .get_agent(&ActorId::new(n))
                    .map_err(anyhow::Error::from)
            })
            .collect::<Result<_>>()?
    };
    let mut done = Vec::new();
    for mut a in targets {
        a.config.desired = desired;
        store.update_agent(
            &a.config,
            &ctx.actor,
            &format!("{} set {} desired {}", ctx.actor, a.id(), desired.as_str()),
        )?;
        done.push(a.id().to_string());
    }
    #[derive(Serialize)]
    struct Out {
        ok: bool,
        desired: Desired,
        agents: Vec<String>,
    }
    let out = Out {
        ok: true,
        desired,
        agents: done,
    };
    ctx.emit(&out, || {
        format!("{} -> {}\n", out.agents.join(", "), desired.as_str())
    })
}

/// The work waiting on an agent: items routed to it or to one of its
/// profiles.
#[derive(Debug, Default, Serialize)]
pub struct Queue {
    /// Pending, claimable now (dependencies done).
    pub ready: usize,
    /// Pending but waiting on other items.
    pub waiting: usize,
    /// Came back by a bounce.
    pub returned: usize,
    pub in_progress: usize,
    pub blocked: usize,
    pub needs_human: usize,
    pub drafts: usize,
}

pub fn queue_for(agent: &Agent, items: &[Item]) -> Queue {
    let names: Vec<&str> = std::iter::once(agent.id().as_str())
        .chain(agent.config.profiles.iter().map(String::as_str))
        .collect();
    let closed: std::collections::HashSet<&vflt_core::ItemId> = items
        .iter()
        .filter(|i| i.status.is_terminal())
        .map(|i| i.id())
        .collect();
    let mut q = Queue::default();
    for i in items {
        let routed = i
            .assignee
            .as_ref()
            .is_some_and(|a| names.contains(&a.as_str()));
        let claimed_by_me = i.claim.as_ref().is_some_and(|c| &c.actor == agent.id());
        if !routed && !claimed_by_me {
            continue;
        }
        match i.status {
            Status::Pending => {
                if i.deps.iter().all(|d| closed.contains(d)) {
                    q.ready += 1;
                } else {
                    q.waiting += 1;
                }
                if i.returned {
                    q.returned += 1;
                }
            }
            Status::Claimed | Status::InProgress => q.in_progress += 1,
            Status::Blocked => q.blocked += 1,
            Status::NeedsHuman => q.needs_human += 1,
            Status::Draft => q.drafts += 1,
            Status::Done | Status::Cancelled => {}
        }
    }
    q
}

/// A one-word reading of an agent for dashboards, and why.
pub fn reading(
    agent: &Agent,
    profile: Option<&Profile>,
    q: &Queue,
    stale_after: chrono::Duration,
) -> (&'static str, String) {
    let alive = agent
        .last_seen
        .is_some_and(|t| Utc::now() - t < stale_after)
        && agent.runtime.status != AgentStatus::Offline;
    if agent.config.kind != AgentKind::Local {
        return (kind_str(agent.config.kind), String::new());
    }
    if !alive {
        let why = match agent.config.desired {
            Desired::Running if q.ready > 0 => format!("not running; {} ready", q.ready),
            Desired::Running => "not running".to_string(),
            d => format!("not running; desired {}", d.as_str()),
        };
        return ("offline", why);
    }
    match agent.runtime.status {
        AgentStatus::Active => (
            "working",
            agent
                .runtime
                .current
                .iter()
                .map(|i| i.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        AgentStatus::Paused => ("paused", format!("{} ready", q.ready)),
        AgentStatus::Gated => ("gated", agent.runtime.note.clone().unwrap_or_default()),
        _ => {
            if agent.config.desired == Desired::Stopped {
                ("stopping", String::new())
            } else if agent.config.desired == Desired::Paused {
                ("pausing", String::new())
            } else if q.ready > 0 {
                ("starting", format!("{} ready", q.ready))
            } else {
                let cycle = agent
                    .config
                    .cycle
                    .clone()
                    .or_else(|| profile.map(|p| p.cycle.clone()))
                    .unwrap_or_default();
                if agent.runtime.snoozed.is_some() {
                    ("idle", "snoozed until work is routed here".into())
                } else if agent.config.hold {
                    ("idle", "hold: routed work only".into())
                } else if cycle.trim().is_empty() {
                    ("idle", "no work".into())
                } else {
                    ("idle", format!("next cycle within {cycle}"))
                }
            }
        }
    }
}

fn list(ctx: &Ctx) -> Result<()> {
    let store = ctx.store();
    let agents = store.list_agents()?;
    let items = store.list_items(&ItemFilter::default())?;
    let profiles = store.profiles().unwrap_or_default();
    let ttl = chrono::Duration::from_std(store.collective()?.claim_ttl())
        .unwrap_or(chrono::Duration::minutes(30));

    #[derive(Serialize)]
    struct Row<'a> {
        #[serde(flatten)]
        agent: &'a Agent,
        queue: Queue,
        reading: &'static str,
        reading_detail: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    }
    let rows: Vec<Row> = agents
        .iter()
        .map(|a| {
            let profile = a
                .config
                .profiles
                .first()
                .and_then(|p| profiles.iter().find(|x| &x.name == p));
            let q = queue_for(a, &items);
            let (reading, detail) = reading(a, profile, &q, ttl);
            Row {
                agent: a,
                queue: q,
                reading,
                reading_detail: detail,
                description: profile
                    .map(|p| p.description.clone())
                    .filter(|d| !d.is_empty()),
            }
        })
        .collect();
    ctx.emit(&rows, || {
        if rows.is_empty() {
            return "no agents\n".to_string();
        }
        let table: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                let a = r.agent;
                vec![
                    a.id().to_string(),
                    a.config.profiles.join(","),
                    a.config.desired.as_str().to_string(),
                    r.reading.to_string(),
                    format!(
                        "{}{}",
                        r.queue.ready,
                        if r.queue.returned > 0 {
                            format!(" ({} returned)", r.queue.returned)
                        } else {
                            String::new()
                        }
                    ),
                    if a.config.hold {
                        "hold".into()
                    } else {
                        String::new()
                    },
                    a.last_seen
                        .map(|t| format!("{} ago", util::age(t)))
                        .unwrap_or_else(|| "never".into()),
                    util::truncate(&r.reading_detail, 50),
                ]
            })
            .collect();
        util::table(
            &[
                "agent", "profiles", "desired", "now", "ready", "hold", "seen", "detail",
            ],
            &table,
        )
    })
}

/// Register a local agent on another collective's board and record the loan
/// on both sides. Only collectives on this filesystem are reachable.
fn lend(ctx: &Ctx, agent: &str, to: &str) -> Result<()> {
    if to.contains("://") {
        bail!(
            "lending to {to} needs that collective's store to be reachable from here (an http store); \
             remote stores are not implemented in this release. Lend to a collective root path on this machine instead."
        );
    }
    let id = ActorId::new(agent);
    let mut local = ctx.store().get_agent(&id)?;
    let target_root = discover::find(Some(std::path::Path::new(to)), &std::env::current_dir()?)?;
    let target_root = std::fs::canonicalize(&target_root).unwrap_or(target_root);
    let here = std::fs::canonicalize(&ctx.root).unwrap_or_else(|_| ctx.root.clone());
    if target_root == here {
        bail!("{agent} is already a member of this collective");
    }
    let target_cfg = Collective::load(&target_root.join(discover::CONFIG_FILE))?;
    if !matches!(target_cfg.store.kind, StoreKind::File | StoreKind::Sqlite) {
        bail!(
            "collective {} uses a {} store, which this release cannot reach",
            target_cfg.name,
            target_cfg.store.kind.as_str()
        );
    }
    let target = vflt_core::open_store(&target_root)?;
    let lent = target.register_agent(NewAgent {
        name: local.config.name.clone(),
        profiles: local.config.profiles.clone(),
        kind: Some(local.config.kind),
        host: Some(local.config.host.clone()),
        lent_from: Some(here.to_string_lossy().into_owned()),
        ..Default::default()
    })?;
    local.config.lent_to = Some(target_root.to_string_lossy().into_owned());
    ctx.store().update_agent(
        &local.config,
        &ctx.actor,
        &format!("lent {} to {}", local.id(), target_cfg.name),
    )?;
    ctx.emit(&lent, || {
        format!(
            "lent {} to collective {} at {}\nrun it there with: vflt --collective {} agent run --profile {} --name {}\n",
            lent.id(),
            target_cfg.name,
            target_root.display(),
            target_root.display(),
            lent.config.profiles.first().cloned().unwrap_or_default(),
            lent.id()
        )
    })
}
