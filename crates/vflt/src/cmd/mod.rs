//! Command handlers.

pub mod agent;
mod item;

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::Serialize;
use vflt_core::{
    AgentKind, BundleRecord, ClaimMode, Collective, Desired, EventFilter, IdStyle, Item,
    ItemFilter, NewItem, RaiseKind, Stage, Status, StoreConfig, Transition,
};

use crate::cli::{Cli, CollectiveCmd, Command, FleetCmd};
use crate::ctx::Ctx;
use crate::util;

pub fn dispatch(args: Cli) -> Result<()> {
    match args.command {
        Command::Collective {
            cmd:
                CollectiveCmd::Init {
                    dir,
                    name,
                    store,
                    no_git,
                    claim_mode,
                    ids,
                    mirror,
                    pipeline,
                },
        } => collective_init(InitArgs {
            dir,
            name,
            store,
            no_git,
            claim_mode,
            ids,
            mirror,
            pipeline,
            json: args.json,
        }),
        other => {
            let ctx = Ctx::open(args.collective.as_ref(), args.actor.as_deref(), args.json)?;
            match other {
                Command::Collective { cmd } => match cmd {
                    CollectiveCmd::Show => collective_show(&ctx),
                    CollectiveCmd::Export { to, mirror } => export(&ctx, &to, mirror),
                    CollectiveCmd::Import { from, mirror } => import(&ctx, from, mirror),
                    CollectiveCmd::Init { .. } => unreachable!("init handled above"),
                },
                Command::Item { cmd } => item::run(&ctx, cmd),
                Command::Agent { cmd } => agent::run(&ctx, cmd),
                Command::Fleet { cmd } => fleet(&ctx, cmd),
                Command::Flag {
                    key,
                    title,
                    body,
                    assign,
                    kind,
                    tags,
                    priority,
                } => flag(&ctx, key, title, body, assign, &kind, tags, priority),
                Command::Inbox { kind } => inbox(&ctx, &kind),
                Command::Board => board(&ctx),
                Command::Events {
                    item,
                    since,
                    kind,
                    actor,
                    limit,
                } => events(&ctx, item, since, kind, actor, limit),
                Command::Cost { since, by } => cost(&ctx, &since, &by),
                Command::Profiles => profiles(&ctx),
            }
        }
    }
}

struct InitArgs {
    dir: Option<std::path::PathBuf>,
    name: Option<String>,
    store: String,
    no_git: bool,
    claim_mode: String,
    ids: String,
    mirror: Option<String>,
    pipeline: Vec<String>,
    json: bool,
}

