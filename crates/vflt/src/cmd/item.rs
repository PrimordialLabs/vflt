use anyhow::{bail, Result};
use serde::Serialize;
use vflt_core::{
    ActorId, Event, EventFilter, ExternalRef, Item, ItemFilter, ItemUpdate, NewItem, Note,
    RaiseKind, Stage, Status, Transition,
};

use crate::cli::{ItemCmd, SpecInput};
use crate::ctx::Ctx;
use crate::identity;
use crate::util;

fn spec_text(spec: SpecInput) -> Result<Option<String>> {
    Ok(match (spec.spec, spec.spec_file) {
        (Some(s), _) => Some(s),
        (None, Some(p)) => Some(util::read_text(&p)?),
        (None, None) => None,
    })
}

fn parse_statuses(v: &[String]) -> Result<Vec<Status>> {
    v.iter()
        .map(|s| s.trim().parse::<Status>().map_err(anyhow::Error::msg))
        .collect()
}

pub fn run(ctx: &Ctx, cmd: ItemCmd) -> Result<()> {
    let store = ctx.store();
    let me = &ctx.actor;
    match cmd {
        ItemCmd::Add {
            title,
            slug,
            summary,
            stage,
            priority,
            assign,
            deps,
            tags,
            parent,
            draft,
            spec,
        } => {
            let spec_text = spec_text(spec)?.unwrap_or_default();
            let deps = ctx.resolve_ids(&deps)?;
            let parent = parent.map(|p| ctx.resolve_id(&p)).transpose()?;
            let item = store.create_item(
                &spec_text,
                NewItem {
                    title,
                    slug,
                    summary,
                    stage: stage.map(Stage::new),
                    priority,
                    owner: me.clone(),
                    assignee: assign.map(ActorId::new),
                    tags,
                    deps,
                    external: None,
                    parent,
                    key: None,
                    draft,
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
            not_status,
            assignee,
            mine,
            owner,
            tag,
            not_tag,
            parent,
            key,
            stale,
            all,
            sort,
            desc,
            limit,
            offset,
        } => {
            let statuses = parse_statuses(&status)?;
            let not_statuses = parse_statuses(&not_status)?;
            let updated_before = stale.map(|s| util::parse_since(&s)).transpose()?;
            let filter = ItemFilter {
                stage: stage.map(Stage::new),
                statuses: statuses.clone(),
                assignee: if mine {
                    Some(me.clone())
                } else {
                    assignee.map(ActorId::new)
                },
                owner: owner.map(ActorId::new),
                tags: tag,
                key,
                parent: parent.map(|p| ctx.resolve_id(&p)).transpose()?,
                updated_before,
            };
            let mut items = store.list_items(&filter)?;
            if !all && statuses.is_empty() {
                items.retain(|i| i.status.is_live());
            }
            if !not_statuses.is_empty() {
                items.retain(|i| !not_statuses.contains(&i.status));
            }
            if !not_tag.is_empty() {
                let not_tag: Vec<String> =
                    not_tag.iter().map(|t| t.trim().to_lowercase()).collect();
                items.retain(|i| !i.tags.iter().any(|t| not_tag.contains(t)));
            }
            match sort.as_str() {
                "priority" => {}
                "created" => {
                    items.sort_by(|a, b| a.created.cmp(&b.created).then(a.id().cmp(b.id())))
                }
                "updated" => {
                    items.sort_by(|a, b| a.updated.cmp(&b.updated).then(a.id().cmp(b.id())))
                }
                other => bail!("--sort must be priority, created or updated (got {other})"),
            }
            if desc {
                items.reverse();
            }
            let items: Vec<Item> = items
                .into_iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
                .collect();
            ctx.emit(&items, || list_table(&items))
        }
        ItemCmd::Show { id, events } => {
            let id = ctx.resolve_id(&id)?;
            let item = store.get_item(&id)?;
            let notes = store.notes(&id)?;
            let evs = if events {
                Some(store.events(&EventFilter {
                    item: Some(id.clone()),
                    ..Default::default()
                })?)
            } else {
                None
            };
            #[derive(Serialize)]
            struct Out<'a> {
                #[serde(flatten)]
                item: &'a Item,
                notes: &'a [Note],
                #[serde(skip_serializing_if = "Option::is_none")]
                events: Option<&'a [Event]>,
            }
            ctx.emit(
                &Out {
                    item: &item,
                    notes: &notes,
                    events: evs.as_deref(),
                },
                || show_text(&item, &notes, evs.as_deref()),
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
        ItemCmd::Start { id } => transition(ctx, &id, Transition::Start, None),
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
            None,
        ),
        ItemCmd::Block { id, on } => transition(ctx, &id, Transition::Block { on }, None),
        ItemCmd::Raise { id, question, kind } => {
            let kind: RaiseKind = kind.parse().map_err(anyhow::Error::msg)?;
            transition(ctx, &id, Transition::Raise { question, kind }, None)
        }
        ItemCmd::Handoff {
            id,
            to,
            stage,
            note,
        } => transition(
            ctx,
            &id,
            Transition::Handoff {
                to: ActorId::new(to),
                stage: stage.map(Stage::new),
            },
            note,
        ),
        ItemCmd::Wait { id, on, note } => {
            let on = ctx.resolve_ids(&on)?;
            transition(ctx, &id, Transition::Wait { on }, note)
        }
        ItemCmd::Reopen { id, to } => transition(
            ctx,
            &id,
            Transition::Reopen {
                to: to.map(ActorId::new),
            },
            None,
        ),
        ItemCmd::Respond {
            id,
            note,
            note_only,
        } => respond(ctx, &id, &note, note_only),
        ItemCmd::Promote { id } => transition(ctx, &id, Transition::Promote, None),
        ItemCmd::Demote { id } => transition(ctx, &id, Transition::Demote, None),
        ItemCmd::Cancel { id, note } => transition(ctx, &id, Transition::Cancel, note),
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
        ItemCmd::Set {
            id,
            title,
            summary,
            priority,
            parent,
            no_parent,
            spec,
        } => {
            let id = ctx.resolve_id(&id)?;
            let parent = if no_parent {
                Some(None)
            } else {
                parent.map(|p| ctx.resolve_id(&p).map(Some)).transpose()?
            };
            let update = ItemUpdate {
                title,
                summary,
                priority,
                parent,
                spec: spec_text(spec)?,
                ..Default::default()
            };
            if update.is_empty() {
                bail!("nothing to change: give --title, --summary, --priority, --parent/--no-parent or a spec");
            }
            let item = store.update_item(&id, me, update)?;
            ctx.emit(&item, || format!("updated {}\n", id))
        }
        ItemCmd::Tag { id, add, remove } => {
            let id = ctx.resolve_id(&id)?;
            if add.is_empty() && remove.is_empty() {
                bail!("give --add and/or --remove");
            }
            let item = store.update_item(
                &id,
                me,
                ItemUpdate {
                    add_tags: add,
                    remove_tags: remove,
                    ..Default::default()
                },
            )?;
            ctx.emit(&item, || format!("{} tags: {}\n", id, item.tags.join(", ")))
        }
        ItemCmd::Dep { id, add, remove } => {
            let id = ctx.resolve_id(&id)?;
            if add.is_empty() && remove.is_empty() {
                bail!("give --add and/or --remove");
            }
            let item = store.update_item(
                &id,
                me,
                ItemUpdate {
                    add_deps: ctx.resolve_ids(&add)?,
                    remove_deps: ctx.resolve_ids(&remove)?,
                    ..Default::default()
                },
            )?;
            ctx.emit(&item, || {
                format!(
                    "{} deps: {}\n",
                    id,
                    item.deps
                        .iter()
                        .map(|d| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        }
    }
}

/// Apply a transition, attaching `note` (when given) first so it lands in the
/// trail before the move it explains.
fn transition(ctx: &Ctx, id: &str, t: Transition, note: Option<String>) -> Result<()> {
    let id = ctx.resolve_id(id)?;
    if let Some(n) = note.as_deref().filter(|n| !n.trim().is_empty()) {
        ctx.store().add_note(&id, &ctx.actor, n)?;
    }
    let verb = t.verb();
    let item = ctx.store().transition(&id, &ctx.actor, t)?;
    ctx.emit(&item, || {
        format!(
            "{verb} {} -> {} at {}{}\n",
            id,
            item.status,
            item.stage,
            item.assignee
                .as_ref()
                .map(|a| format!(" ({a})"))
                .unwrap_or_default()
        )
    })
}

/// Answer an item. The note always lands; then a raised or blocked item is
/// reopened for its assignee, and a pending item resting with the responder
/// goes back to its owner, so an answer never strands with the person who
/// gave it.
fn respond(ctx: &Ctx, id: &str, note: &str, note_only: bool) -> Result<()> {
    let store = ctx.store();
    let id = ctx.resolve_id(id)?;
    if note.trim().is_empty() {
        bail!("--note is empty");
    }
    let before = store.get_item(&id)?;
    store.add_note(&id, &ctx.actor, note)?;
    let mut requeued = false;
    if !note_only {
        match before.status {
            Status::Blocked | Status::NeedsHuman => {
                let to =
                    if before.assignee.as_ref() == Some(&ctx.actor) || before.assignee.is_none() {
                        Some(before.owner.clone()).filter(|o| o != &ctx.actor)
                    } else {
                        None
                    };
                store.transition(&id, &ctx.actor, Transition::Reopen { to })?;
                requeued = true;
            }
            Status::Pending
                if before.assignee.as_ref() == Some(&ctx.actor) && before.owner != ctx.actor =>
            {
                store.transition(
                    &id,
                    &ctx.actor,
                    Transition::Handoff {
                        to: before.owner.clone(),
                        stage: None,
                    },
                )?;
                requeued = true;
            }
            _ => {}
        }
    }
    let after = store.get_item(&id)?;
    #[derive(Serialize)]
    struct Out {
        id: String,
        requeued: bool,
        was_status: Status,
        status: Status,
        assignee: Option<ActorId>,
        note_only: bool,
    }
    let out = Out {
        id: id.to_string(),
        requeued,
        was_status: before.status,
        status: after.status,
        assignee: after.assignee.clone(),
        note_only,
    };
    ctx.emit(&out, || {
        if requeued {
            format!(
                "answered {}; now {} for {}\n",
                id,
                out.status,
                out.assignee
                    .as_ref()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "(pool)".into())
            )
        } else {
            format!("noted {} ({})\n", id, out.status)
        }
    })
}

pub fn list_table(items: &[Item]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|i| {
            vec![
                i.id().to_string(),
                i.stage.to_string(),
                format!("{}{}", i.status, if i.returned { "*" } else { "" }),
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

fn show_text(item: &Item, notes: &[Note], events: Option<&[Event]>) -> String {
    let mut s = format!("{}  {}\n", item.id(), item.title);
    s.push_str(&format!(
        "stage: {}   status: {}{}   priority: {}\nowner: {}   assignee: {}\n",
        item.stage,
        item.status,
        if item.returned { " (returned)" } else { "" },
        item.priority,
        item.owner,
        item.assignee
            .as_ref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "(pool)".into())
    ));
    if let Some(sm) = &item.summary {
        s.push_str(&format!("summary: {sm}\n"));
    }
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
    if let Some(p) = &item.parent {
        s.push_str(&format!("parent: {p}\n"));
    }
    if let Some(o) = &item.origin {
        s.push_str(&format!(
            "bounces to: {}{}\n",
            o.stage,
            o.assignee
                .as_ref()
                .map(|a| format!(" ({a})"))
                .unwrap_or_default()
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
        s.push_str(&format!(
            "waiting on{}: {w}\n",
            item.raise_kind
                .map(|k| format!(" ({})", k.as_str()))
                .unwrap_or_default()
        ));
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
    if let Some(evs) = events {
        s.push_str("\n--- events ---\n");
        for e in evs {
            s.push_str(&format!(
                "{}  {}  {}\n",
                e.ts.format("%Y-%m-%d %H:%M:%S"),
                e.kind,
                e.summary
            ));
        }
    }
    s
}
