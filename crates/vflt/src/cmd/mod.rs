//! Command handlers.

mod agent;
mod item;

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use vflt_core::{ClaimMode, Collective, EventFilter, FileStore, ItemFilter, Status, Store};

use crate::cli::{Cli, CollectiveCmd, Command};
use crate::ctx::Ctx;
use crate::util;

pub fn dispatch(args: Cli) -> Result<()> {
    match args.command {
        Command::Collective {
            cmd:
                CollectiveCmd::Init {
                    dir,
                    name,
                    no_git,
                    claim_mode,
                },
        } => collective_init(dir, name, no_git, &claim_mode, args.json),
        other => {
            let ctx = Ctx::open(args.collective.as_ref(), args.actor.as_deref(), args.json)?;
            match other {
                Command::Collective {
                    cmd: CollectiveCmd::Show,
                } => collective_show(&ctx),
                Command::Collective { .. } => unreachable!("init handled above"),
                Command::Item { cmd } => item::run(&ctx, cmd),
                Command::Agent { cmd } => agent::run(&ctx, cmd),
                Command::Board => board(&ctx),
                Command::Events { item, since, kind } => events(&ctx, item, since, kind),
                Command::Profiles => profiles(&ctx),
            }
        }
    }
}

fn collective_init(
    dir: Option<std::path::PathBuf>,
    name: Option<String>,
    no_git: bool,
    claim_mode: &str,
    json: bool,
) -> Result<()> {
    let dir = dir.unwrap_or_else(|| std::path::PathBuf::from(vflt_core::discover::DEFAULT_DIR));
    let cwd = std::env::current_dir()?;
    let abs = if dir.is_absolute() {
        dir.clone()
    } else {
        cwd.join(&dir)
    };
    let name = name.unwrap_or_else(|| {
        let base = if abs
            .file_name()
            .is_some_and(|f| f == vflt_core::discover::DEFAULT_DIR)
        {
            abs.parent().and_then(|p| p.file_name())
        } else {
            abs.file_name()
        };
        base.map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "collective".into())
    });
    let mut coll = Collective::new(&name);
    coll.store.git = !no_git;
    coll.claim_mode = match claim_mode {
        "pull" => ClaimMode::Pull,
        "delegated" => ClaimMode::Delegated,
        "both" => ClaimMode::Both,
        other => bail!("--claim-mode must be pull, delegated or both (got {other})"),
    };
    let store = FileStore::init(&abs, coll).context("initialising collective")?;
    #[derive(Serialize)]
    struct Out<'a> {
        root: &'a std::path::Path,
        collective: Collective,
    }
    let out = Out {
        root: store.root(),
        collective: store.collective()?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "initialised collective {} at {} (store: file, git: {})",
            out.collective.name,
            out.root.display(),
            if out.collective.store.git {
                "on"
            } else {
                "off"
            }
        );
        println!(
            "profiles: {}",
            store
                .profiles()?
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

fn collective_show(ctx: &Ctx) -> Result<()> {
    #[derive(Serialize)]
    struct Out<'a> {
        root: &'a std::path::Path,
        collective: Collective,
        items: usize,
        agents: usize,
        profiles: Vec<String>,
    }
    let out = Out {
        root: &ctx.root,
        collective: ctx.store().collective()?,
        items: ctx.store().list_items(&ItemFilter::default())?.len(),
        agents: ctx.store().list_agents()?.len(),
        profiles: ctx
            .store()
            .profiles()?
            .into_iter()
            .map(|p| p.name)
            .collect(),
    };
    ctx.emit(&out, || {
        let c = &out.collective;
        format!(
            "collective: {}\nroot: {}\nstore: {} (git: {})\nclaim_mode: {:?}\nclaim_ttl: {}\npipeline: {}\nbounce_to: {}\nitems: {}\nagents: {}\nprofiles: {}\n",
            c.name,
            out.root.display(),
            c.store.kind.as_str(),
            if c.store.git { "on" } else { "off" },
            c.claim_mode,
            c.claim_ttl,
            c.pipeline.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" -> "),
            c.bounce_to,
            out.items,
            out.agents,
            out.profiles.join(", ")
        )
    })
}