fn collective_init(a: InitArgs) -> Result<()> {
    let dir = a
        .dir
        .unwrap_or_else(|| std::path::PathBuf::from(vflt_core::discover::DEFAULT_DIR));
    let cwd = std::env::current_dir()?;
    let abs = if dir.is_absolute() {
        dir.clone()
    } else {
        cwd.join(&dir)
    };
    let name = a.name.unwrap_or_else(|| {
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
    coll.store = match a.store.as_str() {
        "file" => StoreConfig {
            git: !a.no_git,
            ..Default::default()
        },
        "sqlite" => StoreConfig::sqlite(),
        other => bail!("--store must be file or sqlite (got {other})"),
    };
    if let Some(m) = a.mirror {
        if coll.store.kind != vflt_core::StoreKind::Sqlite {
            bail!("--mirror applies to the sqlite store; the file store is already plain files");
        }
        coll.store.mirror = Some(m);
    }
    coll.claim_mode = match a.claim_mode.as_str() {
        "pull" => ClaimMode::Pull,
        "delegated" => ClaimMode::Delegated,
        "both" => ClaimMode::Both,
        other => bail!("--claim-mode must be pull, delegated or both (got {other})"),
    };
    coll.ids = match a.ids.as_str() {
        "ulid" => IdStyle::Ulid,
        "seq" => IdStyle::Seq,
        other => bail!("--ids must be ulid or seq (got {other})"),
    };
    if !a.pipeline.is_empty() {
        let stages: Vec<Stage> = a
            .pipeline
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(Stage::new)
            .collect();
        if stages.len() < 2 {
            bail!("--pipeline needs at least a working stage and a terminal stage");
        }
        if !stages.contains(&coll.bounce_to) {
            coll.bounce_to = stages[0].clone();
        }
        coll.pipeline = stages;
    }
    let store = vflt_core::init_store(&abs, coll).context("initialising collective")?;
    #[derive(Serialize)]
    struct Out<'a> {
        root: &'a std::path::Path,
        collective: Collective,
    }
    let out = Out {
        root: store.root(),
        collective: store.collective()?,
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "initialised collective {} at {} (store: {}{})",
            out.collective.name,
            out.root.display(),
            out.collective.store.kind.as_str(),
            match out.collective.store.kind {
                vflt_core::StoreKind::File => format!(
                    ", git: {}",
                    if out.collective.store.git {
                        "on"
                    } else {
                        "off"
                    }
                ),
                _ => out
                    .collective
                    .store
                    .mirror
                    .as_ref()
                    .map(|m| format!(", mirror: {m}"))
                    .unwrap_or_default(),
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
            "collective: {}\nroot: {}\nstore: {}{}\nids: {:?}\nclaim_mode: {:?}\nclaim_ttl: {}\npipeline: {}\nbounce_to: {}\nitems: {}\nagents: {}\nprofiles: {}\n",
            c.name,
            out.root.display(),
            c.store.kind.as_str(),
            match c.store.kind {
                vflt_core::StoreKind::File => format!(" (git: {})", if c.store.git { "on" } else { "off" }),
                _ => c.store.mirror.as_ref().map(|m| format!(" (mirror: {m})")).unwrap_or_default(),
            },
            c.ids,
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

fn export(ctx: &Ctx, to: &std::path::Path, mirror: bool) -> Result<()> {
    if mirror {
        let n = ctx.store().rebuild_mirror()?;
        return ctx.emit(&serde_json::json!({"mirrored": n}), || {
            format!("rewrote {n} mirror files\n")
        });
    }
    let records = ctx.store().export_records()?;
    let mut out: Box<dyn Write> = if to.as_os_str() == "-" {
        Box::new(std::io::stdout().lock())
    } else {
        Box::new(std::io::BufWriter::new(
            std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?,
        ))
    };
    for r in &records {
        writeln!(out, "{}", serde_json::to_string(r)?)?;
    }
    out.flush()?;
    if to.as_os_str() != "-" {
        eprintln!("vflt: wrote {} records to {}", records.len(), to.display());
    }
    Ok(())
}

fn import(
    ctx: &Ctx,
    from: Option<std::path::PathBuf>,
    mirror: Option<std::path::PathBuf>,
) -> Result<()> {
    let store = ctx.store();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    if let Some(dir) = mirror {
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        paths.sort();
        let mut notes = Vec::new();
        for p in paths {
            let text = std::fs::read_to_string(&p)?;
            let (state, spec, ns) = vflt_core::mirror::parse(&text)
                .with_context(|| format!("parsing {}", p.display()))?;
            let id = state.id.clone();
            store.import_record(BundleRecord::Item { state, spec })?;
            *counts.entry("item").or_default() += 1;
            for n in ns {
                notes.push(BundleRecord::Note {
                    item: id.clone(),
                    note: n,
                });
            }
        }
        for n in notes {
            store.import_record(n)?;
            *counts.entry("note").or_default() += 1;
        }
    } else {
        let from = from.unwrap_or_else(|| "-".into());
        let reader: Box<dyn BufRead> = if from.as_os_str() == "-" {
            Box::new(std::io::BufReader::new(std::io::stdin()))
        } else {
            Box::new(std::io::BufReader::new(
                std::fs::File::open(&from)
                    .with_context(|| format!("opening {}", from.display()))?,
            ))
        };
        for (n, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: BundleRecord =
                serde_json::from_str(&line).with_context(|| format!("bundle line {}", n + 1))?;
            let kind = match &rec {
                BundleRecord::Item { .. } => "item",
                BundleRecord::Note { .. } => "note",
                BundleRecord::Event(_) => "event",
                BundleRecord::Agent(_) => "agent",
            };
            store
                .import_record(rec)
                .with_context(|| format!("bundle line {}", n + 1))?;
            *counts.entry(kind).or_default() += 1;
        }
    }
    let _ = store.rebuild_mirror();
    ctx.emit(&counts, || {
        format!(
            "imported {}\n",
            counts
                .iter()
                .map(|(k, v)| format!("{v} {k}s"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

fn fleet(ctx: &Ctx, cmd: FleetCmd) -> Result<()> {
    match cmd {
        FleetCmd::Run {
            only,
            interval,
            once,
        } => {
            let interval = humantime::parse_duration(&interval)
                .with_context(|| format!("--interval {interval:?}"))?;
            crate::fleet::run(ctx, only, interval, once)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flag(
    ctx: &Ctx,
    key: String,
    title: String,
    body: Option<String>,
    assign: Option<String>,
    kind: &str,
    tags: Vec<String>,
    priority: Option<i32>,
) -> Result<()> {
    let store = ctx.store();
    let kind: RaiseKind = kind.parse().map_err(anyhow::Error::msg)?;
    #[derive(Serialize)]
    struct Out {
        created: bool,
        item: Item,
    }
    if let Some(open) = store.open_by_key(&key)? {
        let text = format!(
            "Flagged again by {} at {}.\n\n{}",
            ctx.actor,
            Utc::now().format("%Y-%m-%d %H:%M UTC"),
            body.as_deref().unwrap_or(&title)
        );
        store.add_note(open.id(), &ctx.actor, &text)?;
        let item = store.get_item(open.id())?;
        let out = Out {
            created: false,
            item,
        };
        return ctx.emit(&out, || {
            format!("{} already open for {key:?}; noted\n", out.item.id())
        });
    }
    let spec = body.clone().unwrap_or_else(|| title.clone());
    let it = store.create_item(
        &spec,
        NewItem {
            title: title.clone(),
            owner: ctx.actor.clone(),
            assignee: assign
                .map(vflt_core::ActorId::new)
                .or_else(|| Some(ctx.actor.clone())),
            tags,
            key: Some(key.clone()),
            priority,
            ..Default::default()
        },
    )?;
    let item = store.transition(
        it.id(),
        &ctx.actor,
        Transition::Raise {
            question: title,
            kind,
        },
    )?;
    let item = store.get_item(item.id())?;
    let out = Out {
        created: true,
        item,
    };
    ctx.emit(&out, || format!("flagged {} for {key:?}\n", out.item.id()))
}

#[derive(Serialize)]
struct Attention {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    summary: String,
}

fn inbox(ctx: &Ctx, kind: &str) -> Result<()> {
    let store = ctx.store();
    let want: Option<RaiseKind> = match kind {
        "all" => None,
        k => Some(k.parse().map_err(anyhow::Error::msg)?),
    };
    let coll = store.collective()?;
    let all = store.list_items(&ItemFilter::default())?;
    let agents = store.list_agents()?;
    let profiles = store.profiles().unwrap_or_default();
    let now = Utc::now();
    let ttl = chrono::Duration::from_std(coll.claim_ttl()).unwrap_or(chrono::Duration::minutes(30));

    let mut raised: Vec<Item> = all
        .iter()
        .filter(|i| i.status == Status::NeedsHuman)
        .filter(|i| want.is_none_or(|k| i.raise_kind.unwrap_or_default() == k))
        .cloned()
        .collect();
    raised.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.updated.cmp(&b.updated)));
    let raised: Vec<Item> = raised
        .into_iter()
        .map(|i| store.get_item(i.id()).map_err(anyhow::Error::from))
        .collect::<Result<_>>()?;

    let mut attention: Vec<Attention> = Vec::new();
    let by_id: std::collections::HashMap<&vflt_core::ItemId, &Item> =
        all.iter().map(|i| (i.id(), i)).collect();
    let humans: std::collections::HashSet<&str> = agents
        .iter()
        .filter(|a| a.config.kind == AgentKind::Human)
        .map(|a| a.id().as_str())
        .collect();
    let mut names: std::collections::HashSet<String> =
        agents.iter().map(|a| a.id().to_string()).collect();
    names.extend(profiles.iter().map(|p| p.name.clone()));
    for i in &all {
        if let Some(c) = &i.claim {
            if store.is_stale(c)? {
                attention.push(Attention {
                    kind: "stale_claim",
                    item: Some(i.id().to_string()),
                    agent: Some(c.actor.to_string()),
                    summary: format!(
                        "{} holds {} but has not been seen for over {}",
                        c.actor,
                        i.id(),
                        util::age(now - ttl)
                    ),
                });
            }
        }
        match i.status {
            Status::Blocked if now - i.updated > chrono::Duration::hours(72) => {
                attention.push(Attention {
                    kind: "long_blocked",
                    item: Some(i.id().to_string()),
                    agent: i.assignee.as_ref().map(|a| a.to_string()),
                    summary: format!(
                        "blocked {} on: {}",
                        util::age(i.updated),
                        util::truncate(i.waiting_on.as_deref().unwrap_or("?"), 100)
                    ),
                })
            }
            Status::Draft if now - i.updated > chrono::Duration::days(7) => {
                attention.push(Attention {
                    kind: "stale_draft",
                    item: Some(i.id().to_string()),
                    agent: Some(i.owner.to_string()),
                    summary: format!(
                        "draft untouched for {}: {}",
                        util::age(i.updated),
                        util::truncate(&i.title, 80)
                    ),
                })
            }
            Status::Pending => {
                let stranded = match &i.assignee {
                    Some(a) => !names.contains(a.as_str()),
                    None => coll.claim_mode == ClaimMode::Delegated,
                };
                if stranded {
                    attention.push(Attention {
                        kind: "stranded",
                        item: Some(i.id().to_string()),
                        agent: i.assignee.as_ref().map(|a| a.to_string()),
                        summary: format!(
                            "assigned to {}, which no agent, profile or registered person answers to",
                            i.assignee.as_ref().map(|a| a.as_str()).unwrap_or("the pool")
                        ),
                    });
                }
                // Work resting with a person, not raised: nobody else will
                // pick it up, and nothing about it says it is waiting.
                if let Some(a) = i.assignee.as_ref().filter(|a| humans.contains(a.as_str())) {
                    if now - i.updated > chrono::Duration::hours(24) && i.owner != *a {
                        attention.push(Attention {
                            kind: "with_person",
                            item: Some(i.id().to_string()),
                            agent: Some(a.to_string()),
                            summary: format!(
                                "pending with {a} for {} without being raised: {}",
                                util::age(i.updated),
                                util::truncate(&i.title, 80)
                            ),
                        });
                    }
                }
                let stuck: Vec<String> = i
                    .deps
                    .iter()
                    .filter_map(|d| by_id.get(d))
                    .filter(|d| matches!(d.status, Status::Blocked | Status::NeedsHuman))
                    .map(|d| format!("{} ({})", d.id(), d.status))
                    .collect();
                if !stuck.is_empty() {
                    attention.push(Attention {
                        kind: "stalled_chain",
                        item: Some(i.id().to_string()),
                        agent: i.assignee.as_ref().map(|a| a.to_string()),
                        summary: format!("waits on {}", stuck.join(", ")),
                    });
                }
            }
            _ => {}
        }
    }
    for a in &agents {
        if a.config.kind != AgentKind::Local {
            continue;
        }
        let q = agent::queue_for(a, &all);
        if q.ready == 0 {
            continue;
        }
        let alive = a.last_seen.is_some_and(|t| now - t < ttl)
            && a.runtime.status != vflt_core::AgentStatus::Offline;
        match a.config.desired {
            Desired::Paused | Desired::Stopped => attention.push(Attention {
                kind: "paused_backlog",
                item: None,
                agent: Some(a.id().to_string()),
                summary: format!(
                    "{} is {} with {} ready",
                    a.id(),
                    a.config.desired.as_str(),
                    q.ready
                ),
            }),
            Desired::Running if !alive => attention.push(Attention {
                kind: "idle_backlog",
                item: None,
                agent: Some(a.id().to_string()),
                summary: format!("{} ready for {}, which is not running", q.ready, a.id()),
            }),
            _ => {}
        }
    }

    #[derive(Serialize)]
    struct Out {
        items: Vec<Item>,
        decisions: usize,
        actions: usize,
        attention: Vec<Attention>,
    }
    let out = Out {
        decisions: raised
            .iter()
            .filter(|i| i.raise_kind.unwrap_or_default() == RaiseKind::Decision)
            .count(),
        actions: raised
            .iter()
            .filter(|i| i.raise_kind == Some(RaiseKind::Action))
            .count(),
        items: raised,
        attention,
    };
    ctx.emit(&out, || {
        let mut s = String::new();
        if out.items.is_empty() {
            s.push_str("nothing raised\n");
        }
        for i in &out.items {
            s.push_str(&format!(
                "{} [{}] {}\n    {}\n",
                i.id(),
                i.raise_kind.unwrap_or_default().as_str().to_uppercase(),
                util::truncate(&i.title, 70),
                util::truncate(i.waiting_on.as_deref().unwrap_or(""), 110)
            ));
        }
        if !out.attention.is_empty() {
            s.push_str("\nattention:\n");
            for a in &out.attention {
                s.push_str(&format!(
                    "  {:<15} {}{}\n",
                    a.kind,
                    a.item.as_ref().map(|i| format!("{i} ")).unwrap_or_default(),
                    util::truncate(&a.summary, 100)
                ));
            }
        }
        s
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
    struct AttentionRow {
        id: String,
        title: String,
        status: String,
        waiting_on: Option<String>,
    }
    #[derive(Serialize)]
    struct AgentRow {
        id: String,
        profiles: Vec<String>,
        desired: String,
        status: String,
        last_seen_age: Option<String>,
        lent_to: Option<String>,
    }
    #[derive(Serialize)]
    struct Out {
        collective: String,
        total: usize,
        open: usize,
        cells: Vec<Cell>,
        attention: Vec<AttentionRow>,
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
    let attention: Vec<AttentionRow> = items
        .iter()
        .filter(|i| matches!(i.status, Status::NeedsHuman | Status::Blocked))
        .map(|i| AttentionRow {
            id: i.id().to_string(),
            title: i.title.clone(),
            status: i.status.to_string(),
            waiting_on: i.waiting_on.clone(),
        })
        .collect();
    let agent_rows: Vec<AgentRow> = agents
        .iter()
        .map(|a| AgentRow {
            id: a.id().to_string(),
            profiles: a.config.profiles.clone(),
            desired: a.config.desired.as_str().to_string(),
            status: a.runtime.status.as_str().to_string(),
            last_seen_age: a.last_seen.map(util::age),
            lent_to: a.config.lent_to.clone(),
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
                        a.desired.clone(),
                        a.status.clone(),
                        a.last_seen_age
                            .as_ref()
                            .map(|x| format!("{x} ago"))
                            .unwrap_or_else(|| "never".into()),
                        a.lent_to.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            s.push_str(&util::table(
                &[
                    "agent",
                    "profiles",
                    "desired",
                    "status",
                    "last seen",
                    "lent to",
                ],
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
    actor: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    let filter = EventFilter {
        item: item.map(|i| ctx.resolve_id(&i)).transpose()?,
        since: since.map(|s| util::parse_since(&s)).transpose()?,
        kind,
        actor: actor.map(vflt_core::ActorId::new),
        limit,
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

fn cost(ctx: &Ctx, since: &str, by: &str) -> Result<()> {
    let since = util::parse_since(since)?;
    let mut groups: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for kind in ["agent.run", "agent.cycle"] {
        for e in ctx.store().events(&EventFilter {
            since: Some(since),
            kind: Some(kind.into()),
            ..Default::default()
        })? {
            let usd = e
                .detail
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let key = match by {
                "agent" => e.actor.to_string(),
                "day" => e.ts.format("%Y-%m-%d").to_string(),
                "item" => e
                    .item
                    .as_ref()
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "(cycle)".into()),
                other => bail!("--by must be agent, day or item (got {other})"),
            };
            let g = groups.entry(key).or_default();
            g.0 += 1;
            g.1 += usd;
        }
    }
    #[derive(Serialize)]
    struct Row {
        key: String,
        runs: usize,
        cost_usd: f64,
    }
    let rows: Vec<Row> = groups
        .into_iter()
        .map(|(key, (runs, cost_usd))| Row {
            key,
            runs,
            cost_usd,
        })
        .collect();
    ctx.emit(&rows, || {
        if rows.is_empty() {
            return "no runs\n".into();
        }
        let total: f64 = rows.iter().map(|r| r.cost_usd).sum();
        let mut t: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.key.clone(),
                    r.runs.to_string(),
                    format!("${:.2}", r.cost_usd),
                ]
            })
            .collect();
        t.push(vec![
            "total".into(),
            rows.iter().map(|r| r.runs).sum::<usize>().to_string(),
            format!("${total:.2}"),
        ]);
        util::table(&[by, "runs", "cost"], &t)
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
                        format!("every {}", p.cycle)
                    },
                    format!("{} turns / {}", p.budget.max_turns, p.budget.wall_clock),
                    util::truncate(&p.description, 60),
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
