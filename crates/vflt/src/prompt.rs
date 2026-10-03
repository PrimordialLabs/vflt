//! Prompt assembly for agent turns: the profile prompt, the primitive guide,
//! and the item context.

use vflt_core::{Collective, Item, Note, Profile, Status, Store};

/// The system prompt: profile prompt plus the vflt primitive guide.
pub fn system_prompt(
    profile: &Profile,
    coll: &Collective,
    root: &str,
    agent: &str,
    item: Option<&Item>,
) -> String {
    let mut s = String::new();
    s.push_str(profile.prompt.trim_end());
    s.push_str("\n\n");
    s.push_str(&guide(profile, coll, root, agent, item));
    s
}

/// Exactly which `vflt` commands the agent uses, with literal ids so it works
/// in any shell.
pub fn guide(
    profile: &Profile,
    coll: &Collective,
    root: &str,
    agent: &str,
    item: Option<&Item>,
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
    g.push_str("    vflt board --json\n    vflt item list --json\n    vflt item show <id>\n");
    g.push_str("    vflt item add --title \"...\" --stage <stage> --spec-file spec.md [--dep <id>] [--priority N] [--assign <profile|agent>]\n\n");

    match item {
        Some(it) => {
            let id = it.id();
            let next = coll
                .next_stage(&it.stage)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "done".to_string());
            g.push_str(&format!(
                "The item you are working on is `{id}` (VFLT_ITEM). Record findings as you go:\n\n"
            ));
            g.push_str(&format!(
                "    vflt item note {id} --body \"...\"           # or --body-file notes.md\n\n"
            ));
            g.push_str("Finish with exactly one of these. If you exit without one, the item is marked needs_human automatically.\n\n");
            g.push_str(&format!("    vflt item complete {id} [--note \"...\"]              # spec satisfied; advances to `{next}`\n"));
            if profile.handles(&vflt_core::Stage::new(vflt_core::Stage::REVIEW))
                || profile.handles(&vflt_core::Stage::new(vflt_core::Stage::TEST))
            {
                g.push_str(&format!(
                    "    vflt item complete {id} --bounce --note \"...\"      # not satisfied; back to `{}`\n",
                    coll.bounce_to
                ));
            }
            g.push_str(&format!("    vflt item block {id} --on \"...\"                    # hard dependency, name it precisely\n"));
            g.push_str(&format!(
                "    vflt item raise {id} --question \"...\"              # a human must decide\n"
            ));
        }
        None => {
            g.push_str("This is a cycle turn: there is no single item. Act on the board as a whole with `vflt item add`, `vflt item assign`, `vflt item handoff`, `vflt item reopen`, `vflt item cancel`, and notes. Finish by printing a one-paragraph summary of what you changed.\n");
        }
    }
    g
}

/// The user prompt for an item turn.
pub fn item_prompt(
    store: &dyn Store,
    coll: &Collective,
    item: &Item,
    notes: &[Note],
    workspace: &str,
) -> String {
    let mut p = String::new();
    p.push_str(&format!("# Item {}: {}\n\n", item.id(), item.title));
    p.push_str(&format!(
        "- stage: {}\n- status: {}\n- priority: {}\n- owner: {}\n",
        item.stage, item.status, item.priority, item.owner
    ));
    if !item.tags.is_empty() {
        p.push_str(&format!("- tags: {}\n", item.tags.join(", ")));
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
    p.push_str(&format!("- workspace: {workspace}\n"));
    p.push_str(&format!(
        "- pipeline: {}\n\n",
        coll.pipeline
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" -> ")
    ));
    p.push_str("## Spec\n\n");
    p.push_str(item.spec.trim_end());
    p.push_str("\n\n");
    if notes.is_empty() {
        p.push_str("## Notes\n\n(none yet)\n");
    } else {
        p.push_str("## Notes\n\n");
        for n in notes {
            p.push_str(&format!(
                "### #{} by {} at {}\n\n{}\n\n",
                n.seq,
                n.actor,
                n.at.to_rfc3339(),
                n.body.trim_end()
            ));
        }
    }
    p
}

/// The user prompt for a cycle turn (supervisor).
pub fn cycle_prompt(items: &[Item], coll: &Collective) -> String {
    let mut p = String::new();
    p.push_str(&format!("# Cycle turn for collective {}\n\n", coll.name));
    let open: Vec<&Item> = items.iter().filter(|i| !i.status.is_terminal()).collect();
    let needs: Vec<&Item> = items
        .iter()
        .filter(|i| i.status == Status::NeedsHuman)
        .collect();
    let blocked: Vec<&Item> = items
        .iter()
        .filter(|i| i.status == Status::Blocked)
        .collect();
    p.push_str(&format!(
        "- items: {} total, {} open, {} needs_human, {} blocked\n\n",
        items.len(),
        open.len(),
        needs.len(),
        blocked.len()
    ));
    p.push_str("## Open items\n\n");
    if open.is_empty() {
        p.push_str("(none)\n\n");
    }
    for i in open {
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
    p.push_str(
        "\nRun `vflt item show <id>` for specs and notes. Make the board truthful, then stop.\n",
    );
    p
}
