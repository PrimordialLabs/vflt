use anyhow::{bail, Result};
use serde::Serialize;
use vflt_core::{ActorId, ExternalRef, Item, ItemFilter, NewItem, Note, Stage, Status, Transition};

use crate::cli::ItemCmd;
use crate::ctx::Ctx;
use crate::identity;
use crate::util;

pub fn run(ctx: &Ctx, cmd: ItemCmd) -> Result<()> {
    let store = ctx.store();
    let me = &ctx.actor;
    match cmd {
        ItemCmd::Add {
            title,
            stage,
            priority,
            assign,
            deps,
            tags,
            spec,
        } => {
            let spec_text = match (spec.spec, spec.spec_file) {
                (Some(s), _) => s,
                (None, Some(p)) => util::read_text(&p)?,
                (None, None) => String::new(),
            };
            let deps = deps
                .iter()
                .map(|d| ctx.resolve_id(d))
                .collect::<Result<Vec<_>>>()?;
            let item = store.create_item(
                &spec_text,
                NewItem {
                    title,
                    stage: stage.map(Stage::new),
                    priority,
                    owner: me.clone(),
                    assignee: assign.map(ActorId::new),
                    tags,
                    deps,
                    external: None,
                },
            )?;
            ctx.emit(&item, || {
                format!(
                    "{} {} [{} / {}]\n",
                    item.id(),
                    item.title,
                    item.stage,
                    item.status
                )
            })
        }
        ItemCmd::List {
            stage,
            status,
            assignee,
            mine,
            owner,
            tag,
            all,
        } => {
            let status = status
                .map(|s| s.parse::<Status>().map_err(anyhow::Error::msg))
                .transpose()?;
            let filter = ItemFilter {
                stage: stage.map(Stage::new),
                status,
                assignee: if mine {
                    Some(me.clone())
                } else {
                    assignee.map(ActorId::new)
                },
                owner: owner.map(ActorId::new),
                tag,
            };
            let mut items = store.list_items(&filter)?;
            if !all && filter.status.is_none() {
                items.retain(|i| !i.status.is_terminal());
            }
            ctx.emit(&items, || list_table(&items))
        }
        ItemCmd::Show { id } => {
            let id = ctx.resolve_id(&id)?;
            let item = store.get_item(&id)?;
            let notes = store.notes(&id)?;
            #[derive(Serialize)]
            struct Out<'a> {
                #[serde(flatten)]
                item: &'a Item,
                notes: &'a [Note],
            }
            ctx.emit(
                &Out {
                    item: &item,
                    notes: &notes,
                },
                || show_text(&item, &notes),
            )
        }
        ItemCmd::Claim { id, me: as_me } => {
            let id = ctx.resolve_id(&id)?;
            let actor = if as_me {
                ActorId::new(identity::username())
            } else {
                me.clone()
            };
            let claim = store.claim(&id, &actor)?;
            ctx.emit(&claim, || format!("{} claimed {}\n", claim.actor, id))
        }
        ItemCmd::Start { id } => transition(ctx, &id, Transition::Start),
        ItemCmd::Note {
            id,
            body,
            body_file,
        } => {
            let id = ctx.resolve_id(&id)?;
            let text = match (body, body_file) {
                (Some(b), _) => b,
                (None, Some(p)) => util::read_text(&p)?,
                (None, None) => util::read_stdin()?,
            };
            if text.trim().is_empty() {
                bail!("note body is empty");
            }
            let n = store.add_note(&id, me, &text)?;
            ctx.emit(&n, || format!("note #{} added to {}\n", n.seq, id))
        }
        ItemCmd::Complete {
            id,
            bounce,
            to_stage,
            note,
        } => transition(
            ctx,
            &id,
            Transition::Complete {
                bounce,
                to_stage: to_stage.map(Stage::new),
                note,
            },
        ),
        ItemCmd::Block { id, on } => transition(ctx, &id, Transition::Block { on }),
        ItemCmd::Raise { id, question } => transition(ctx, &id, Transition::Raise { question }),
        ItemCmd::Handoff { id, to, stage } => transition(
            ctx,
            &id,
            Transition::Handoff {
                to: ActorId::new(to),
                stage: stage.map(Stage::new),
            },
        ),
        ItemCmd::Reopen { id } => transition(ctx, &id, Transition::Reopen),
        ItemCmd::Cancel { id } => transition(ctx, &id, Transition::Cancel),
        ItemCmd::Release { id, force } => {
            let id = ctx.resolve_id(&id)?;
            if force {
                let c = store.break_claim(&id, me)?;
                ctx.emit(&c, || {
                    format!("broke claim on {} held by {}\n", id, c.actor)
                })
            } else {
                store.release(&id, me)?;
                let item = store.get_item(&id)?;
                ctx.emit(&item, || format!("released {} ({})\n", id, item.status))
            }
        }
        ItemCmd::Link {
            id,
            system,
            key,
            url,
        } => {
            let id = ctx.resolve_id(&id)?;
            let item = store.link_external(&id, me, ExternalRef { system, key, url })?;
            let e = item.external.clone().unwrap();
            ctx.emit(&item, || {
                format!("{} linked to {}:{}\n", id, e.system, e.key)
            })
        }
        ItemCmd::Assign { id, actor, none } => {
            let id = ctx.resolve_id(&id)?;
            let to = match (actor, none) {
                (Some(a), _) => Some(ActorId::new(a)),
                (None, true) => None,
                (None, false) => bail!("give an actor, or --none to return the item to the pool"),
            };
            let item = store.set_assignee(&id, me, to)?;
            ctx.emit(&item, || {
                format!(
                    "{} assigned to {}\n",
                    id,
                    item.assignee
                        .as_ref()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "(pool)".into())
                )
            })
        }
    }
}

