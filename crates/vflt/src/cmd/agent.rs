use anyhow::{bail, Context, Result};
use vflt_core::{discover, ActorId, AgentKind, Collective, NewAgent, StoreKind};

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
        } => {
            for p in &profiles {
                store.profile(p)?;
            }
            let a = store.register_agent(NewAgent {
                name,
                profiles,
                kind: Some(if remote.is_some() {
                    AgentKind::Remote
                } else {
                    AgentKind::Local
                }),
                remote,
                ..Default::default()
            })?;
            ctx.emit(&a, || {
                format!(
                    "registered agent {} with profiles {}\n",
                    a.id,
                    a.profiles.join(",")
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
            agent_loop::run(
                ctx,
                RunOptions {
                    profile,
                    once,
                    interval,
                    name,
                    runner,
                },
            )
        }
        AgentCmd::Lend { agent, to } => lend(ctx, &agent, &to),
        AgentCmd::List => {
            let agents = store.list_agents()?;
            ctx.emit(&agents, || {
                if agents.is_empty() {
                    return "no agents\n".to_string();
                }
                let rows: Vec<Vec<String>> = agents
                    .iter()
                    .map(|a| {
                        vec![
                            a.id.to_string(),
                            a.profiles.join(","),
                            format!("{:?}", a.kind).to_lowercase(),
                            format!("{:?}", a.status).to_lowercase(),
                            a.host.clone(),
                            format!("{} ago", util::age(a.last_seen)),
                            a.lent_to
                                .clone()
                                .or_else(|| a.lent_from.as_ref().map(|f| format!("from {f}")))
                                .unwrap_or_default(),
                        ]
                    })
                    .collect();
                util::table(
                    &[
                        "agent",
                        "profiles",
                        "kind",
                        "status",
                        "host",
                        "last seen",
                        "lent",
                    ],
                    &rows,
                )
            })
        }
        AgentCmd::Remove { name } => {
            let id = ActorId::new(&name);
            store.deregister_agent(&id)?;
            ctx.emit(&serde_json::json!({"removed": name}), || {
                format!("removed agent {name}\n")
            })
        }
    }
}

/// Register a local agent on another collective's board and record the loan
/// on both sides. Only file-store collectives on this filesystem are reachable
/// in this release.
fn lend(ctx: &Ctx, agent: &str, to: &str) -> Result<()> {
    if to.contains("://") {
        bail!(
            "lending to {to} needs that collective's store to be reachable from here (a database or http store); \
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
    if target_cfg.store.kind != StoreKind::File {
        bail!(
            "collective {} uses a {} store, which this release cannot reach",
            target_cfg.name,
            target_cfg.store.kind.as_str()
        );
    }
    let target = vflt_core::open_store(&target_root)?;
    let lent = target.register_agent(NewAgent {
        name: local.name.clone(),
        profiles: local.profiles.clone(),
        kind: Some(local.kind),
        host: Some(local.host.clone()),
        lent_from: Some(here.to_string_lossy().into_owned()),
        ..Default::default()
    })?;
    local.lent_to = Some(target_root.to_string_lossy().into_owned());
    ctx.store().update_agent(&local)?;
    ctx.emit(&lent, || {
        format!(
            "lent {} to collective {} at {}\nrun it there with: vflt --collective {} agent run --profile {} --name {}\n",
            lent.id,
            target_cfg.name,
            target_root.display(),
            target_root.display(),
            lent.profiles.first().cloned().unwrap_or_default(),
            lent.id
        )
    })
}
