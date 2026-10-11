//! Prompt assembly for agent turns: the profile prompt, the primitive guide,
//! and the item context.

use vflt_core::{Agent, Collective, Item, Note, Profile, Status, Store};

/// The system prompt: profile prompt plus the vflt primitive guide.
pub fn system_prompt(
    profile: &Profile,
    coll: &Collective,
    root: &str,
    agent: &str,
    items: &[Item],
) -> String {
    let mut s = String::new();
    s.push_str(profile.prompt.trim_end());
    s.push_str("\n\n");
    s.push_str(&guide(profile, coll, root, agent, items));
    s
}

/// Exactly which `vflt` commands the agent uses, with literal ids so it works
/// in any shell.
pub fn guide(
    profile: &Profile,
    coll: &Collective,
    root: &str,
    agent: &str,
    items: &[Item],
) -> String {
    let mut g = String::new();
    g.push_str("## vflt primitives\n\n");
    g.push_str(&format!(
        "You are agent `{agent}` running profile `{}` in collective `{}` (root: `{root}`).\n",
        profile.name, coll.name
    ));
    g.push_str("The environment variables VFLT_COLLECTIVE and VFLT_AGENT are set, so plain `vflt` commands act on this board as you. If `vflt` is not on PATH, run the binary at $VFLT_BIN instead.\n");
    g.push_str("Every vflt command accepts `--json`. The board is the only shared memory: anything you learn that others need goes in a note or a new item.\n\n");
    g.push_str("Useful at any time:\n\n");
    g.push_str("    vflt board\n    vflt item list [--mine] [--status pending,blocked] [--tag t]\n    vflt item show <id>\n    vflt inbox\n");
    g.push_str("    vflt item add --title \"...\" --assign <profile|agent|person> [--spec-file spec.md | --spec \"...\"] [--parent <id>] [--dep <id>] [--tag t] [--priority N] [--draft]\n");
    g.push_str("    vflt item note <id> --body \"...\"            # or --body-file notes.md\n");
    g.push_str("    vflt flag --key <fact> --title \"...\" --body \"...\"   # one item per fleet-wide fact, however many agents hit it\n\n");

    if items.is_empty() {
        g.push_str("This is a cycle turn: no item is assigned to you right now. Act as your standing instructions say. Look over the board and the items you own, then file the next piece of work as an item: `vflt item add --assign ");
        g.push_str(agent);
        g.push_str(" ...` for work you will do yourself (the next turn picks it up with a clean context), or assign it to whoever should do it. Prefer filing over doing the work in this turn, so every change has an item behind it.\n\n");
        g.push_str("If you are caught up and nothing is worth filing, run `vflt agent snooze` so no more cycle turns run until work is routed to you.\n\nFinish by printing a one-paragraph summary of what you changed.\n");
        return g;
    }

    if items.len() == 1 {
        g.push_str(&format!(
            "This turn you hold item `{}` (VFLT_ITEM).",
            items[0].id()
        ));
    } else {
        g.push_str(&format!(
            "This turn you hold {} items (VFLT_ITEMS): {}.",
            items.len(),
            items
                .iter()
                .map(|i| format!("`{}`", i.id()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    g.push_str(" Record findings as you go with `vflt item note`.\n\n");
    g.push_str("Finish every item you hold with exactly one of these. An item left in progress when you exit is raised to a human with the tail of your transcript.\n\n");
    for it in items {
        let id = it.id();
        let next = coll
            .next_stage(&it.stage)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "done".to_string());
        let back = match &it.origin {
            Some(o) => format!(
                "back to `{}`{}",
                o.stage,
                o.assignee
                    .as_ref()
                    .map(|a| format!(" for {a}"))
                    .unwrap_or_default()
            ),
            None => format!("back to `{}`", coll.bounce_to),
        };
        if items.len() > 1 {
            g.push_str(&format!("For `{id}`:\n\n"));
        }
        g.push_str(&format!("    vflt item complete {id} [--note \"...\"]                # done; advances to `{next}`\n"));
        g.push_str(&format!("    vflt item complete {id} --bounce --note \"...\"         # not acceptable as handed over; {back}\n"));
        g.push_str(&format!("    vflt item wait {id} --on <other-id> [--note \"...\"]    # parked until that item is done or cancelled\n"));
        g.push_str(&format!("    vflt item handoff {id} --to <actor> [--note \"...\"]    # someone else should act next\n"));
        g.push_str(&format!("    vflt item raise {id} --question \"...\" [--kind action] # a human must decide (decision) or do something (action)\n"));
        g.push_str(&format!("    vflt item block {id} --on \"...\"                       # waiting on something no item will ever clear\n\n"));
    }
    g
}

fn item_section(store: &dyn Store, item: &Item, notes: &[Note], heading: &str) -> String {
    let mut p = String::new();
    p.push_str(&format!("{heading} {}: {}\n\n", item.id(), item.title));
    p.push_str(&format!(
        "- stage: {}\n- status: {}\n- priority: {}\n- owner: {}\n",
        item.stage, item.status, item.priority, item.owner
    ));
    if item.returned {
        p.push_str(&format!(
            "- returned: this came back to you by a bounce ({} so far); the latest notes say why\n",
            item.bounces
        ));
    }
    if let Some(sm) = &item.summary {
        p.push_str(&format!("- summary: {sm}\n"));
    }
    if !item.tags.is_empty() {
        p.push_str(&format!("- tags: {}\n", item.tags.join(", ")));
    }
    if let Some(parent) = &item.parent {
        let t = store
            .get_item(parent)
            .map(|i| format!("{} ({})", i.title, i.status))
            .unwrap_or_else(|_| "unknown".into());
        p.push_str(&format!("- follows up on: {parent}: {t}\n"));
    }
    if !item.deps.is_empty() {
        p.push_str("- depends on:\n");
        for d in &item.deps {
            let status = store
                .get_item(d)
                .map(|i| format!("{} ({}, {})", i.title, i.stage, i.status))
                .unwrap_or_else(|_| "unknown".to_string());
            p.push_str(&format!("  - {d}: {status}\n"));
        }
    }
    if let Some(w) = &item.waiting_on {
        p.push_str(&format!("- previously waiting on: {w}\n"));
    }
    p.push_str("\n### Spec\n\n");
    p.push_str(item.spec.trim_end());
    p.push_str("\n\n");
    if notes.is_empty() {
        p.push_str("### Notes\n\n(none yet)\n\n");
    } else {
        p.push_str("### Notes\n\n");
        for n in notes {
            p.push_str(&format!(
                "#### #{} by {} at {}\n\n{}\n\n",
                n.seq,
                n.actor,
                n.at.to_rfc3339(),
                n.body.trim_end()
            ));
        }
    }
    p
}

/// The user prompt for an item turn (one item or a batch).
pub fn items_prompt(
    store: &dyn Store,
    coll: &Collective,
    items: &[(Item, Vec<Note>)],
    workspace: &str,
) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "- workspace: {workspace}\n- pipeline: {}\n\n",
        coll.pipeline
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    ));
    if items.len() > 1 {
        p.push_str(&format!(
            "You hold {} items this turn. Work them in the order given and finish each one.\n\n",
            items.len()
        ));
    }
    for (item, notes) in items {
        let heading = if items.len() > 1 { "## Item" } else { "# Item" };
        p.push_str(&item_section(store, item, notes, heading));
    }
    p
}

/// The user prompt for a cycle turn.
pub fn cycle_prompt(items: &[Item], coll: &Collective, agent: &Agent, profile: &Profile) -> String {
    let me = agent.id().as_str();
    let mine = |i: &&&Item| {
        i.owner.as_str() == me
            || i.assignee
                .as_ref()
                .is_some_and(|a| a.as_str() == me || a.as_str() == profile.name)
    };
    let mut p = String::new();
    p.push_str(&format!(
        "# Cycle turn for {me} in collective {}\n\n",
        coll.name
    ));
    let open: Vec<&Item> = items.iter().filter(|i| i.status.is_live()).collect();
    let needs = open
        .iter()
        .filter(|i| i.status == Status::NeedsHuman)
        .count();
    let blocked = open.iter().filter(|i| i.status == Status::Blocked).count();
    p.push_str(&format!(
        "- board: {} items, {} open, {} needs_human, {} blocked\n\n",
        items.len(),
        open.len(),
        needs,
        blocked
    ));
    let yours: Vec<&&Item> = open.iter().filter(mine).collect();
    p.push_str("## Open items you own or that are routed to you\n\n");
    if yours.is_empty() {
        p.push_str("(none)\n\n");
    }
    for i in yours {
        p.push_str(&format!(
            "- {} [{} / {} / p{}] {}{}\n",
            i.id(),
            i.stage,
            i.status,
            i.priority,
            i.title,
            i.assignee
                .as_ref()
                .map(|a| format!(" -> {a}"))
                .unwrap_or_default()
        ));
        if let Some(w) = &i.waiting_on {
            p.push_str(&format!("    waiting on: {w}\n"));
        }
    }
    if profile.is_cycle_only() {
        p.push_str("\n## Every open item\n\n");
        for i in &open {
            p.push_str(&format!(
                "- {} [{} / {} / p{}] {}{}\n",
                i.id(),
                i.stage,
                i.status,
                i.priority,
                i.title,
                i.assignee
                    .as_ref()
                    .map(|a| format!(" -> {a}"))
                    .unwrap_or_default()
            ));
        }
    }
    p.push_str(
        "\nRun `vflt item show <id>` for specs and notes. Make the board truthful, then stop.\n",
    );
    p
}