fn transition(ctx: &Ctx, id: &str, t: Transition) -> Result<()> {
    let id = ctx.resolve_id(id)?;
    let verb = t.verb();
    let item = ctx.store().transition(&id, &ctx.actor, t)?;
    ctx.emit(&item, || {
        format!("{verb} {} -> {} at {}\n", id, item.status, item.stage)
    })
}

fn list_table(items: &[Item]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|i| {
            vec![
                i.id().to_string(),
                i.stage.to_string(),
                i.status.to_string(),
                i.priority.to_string(),
                i.assignee
                    .as_ref()
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
                util::truncate(&i.title, 60),
            ]
        })
        .collect();
    util::table(
        &["id", "stage", "status", "prio", "assignee", "title"],
        &rows,
    )
}

fn show_text(item: &Item, notes: &[Note]) -> String {
    let mut s = format!("{}  {}\n", item.id(), item.title);
    s.push_str(&format!(
        "stage: {}   status: {}   priority: {}\nowner: {}   assignee: {}\n",
        item.stage,
        item.status,
        item.priority,
        item.owner,
        item.assignee
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(pool)".into())
    ));
    if let Some(c) = &item.claim {
        s.push_str(&format!(
            "claim: {} since {} ago\n",
            c.actor,
            util::age(c.at)
        ));
    }
    if !item.tags.is_empty() {
        s.push_str(&format!("tags: {}\n", item.tags.join(", ")));
    }
    if !item.deps.is_empty() {
        s.push_str(&format!(
            "deps: {}\n",
            item.deps
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(e) = &item.external {
        s.push_str(&format!(
            "external: {}:{}{}\n",
            e.system,
            e.key,
            e.url.as_ref().map(|u| format!(" {u}")).unwrap_or_default()
        ));
    }
    if let Some(w) = &item.waiting_on {
        s.push_str(&format!("waiting on: {w}\n"));
    }
    s.push_str(&format!(
        "created: {}   updated: {}\n",
        item.created.to_rfc3339(),
        item.updated.to_rfc3339()
    ));
    s.push_str("\n--- spec ---\n");
    s.push_str(item.spec.trim_end());
    s.push('\n');
    for n in notes {
        s.push_str(&format!(
            "\n--- note #{} by {} ({} ago) ---\n{}\n",
            n.seq,
            n.actor,
            util::age(n.at),
            n.body.trim_end()
        ));
    }
    s
}