fn board(ctx: &Ctx) -> Result<()> {
    let store = ctx.store();
    let coll = store.collective()?;
    let items = store.list_items(&ItemFilter::default())?;
    let agents = store.list_agents()?;

    #[derive(Serialize)]
    struct Cell {
        stage: String,
        status: String,
        count: usize,
    }
    #[derive(Serialize)]
    struct Attention {
        id: String,
        title: String,
        status: String,
        waiting_on: Option<String>,
    }
    #[derive(Serialize)]
    struct AgentRow {
        id: String,
        profiles: Vec<String>,
        status: String,
        last_seen_age: String,
        lent_to: Option<String>,
    }
    #[derive(Serialize)]
    struct Out {
        collective: String,
        total: usize,
        open: usize,
        cells: Vec<Cell>,
        attention: Vec<Attention>,
        agents: Vec<AgentRow>,
    }

    let mut counts: BTreeMap<(String, Status), usize> = BTreeMap::new();
    for i in &items {
        *counts.entry((i.stage.to_string(), i.status)).or_default() += 1;
    }
    let mut stages: Vec<String> = coll.pipeline.iter().map(|s| s.to_string()).collect();
    for i in &items {
        if !stages.contains(&i.stage.0) {
            stages.push(i.stage.to_string());
        }
    }
    let cells: Vec<Cell> = counts
        .iter()
        .map(|((st, s), n)| Cell {
            stage: st.clone(),
            status: s.to_string(),
            count: *n,
        })
        .collect();
    let attention: Vec<Attention> = items
        .iter()
        .filter(|i| matches!(i.status, Status::NeedsHuman | Status::Blocked))
        .map(|i| Attention {
            id: i.id().to_string(),
            title: i.title.clone(),
            status: i.status.to_string(),
            waiting_on: i.waiting_on.clone(),
        })
        .collect();
    let agent_rows: Vec<AgentRow> = agents
        .iter()
        .map(|a| AgentRow {
            id: a.id.to_string(),
            profiles: a.profiles.clone(),
            status: format!("{:?}", a.status).to_lowercase(),
            last_seen_age: util::age(a.last_seen),
            lent_to: a.lent_to.clone(),
        })
        .collect();
    let out = Out {
        collective: coll.name.clone(),
        total: items.len(),
        open: items.iter().filter(|i| !i.status.is_terminal()).count(),
        cells,
        attention,
        agents: agent_rows,
    };

    ctx.emit(&out, || {
        let mut s = format!(
            "collective {}  ({} items, {} open)\n\n",
            out.collective, out.total, out.open
        );
        let statuses: Vec<Status> = Status::ALL.to_vec();
        let mut headers: Vec<&str> = vec!["stage"];
        headers.extend(statuses.iter().map(|s| s.as_str()));
        let rows: Vec<Vec<String>> = stages
            .iter()
            .map(|st| {
                let mut r = vec![st.clone()];
                for s in &statuses {
                    let n = counts.get(&(st.clone(), *s)).copied().unwrap_or(0);
                    r.push(if n == 0 {
                        "·".to_string()
                    } else {
                        n.to_string()
                    });
                }
                r
            })
            .collect();
        s.push_str(&util::table(&headers, &rows));
        if !out.attention.is_empty() {
            s.push_str("\nneeds attention:\n");
            for a in &out.attention {
                s.push_str(&format!(
                    "  {} [{}] {}{}\n",
                    a.id,
                    a.status,
                    util::truncate(&a.title, 60),
                    a.waiting_on
                        .as_ref()
                        .map(|w| format!("\n      waiting on: {}", util::truncate(w, 100)))
                        .unwrap_or_default()
                ));
            }
        }
        if !out.agents.is_empty() {
            s.push_str("\nagents:\n");
            let rows: Vec<Vec<String>> = out
                .agents
                .iter()
                .map(|a| {
                    vec![
                        a.id.clone(),
                        a.profiles.join(","),
                        a.status.clone(),
                        format!("{} ago", a.last_seen_age),
                        a.lent_to.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            s.push_str(&util::table(
                &["agent", "profiles", "status", "last seen", "lent to"],
                &rows,
            ));
        }
        s
    })
}

fn events(
    ctx: &Ctx,
    item: Option<String>,
    since: Option<String>,
    kind: Option<String>,
) -> Result<()> {
    let filter = EventFilter {
        item: item.map(|i| ctx.resolve_id(&i)).transpose()?,
        since: since.map(|s| util::parse_since(&s)).transpose()?,
        kind,
    };
    let evs = ctx.store().events(&filter)?;
    ctx.emit(&evs, || {
        let rows: Vec<Vec<String>> = evs
            .iter()
            .map(|e| {
                vec![
                    e.ts.format("%Y-%m-%d %H:%M:%S").to_string(),
                    e.kind.clone(),
                    e.actor.to_string(),
                    e.item.as_ref().map(|i| i.to_string()).unwrap_or_default(),
                    util::truncate(&e.summary, 100),
                ]
            })
            .collect();
        if rows.is_empty() {
            "no events\n".to_string()
        } else {
            util::table(&["time (utc)", "kind", "actor", "item", "summary"], &rows)
        }
    })
}

fn profiles(ctx: &Ctx) -> Result<()> {
    let ps = ctx.store().profiles()?;
    ctx.emit(&ps, || {
        let rows: Vec<Vec<String>> = ps
            .iter()
            .map(|p| {
                vec![
                    p.name.clone(),
                    if p.stages.is_empty() {
                        "(cycle)".into()
                    } else {
                        p.stages
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    },
                    p.runner.clone(),
                    if p.cycle.is_empty() {
                        "wake-on-work".into()
                    } else {
                        p.cycle.clone()
                    },
                    format!("{} turns / {}", p.budget.max_turns, p.budget.wall_clock),
                    util::truncate(&p.description, 70),
                ]
            })
            .collect();
        util::table(
            &[
                "profile",
                "stages",
                "runner",
                "cycle",
                "budget",
                "description",
            ],
            &rows,
        )
    })
}
